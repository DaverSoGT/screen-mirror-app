//! Tauri IPC bridge — sender commands.
//!
//! Implements the Tauri command surface for the screen-mirror sender:
//! `start_sender`, `stop_sender`, `sender_diagnostics`.
//!
//! # Architecture
//!
//! The bridge owns a `SenderBridge` state container (managed by Tauri) that holds:
//! - The active `SenderSession` (pipeline + drain threads).
//! - `RestartCache` (connection params + session nonce, for `retry_session` Phase 11).
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
//!
//! # Reconnect supervisor
//!
//! The reconnect supervisor (`ReconnectSupervisor`) runs on a short-lived thread
//! spawned when the first `IceFailed`/`ConnectionLost` event arrives on the transport
//! drain thread. The drain thread forwards events as `SupervisorSignal`s and reads
//! `SupervisorOutcome`s to emit frontend events.
//!
//! `stop_sender_session` sends `SupervisorSignal::Stop` via `supervisor_signal_tx`
//! before joining drain threads, interrupting any in-flight backoff sleep (AC-13).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use sm_domain::session::{DeadReason, ReconnectPolicy, ReconnectTrigger, SessionState};
use sm_domain::signaling::{IceCandidate, SdpAnswer, SignalingEvent};
use sm_domain::supervisor::{ReconnectSupervisor, SupervisorOutcome, SupervisorSignal};
use sm_domain::transport::{TransportError, TransportEvent};
use tauri::ipc::InvokeResponseBody;

pub use crate::commands::stream::{BundleError, ChannelLike, PortRejectReason};

// ─── SenderBuilderFn — injectable seam for SenderBundle construction ──────────

/// Factory closure type: produces a fully-started `SenderBundle` given runtime
/// args `(udp_port, service_name, stop_flag, channel)`.
///
/// 4-arg form (Amendment A): no `BindCtx` — the sender binds on port 0 (ephemeral)
/// inside `Str0mVideoSender::start()` directly. No pre-bind probe for the sender.
///
/// Production: wraps `build_production_sender_bundle` (Windows-only).
/// Tests inject a closure returning a fake bundle with cross-platform fake adapters.
pub type SenderBuilderFn = Arc<
    dyn Fn(u16, String, Arc<AtomicBool>, Arc<dyn ChannelLike>) -> Result<SenderBundle, BundleError>
        + Send
        + Sync,
>;

// ─── SenderBundle — result of SenderBuilderFn ─────────────────────────────────

/// The fully-initialised sender pipeline returned by `SenderBuilderFn`.
///
/// `drain_handles` are joined by `stop_sender_session`.
///
/// `shutdown` owns the production resources (capture, encoder Arc, sender Arc,
/// signaling Arc) and is invoked by `stop_sender_session` BEFORE joining drains.
/// This guarantees the resources stay alive across the full session lifetime —
/// fixes C1 (verify-report #362), where the previous design dropped them at the
/// end of bundle construction and stopped the signaling thread before ICE.
///
/// Test bundles set `shutdown: None`.
pub struct SenderBundle {
    /// Drain thread handles (signaling drain + transport event drain).
    pub drain_handles: Vec<JoinHandle<()>>,
    /// Owns production-only resources whose `Drop` impls perform ordered teardown
    /// (capture → sender Arc → encoder Arc → signaling Arc). `None` for test stubs.
    pub shutdown: Option<Box<dyn FnOnce() + Send>>,
}

