//! Tauri IPC bridge — sender commands.
//!
//! Implements the Tauri command surface for the screen-mirror sender:
//! `start_sender`, `stop_sender`, `sender_diagnostics`.
//!
//! # Architecture
//!
//! The bridge owns a `SenderBridge` state container (managed by Tauri) that holds:
//! - The active `SenderSession` (pipeline + drain threads).
//! - Bookkeeping counters (`dropped_frames_encoder`, `dropped_frames_transport`, etc.).
//!
//! A `SenderBuilderFn` injection seam enables cross-platform tests (R17): tests
//! inject fake adapters; production uses `build_production_sender_bundle` (Windows-only).
//!
//! # IPC channel protocol
//!
//! The sender emits JSON status events over `Channel<InvokeResponseBody>`.
//! Unlike the receiver (which uses binary `send_raw` for fMP4 segments), the sender
//! sends all messages as JSON bytes via `send_raw(0, json_bytes)`.
//! This avoids adding a `send_json` method to the shared `ChannelLike` trait and
//! keeps the receiver's binary path intact.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use sm_domain::signaling::{IceCandidate, SdpAnswer, SignalingEvent};
use sm_domain::transport::{TransportError, TransportEvent};
use tauri::ipc::InvokeResponseBody;

use crate::commands::stream::{BundleError, ChannelLike, PortRejectReason};

// ─── SenderBuilderFn — injectable seam for SenderBundle construction ──────────

/// Factory closure type: produces a fully-started `SenderBundle` given runtime
/// args `(udp_port, service_name, stop_flag, channel)`.
///
/// 4-arg form (Amendment A): no `BindCtx` — the sender binds on port 0 (ephemeral)
/// inside `Str0mVideoSender::start()` directly. No pre-bind probe for the sender.
///
/// Production: wraps `build_production_sender_bundle` (Windows-only).
/// Tests inject a closure returning a fake bundle with cross-platform fake adapters.
pub(crate) type SenderBuilderFn = Arc<
    dyn Fn(u16, String, Arc<AtomicBool>, Arc<dyn ChannelLike>) -> Result<SenderBundle, BundleError>
        + Send
        + Sync,
>;

// ─── SenderBundle — result of SenderBuilderFn ─────────────────────────────────

/// The fully-initialised sender pipeline returned by `SenderBuilderFn`.
///
/// Fields held in `SenderSession` after bring-up.
/// `frame_tx_owned` / `enc_tx_owned` are NOT retained — both are consumed by
/// `CaptureSource::start` / `VideoEncoder::start`. Teardown relies on
/// `capture.stop()` → rx-disconnect chain (design §6 correction).
pub struct SenderBundle {
    /// Drain thread handles (signaling drain + transport event drain).
    pub(crate) drain_handles: Vec<JoinHandle<()>>,
}

impl SenderBundle {
    /// Construct a minimal bundle suitable for unit tests.
    /// Spawns no real threads; drain_handles is empty.
    pub fn test_stub() -> Self {
        Self {
            drain_handles: vec![],
        }
    }
}

// ─── SenderCounters — live telemetry atomics ──────────────────────────────────

/// Atomic counters shared between the drain threads and `sender_diagnostics`.
#[derive(Debug, Default)]
pub struct SenderCounters {
    pub dropped_frames_encoder: AtomicU64,
    pub dropped_frames_transport: AtomicU64,
    pub keyframe_requests_received: AtomicU64,
}

// ─── SenderArgs — args of the currently-active session ───────────────────────

/// Stored in `SenderBridge::current_args` while a session is active.
#[derive(Clone, Debug)]
pub(crate) struct SenderArgs {
    pub(crate) udp_port: u16,
    pub(crate) service_name: String,
}

// ─── SenderSession — active pipeline state ───────────────────────────────────

/// Holds all resources for one active sender session.
pub struct SenderSession {
    pub(crate) stop_flag: Arc<AtomicBool>,
    pub(crate) drain_handles: Vec<JoinHandle<()>>,
    pub(crate) channel: Arc<dyn ChannelLike>,
    pub(crate) counters: Arc<SenderCounters>,
}

// ─── SenderBridge — Tauri managed state ──────────────────────────────────────

