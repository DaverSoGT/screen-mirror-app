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

// ─── SenderCoordinatorHooks — production wiring seam ─────────────────────────

/// Callbacks invoked by the sender supervisor coordinator when the supervisor
/// emits outcomes that require side-effects beyond frontend event emission.
///
/// Production: hooks call `MdnsSignaling::publish_reconnect_request()`, etc.
/// Tests: hooks are counting closures (no real signaling).
///
/// Using `Arc<dyn Fn(...)>` closures matches the existing `SenderBuilderFn`
/// pattern and avoids a new trait object vtable while keeping things testable.
pub struct SenderCoordinatorHooks {
    /// Called when supervisor emits `PublishReconnectRequest`.
    /// Arguments: `(attempt: u8, session_nonce: u64)`.
    pub publish_reconnect_request: Arc<dyn Fn(u8, u64) + Send + Sync>,
    /// Called when supervisor emits `PublishReconnectAck`.
    /// Arguments: `(attempt: u8, session_nonce: u64)`.
    pub publish_reconnect_ack: Arc<dyn Fn(u8, u64) + Send + Sync>,
    /// Called when supervisor emits `InitiateRebuild`.
    /// Receives a clone of `signal_tx` so it can send `RebuildSucceeded` or
    /// `RebuildFailed` back to the supervisor after the rebuild attempt.
    pub initiate_rebuild: Arc<dyn Fn(SyncSender<SupervisorSignal>) + Send + Sync>,
    /// Called when supervisor emits `InitiateMdnsReset`.
    /// Must tear down the current `MdnsSignaling` and re-start discovery.
    pub initiate_mdns_reset: Arc<dyn Fn() + Send + Sync>,
}

impl SenderCoordinatorHooks {
    /// No-op hooks — used by existing drain functions that don't need production
    /// coordinator wiring (tests that only check event emission, not wiring calls).
    pub fn noop() -> Self {
        Self {
            publish_reconnect_request: Arc::new(|_, _| {}),
            publish_reconnect_ack: Arc::new(|_, _| {}),
            initiate_rebuild: Arc::new(|signal_tx| {
                // No-op: signal RebuildFailed so the supervisor doesn't block.
                let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
            }),
            initiate_mdns_reset: Arc::new(|| {}),
        }
    }
}

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
///
/// `session` and `restart_cache` are wrapped in `Arc` so the rebuild worker
/// (spawned by `make_sender_rebuild_hook`) can hold a clone of these arcs and
/// perform the session swap without holding a reference to the bridge itself.
/// This lets the builder closure (stored on `builder`) capture these arcs at
/// bridge-construction time and share them with the worker thread.
pub struct SenderBridge {
    pub session: Arc<Mutex<Option<SenderSession>>>,
    pub(crate) builder: SenderBuilderFn,
    pub current_args: Mutex<Option<SenderArgs>>,
    /// Cached construction params + session nonce; populated by `start_sender_inner`;
    /// cleared by `stop_sender_session`; read by `retry_session` (Phase 11).
    pub restart_cache: Arc<Mutex<Option<RestartCache>>>,
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
    ///
    /// `session` and `restart_cache` arcs are created here and also captured by the
    /// builder closure so `make_sender_rebuild_hook` (wired inside
    /// `build_production_sender_bundle`) can swap the session without a reference to
    /// the bridge itself.
    pub fn new() -> Self {
        let session_arc: Arc<Mutex<Option<SenderSession>>> = Arc::new(Mutex::new(None));
        let restart_cache_arc: Arc<Mutex<Option<RestartCache>>> = Arc::new(Mutex::new(None));
        let session_for_builder = session_arc.clone();
        let cache_for_builder = restart_cache_arc.clone();
        Self {
            session: session_arc,
            builder: Arc::new(move |udp_port, service_name, stop_flag, channel| {
                build_production_sender_bundle(
                    udp_port,
                    service_name,
                    stop_flag,
                    channel,
                    session_for_builder.clone(),
                    cache_for_builder.clone(),
                )
            }),
            current_args: Mutex::new(None),
            restart_cache: restart_cache_arc,
            supervisor_signal_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a bridge with a custom builder factory (test seam, R17).
    pub fn new_with_builder(builder: SenderBuilderFn) -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
            builder,
            current_args: Mutex::new(None),
            restart_cache: Arc::new(Mutex::new(None)),
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
            session: Arc::new(Mutex::new(None)),
            builder,
            current_args: Mutex::new(None),
            restart_cache: Arc::new(Mutex::new(None)),
            supervisor_signal_tx,
        }
    }