impl SenderBundle {
    /// Construct a minimal bundle suitable for unit tests.
    /// Spawns no real threads; drain_handles is empty; no production shutdown.
    pub fn test_stub() -> Self {
        Self {
            drain_handles: vec![],
            shutdown: None,
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
pub struct SenderArgs {
    pub udp_port: u16,
    pub service_name: String,
}

// ─── RestartCache — construction params for retry_session ────────────────────

/// Cached construction parameters for the active or most-recent sender session.
///
/// Persisted by `start_sender_inner` and read by `retry_session` (Phase 11) to
/// re-arm after a `Dead` state without requiring the user to re-enter parameters.
///
/// `session_nonce` is a random u64 generated once per session lifetime (not per
/// reconnect attempt). Used by `ReconnectSupervisor` for race tie-breaking (AC-10).
#[derive(Clone)]
pub struct RestartCache {
    /// UDP port the session was started on (0 = ephemeral; may differ after restart).
    pub udp_port: u16,
    /// mDNS service name for this session.
    pub service_name: String,
    /// Frontend IPC channel — re-used during `retry_session`.
    pub channel: Arc<dyn ChannelLike>,
    /// Random u64 nonce generated once at session start. Lower nonce wins race (AC-10).
    pub session_nonce: u64,
}

// ─── SenderSession — active pipeline state ───────────────────────────────────

/// Holds all resources for one active sender session.
pub struct SenderSession {
    pub stop_flag: Arc<AtomicBool>,
    pub drain_handles: Vec<JoinHandle<()>>,
    pub channel: Arc<dyn ChannelLike>,
    pub counters: Arc<SenderCounters>,
    /// Production-only ordered teardown closure (C1 fix). See [`SenderBundle::shutdown`].
    pub shutdown: Option<Box<dyn FnOnce() + Send>>,
}

// ─── SenderBridge — Tauri managed state ──────────────────────────────────────

/// Tauri managed state for an active sender session.
///
/// Held behind `State<SenderBridge>` in Tauri commands.
pub struct SenderBridge {
    pub session: Mutex<Option<SenderSession>>,
    pub(crate) builder: SenderBuilderFn,
    pub current_args: Mutex<Option<SenderArgs>>,
    /// Cached construction params + session nonce; populated by `start_sender_inner`;
    /// cleared by `stop_sender_session`; read by `retry_session` (Phase 11).
    pub restart_cache: Mutex<Option<RestartCache>>,
    /// Signal channel to the reconnect supervisor, if one is active.
    ///
    /// Shared between `stop_sender_session` (which sends `Stop`) and the drain thread
    /// (which sets it when the supervisor is spawned). Stored on `SenderBridge` (not
    /// `SenderSession`) so `start_sender_inner` can provision the same Arc that the
    /// builder captures, before the session is constructed.
    pub supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
}

impl SenderBridge {
    /// Create a bridge using the production `build_production_sender_bundle` factory.
    pub fn new() -> Self {
        Self::new_with_builder(Arc::new(|udp_port, service_name, stop_flag, channel| {
            build_production_sender_bundle(udp_port, service_name, stop_flag, channel)
        }))
    }

    /// Create a bridge with a custom builder factory (test seam, R17).
    pub fn new_with_builder(builder: SenderBuilderFn) -> Self {
        Self {
            session: Mutex::new(None),
            builder,
            current_args: Mutex::new(None),
            restart_cache: Mutex::new(None),
            supervisor_signal_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a bridge with a pre-provisioned `supervisor_signal_tx` Arc.
    ///
    /// Used in tests where the builder closure must capture the same Arc that the
    /// bridge stores, so `stop_sender_session` can reach the supervisor. The caller
    /// creates the Arc before the builder and before the bridge:
    ///
    /// ```rust,ignore
    /// let sup_tx = Arc::new(Mutex::new(None));
    /// let sup_tx_for_drain = sup_tx.clone();
    /// let bridge = SenderBridge::new_with_builder_and_sup_tx(
    ///     Arc::new(move |_, _, sf, ch| {
    ///         run_drain(ev_rx, sf, ch, counters, sup_tx_for_drain.clone());
    ///         Ok(bundle)
    ///     }),
    ///     sup_tx,
    /// );
    /// ```
    pub fn new_with_builder_and_sup_tx(
        builder: SenderBuilderFn,
        supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    ) -> Self {
        Self {
            session: Mutex::new(None),
            builder,
            current_args: Mutex::new(None),
            restart_cache: Mutex::new(None),
            supervisor_signal_tx,
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
    Reconnecting {
        attempt: u8,
        max: u8,
    },
    Dead {
        reason: String,
    },
    #[serde(rename = "button")]
    Button {
        label: String,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
    },
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
pub(crate) fn validate_udp_port_for_sender(value: u16) -> Result<(), StartSenderError> {
    if (1..1024).contains(&value) {
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
pub(crate) fn validate_service_name_for_sender(s: &str) -> Result<(), StartSenderError> {
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

/// Convert a `DeadReason` to its snake_case string representation for the frontend.
fn dead_reason_to_str(reason: &DeadReason) -> &'static str {
    match reason {
        DeadReason::IceFailedRepeatedly => "ice_failed_repeatedly",
        DeadReason::ConnectionLostRepeatedly => "connection_lost_repeatedly",
        DeadReason::SignalingChannelDead => "signaling_channel_dead",
        DeadReason::UserCanceled => "user_canceled",
    }
}

// ─── SignalingSenderOps — abstraction for signaling drain ─────────────────────

/// Operations the signaling drain thread needs on the sender transport.
pub trait SignalingSenderOps: Send + Sync {
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
pub fn run_sender_signaling_drain(
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
                        eprintln!("[sm-sender-signaling-drain] apply_remote_answer failed: {e}");
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
                        eprintln!("[sm-sender-signaling-drain] add_remote_candidate failed: {e}");
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

/// Transport-event drain loop for the sender — WITHOUT reconnect supervisor.
///
/// Legacy variant kept for existing tests that don't wire the supervisor.
/// IceFailed/ConnectionLost still emit the old PeerLost + Restart button here.
/// Production and new tests use `run_sender_transport_event_drain_with_supervisor`.
pub fn run_sender_transport_event_drain(
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
                    eprintln!("[sm-sender-transport-drain] ICE connected");
                    emit_event(&channel, &SenderStatusEvent::Streaming);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Stop streaming".to_string(),
                        },
                    );
                }
                TransportEvent::IceFailed => {
                    eprintln!(
                        "[sm-sender-transport-drain] ICE failed — emitting PeerLost + Restart button"
                    );
                    emit_event(&channel, &SenderStatusEvent::PeerLost);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Restart".to_string(),
                        },
                    );
                }
                TransportEvent::ConnectionLost { reason } => {
                    eprintln!(
                        "[sm-sender-transport-drain] connection lost: {reason} — emitting PeerLost + Restart button"
                    );
                    emit_event(&channel, &SenderStatusEvent::PeerLost);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Restart".to_string(),
                        },
                    );
                }
                TransportEvent::KeyframeRequested => {
                    let n = counters
                        .keyframe_requests_received
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    eprintln!(
                        "[sm-sender-transport-drain] KeyframeRequested #{n} — encoder.request_keyframe() will fire next frame"
                    );
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Transport-event drain loop for the sender — WITH reconnect supervisor wiring.
///
/// Uses production defaults: `ack_timeout = 2s`, `policy = ReconnectPolicy::v1_default()`.
/// For tests that drive the supervisor directly (via `supervisor_signal_tx`), use
/// `run_sender_transport_event_drain_with_supervisor_custom` with a fast policy.
pub fn run_sender_transport_event_drain_with_supervisor(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    counters: Arc<SenderCounters>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) {
    // Production ack_timeout: 2s per design §3.
    let ack_timeout = Duration::from_secs(2);

    // Session nonce is generated once when the first reconnect is needed.
    let session_nonce: u64 = rand::random();

    'drain: loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                TransportEvent::IceConnected => {
                    eprintln!("[sm-sender-transport-drain+sup] ICE connected");
                    emit_event(&channel, &SenderStatusEvent::Streaming);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Stop streaming".to_string(),
                        },
                    );
                }
                TransportEvent::IceFailed => {
                    eprintln!("[sm-sender-transport-drain+sup] ICE failed — entering supervisor mode");
                    enter_supervisor_mode(
                        ReconnectTrigger::IceFailed,
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        ReconnectPolicy::v1_default(),
                        ack_timeout,
                    );
                    break 'drain;
                }
                TransportEvent::ConnectionLost { reason } => {
                    eprintln!("[sm-sender-transport-drain+sup] connection lost: {reason} — entering supervisor mode");
                    enter_supervisor_mode(
                        ReconnectTrigger::ConnectionLost { reason },
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        ReconnectPolicy::v1_default(),
                        ack_timeout,
                    );
                    break 'drain;
                }
                TransportEvent::KeyframeRequested => {
                    let n = counters
                        .keyframe_requests_received
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    eprintln!(
                        "[sm-sender-transport-drain+sup] KeyframeRequested #{n}"
                    );
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Transport-event drain loop — WITH supervisor wiring AND custom policy/ack_timeout.
///
/// Identical to `run_sender_transport_event_drain_with_supervisor` but accepts an
/// explicit `ReconnectPolicy` and `ack_timeout`. Tests use this to inject a fast
/// policy (millisecond-scale backoff) so supervisor state transitions are exercisable
/// without waiting minutes for v1_default() delays.
pub fn run_sender_transport_event_drain_with_supervisor_custom(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    counters: Arc<SenderCounters>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    policy: ReconnectPolicy,
    ack_timeout: Duration,
) {
    let session_nonce: u64 = rand::random();

    'drain: loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                TransportEvent::IceConnected => {
                    eprintln!("[sm-sender-transport-drain+sup-custom] ICE connected");
                    emit_event(&channel, &SenderStatusEvent::Streaming);
                    emit_event(
                        &channel,
                        &SenderStatusEvent::Button {
                            label: "Stop streaming".to_string(),
                        },
                    );
                }
                TransportEvent::IceFailed => {
                    enter_supervisor_mode(
                        ReconnectTrigger::IceFailed,
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        policy,
                        ack_timeout,
                    );
                    break 'drain;
                }
                TransportEvent::ConnectionLost { reason } => {
                    enter_supervisor_mode(
                        ReconnectTrigger::ConnectionLost { reason },
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        policy,
                        ack_timeout,
                    );
                    break 'drain;
                }
                TransportEvent::KeyframeRequested => {
                    let n = counters
                        .keyframe_requests_received
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    eprintln!(
                        "[sm-sender-transport-drain+sup-custom] KeyframeRequested #{n}"
                    );
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Supervisor coordinator mode.
///
/// Spawns `ReconnectSupervisor` on a short-lived thread, registers the signal sender
/// in `supervisor_signal_tx` (for `stop_sender_session` to reach), then loops:
/// - Reads supervisor outcomes (non-blocking) and emits frontend events.
/// - Reads transport events with short timeout and forwards as supervisor signals.
///
/// Returns when the supervisor thread exits (Dead or Stopped terminal state).
#[allow(clippy::too_many_arguments)]
fn enter_supervisor_mode(
    initial_trigger: ReconnectTrigger,
    session_nonce: u64,
    ev_rx: &std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: &Arc<AtomicBool>,
    channel: &Arc<dyn ChannelLike>,
    supervisor_signal_tx: &Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    policy: ReconnectPolicy,
    ack_timeout: Duration,
) {
    use std::sync::mpsc::sync_channel;

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(16);
    let (outcome_tx, outcome_rx) = sync_channel::<SupervisorOutcome>(32);

    // Register signal_tx so stop_sender_session can interrupt backoff sleep.
    *supervisor_signal_tx.lock().unwrap() = Some(signal_tx.clone());

    // Send initial trigger to kick off the supervisor.
    let _ = signal_tx.try_send(SupervisorSignal::LocalFailure {
        trigger: initial_trigger,
    });

    // Spawn supervisor on a short-lived thread.
    let sup_join = std::thread::Builder::new()
        .name("sm-sender-supervisor".into())
        .spawn(move || {
            let mut sup = ReconnectSupervisor::new(policy, session_nonce, signal_rx, outcome_tx);
            sup.run(ack_timeout)
        })
        .expect("supervisor thread spawn must not fail");

    // Coordinator loop: interleave reading outcomes and transport events.
    'coord: loop {
        if stop_flag.load(Ordering::Relaxed) {
            // Stop was signaled externally (stop_flag set by stop_sender_session).
            // The supervisor_signal_tx.Stop was already sent by stop_sender_session.
            break 'coord;
        }

        // Drain all available outcomes (non-blocking).
        loop {
            match outcome_rx.try_recv() {
                Ok(outcome) => {
                    handle_supervisor_outcome(&outcome, channel);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Supervisor exited — drain done.
                    break 'coord;
                }
            }
        }

        // Read transport events with short timeout to stay responsive.
        match ev_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(TransportEvent::IceConnected) => {
                // Rebuild succeeded — signal supervisor.
                let _ = signal_tx.try_send(SupervisorSignal::RebuildSucceeded);
            }
            Ok(TransportEvent::IceFailed) | Ok(TransportEvent::ConnectionLost { .. }) => {
                // Rebuild failed — signal supervisor.
                let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
            }
            Ok(_) => {
                // Other transport events during reconnect: discard (AC-11).
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Normal poll — continue.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Transport channel dropped — treat as rebuild failure.
                let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
            }
        }

        // Check if supervisor thread has finished (peek at outcome_rx for Disconnected).
        // We'll detect this on the next iteration's try_recv loop.
    }

    // Clear signal_tx from the session before joining.
    *supervisor_signal_tx.lock().unwrap() = None;

    // Join the supervisor thread.
    let _ = sup_join.join();
}

/// Handle a single `SupervisorOutcome` by emitting the appropriate frontend event.
fn handle_supervisor_outcome(outcome: &SupervisorOutcome, channel: &Arc<dyn ChannelLike>) {
    match outcome {
        SupervisorOutcome::StateChanged(SessionState::Reconnecting { attempt, max }) => {
            emit_event(
                channel,
                &SenderStatusEvent::Reconnecting {
                    attempt: attempt.get(),
                    max: max.get(),
                },
            );
        }
        SupervisorOutcome::StateChanged(SessionState::Dead { reason }) => {
            emit_event(
                channel,
                &SenderStatusEvent::Dead {
                    reason: dead_reason_to_str(reason).to_string(),
                },
            );
        }
        SupervisorOutcome::StateChanged(SessionState::Connected) => {
            // Reconnect succeeded — emit streaming event.
            emit_event(channel, &SenderStatusEvent::Streaming);
        }
        SupervisorOutcome::Dead(reason) => {
            // Terminal dead — emit the dead event (StateChanged(Dead) is emitted first
            // by the supervisor, so this is a secondary notification; skip to avoid double emit).
            let _ = reason; // already emitted via StateChanged(Dead) above
        }
        SupervisorOutcome::PublishReconnectRequest { attempt, session_nonce } => {
            eprintln!("[sm-sender-sup-coord] publish ReconnectRequest attempt={attempt} nonce={session_nonce}");
            // TODO Phase 6 production: call MdnsSignaling::publish_reconnect_request()
        }
        SupervisorOutcome::PublishReconnectAck { attempt, session_nonce } => {
            eprintln!("[sm-sender-sup-coord] publish ReconnectAck attempt={attempt} nonce={session_nonce}");
            // TODO Phase 6 production: call MdnsSignaling::publish_reconnect_ack()
        }
        SupervisorOutcome::InitiateRebuild => {
            eprintln!("[sm-sender-sup-coord] InitiateRebuild — bundle rebuild TODO (Phase 6 production)");
            // TODO Phase 6 production: teardown old bundle, call builder again, signal RebuildSucceeded/Failed
        }
        SupervisorOutcome::InitiateMdnsReset => {
            eprintln!("[sm-sender-sup-coord] InitiateMdnsReset — mDNS reset TODO (Phase 6 production)");
            // TODO Phase 6 production: call MdnsSignaling::reset()
        }
        SupervisorOutcome::Stopped => {
            eprintln!("[sm-sender-sup-coord] supervisor stopped");
        }
        SupervisorOutcome::StateChanged(_) => {
            // Connecting or other transient states — no frontend event needed.
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
/// 5. Generate session_nonce (rand::random::<u64>()).
/// 6. Allocate stop_flag.
/// 7. Invoke builder(port, name, stop_flag, channel).
/// 8. Store SenderSession + current_args + restart_cache.
/// 9. Emit Connecting status.
pub fn start_sender_inner(
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
    let resolved_name = service_name.unwrap_or_else(|| "_screen-mirror._tcp.local.".to_string());

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

    // Step 5 — generate session nonce (rand — collision prob ≈ 5×10⁻²⁰ per pair).
    let session_nonce: u64 = rand::random();

    // Step 6 — allocate stop_flag and clone builder.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let builder = bridge.builder.clone();

    // Reset the bridge-level supervisor_signal_tx for this new session.
    *bridge.supervisor_signal_tx.lock().unwrap() = None;

    // Step 7 — invoke builder (no lock held).
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

    // Step 8 — store session and current_args.
    let session = SenderSession {
        stop_flag,
        drain_handles: bundle.drain_handles,
        channel: channel.clone(),
        counters: Arc::new(SenderCounters::default()),
        shutdown: bundle.shutdown,
    };
    *bridge.session.lock().unwrap() = Some(session);
    *bridge.current_args.lock().unwrap() = Some(SenderArgs {
        udp_port: resolved_port,
        service_name: resolved_name.clone(),
    });
    *bridge.restart_cache.lock().unwrap() = Some(RestartCache {
        udp_port: resolved_port,
        service_name: resolved_name,
        channel: channel.clone(),
        session_nonce,
    });

    // Step 9 — emit Connecting status.
    emit_event(&channel, &SenderStatusEvent::Connecting);

    Ok(())
}

// ─── stop_sender_session — ordered teardown ───────────────────────────────────

/// Ordered teardown for an active sender session.
///
/// Idempotent: if no session is active, returns immediately.
/// Mirrors stream.rs stop_stream_session lock ordering: session FIRST, then current_args.
///
/// Teardown order (C1 fix, with AC-13 supervisor cancel):
/// 1. Send `SupervisorSignal::Stop` to interrupt any in-flight backoff sleep (AC-13).
/// 2. Set stop_flag (drain threads exit on next timeout).
/// 3. Run `shutdown` closure (drops production resources in order).
/// 4. Join drain handles (now ready to exit via stop_flag or tx-disconnect).
/// 5. Emit Stopped event and release channel.
/// 6. Clear current_args and restart_cache.
pub fn stop_sender_session(bridge: &SenderBridge) {
    let session_opt = {
        let mut guard = bridge.session.lock().unwrap();
        guard.take()
    };

    let Some(mut session) = session_opt else {
        return;
    };

    // 1. Interrupt supervisor backoff sleep (AC-13).
    //    The bridge-level supervisor_signal_tx is shared with the drain thread.
    let sup_tx_opt = bridge.supervisor_signal_tx.lock().unwrap().clone();
    if let Some(sup_tx) = sup_tx_opt {
        let _ = sup_tx.try_send(SupervisorSignal::Stop);
    }

    // 2. Signal drains.
    session.stop_flag.store(true, Ordering::Relaxed);

    // 3. Drop production resources in order (C1 fix). No-op for test stubs.
    if let Some(shutdown) = session.shutdown.take() {
        shutdown();
    }

    // 4. Join drain threads.
    for h in session.drain_handles.drain(..) {
        let _ = h.join();
    }

    // 5. Emit Stopped event and release channel.
    emit_event(&session.channel, &SenderStatusEvent::Stopped);
    drop(session.channel);

    // 6. Clear current_args and restart_cache AFTER session lock is released.
    *bridge.current_args.lock().unwrap() = None;
    *bridge.restart_cache.lock().unwrap() = None;
}

// ─── sender_diagnostics_impl ──────────────────────────────────────────────────

/// Core of `sender_diagnostics` — extracted for unit testing.
pub fn sender_diagnostics_impl(bridge: &SenderBridge) -> Result<SenderStats, String> {
    let guard = bridge.session.lock().unwrap();
    match guard.as_ref() {
        None => Err("not running".to_string()),
        Some(s) => Ok(SenderStats {
            dropped_frames_encoder: s.counters.dropped_frames_encoder.load(Ordering::Relaxed),
            dropped_frames_transport: s.counters.dropped_frames_transport.load(Ordering::Relaxed),
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
    use sm_domain::capture::BorderPolicy;
    use sm_domain::signaling::{Signaling, SignalingConfig, SignalingRole};
    use sm_domain::transport::{TransportConfig, TransportRole, VideoSender};
    use sm_domain::{CaptureConfig, CaptureSource, EncoderConfig, MonitorSelector, VideoEncoder};
    use sm_infra::capture::WindowsCaptureSource;
    use sm_infra::encode::windows::WindowsOpenH264Encoder;
    use sm_infra::signaling::mdns::MdnsSignaling;
    use sm_infra::transport::{Str0mVideoSender, publish_host_candidate};
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

    // PQ-ST-5 hardcoded defaults: Primary monitor, 30 fps, border explicitly off.
    // Spec said "BorderPolicy::Hidden" — domain enum is named AlwaysOff (same intent:
    // always attempt to hide the yellow capture border, fallback to OS default on
    // unsupported builds). Explicit > implicit `Auto` to match spec R5 intent (W2 fix).
    let capture_config = CaptureConfig {
        monitor: MonitorSelector::Primary,
        max_fps: Some(30),
        border: BorderPolicy::AlwaysOff,
        ..CaptureConfig::default()
    };
    let mut capture =
        WindowsCaptureSource::new(capture_config).map_err(|e| BundleError::Other(e.to_string()))?;

    let encoder_config = EncoderConfig::default();
    let mut encoder = WindowsOpenH264Encoder::new(encoder_config)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    let transport_config = TransportConfig {
        udp_port,
        role: TransportRole::Sender,
        ..TransportConfig::default()
    };
    let mut sender =
        Str0mVideoSender::new(transport_config).map_err(|e| BundleError::Other(e.to_string()))?;

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

    // Extract offer BEFORE start(): start() consumes pre_neg via guard.take(),
    // after which create_local_offer() returns "Rtc already moved to thread".
    let offer = sender
        .create_local_offer()
        .map_err(|e| BundleError::Other(e.to_string()))?;

    sender
        .start(enc_to_sender_rx, tr_ev_tx)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    // Publish offer immediately (Amendment B — buffers in inbox; written on connect).
    signaling
        .publish_local_offer(offer)
        .map_err(|e| BundleError::Other(e.to_string()))?;

    // Trickle ICE: publish host candidate AFTER offer so the peer receives
    // Offer → Candidate in FIFO order (design §3.1 revised ordering).
    if let Some(addr) = sender.candidate_addr() {
        publish_host_candidate(&signaling, addr).unwrap_or_else(|e| {
            eprintln!("[sm-sender-bundle] publish_host_candidate failed: {e}");
        });
    } else {
        eprintln!("[sm-sender-bundle] no non-loopback NIC; skipping candidate publish");
    }

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

    let sender_ops: Arc<dyn SignalingSenderOps> = Arc::new(Str0mSenderOpsImpl(sender_arc.clone()));

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

    // Production uses supervisor-aware drain.
    let sup_tx = Arc::new(Mutex::new(None::<SyncSender<SupervisorSignal>>));
    let tr_drain = std::thread::Builder::new()
        .name("sm-sender-transport-drain".into())
        .spawn(move || {
            run_sender_transport_event_drain_with_supervisor(
                tr_ev_rx,
                tr_stop,
                tr_channel,
                tr_counters,
                sup_tx,
            );
        })
        .map_err(|e| BundleError::Other(format!("spawn transport drain: {e}")))?;

    // C1 fix: move production arcs into the shutdown closure so they outlive the
    // bundle-build call and are dropped in order ONLY when stop_sender_session runs.
    let shutdown: Box<dyn FnOnce() + Send> = Box::new(move || {
        drop(capture);
        drop(sender_arc);
        drop(encoder_arc);
        drop(signaling_arc);
    });

    Ok(SenderBundle {
        drain_handles: vec![sig_drain, tr_drain],
        shutdown: Some(shutdown),
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
pub fn sender_diagnostics(bridge: tauri::State<SenderBridge>) -> Result<SenderStats, String> {
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