/// Tauri managed state for an active sender session.
///
/// Held behind `State<SenderBridge>` in Tauri commands.
pub struct SenderBridge {
    pub session: Mutex<Option<SenderSession>>,
    pub(crate) builder: SenderBuilderFn,
    pub(crate) current_args: Mutex<Option<SenderArgs>>,
}

impl SenderBridge {
    /// Create a bridge using the production `build_production_sender_bundle` factory.
    pub fn new() -> Self {
        Self::new_with_builder(Arc::new(
            |udp_port, service_name, stop_flag, channel| {
                build_production_sender_bundle(udp_port, service_name, stop_flag, channel)
            },
        ))
    }

    /// Create a bridge with a custom builder factory (test seam, R17).
    pub(crate) fn new_with_builder(builder: SenderBuilderFn) -> Self {
        Self {
            session: Mutex::new(None),
            builder,
            current_args: Mutex::new(None),
        }
    }
}

impl Default for SenderBridge {
    fn default() -> Self {
        Self::new()
    }
}

// ─── StartSenderError — typed error enum ─────────────────────────────────────

/// Typed error returned by `start_sender`.
///
/// Mirrors `StartStreamError` with `#[serde(tag = "kind", content = "data")]`
/// to match the existing receiver convention (stream.rs:284).
#[derive(Debug, thiserror::Error, serde::Serialize)]
#[serde(tag = "kind", content = "data")]
pub enum StartSenderError {
    /// A session is already active.
    #[error("sender already running on port {udp_port} ({service_name})")]
    AlreadyRunning { udp_port: u16, service_name: String },

    /// `udp_port` failed validation (privileged port 1–1023).
    #[error("invalid udp_port {value}: {reason:?}")]
    InvalidPort {
        value: u16,
        reason: PortRejectReason,
    },

    /// `service_name` failed RFC 6763 validation.
    #[error("invalid service_name {value:?}: {reason}")]
    InvalidServiceName { value: String, reason: String },

    /// The OS-level socket bind failed (e.g. AddrInUse).
    #[error("UDP port {port} is already in use")]
    PortInUse { port: u16 },

    /// Catch-all for failures inside `SenderBuilderFn`.
    #[error("bundle build failed: {0}")]
    BundleBuildFailed(String),
}

// ─── SenderStats — diagnostics payload ───────────────────────────────────────

/// Stats returned by `sender_diagnostics`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SenderStats {
    pub dropped_frames_encoder: u64,
    pub dropped_frames_transport: u64,
    pub keyframe_requests_received: u64,
    pub running: bool,
}

// ─── SenderStatusEvent — internal JSON event shapes ──────────────────────────

/// JSON events emitted over the channel to the frontend.
/// Serialised with snake_case kind tags.
#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SenderStatusEvent {
    Connecting,
    Streaming,
    PeerLost,
    Stopped,
    #[serde(rename = "button")]
    Button { label: String },
    #[serde(rename = "error")]
    Error { message: String },
}

// ─── Validation helpers ───────────────────────────────────────────────────────

/// Validate `udp_port` for the sender.
///
/// Unlike `validate_udp_port` (stream.rs), this ALLOWS port 0 (ephemeral).
/// Only rejects privileged ports 1–1023 (Amendment A).
///
/// - 0        → Ok(()) — OS-assigned ephemeral port
/// - 1..=1023 → Err(InvalidPort { reason: Privileged })
/// - 1024..   → Ok(())
pub(crate) fn validate_udp_port_for_sender(
    value: u16,
) -> Result<(), StartSenderError> {
    if value >= 1 && value < 1024 {
        return Err(StartSenderError::InvalidPort {
            value,
            reason: PortRejectReason::Privileged,
        });
    }
    Ok(())
}

/// Validate `service_name` for the sender.
/// Delegates to the shared `validate_service_name` from stream.rs and adapts
/// the error type to `StartSenderError`.
pub(crate) fn validate_service_name_for_sender(
    s: &str,
) -> Result<(), StartSenderError> {
    crate::commands::stream::validate_service_name(s).map_err(|e| match e {
        crate::commands::stream::StartStreamError::InvalidServiceName { value, reason } => {
            StartSenderError::InvalidServiceName { value, reason }
        }
        other => StartSenderError::BundleBuildFailed(other.to_string()),
    })
}

// ─── emit helpers ─────────────────────────────────────────────────────────────