    /// Create a bridge with pre-provisioned session, restart_cache, and supervisor_signal_tx Arcs.
    ///
    /// Used in tests where the builder closure must capture the SAME session and
    /// restart_cache arcs that the bridge owns, so `make_sender_rebuild_hook` can
    /// swap sessions using the bridge's actual state.
    ///
    /// ```rust,ignore
    /// let session_arc = Arc::new(Mutex::new(None));
    /// let cache_arc   = Arc::new(Mutex::new(None));
    /// let sup_tx      = Arc::new(Mutex::new(None));
    /// let ses_clone   = session_arc.clone();
    /// let cache_clone = cache_arc.clone();
    /// let bridge = SenderBridge::new_with_builder_and_arcs(
    ///     Arc::new(move |_, _, sf, ch| {
    ///         let hook = make_sender_rebuild_hook(..., cache_clone.clone(), ses_clone.clone(), sf.clone(), 1);
    ///         // spawn drain with hook...
    ///         Ok(bundle)
    ///     }),
    ///     session_arc,
    ///     cache_arc,
    ///     sup_tx,
    /// );
    /// ```
    pub fn new_with_builder_and_arcs(
        builder: SenderBuilderFn,
        session: Arc<Mutex<Option<SenderSession>>>,
        restart_cache: Arc<Mutex<Option<RestartCache>>>,
        supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    ) -> Self {
        Self {
            session,
            builder,
            current_args: Mutex::new(None),
            restart_cache,
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
                    eprintln!(
                        "[sm-sender-transport-drain+sup] ICE failed — entering supervisor mode"
                    );
                    enter_supervisor_mode(
                        ReconnectTrigger::IceFailed,
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        ReconnectPolicy::v1_default(),
                        ack_timeout,
                        SenderCoordinatorHooks::noop(),
                    );
                    break 'drain;
                }
                TransportEvent::ConnectionLost { reason } => {
                    eprintln!(
                        "[sm-sender-transport-drain+sup] connection lost: {reason} — entering supervisor mode"
                    );
                    enter_supervisor_mode(
                        ReconnectTrigger::ConnectionLost { reason },
                        session_nonce,
                        &ev_rx,
                        &stop_flag,
                        &channel,
                        &supervisor_signal_tx,
                        ReconnectPolicy::v1_default(),
                        ack_timeout,
                        SenderCoordinatorHooks::noop(),
                    );
                    break 'drain;
                }
                TransportEvent::KeyframeRequested => {
                    let n = counters
                        .keyframe_requests_received
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    eprintln!("[sm-sender-transport-drain+sup] KeyframeRequested #{n}");
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
/// Uses no-op coordinator hooks (event emission only). For production coordinator
/// wiring (InitiateRebuild, PublishReconnectRequest, etc.), use
/// `run_sender_transport_event_drain_with_supervisor_custom_and_hooks`.
///
/// Tests use this variant with a fast policy (millisecond-scale backoff) to drive all
/// 3 attempts without waiting for the production 3s/9s/27s delays.
pub fn run_sender_transport_event_drain_with_supervisor_custom(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    counters: Arc<SenderCounters>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    policy: ReconnectPolicy,
    ack_timeout: Duration,
) {
    run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
        ev_rx,
        stop_flag,
        channel,
        supervisor_signal_tx,
        policy,
        ack_timeout,
        SenderCoordinatorHooks::noop(),
    );
    // Note: `counters` not used in the hooks variant — kept in signature for backward compat.
    let _ = counters;
}

/// Transport-event drain loop — WITH supervisor wiring AND explicit hooks.
///
/// This is the primary drain function for production coordinator wiring.
/// `hooks` receives the coordinator actions (rebuild, signaling publish, mDNS reset).
/// For tests that only care about event emission, use `..._custom` (no-op hooks).
pub fn run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    policy: ReconnectPolicy,
    ack_timeout: Duration,
    hooks: SenderCoordinatorHooks,
) {
    let session_nonce: u64 = rand::random();

    'drain: loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                TransportEvent::IceConnected => {
                    eprintln!("[sm-sender-transport-drain+sup-hooks] ICE connected");
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
                        hooks,
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
                        hooks,
                    );
                    break 'drain;
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
/// Production coordinator actions (InitiateRebuild, PublishReconnectRequest, etc.)
/// are dispatched via `hooks` — see [`SenderCoordinatorHooks`].
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
    hooks: SenderCoordinatorHooks,
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
        // Drain all available outcomes BEFORE checking stop_flag.
        //
        // WHY outcomes first: the rebuild worker sets the OLD session's stop_flag
        // to `true` (design §3 step 6) and then sends `RebuildSucceeded` to the
        // supervisor (step 13). The supervisor emits `StateChanged(Connected)` into
        // outcome_rx. If we checked stop_flag BEFORE draining outcomes, the
        // coordinator would exit before processing `StateChanged(Connected)` and
        // the `"streaming"` event would never reach the frontend (T2.1 RED→GREEN).
        loop {
            match outcome_rx.try_recv() {
                Ok(outcome) => {
                    handle_supervisor_outcome(&outcome, channel, &signal_tx, &hooks);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // Supervisor exited — drain done.
                    break 'coord;
                }
            }
        }