/// Encode a `SenderStatusEvent` to JSON bytes and send via `ChannelLike::send_raw`.
///
/// Uses `send_raw(0, json_bytes)` directly — avoids modifying the shared
/// `ChannelLike` trait. Discriminant 0 signals "JSON payload" on the sender path
/// (the receiver uses 0x00 for fMP4 init and 0x01 for segments, but those paths
/// never mix with the sender's channel).
fn emit_event(channel: &Arc<dyn ChannelLike>, event: &SenderStatusEvent) {
    if let Ok(bytes) = serde_json::to_vec(event) {
        let _ = channel.send_raw(0, bytes);
    }
}

// ─── SignalingSenderOps — abstraction for signaling drain ─────────────────────

/// Operations the signaling drain thread needs on the sender transport.
pub(crate) trait SignalingSenderOps: Send + Sync {
    fn apply_remote_answer(&self, ans: SdpAnswer) -> Result<(), TransportError>;
    fn add_remote_candidate(&self, c: IceCandidate) -> Result<(), TransportError>;
}

// ─── Drain functions ──────────────────────────────────────────────────────────

/// Signaling-event drain loop for the sender.
///
/// Per Amendment B: the offer was already published before this drain starts;
/// `PeerFound` is log-only (no publish here).
///
/// Exits when stop_flag is set, the channel disconnects, or `Closed` arrives.
pub(crate) fn run_sender_signaling_drain(
    ev_rx: std::sync::mpsc::Receiver<SignalingEvent>,
    sender: Arc<dyn SignalingSenderOps>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                SignalingEvent::PeerFound { host, port } => {
                    eprintln!("[sm-sender-signaling-drain] peer found: {host}:{port}");
                    emit_event(&channel, &SenderStatusEvent::Connecting);
                }
                SignalingEvent::AnswerReceived(ans) => {
                    if let Err(e) = sender.apply_remote_answer(ans) {
                        eprintln!(
                            "[sm-sender-signaling-drain] apply_remote_answer failed: {e}"
                        );
                        emit_event(
                            &channel,
                            &SenderStatusEvent::Error {
                                message: format!("apply_remote_answer failed: {e}"),
                            },
                        );
                    }
                }
                SignalingEvent::CandidateReceived(c) => {
                    if let Err(e) = sender.add_remote_candidate(c) {
                        eprintln!(
                            "[sm-sender-signaling-drain] add_remote_candidate failed: {e}"
                        );
                    }
                }
                SignalingEvent::OfferReceived(_) => {
                    // Sender role: ignore incoming offers.
                }
                SignalingEvent::Closed => {
                    emit_event(&channel, &SenderStatusEvent::PeerLost);
                    break;
                }
                SignalingEvent::Error(e) => {
                    eprintln!("[sm-sender-signaling-drain] signaling error: {e}");
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Transport-event drain loop for the sender.
///
/// Emits JSON status events to the frontend channel.
/// Increments `SenderCounters::keyframe_requests_received` on `KeyframeRequested`.
pub(crate) fn run_sender_transport_event_drain(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    counters: Arc<SenderCounters>,
) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                TransportEvent::IceConnected => {
                    emit_event(&channel, &SenderStatusEvent::Streaming);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Stop streaming".to_string(),
                        },
                    );
                }
                TransportEvent::IceFailed => {
                    emit_event(&channel, &SenderStatusEvent::PeerLost);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Restart".to_string(),
                        },
                    );
                }
                TransportEvent::ConnectionLost { .. } => {
                    emit_event(&channel, &SenderStatusEvent::PeerLost);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Restart".to_string(),
                        },
                    );
                }
                TransportEvent::KeyframeRequested => {
                    counters
                        .keyframe_requests_received
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ─── start_sender_inner — core implementation ─────────────────────────────────

/// Core of `start_sender` — extracted for unit testing without the Tauri runtime.
///
/// Execution order (Amendment A — no bind_probe):
/// 1. Validate udp_port (if Some). port 0 allowed (ephemeral).
/// 2. Validate service_name (if Some).
/// 3. Resolve defaults: udp_port.unwrap_or(0), service_name.unwrap_or(default).
/// 4. Acquire current_args lock; check AlreadyRunning. Release.
/// 5. Allocate stop_flag.
/// 6. Invoke builder(port, name, stop_flag, channel).
/// 7. Store SenderSession + current_args.
/// 8. Emit Connecting status.
pub(crate) fn start_sender_inner(
    bridge: &SenderBridge,
    channel: Arc<dyn ChannelLike>,
    udp_port: Option<u16>,
    service_name: Option<String>,
) -> Result<(), StartSenderError> {
    // Step 1 — validate udp_port.
    if let Some(p) = udp_port {
        validate_udp_port_for_sender(p)?;
    }

    // Step 2 — validate service_name.
    if let Some(ref s) = service_name {
        validate_service_name_for_sender(s)?;
    }

    // Step 3 — resolve defaults (Amendment A: port 0 = ephemeral).
    let resolved_port = udp_port.unwrap_or(0);
    let resolved_name =
        service_name.unwrap_or_else(|| "_screen-mirror._tcp.local.".to_string());

    // Step 4 — AlreadyRunning check.
    {
        let args_guard = bridge.current_args.lock().unwrap();
        if let Some(cur) = &*args_guard {
            return Err(StartSenderError::AlreadyRunning {
                udp_port: cur.udp_port,
                service_name: cur.service_name.clone(),
            });
        }
    }

    // Step 5 — allocate stop_flag and clone builder.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let builder = bridge.builder.clone();

    // Step 6 — invoke builder (no lock held).
    let bundle = (builder)(
        resolved_port,
        resolved_name.clone(),
        stop_flag.clone(),
        channel.clone(),
    )
    .map_err(|e| match e {
        BundleError::PortInUse(port) => StartSenderError::PortInUse { port },
        BundleError::Other(s) => StartSenderError::BundleBuildFailed(s),
    })?;

    // Step 7 — store session and current_args.
    let session = SenderSession {
        stop_flag,
        drain_handles: bundle.drain_handles,
        channel: channel.clone(),
        counters: Arc::new(SenderCounters::default()),
    };
    *bridge.session.lock().unwrap() = Some(session);
    *bridge.current_args.lock().unwrap() = Some(SenderArgs {
        udp_port: resolved_port,
        service_name: resolved_name,
    });

    // Step 8 — emit Connecting status.
    emit_event(&channel, &SenderStatusEvent::Connecting);

    Ok(())
}

// ─── stop_sender_session — ordered teardown ───────────────────────────────────

/// Ordered teardown for an active sender session.
///
/// Idempotent: if no session is active, returns Ok(()) immediately.
/// Mirrors stream.rs stop_stream_session lock ordering: session FIRST, then current_args.
pub(crate) fn stop_sender_session(bridge: &SenderBridge) {
    let session_opt = {
        let mut guard = bridge.session.lock().unwrap();
        guard.take()
    };

    let Some(mut session) = session_opt else {
        return;
    };

    // Set stop flag — signals drain threads.
    session.stop_flag.store(true, Ordering::Relaxed);

    // Join drain threads.
    for h in session.drain_handles.drain(..) {
        let _ = h.join();
    }

    // Emit Stopped event.
    emit_event(&session.channel, &SenderStatusEvent::Stopped);
    drop(session.channel);

    // Clear current_args AFTER session lock is released.
    *bridge.current_args.lock().unwrap() = None;
}

// ─── sender_diagnostics_impl ──────────────────────────────────────────────────

/// Core of `sender_diagnostics` — extracted for unit testing.
pub(crate) fn sender_diagnostics_impl(
    bridge: &SenderBridge,
) -> Result<SenderStats, String> {
    let guard = bridge.session.lock().unwrap();
    match guard.as_ref() {
        None => Err("not running".to_string()),
        Some(s) => Ok(SenderStats {
            dropped_frames_encoder: s
                .counters
                .dropped_frames_encoder
                .load(Ordering::Relaxed),
            dropped_frames_transport: s
                .counters
                .dropped_frames_transport
                .load(Ordering::Relaxed),
            keyframe_requests_received: s
                .counters
                .keyframe_requests_received
                .load(Ordering::Relaxed),
            running: true,
        }),
    }
}

// ─── Production bundle builder (Windows-only skeleton) ────────────────────────

/// Build the production sender bundle.
///
/// Windows-only: `WindowsCaptureSource`, `WindowsOpenH264Encoder`, `Str0mVideoSender`,
/// `MdnsSignaling`. On non-Windows, returns Err immediately (guarded by #[cfg]).
///
/// Known limitation (RD-5): TCP signaling port is 7889 — same as receiver.
/// Running sender + receiver on the same machine will collide on TCP 7889.
/// UDP is ephemeral (port 0) so no UDP collision (Amendment A).
#[cfg(target_os = "windows")]
fn build_production_sender_bundle(
    udp_port: u16,
    service_name: String,
    _stop_flag: Arc<AtomicBool>,
    _channel: Arc<dyn ChannelLike>,
) -> Result<SenderBundle, BundleError> {
    use sm_domain::signaling::{Signaling, SignalingConfig, SignalingRole};
    use sm_domain::transport::{TransportConfig, TransportRole, VideoSender};
    use sm_domain::{
        CaptureConfig, CaptureSource, EncoderConfig, MonitorSelector, VideoEncoder,
    };
    use sm_infra::capture::WindowsCaptureSource;
    use sm_infra::encode::windows::WindowsOpenH264Encoder;
    use sm_infra::signaling::mdns::MdnsSignaling;
    use sm_infra::transport::Str0mVideoSender;
    use std::sync::mpsc::sync_channel;

    const CHANNEL_CAP: usize = 4;

    // ── 1. Build adapters ─────────────────────────────────────────────────────
    let sig_config = SignalingConfig {
        service_name,
        // TCP control port: same number as udp_port per receiver convention.
        // On same-machine setups this may collide with the receiver's TCP 7889.
        control_port: 7889,
        role: SignalingRole::Sender,
        peer_hint: None,
    };
    let mut signaling =
        MdnsSignaling::new(sig_config).map_err(|e| BundleError::Other(e.to_string()))?;

    let capture_config = CaptureConfig {
        monitor: MonitorSelector::Primary,
        max_fps: Some(30),
        ..CaptureConfig::default()
    };
    let mut capture = WindowsCaptureSource::new(capture_config)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    let encoder_config = EncoderConfig::default();
    let mut encoder = WindowsOpenH264Encoder::new(encoder_config)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    let transport_config = TransportConfig {
        udp_port,
        role: TransportRole::Sender,
        ..TransportConfig::default()
    };
    let mut sender = Str0mVideoSender::new(transport_config)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    // ── 2. Channels ───────────────────────────────────────────────────────────
    let (capture_to_enc_tx, capture_to_enc_rx) = sync_channel(CHANNEL_CAP);
    let (enc_to_sender_tx, enc_to_sender_rx) = sync_channel(CHANNEL_CAP);
    let (sig_ev_tx, sig_ev_rx) = sync_channel(CHANNEL_CAP);
    let (tr_ev_tx, tr_ev_rx) = sync_channel(CHANNEL_CAP);

    // ── 3. Start pipeline (canonical order from design §9) ───────────────────
    signaling
        .start(sig_ev_tx)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    capture
        .start(capture_to_enc_tx)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    encoder
        .start(capture_to_enc_rx, enc_to_sender_tx)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    let encoder_arc: Arc<dyn VideoEncoder + Send + Sync> = Arc::new(encoder);
    sender.set_encoder(Arc::clone(&encoder_arc));

    sender
        .start(enc_to_sender_rx, tr_ev_tx)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    let offer = sender
        .create_local_offer()
        .map_err(|e| BundleError::Other(e.to_string()))?;

    // Publish offer immediately (Amendment B — buffers in inbox; written on connect).
    signaling
        .publish_local_offer(offer)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    // ── 4. Wrap in Arc<Mutex<>> for drain thread sharing ──────────────────────
    let sender_arc = Arc::new(Mutex::new(sender));
    let signaling_arc = Arc::new(Mutex::new(signaling));

    struct Str0mSenderOpsImpl(Arc<Mutex<Str0mVideoSender>>);
    impl SignalingSenderOps for Str0mSenderOpsImpl {
        fn apply_remote_answer(&self, ans: SdpAnswer) -> Result<(), TransportError> {
            self.0.lock().unwrap().apply_remote_answer(ans)
        }
        fn add_remote_candidate(&self, c: IceCandidate) -> Result<(), TransportError> {
            self.0.lock().unwrap().add_remote_candidate(c)
        }
    }

    let sender_ops: Arc<dyn SignalingSenderOps> =
        Arc::new(Str0mSenderOpsImpl(sender_arc.clone()));

    let counters = Arc::new(SenderCounters::default());

    // ── 5. Spawn drain threads ────────────────────────────────────────────────
    let stop_flag = _stop_flag.clone();
    let sig_channel = _channel.clone();
    let tr_channel = _channel.clone();
    let tr_counters = counters.clone();
    let sig_stop = stop_flag.clone();
    let tr_stop = stop_flag.clone();

    let sig_drain = std::thread::Builder::new()
        .name("sm-sender-signaling-drain".into())
        .spawn(move || {
            run_sender_signaling_drain(sig_ev_rx, sender_ops, sig_stop, sig_channel);
        })
        .map_err(|e| BundleError::Other(format!("spawn sig drain: {e}")))?;

    let tr_drain = std::thread::Builder::new()
        .name("sm-sender-transport-drain".into())
        .spawn(move || {
            run_sender_transport_event_drain(tr_ev_rx, tr_stop, tr_channel, tr_counters);
        })
        .map_err(|e| BundleError::Other(format!("spawn transport drain: {e}")))?;

    // Keep signaling and sender arcs alive in closures / session fields.
    // Drop capture here — capture.stop() called by capture's Drop impl on session tear-down.
    // The session captures these arcs to extend their lifetimes.
    drop(capture); // Drop impl: capture thread runs until encoder rx closes.
    drop(sender_arc); // Drop impl: Str0mVideoSender::stop() joins tick thread.
    drop(signaling_arc); // Drop impl: MdnsSignaling::stop() joins signaling thread.
    drop(encoder_arc); // Drop impl: WindowsOpenH264Encoder::stop() joins encoder thread.

    Ok(SenderBundle {
        drain_handles: vec![sig_drain, tr_drain],
    })
}

#[cfg(not(target_os = "windows"))]
fn build_production_sender_bundle(
    _udp_port: u16,
    _service_name: String,
    _stop_flag: Arc<AtomicBool>,
    _channel: Arc<dyn ChannelLike>,
) -> Result<SenderBundle, BundleError> {
    Err(BundleError::Other(
        "sender pipeline requires Windows".to_string(),
    ))
}

// ─── Tauri commands ───────────────────────────────────────────────────────────

/// Start the sender pipeline.
///
/// Accepts `channel` (Tauri IPC), `udp_port` (None = OS-assigned), and
/// `service_name` (None = default "_screen-mirror._tcp.local.").
#[tauri::command]
pub fn start_sender(
    bridge: tauri::State<SenderBridge>,
    channel: tauri::ipc::Channel<InvokeResponseBody>,
    udp_port: Option<u16>,
    service_name: Option<String>,
) -> Result<(), StartSenderError> {
    let channel_arc: Arc<dyn ChannelLike> = Arc::new(TauriSenderChannel(channel));
    start_sender_inner(&bridge, channel_arc, udp_port, service_name)
}

/// Stop the active sender session. Idempotent.
#[tauri::command]
pub fn stop_sender(bridge: tauri::State<SenderBridge>) -> Result<(), String> {
    stop_sender_session(&bridge);
    Ok(())
}

/// Return diagnostics for the active sender session.
#[tauri::command]
pub fn sender_diagnostics(
    bridge: tauri::State<SenderBridge>,
) -> Result<SenderStats, String> {
    sender_diagnostics_impl(&bridge)
}

// ─── TauriSenderChannel — production ChannelLike for sender ──────────────────

/// Production wrapper: sends JSON bytes (not binary fMP4) via the Tauri Channel.
struct TauriSenderChannel(tauri::ipc::Channel<InvokeResponseBody>);

impl ChannelLike for TauriSenderChannel {
    fn send_raw(&self, _discriminant: u8, bytes: Vec<u8>) -> Result<(), String> {
        // Sender always sends JSON. The discriminant is ignored.
        // InvokeResponseBody::Raw delivers the raw bytes to JS as ArrayBuffer.
        // The JS onmessage handler parses them as UTF-8 JSON.
        self.0
            .send(InvokeResponseBody::Raw(bytes))
            .map_err(|e| e.to_string())
    }
}