        // Check stop_flag AFTER processing pending outcomes.
        // This ensures StateChanged(Connected) from a successful rebuild is
        // always emitted before the coordinator exits.
        if stop_flag.load(Ordering::Relaxed) {
            // Stop was signaled externally (stop_flag set by stop_sender_session
            // or by the rebuild worker post-swap, design §3 step 6).
            // The supervisor_signal_tx.Stop was already sent by stop_sender_session
            // (or the supervisor will exit via signal_tx drop when we break here).
            break 'coord;
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

/// Handle a single `SupervisorOutcome` — emits frontend events AND dispatches
/// production coordinator actions via `hooks` (CRITICAL-2 wiring).
///
/// `signal_tx` is the sender's own channel to the supervisor, used by
/// `hooks.initiate_rebuild` to report `RebuildSucceeded` / `RebuildFailed`.
fn handle_supervisor_outcome(
    outcome: &SupervisorOutcome,
    channel: &Arc<dyn ChannelLike>,
    signal_tx: &SyncSender<SupervisorSignal>,
    hooks: &SenderCoordinatorHooks,
) {
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
        SupervisorOutcome::PublishReconnectRequest {
            attempt,
            session_nonce,
        } => {
            eprintln!(
                "[sm-sender-sup-coord] publish ReconnectRequest attempt={attempt} nonce={session_nonce}"
            );
            // CRITICAL-2: call production hook (MdnsSignaling::publish_reconnect_request).
            (hooks.publish_reconnect_request)(*attempt, *session_nonce);
        }
        SupervisorOutcome::PublishReconnectAck {
            attempt,
            session_nonce,
        } => {
            eprintln!(
                "[sm-sender-sup-coord] publish ReconnectAck attempt={attempt} nonce={session_nonce}"
            );
            // CRITICAL-2: call production hook (MdnsSignaling::publish_reconnect_ack).
            (hooks.publish_reconnect_ack)(*attempt, *session_nonce);
        }
        SupervisorOutcome::InitiateRebuild => {
            eprintln!("[sm-sender-sup-coord] InitiateRebuild — invoking rebuild hook");
            // CRITICAL-2: call production hook (teardown + builder + signal result).
            // The hook receives a clone of signal_tx so it can feed back the result.
            (hooks.initiate_rebuild)(signal_tx.clone());
        }
        SupervisorOutcome::InitiateMdnsReset => {
            eprintln!("[sm-sender-sup-coord] InitiateMdnsReset — invoking mDNS reset hook");
            // CRITICAL-2: call production hook (MdnsSignaling::reset + restart).
            (hooks.initiate_mdns_reset)();
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

// ─── stop_sender_session_internal — partial teardown (session only) ───────────

/// Partial teardown for an active sender session: steps 1-5 only.
///
/// Tears down the session (supervisor interrupt, stop_flag, shutdown closure,
/// drain join, Stopped event) but does NOT clear `current_args` or
/// `restart_cache`. This is used by the rebuild worker's cancel-gate D so it
/// can tear down a newly-installed session without erasing the restart
/// parameters needed for the next attempt.
///
/// The public `stop_sender_session` is a thin wrapper: call internal + clear
/// args/cache. No behavior change is visible from outside the module.
///
/// Idempotent: if no session is active, returns immediately.
pub fn stop_sender_session_internal(bridge: &SenderBridge) {
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
///
/// Thin wrapper over `stop_sender_session_internal`: calls internal (steps 1-5),
/// then clears `current_args` and `restart_cache` (step 6).
pub fn stop_sender_session(bridge: &SenderBridge) {
    stop_sender_session_internal(bridge);

    // 6. Clear current_args and restart_cache AFTER session lock is released.
    *bridge.current_args.lock().unwrap() = None;
    *bridge.restart_cache.lock().unwrap() = None;
}

// ─── make_sender_rebuild_hook — V2 rebuild hook factory ──────────────────────

/// Build the `initiate_rebuild` hook for the sender coordinator.
///
/// The returned closure matches the `SenderCoordinatorHooks::initiate_rebuild`
/// signature (`Arc<dyn Fn(SyncSender<SupervisorSignal>) + Send + Sync>`).
///
/// When invoked by the coordinator, it:
/// 1. Spawns a named worker thread `sm-rebuild-worker-sender-{attempt}`.
/// 2. Returns immediately (≤10ms) so the drain loop is not blocked.
/// 3. The worker performs the canonical rebuild sequence (design §3):
///    - Gate A: abort if `old_stop_flag` is already set.
///    - Read `RestartCache`; abort if `None`.
///    - Tear down the OLD session (set stop_flag, run shutdown closure — do NOT join drain_handles).
///    - Invoke `builder` with a fresh `stop_flag` to construct the NEW bundle.
///    - Swap `bridge_session` under a brief Mutex lock.
///    - Set OLD `stop_flag = true` (zombie-drain exit, design §3 step 14).
///    - Signal `RebuildSucceeded` or `RebuildFailed` on `signal_tx`.
///
/// # Cancel gates
///
/// Gate A (before teardown) is implemented here. Gates B/C/D are deferred to Batch 5.
/// Gate A is load-bearing for the zombie-drain correctness invariant.
///
/// # INVARIANT — do NOT join `bridge_session.drain_handles`
///
/// The drain thread that HOSTS the coordinator loop (which invokes this hook) is
/// itself one of those drain handles. Joining it from the worker would deadlock.
/// The OLD drain exits naturally when it sees `old_stop_flag = true` on its next
/// poll iteration (sender.rs:755-800 pattern). Do NOT join drain handles here.
/// # Parameters
///
/// - `builder`: The bridge's `SenderBuilderFn` — called by the worker to build the new bundle.
/// - `bridge_cache`: Arc to the bridge's `restart_cache` field — read for construction params.
/// - `bridge_session`: Arc to the bridge's `session` field — swapped by the worker under lock.
/// - `old_stop_flag`: The OLD session's `stop_flag` — used as the cancel signal (Gates A–D).
/// - `attempt`: Reconnect attempt number — embedded in the worker thread name for diagnostics.
pub fn make_sender_rebuild_hook(
    builder: SenderBuilderFn,
    bridge_cache: Arc<Mutex<Option<RestartCache>>>,
    bridge_session: Arc<Mutex<Option<SenderSession>>>,
    old_stop_flag: Arc<std::sync::atomic::AtomicBool>,
    attempt: u32,
) -> Arc<dyn Fn(SyncSender<SupervisorSignal>) + Send + Sync> {
    Arc::new(move |signal_tx: SyncSender<SupervisorSignal>| {
        let builder = builder.clone();
        let bridge_cache = bridge_cache.clone();
        let bridge_session = bridge_session.clone();
        let old_stop_flag = old_stop_flag.clone();
        let signal_tx_for_err = signal_tx.clone();

        let spawn_result = std::thread::Builder::new()
            .name(format!("sm-rebuild-worker-sender-{attempt}"))
            .spawn(move || {
                use std::sync::atomic::Ordering;

                // Gate A: abort if stop already arrived before we started any work.
                if old_stop_flag.load(Ordering::Relaxed) {
                    let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
                    return;
                }

                // Step 4: read RestartCache snapshot.
                let cache = {
                    let g = bridge_cache.lock().unwrap();
                    match g.clone() {
                        None => {
                            // RestartCache cleared by a concurrent stop — abort.
                            let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
                            return;
                        }
                        Some(c) => c,
                    }
                };

                // Step 6: tear down the OLD session's production resources.
                //
                // NOTE: do NOT set `s.stop_flag` here. The coordinator loop's
                // `stop_flag` check must remain false until AFTER we signal
                // RebuildSucceeded (step 13) and the coordinator has had a chance
                // to process StateChanged(Connected). Setting stop_flag prematurely
                // causes the coordinator to exit before emitting "streaming".
                // The stop_flag is set in step 14 (zombie-drain exit) after success.
                //
                // INVARIANT: do NOT join `session.drain_handles`. Those handles include
                // the drain thread that spawned us — joining would deadlock.
                // The OLD drain exits naturally when it polls `stop_flag = true` (step 14).
                let old_session = { bridge_session.lock().unwrap().take() };
                if let Some(mut s) = old_session {
                    // Run the shutdown closure to drop production resources in order
                    // (capture → sender_arc → encoder_arc → signaling_arc). For test
                    // stubs this is a no-op (shutdown = None). The drain threads hold
                    // their own Arc clones and keep resources alive until they exit
                    // (which happens when stop_flag is set in step 14).
                    if let Some(sd) = s.shutdown.take() {
                        sd();
                    }
                    // drain_handles intentionally NOT joined — see INVARIANT above.
                    // We drop s here, which detaches any remaining JoinHandle.
                }

                // Step 9: invoke cached builder with a fresh stop_flag.
                let fresh_stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let new_bundle = match (builder)(
                    cache.udp_port,
                    cache.service_name.clone(),
                    fresh_stop_flag.clone(),
                    cache.channel.clone(),
                ) {
                    Ok(b) => b,
                    Err(_) => {
                        let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
                        return;
                    }
                };

                // Step 11: acquire bridge.session and swap to the new session.
                {
                    let mut g = bridge_session.lock().unwrap();
                    *g = Some(SenderSession {
                        stop_flag: fresh_stop_flag,
                        drain_handles: new_bundle.drain_handles,
                        channel: cache.channel.clone(),
                        counters: Arc::new(SenderCounters::default()),
                        shutdown: new_bundle.shutdown,
                    });
                }

                // Step 13: signal success — supervisor wakes from recv_timeout,
                // transitions Rebuilding → Connected, and emits StateChanged(Connected).
                let _ = signal_tx.try_send(SupervisorSignal::RebuildSucceeded);

                // Step 14 (zombie-drain exit): stop the OLD supervisor so the coordinator
                // loop exits via the natural `outcome_rx` Disconnected path.
                //
                // We send Stop AFTER RebuildSucceeded. The supervisor processes them in
                // FIFO order: first RebuildSucceeded (→ Connected, emit StateChanged),
                // then Stop (→ Stopped, return None → outcome_rx disconnects).
                // The coordinator drains both outcomes before exiting. This avoids the
                // race where setting `old_stop_flag = true` causes the coordinator to
                // exit BEFORE processing StateChanged(Connected).
                //
                // The NEW bundle's NEW drain is already running independently; it will
                // handle its own supervisor lifecycle. The OLD coordinator loop is now
                // a zombie — exiting it is correct.
                let _ = signal_tx.try_send(SupervisorSignal::Stop);

                // Also set old_stop_flag so the OLD drain's transport-event loop
                // (which runs before entering supervisor mode) exits if the worker
                // fires during a non-supervisor iteration. This is belt-and-suspenders
                // for the case where stop_flag is checked in the outer drain loop.
                old_stop_flag.store(true, Ordering::Relaxed);
            });

        if spawn_result.is_err() {
            // Thread spawn failed — signal failure immediately so supervisor doesn't block.
            let _ = signal_tx_for_err.try_send(SupervisorSignal::RebuildFailed);
        }
        // Worker thread is detached (JoinHandle dropped). It exits after signaling.
    })
}

// ─── retry_session_inner — core of retry_session ─────────────────────────────

/// Retry a sender session after `Dead` state (spec §4.2, T11.1, AC-8).
///
/// Reads the cached start params from `SenderBridge::restart_cache` and
/// re-initialises the session using a fresh `channel`.
///
/// # Error variants
///
/// | Error string | Condition |
/// |---|---|
/// | `"NoCachedParams: ..."` | No session was ever started (cache is empty). |
///
/// # Behaviour
///
/// If a session is still active (e.g. the user invokes retry while streaming),
/// `retry_session_inner` stops the existing session first and re-starts it.
/// This is idempotent: stopping an already-dead session is a no-op for join/cleanup.
pub fn retry_session_inner(
    bridge: &SenderBridge,
    channel: Arc<dyn ChannelLike>,
) -> Result<(), String> {
    // Read cached params — None means no session was ever started.
    let (udp_port, service_name) = {
        let guard = bridge.restart_cache.lock().unwrap();
        match &*guard {
            None => {
                return Err(
                    "NoCachedParams: no cached session params — start a session first".to_string(),
                );
            }
            Some(c) => (c.udp_port, c.service_name.clone()),
        }
    };

    // Stop any existing session (idempotent — fast if drain threads have already exited).
    // This also clears current_args so start_sender_inner won't see AlreadyRunning.
    stop_sender_session(bridge);

    // Re-start with cached params and the new channel.
    // start_sender_inner populates restart_cache with a fresh session_nonce.
    start_sender_inner(bridge, channel, Some(udp_port), Some(service_name))
        .map_err(|e| format!("retry_session start_sender_inner failed: {e}"))
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
    _bridge_session: Arc<Mutex<Option<SenderSession>>>,
    _bridge_cache: Arc<Mutex<Option<RestartCache>>>,
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
    // signaling_arc is shared between the drain thread (coordinator hooks) and the
    // shutdown closure. Both hold an Arc clone so the MdnsSignaling stays alive
    // until shutdown() is called by stop_sender_session.
    let signaling_arc = Arc::new(Mutex::new(signaling));
    // Clone for the production coordinator hooks BEFORE moving into shutdown.
    let signaling_for_hooks = signaling_arc.clone();

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

    // `_counters` not forwarded in the production path — production drain uses
    // `coordinator_hooks` instead. Kept to avoid removing the type from scope.
    let _counters = Arc::new(SenderCounters::default());

    // ── 5. Build production coordinator hooks ─────────────────────────────────
    // These closures close over `signaling_for_hooks` (Arc<Mutex<MdnsSignaling>>).
    // CRITICAL-2: the TODO stubs are now wired to real signaling calls.
    let sig_for_req = signaling_for_hooks.clone();
    let sig_for_ack = signaling_for_hooks.clone();
    let sig_for_reset = signaling_for_hooks.clone();

    let coordinator_hooks = SenderCoordinatorHooks {
        publish_reconnect_request: Arc::new(move |attempt, session_nonce| {
            let sig = sig_for_req.lock().unwrap();
            if let Err(e) = sig.publish_reconnect_request(
                attempt,
                sm_domain::signaling::SignalingRole::Sender,
                session_nonce,
            ) {
                eprintln!("[sm-sender-coord] publish_reconnect_request failed: {e}");
            }
        }),
        publish_reconnect_ack: Arc::new(move |attempt, session_nonce| {
            let sig = sig_for_ack.lock().unwrap();
            if let Err(e) = sig.publish_reconnect_ack(attempt, session_nonce) {
                eprintln!("[sm-sender-coord] publish_reconnect_ack failed: {e}");
            }
        }),
        // V2: spawn a worker thread that rebuilds the bundle without blocking the drain.
        // The worker uses `bridge_session` and `bridge_cache` arcs (passed in alongside
        // the regular builder args) so it can swap the session under a brief lock.
        // `_stop_flag` is the OLD session's stop_flag — used as the cancel signal.
        //
        // FIX (Batch 2 bugfix): the inner builder closure MUST capture and forward the
        // REAL `_bridge_session` / `_bridge_cache` arcs to every recursive call of
        // `build_production_sender_bundle`.  Passing `Arc::new(Mutex::new(None))` here
        // was the bug: the newly-built bundle's own hook held dummy arcs that nobody
        // observed, so a second-generation failure swapped into the void rather than into
        // `bridge.session`, causing a ZOMBIE after the first auto-rebuild (AC-5 violated).
        initiate_rebuild: make_sender_rebuild_hook(
            // Pass the REAL bridge arcs through so every generation's hook can swap
            // into the same `bridge.session` field the supervisor observes.
            {
                let session_for_inner = _bridge_session.clone();
                let cache_for_inner = _bridge_cache.clone();
                Arc::new(move |udp_port, service_name, stop_flag, channel| {
                    build_production_sender_bundle(
                        udp_port,
                        service_name,
                        stop_flag,
                        channel,
                        session_for_inner.clone(),
                        cache_for_inner.clone(),
                    )
                })
            },
            _bridge_cache.clone(),
            _bridge_session.clone(),
            _stop_flag.clone(),
            1, // attempt — supervisor attempt counter; 1 as the default for production hook
        ),
        initiate_mdns_reset: Arc::new(move || {
            // MdnsSignaling::reset() consumes self. Since we hold an Arc<Mutex<>>,
            // we call stop() in-place (which is what reset() does under the hood)
            // then call start() again with the same config to re-engage discovery.
            // This is safe: the coordinator is the only writer during reconnect.
            eprintln!(
                "[sm-sender-coord] InitiateMdnsReset — calling MdnsSignaling::stop() + re-engaging discovery"
            );
            let mut sig = sig_for_reset.lock().unwrap();
            if let Err(e) = sig.stop() {
                eprintln!("[sm-sender-coord] MdnsSignaling::stop() failed: {e}");
            }
            // Re-start with a fresh event channel. The supervisor will route incoming
            // frames via the existing supervisor_signal_tx (already set on the signaling
            // instance via set_supervisor_signal_tx before start() was first called).
            let (sig_ev_tx, _sig_ev_rx) = std::sync::mpsc::sync_channel(4);
            if let Err(e) = sig.start(sig_ev_tx) {
                eprintln!("[sm-sender-coord] MdnsSignaling::start() after reset failed: {e}");
            }
        }),
    };

    // ── 6. Spawn drain threads ────────────────────────────────────────────────
    let stop_flag = _stop_flag.clone();
    let sig_channel = _channel.clone();
    let tr_channel = _channel.clone();
    let sig_stop = stop_flag.clone();
    let tr_stop = stop_flag.clone();

    let sig_drain = std::thread::Builder::new()
        .name("sm-sender-signaling-drain".into())
        .spawn(move || {
            run_sender_signaling_drain(sig_ev_rx, sender_ops, sig_stop, sig_channel);
        })
        .map_err(|e| BundleError::Other(format!("spawn sig drain: {e}")))?;

    // Production transport drain with real coordinator hooks (CRITICAL-2).
    let sup_tx = Arc::new(Mutex::new(None::<SyncSender<SupervisorSignal>>));
    let tr_drain = std::thread::Builder::new()
        .name("sm-sender-transport-drain".into())
        .spawn(move || {
            run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                tr_ev_rx,
                tr_stop,
                tr_channel,
                sup_tx,
                ReconnectPolicy::v1_default(),
                Duration::from_secs(2),
                coordinator_hooks,
            );
        })
        .map_err(|e| BundleError::Other(format!("spawn transport drain: {e}")))?;

    // C1 fix: move production arcs into the shutdown closure so they outlive the
    // bundle-build call and are dropped in order ONLY when stop_sender_session runs.
    let shutdown: Box<dyn FnOnce() + Send> = Box::new(move || {
        drop(capture);
        drop(sender_arc);
        drop(encoder_arc);
        drop(signaling_arc); // drops AFTER signaling_for_hooks clones — correct lifecycle
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
    _bridge_session: Arc<Mutex<Option<SenderSession>>>,
    _bridge_cache: Arc<Mutex<Option<RestartCache>>>,
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

/// Retry the sender session after `Dead` state (spec §4.2, T11.1, AC-8).
///
/// Reads cached start params from `SenderBridge::restart_cache` and
/// re-initialises the session on the new `channel`. The attempt counter resets to 0
/// (fresh 3-attempt cycle). Any existing session residue is torn down first.
///
/// Also updates `dist/sender.js` Retry button: when Phase 11 lands, the JS
/// TODO stub `invoke("start_sender")` can be swapped to `invoke("retry_session", { channel })`.
///
/// # Errors
///
/// `"NoCachedParams"` — no session was ever started (the user cannot retry what they never started).
#[tauri::command]
pub fn retry_session(
    bridge: tauri::State<SenderBridge>,
    channel: tauri::ipc::Channel<InvokeResponseBody>,
) -> Result<(), String> {
    let channel_arc: Arc<dyn ChannelLike> = Arc::new(TauriSenderChannel(channel));
    retry_session_inner(&bridge, channel_arc)
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
