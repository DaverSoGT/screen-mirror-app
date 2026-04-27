//! Tauri IPC bridge — stream commands.
//!
//! Implements the Tauri command surface for the screen-mirror live stream:
//! `start_stream`, `stop_stream`, `attach_stream`, `stream_diagnostics`.
//!
//! # Architecture
//!
//! The bridge owns a `StreamBridge` state container (managed by Tauri) that holds:
//! - The active `VideoReceiver` (receives `EncodedPacket`s from the transport layer).
//! - An `Mp4Muxer` (builds fMP4 init + media segments).
//! - Bookkeeping counters (`init_emitted`, `dropped_segments`).
//!
//! A background thread (`sm-stream-mux`) drains the packet channel, fires PLI on
//! the first packet, buffers non-keyframe packets until the first IDR, builds the
//! init segment from SPS+PPS, and sends fMP4 frames to the WebView via
//! `tauri::ipc::Channel<InvokeResponseBody>` (binary, no JSON encoding).
//!
//! # OQ-tauri-emit-1 resolution — Channel\<Bytes\>
//!
//! V1 target: 1080p30 H.264 (~500 KB/segment, one fragment per IDR).
//! The original `app.emit(Vec<u8>)` path serialises via `serde_json` → JSON
//! `Array<number>` on the JS side: 1.5–2 MB per segment plus a synchronous
//! `JSON.parse` on the WebView main thread → perceptible jank at 1080p.
//!
//! **Resolution (B7-fix)**: `start_stream` accepts a
//! `tauri::ipc::Channel<InvokeResponseBody>` argument from the frontend.
//! The mux thread calls `channel.send(InvokeResponseBody::Raw(frame_bytes))`
//! where `frame_bytes[0]` is a discriminant byte (`0x00` = init, `0x01` = segment).
//! The Channel API delivers the raw bytes as an `ArrayBuffer` in JS — no JSON round-trip.
//!
//! API surface verified from `tauri-2.10.3/src/ipc/channel.rs`:
//! - `Channel<TSend>` is `Clone` + `Send + Sync` — safe to clone into the mux thread.
//! - `channel.send(body: TSend) -> crate::Result<()>` where `TSend: IpcResponse`.
//! - `InvokeResponseBody::Raw(Vec<u8>)` implements `IpcResponse` without JSON encoding.
//! - Small payloads (< 1024 B, e.g. init segment) take the `webview.eval` fast path.
//! - Larger payloads (fMP4 segments) use the fetch API path (async, no main-thread block).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sm_domain::encode::EncodedPacket;
use sm_domain::signaling::{Signaling, SignalingConfig, SignalingEvent, SignalingRole};
use sm_domain::transport::{
    TRANSPORT_CHANNEL_CAPACITY, TransportConfig, TransportError, TransportEvent, TransportRole,
    VideoReceiver,
};
use sm_infra::render::fmp4_muxer::{Mp4Muxer, extract_sps_pps_from_idr};
use sm_infra::signaling::mdns::MdnsSignaling;
use sm_infra::transport::Str0mVideoReceiver;
use tauri::ipc::InvokeResponseBody;

// ─── Frame discriminants ──────────────────────────────────────────────────────

/// Byte 0 of a raw channel frame identifying the payload type.
pub(crate) const FRAME_INIT: u8 = 0x00;
/// Byte 0 of a raw channel frame identifying a media segment.
pub(crate) const FRAME_SEGMENT: u8 = 0x01;

// ─── ChannelLike — abstraction over tauri::ipc::Channel for testability ──────

/// Minimal interface over a binary streaming channel.
///
/// Production impl wraps `tauri::ipc::Channel<InvokeResponseBody>` (Clone,
/// Send + Sync). Test impl (`FakeChannel`) captures bytes in a `Mutex<Vec<_>>`.
pub(crate) trait ChannelLike: Send + Sync {
    /// Send a raw frame. `discriminant` is byte 0 (`FRAME_INIT` or `FRAME_SEGMENT`).
    fn send_raw(&self, discriminant: u8, bytes: Vec<u8>) -> Result<(), String>;
}

/// Production wrapper: a cloned `tauri::ipc::Channel<InvokeResponseBody>`.
///
/// `Channel<T>` is `Clone` + `Send + Sync` — safe to clone into the mux thread.
struct TauriChannel(tauri::ipc::Channel<InvokeResponseBody>);

impl ChannelLike for TauriChannel {
    fn send_raw(&self, discriminant: u8, bytes: Vec<u8>) -> Result<(), String> {
        let mut frame = Vec::with_capacity(1 + bytes.len());
        frame.push(discriminant);
        frame.extend(bytes);
        self.0
            .send(InvokeResponseBody::Raw(frame))
            .map_err(|e| e.to_string())
    }
}

// ─── Diagnostics ─────────────────────────────────────────────────────────────

/// Counters exposed via `stream_diagnostics`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamStats {
    /// Number of media segments (moof+mdat) successfully emitted to the frontend.
    pub fragments_emitted: u64,
    /// Number of init segments emitted (should be 1 per session in V1).
    pub init_segments_emitted: u64,
    /// Number of segments dropped due to backpressure (drop-newest strategy).
    pub dropped_segments: u64,
    /// Number of `EncodedPacket`s dropped by the transport receiver (backpressure).
    pub receiver_dropped_frames: u64,
    /// Number of PLI (keyframe requests) fired toward the sender.
    pub keyframe_requests_fired: u64,
}

// ─── Bridge bookkeeping ───────────────────────────────────────────────────────

/// Shared bookkeeping counters for the mux thread + diagnostics command.
#[derive(Debug, Default)]
struct BridgeCounters {
    fragments_emitted: AtomicU64,
    init_segments_emitted: AtomicU64,
    dropped_segments: AtomicU64,
    keyframe_requests_fired: AtomicU64,
}

// ─── BuilderFn — injectable seam for ReceiverBundle construction ─────────────

/// Factory closure type: produces a fully-started `ReceiverBundle` given
/// runtime args `(udp_port, service_name, stop_flag)`.
///
/// Production: wraps `build_production_bundle` (ignores port/name for now;
/// B5 will wire them in). Tests inject a closure that returns a fake bundle
/// (FakeReceiver + disconnected pkt_rx + None signaling) without real sockets.
///
/// Resolved design decisions (design #288 §1.1):
/// - PQ-A: `Arc<dyn Fn(...) + Send + Sync>` — non-generic bridge keeps Tauri
///   `.manage()` happy and prevents infra types from leaking into `lib.rs`.
/// - PQ-B: `(u16, String, Arc<AtomicBool>)` — args flow EXPLICITLY; no capture.
/// - OQ-D1: plain `Arc<dyn Fn>` — no `Mutex` wrapper. The `Arc` is `Clone +
///   Send + Sync`; the underlying `Fn` (not `FnMut`) makes concurrent calls safe.
/// - OQ-D7: builder does NOT see the `Channel`; returns just `ReceiverBundle`.
pub(crate) type BuilderFn =
    Arc<dyn Fn(u16, String, Arc<AtomicBool>) -> Result<ReceiverBundle, String> + Send + Sync>;

// ─── BundleError — typed error returned by BuilderFn (C2) ────────────────────

/// App-level error returned by the bundle factory (`BuilderFn` and
/// `build_production_bundle`). Translated into `StartStreamError` at the
/// command boundary (`start_stream_inner`).
///
/// Two variants, no more (PQ-E):
/// - `PortInUse(u16)` — the only actionable failure; the frontend uses this
///   to suggest "try a different port".
/// - `Other(String)` — every other failure path: signaling init, signaling start,
///   `Str0mVideoReceiver::new` (config validation), thread spawn, future paths.
///
/// `pub(crate)`: NEVER reaches IPC (NR5). Only `StartStreamError` is serialized.
///
/// `Send + Sync`: enforced transitively (`u16: Send + Sync`, `String: Send + Sync`).
/// This is required because `BuilderFn`'s return type must be `Send + Sync`-bounded
/// to satisfy the `Arc<dyn Fn(...) -> Result<_, _> + Send + Sync>` typedef.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BundleError {
    #[error("UDP port {0} already in use")]
    PortInUse(u16),

    #[error("bundle build failed: {0}")]
    Other(String),
}

// ─── PortRejectReason — sub-enum for StartStreamError::InvalidPort ───────────

/// Why a `udp_port` value was rejected by `validate_udp_port`.
///
/// Serialized as a string variant (default serde enum representation) so the
/// JS layer sees `"reason": "Privileged"` — a bare JSON string, not an object.
///
/// Design #288 §1.3; spec #287 R3.3.
///
/// NOTE: `OutOfRange` was listed as a placeholder in the PQ decisions (#285)
/// but was DROPPED in the design phase. `u16` already bounds the value to
/// `0..=65535`; no out-of-range case exists after `Zero` and `Privileged` are
/// handled. If a future change adds an upper-bound forbidden range (e.g. forbid
/// 65535), add `OutOfRange { min: u16, max: u16 }` at that time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum PortRejectReason {
    /// `value == 0` — would silently corrupt the ICE candidate (Risk #1 from
    /// proposal: port 0 in the ICE candidate causes silent connectivity failure).
    Zero,
    /// `1..=1023` — privileged port range; cross-platform UX guard (PQ-C).
    Privileged,
}

// ─── StartStreamError — typed error enum for start_stream (C3) ───────────────

/// Typed error returned by `start_stream` (and `start_stream_inner` in B5+).
///
/// Tauri 2 serializes `Result<T, E>` where `E: serde::Serialize` to the JS layer.
/// `#[serde(tag = "kind", content = "data")]` produces JSON of the form:
///   `{"kind":"InvalidPort","data":{"value":80,"reason":"Privileged"}}`
/// which the frontend can branch on without parsing strings (OQ-D6 resolved).
///
/// Design #288 §1.2; spec #287 R3.1–R3.4.
///
/// All variants are now constructed: `BundleBuildFailed` (B2), `InvalidPort`/
/// `InvalidServiceName` (B3/B4 pure fns, called in B5-3 `start_stream_inner`).
/// `PortInUse` is constructed in B5-4; `AlreadyRunning` is constructed in B6.
/// All variants are now constructed: B2 (BundleBuildFailed), B3/B4 (InvalidPort/InvalidServiceName),
/// B5-4 (PortInUse), B6 (AlreadyRunning). No `#[allow(dead_code)]` attributes remain.
#[derive(Debug, thiserror::Error, serde::Serialize)]
#[serde(tag = "kind", content = "data")]
pub enum StartStreamError {
    /// A session is already active.
    ///
    /// Carries the args of the active session so the frontend can show
    /// "running with port=X service=Y". Both same-args and diff-args double-start
    /// return this variant (PQ-E: no silent ignore).
    ///
    #[error("a stream session is already running on port {current_port} ({current_service_name})")]
    AlreadyRunning {
        current_port: u16,
        current_service_name: String,
    },

    /// `udp_port` failed validation. See `PortRejectReason` for the cause.
    #[error("invalid udp_port {value}: {reason:?}")]
    InvalidPort {
        value: u16,
        reason: PortRejectReason,
    },

    /// `service_name` failed RFC 6763 validation.
    ///
    /// `reason` is a human-readable explanation (e.g. `"must end with '.local.'"`).
    #[error("invalid service_name {value:?}: {reason}")]
    InvalidServiceName { value: String, reason: String },

    /// The OS-level socket bind failed (e.g. `AddrInUse` after a recent stop).
    ///
    /// Distinguished from `BundleBuildFailed` so the frontend can suggest
    /// "try a different port" instead of a generic failure message.
    ///
    /// Constructed in `start_stream_inner` by the OQ-A1 substring-detection shim (B5-4).
    #[error("UDP port {port} is already in use")]
    PortInUse { port: u16 },

    /// Catch-all for failures inside `BuilderFn` (signaling start, str0m receiver
    /// init, drain spawn, etc.). Wraps the legacy `String` error from
    /// `build_production_bundle`. Translated at the command boundary.
    #[error("bundle build failed: {0}")]
    BundleBuildFailed(String),
}

// ─── Validation helpers (C4) ─────────────────────────────────────────────────

/// Reject `udp_port` values that would corrupt the ICE candidate or require
/// privileged binding.
///
/// Lives at the Tauri-shell layer, NOT inside the str0m adapter (per PQ-C,
/// spec #287 R4.1, R4.7).
///
/// Rules (spec #287 R4.2–R4.3):
/// - `0` → `Err(InvalidPort { reason: Zero })` — port 0 would silently corrupt
///   the ICE candidate (proposal Risk #1: OS picks an ephemeral port but the
///   ICE candidate would still advertise 0, causing silent connectivity failure).
/// - `1..=1023` → `Err(InvalidPort { reason: Privileged })` — privileged range;
///   cross-platform UX guard.
/// - `1024..=65535` → `Ok(())` — valid, non-privileged range.
///
/// Design #288 §5.1.
///
/// Called from `start_stream_inner` (B5-3) for validated `Some(port)` args.
/// Spec R4.1, R4.7.
pub(crate) fn validate_udp_port(value: u16) -> Result<(), StartStreamError> {
    if value == 0 {
        return Err(StartStreamError::InvalidPort {
            value,
            reason: PortRejectReason::Zero,
        });
    }
    if value < 1024 {
        return Err(StartStreamError::InvalidPort {
            value,
            reason: PortRejectReason::Privileged,
        });
    }
    Ok(())
}

// ─── Validation helper (C5) ──────────────────────────────────────────────────

/// RFC 6763 service-name validator.
///
/// Accepts strings matching `^_[A-Za-z0-9-]+\._[A-Za-z0-9-]+\.local\.$`:
/// - Both segments must start with `_`.
/// - Each segment after `_` must be non-empty `[A-Za-z0-9-]+`.
/// - The string must end with `.local.` (FQDN trailing dot required).
///
/// Hand-rolled char-class validator — NO `regex` crate (spec #287 R5.2, PQ-D,
/// design #288 OQ-D5).
///
/// Spec #287 R5.1, R5.2, R5.3, R5.4, R5.5, R5.6, R5.7.
///
/// Called from `start_stream_inner` (B5-3) for validated `Some(name)` args.
/// Spec R5.1, R5.7.
pub(crate) fn validate_service_name(s: &str) -> Result<(), StartStreamError> {
    let invalid = |reason: &str| StartStreamError::InvalidServiceName {
        value: s.to_string(),
        reason: reason.to_string(),
    };

    // Must end with ".local." (the FQDN trailing dot is required by RFC 6763).
    const SUFFIX: &str = ".local.";
    if !s.ends_with(SUFFIX) {
        return Err(invalid("must end with '.local.'"));
    }
    let head = &s[..s.len() - SUFFIX.len()];

    // Exactly two dot-separated segments between the start and ".local.".
    let dot_pos = match head.find('.') {
        Some(pos) => pos,
        None => {
            return Err(invalid(
                "must contain exactly one '.' separating service and protocol",
            ));
        }
    };
    // If there is more than one dot in `head`, the split would yield >2 parts.
    // Check: no second dot allowed in `head`.
    if head[dot_pos + 1..].contains('.') {
        return Err(invalid(
            "must contain exactly one '.' separating service and protocol",
        ));
    }

    let svc_seg = &head[..dot_pos];
    let proto_seg = &head[dot_pos + 1..];

    // Service segment: must start with '_' followed by 1+ [A-Za-z0-9-] chars.
    let svc_body = match svc_seg.strip_prefix('_') {
        Some(body) => body,
        None => return Err(invalid("service segment must start with '_'")),
    };
    if svc_body.is_empty() {
        return Err(invalid("service segment must not be empty after '_'"));
    }
    if !svc_body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid("service segment may only contain [A-Za-z0-9-]"));
    }

    // Protocol segment: must start with '_' followed by 1+ [A-Za-z0-9-] chars.
    let proto_body = match proto_seg.strip_prefix('_') {
        Some(body) => body,
        None => return Err(invalid("protocol segment must start with '_'")),
    };
    if proto_body.is_empty() {
        return Err(invalid("protocol segment must not be empty after '_'"));
    }
    if !proto_body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(invalid("protocol segment may only contain [A-Za-z0-9-]"));
    }

    Ok(())
}

// ─── StreamBridge — Capability A ─────────────────────────────────────────────

/// Tauri managed state for an active streaming session.
///
/// Held behind `State<StreamBridge>` in Tauri commands.
/// Wraps a `Mutex<Option<StreamSession>>` to allow mutation inside
/// immutable Tauri command references.
pub struct StreamBridge {
    session: Mutex<Option<StreamSession>>,

    /// Factory closure used by `start_stream` to build the `ReceiverBundle`.
    /// Plain `Arc<dyn Fn>` — no `Mutex`. Set once in `new_with_builder`; read-only
    /// thereafter. Cloned cheaply (one atomic increment) before each invocation so
    /// no borrow of `bridge.builder` is held during the (potentially slow) build.
    /// (Design #288 §1.4, §6; spec #287 R1.1, R1.5)
    pub(crate) builder: BuilderFn,

    /// Args of the currently-active session: `Some((port, name))` while running,
    /// `None` otherwise. Set by `start_stream_inner` AFTER the session is stored;
    /// cleared by `stop_stream_session` AFTER all threads join (design #288 §1.4, §4).
    ///
    /// OQ-D3 resolved: one `Mutex<Option<(u16, String)>>` — not two separate
    /// per-field mutexes. Plain `Mutex` (not `RwLock`): contention is bounded to
    /// start/stop only.
    ///
    /// Lock-ordering discipline (design §4):
    ///   start path: current_args FIRST, then session.
    ///   stop path:  session FIRST, then current_args.
    /// Future code MUST NOT acquire these locks in reverse order within a single path.
    pub(crate) current_args: Mutex<Option<(u16, String)>>,
}

impl StreamBridge {
    /// Create a bridge using the production `build_production_bundle` factory.
    ///
    /// `build_production_bundle` now accepts `(udp_port, service_name, stop_flag)`
    /// (B5 wiring). The wrapper closure passes all args through directly.
    /// (Spec #287 R1.2, R2.5; design §1.4 §6)
    pub fn new() -> Self {
        // Direct delegation: BuilderFn(port, name, stop_flag) →
        // build_production_bundle(port, name, stop_flag).
        // B5 removes the prior `|_port, _name, stop_flag|` shim.
        Self::new_with_builder(Arc::new(|port, name, stop_flag| {
            build_production_bundle(port, name, stop_flag)
        }))
    }

    /// Create a bridge with a custom builder factory (test seam).
    ///
    /// Both production code and test code may use this constructor.
    /// No `#[cfg(test)]` gate — the constructor is intentionally public so tests
    /// can inject fake builders without a setter (spec #287 R1.3, R1.4).
    pub(crate) fn new_with_builder(builder: BuilderFn) -> Self {
        Self {
            session: Mutex::new(None),
            builder,
            current_args: Mutex::new(None),
        }
    }

    /// Returns `true` if a session is currently running.
    ///
    /// Used in tests and diagnostics. In production code, running state is
    /// determined via `current_args` (set on start, cleared on stop). The
    /// `#[allow(dead_code)]` suppresses the dead-code lint for the `lib`
    /// target — this method IS exercised by the `#[cfg(test)]` suite.
    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.is_running())
            .unwrap_or(false)
    }
}

impl Default for StreamBridge {
    fn default() -> Self {
        Self::new()
    }
}

// ─── StreamSession — internal per-run state ───────────────────────────────────

/// Active stream session: receiver + channel + mux thread + counters.
struct StreamSession {
    /// Stop flag shared with the mux thread. Set by `stop_stream`.
    stop_flag: Arc<AtomicBool>,
    /// Join handle for the `sm-stream-mux` thread.
    mux_handle: Option<JoinHandle<()>>,
    /// Shared counters observable via `stream_diagnostics`.
    counters: Arc<BridgeCounters>,
    /// The receiver — kept alive so packets flow until stop.
    /// `Option` to allow ownership transfer on stop.
    receiver: Option<Box<dyn ReceiverOps>>,
    /// PLI rate-limit: timestamp of the last keyframe request.
    last_pli: Option<Instant>,
    /// Binary streaming channel to the WebView (F-fix-1: Channel<Bytes>).
    /// Kept here to extend the Arc's lifetime until stop_stream.
    #[allow(dead_code)]
    channel: Arc<dyn ChannelLike>,
    /// Signaling adapter — stopped in stop_stream. None in test sessions.
    signaling: Option<Box<dyn SignalingOps>>,
    /// Drain thread handles (transport-event + signaling-event drains).
    drain_handles: Vec<JoinHandle<()>>,
}

impl StreamSession {
    /// Returns `true` if the session's stop flag has not been set.
    /// Called by `StreamBridge::is_running()` — see that method's doc.
    #[allow(dead_code)]
    fn is_running(&self) -> bool {
        !self.stop_flag.load(Ordering::Relaxed)
    }
}

/// Minimal interface needed from the receiver by the bridge (avoids pulling the
/// full `VideoReceiver` bound into tests).
trait ReceiverOps: Send {
    /// Fire a PLI toward the sender.
    fn request_keyframe(&self) -> Result<(), TransportError>;
    /// Count of dropped frames (backpressure).
    fn dropped_frames(&self) -> u64;
}

/// Minimal interface needed from the signaling adapter by the bridge.
trait SignalingOps: Send {
    /// Stop the signaling adapter.
    fn stop(&mut self) -> Result<(), sm_domain::signaling::SignalingError>;
}

/// Receiver-side operations needed by the signaling drain thread.
///
/// Split from `ReceiverOps` so the drain thread can call `apply_remote_offer`
/// and `add_remote_candidate` without needing the full `ReceiverOps` surface.
trait SignalingReceiverOps: Send + Sync {
    /// Apply the remote SDP offer and return our local answer.
    fn apply_remote_offer(
        &self,
        offer: sm_domain::signaling::SdpOffer,
    ) -> Result<sm_domain::signaling::SdpAnswer, TransportError>;
    /// Forward a remote ICE candidate to the receiver.
    fn add_remote_candidate(
        &self,
        cand: sm_domain::signaling::IceCandidate,
    ) -> Result<(), TransportError>;
}

/// Signaling publish operations needed by the signaling drain thread.
///
/// Split from `SignalingOps` so the drain thread can publish answers and
/// candidates without needing `stop()`.
trait SignalingPublishOps: Send + Sync {
    /// Publish our local SDP answer to the peer.
    fn publish_local_answer(
        &self,
        answer: sm_domain::signaling::SdpAnswer,
    ) -> Result<(), sm_domain::signaling::SignalingError>;
    /// Publish a local ICE candidate to the peer.
    ///
    /// Used when trickle ICE candidates are received after the answer is sent.
    #[allow(dead_code)]
    fn publish_local_candidate(
        &self,
        cand: sm_domain::signaling::IceCandidate,
    ) -> Result<(), sm_domain::signaling::SignalingError>;
}

/// Factory closure type: produces a `Box<dyn ReceiverOps>` that has already had
/// its tick thread started and the `pkt_tx` wired in.
///
/// The factory also returns the `pkt_rx` end (owned by the mux thread) and an
/// optional `Box<dyn SignalingOps>` plus drain thread handles.
///
/// Signature:
/// ```
/// FnOnce(Arc<AtomicBool>) -> Result<ReceiverBundle, String>
/// ```
pub(crate) struct ReceiverBundle {
    /// The receiver, ready for PLI calls and `dropped_frames()` reads.
    receiver: Box<dyn ReceiverOps>,
    /// The packet receive end — handed to the mux thread.
    pkt_rx: Receiver<EncodedPacket>,
    /// Signaling adapter (optional — None in tests using FakeReceiver).
    signaling: Option<Box<dyn SignalingOps>>,
    /// Drain thread handles (transport-event drain + signaling-event drain).
    drain_handles: Vec<JoinHandle<()>>,
    /// Senders kept alive so their associated drain threads keep running.
    /// These are dropped first in stop_stream to unblock the drain threads.
    _drain_senders: Vec<SyncSender<()>>,
}

// ─── Drain functions (W2-fix-B, W2-fix-C) ────────────────────────────────────

/// Signaling-event drain loop.
///
/// Runs on its own OS thread spawned by `build_production_bundle`.
/// Dispatches `SignalingEvent`s:
/// - `OfferReceived(offer)` → `receiver.apply_remote_offer(offer)` → `signaling.publish_local_answer(answer)`
/// - `CandidateReceived(c)` → `receiver.add_remote_candidate(c)`
/// - `PeerFound` → log
/// - `Closed` / `Error` → log + exit
///
/// Exits when `stop_flag` is set or the event channel is disconnected.
fn run_signaling_drain(
    ev_rx: std::sync::mpsc::Receiver<SignalingEvent>,
    receiver: Arc<dyn SignalingReceiverOps>,
    signaling: Arc<dyn SignalingPublishOps>,
    stop_flag: Arc<AtomicBool>,
) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                SignalingEvent::PeerFound { host, port } => {
                    eprintln!("[sm-signaling-drain] peer found: {host}:{port}");
                }
                SignalingEvent::OfferReceived(offer) => match receiver.apply_remote_offer(offer) {
                    Ok(answer) => {
                        if let Err(e) = signaling.publish_local_answer(answer) {
                            eprintln!("[sm-signaling-drain] publish_local_answer failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[sm-signaling-drain] apply_remote_offer failed: {e}");
                    }
                },
                SignalingEvent::CandidateReceived(cand) => {
                    if let Err(e) = receiver.add_remote_candidate(cand) {
                        eprintln!("[sm-signaling-drain] add_remote_candidate failed: {e}");
                    }
                }
                SignalingEvent::Closed => {
                    eprintln!("[sm-signaling-drain] signaling closed");
                    break;
                }
                SignalingEvent::Error(e) => {
                    eprintln!("[sm-signaling-drain] signaling error: {e}");
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Transport-event drain loop (W2-fix-C).
///
/// Runs on its own OS thread. Absorbs `TransportEvent`s — logs significant
/// events (ICE connected/failed) and discards the rest. Exits when `stop_flag`
/// is set or the event channel is disconnected.
fn run_transport_event_drain(
    ev_rx: std::sync::mpsc::Receiver<TransportEvent>,
    stop_flag: Arc<AtomicBool>,
) {
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        match ev_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => match ev {
                TransportEvent::IceConnected => {
                    eprintln!("[sm-transport-drain] ICE connected");
                }
                TransportEvent::IceFailed => {
                    eprintln!("[sm-transport-drain] ICE failed");
                }
                TransportEvent::ConnectionLost { reason } => {
                    eprintln!("[sm-transport-drain] connection lost: {reason}");
                }
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ─── Session builder ─────────────────────────────────────────────────────────

/// Build a `StreamSession` from a `ReceiverBundle`, a `ChannelLike`, and a
/// pre-allocated `stop_flag`.
///
/// Extracted from `start_stream` so tests can inject fake receivers without
/// launching the Tauri runtime. Production code passes the real bundle built
/// from `Str0mVideoReceiver` + `MdnsSignaling` (whose drain threads already
/// hold a clone of `stop_flag`). Tests pass a bundle with a `FakeReceiver`
/// and a fresh disconnected `pkt_rx`.
///
/// The mux thread takes ownership of `bundle.pkt_rx`.
fn build_stream_session(
    channel: Arc<dyn ChannelLike>,
    bundle: ReceiverBundle,
    stop_flag: Arc<AtomicBool>,
) -> Result<StreamSession, String> {
    let counters = Arc::new(BridgeCounters::default());

    let counters_clone = counters.clone();
    let stop_flag_clone = stop_flag.clone();
    let channel_for_thread = channel.clone();
    let pkt_rx = bundle.pkt_rx;

    let handle = thread::Builder::new()
        .name("sm-stream-mux".into())
        .spawn(move || {
            mux_thread(pkt_rx, stop_flag_clone, counters_clone, channel_for_thread);
        })
        .map_err(|e| format!("failed to spawn mux thread: {e}"))?;

    Ok(StreamSession {
        stop_flag,
        mux_handle: Some(handle),
        counters,
        receiver: Some(bundle.receiver),
        last_pli: None,
        channel,
        signaling: bundle.signaling,
        drain_handles: bundle.drain_handles,
    })
}

// ─── Production adapter wrappers ─────────────────────────────────────────────

/// Wrapper around `Arc<Mutex<Str0mVideoReceiver>>` implementing both
/// `ReceiverOps` and `SignalingReceiverOps`. All trait methods take `&self`
/// and acquire the Mutex for each call.
struct Str0mReceiverOps(Arc<Mutex<Str0mVideoReceiver>>);

impl ReceiverOps for Str0mReceiverOps {
    fn request_keyframe(&self) -> Result<(), TransportError> {
        self.0.lock().unwrap().request_keyframe()
    }
    fn dropped_frames(&self) -> u64 {
        self.0.lock().unwrap().dropped_frames()
    }
}

impl SignalingReceiverOps for Str0mReceiverOps {
    fn apply_remote_offer(
        &self,
        offer: sm_domain::signaling::SdpOffer,
    ) -> Result<sm_domain::signaling::SdpAnswer, TransportError> {
        self.0.lock().unwrap().apply_remote_offer(offer)
    }
    fn add_remote_candidate(
        &self,
        cand: sm_domain::signaling::IceCandidate,
    ) -> Result<(), TransportError> {
        self.0.lock().unwrap().add_remote_candidate(cand)
    }
}

/// Wrapper around `Arc<Mutex<MdnsSignaling>>` implementing both
/// `SignalingOps` and `SignalingPublishOps`.
struct MdnsSignalingOps(Arc<Mutex<MdnsSignaling>>);

impl SignalingOps for MdnsSignalingOps {
    fn stop(&mut self) -> Result<(), sm_domain::signaling::SignalingError> {
        self.0.lock().unwrap().stop()
    }
}

impl SignalingPublishOps for MdnsSignalingOps {
    fn publish_local_answer(
        &self,
        answer: sm_domain::signaling::SdpAnswer,
    ) -> Result<(), sm_domain::signaling::SignalingError> {
        self.0.lock().unwrap().publish_local_answer(answer)
    }
    fn publish_local_candidate(
        &self,
        cand: sm_domain::signaling::IceCandidate,
    ) -> Result<(), sm_domain::signaling::SignalingError> {
        self.0.lock().unwrap().publish_local_candidate(cand)
    }
}

/// Build the `SignalingConfig` for a receiver session.
///
/// Extracted pure helper so `build_production_bundle` can be tested without
/// binding real sockets, and so `service_name` is provably threaded into
/// `SignalingConfig::service_name` (B5-fix-A: the original inline construction
/// left `service_name` shadowed by `..SignalingConfig::default()`).
///
/// Spec R2.5: BuilderFn receives the resolved `(port, service_name, stop_flag)` tuple.
/// Explore #284: `SignalingConfig::service_name` is the correct vehicle for the
/// mDNS service-type string.
pub(crate) fn build_signaling_config_for_receiver(
    udp_port: u16,
    service_name: String,
) -> SignalingConfig {
    SignalingConfig {
        service_name,
        control_port: udp_port,
        role: SignalingRole::Receiver,
        peer_hint: None,
    }
}

/// Build the production `ReceiverBundle`: real `Str0mVideoReceiver` + `MdnsSignaling`.
///
/// The signaling adapter is started first so it begins mDNS discovery immediately.
/// The receiver is started second. Drain threads are spawned for transport events
/// (W2-fix-C) and signaling events (W2-fix-B). All threads share `stop_flag`.
///
/// `udp_port` and `service_name` come from the resolved `start_stream` args
/// (defaults 7889 / "_screen-mirror._tcp.local." applied before this call).
/// Both hardcoded values have been removed — the call site controls them.
///
/// Returns immediately — the SDP/ICE handshake completes asynchronously.
///
/// Spec R2.5: BuilderFn receives the resolved (port, service_name, stop_flag) tuple.
/// Design §1 Glossary; Delta-spec table (prior: hardcoded 7889, new: parameterized).
fn build_production_bundle(
    udp_port: u16,
    service_name: String,
    stop_flag: Arc<AtomicBool>,
) -> Result<ReceiverBundle, String> {
    // ── 1. Build MdnsSignaling (Receiver role) ─────────────────────────────
    let sig_config = build_signaling_config_for_receiver(udp_port, service_name);
    let mut signaling =
        MdnsSignaling::new(sig_config).map_err(|e| format!("MdnsSignaling::new failed: {e}"))?;

    let (sig_event_tx, sig_event_rx) = sync_channel::<SignalingEvent>(TRANSPORT_CHANNEL_CAPACITY);
    signaling
        .start(sig_event_tx)
        .map_err(|e| format!("MdnsSignaling::start failed: {e}"))?;

    // ── 2. Build Str0mVideoReceiver (Receiver role) ────────────────────────
    let transport_config = TransportConfig {
        udp_port,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    };
    let mut receiver = Str0mVideoReceiver::new(transport_config)
        .map_err(|e| format!("Str0mVideoReceiver::new failed: {e}"))?;

    let (pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
    let (transport_event_tx, transport_event_rx) =
        sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);

    receiver
        .start(pkt_tx, transport_event_tx)
        .map_err(|e| format!("Str0mVideoReceiver::start failed: {e}"))?;

    // ── 3. Wrap in Arc<Mutex<>> so both trait objects share the same instance ─
    let receiver_mutex = Arc::new(Mutex::new(receiver));
    let signaling_mutex = Arc::new(Mutex::new(signaling));

    // Clone Arcs: each consumer gets its own Arc clone pointing to the same Mutex.
    // recv_ops_for_bridge is a plain Str0mReceiverOps (implements ReceiverOps).
    // recv_ops_for_drain is an Arc<dyn SignalingReceiverOps> for the drain thread.
    let recv_ops_for_bridge = Str0mReceiverOps(receiver_mutex.clone());
    let recv_ops_for_drain: Arc<dyn SignalingReceiverOps> =
        Arc::new(Str0mReceiverOps(receiver_mutex));

    let sig_ops_for_stop: Arc<Mutex<MdnsSignaling>> = signaling_mutex.clone();
    let sig_publish_for_drain: Arc<dyn SignalingPublishOps> =
        Arc::new(MdnsSignalingOps(signaling_mutex));

    // ── 4. Spawn transport-event drain thread (W2-fix-C) ──────────────────
    let stop_flag_t = stop_flag.clone();
    let transport_drain = thread::Builder::new()
        .name("sm-transport-event-drain".into())
        .spawn(move || {
            run_transport_event_drain(transport_event_rx, stop_flag_t);
        })
        .map_err(|e| format!("failed to spawn transport-event drain: {e}"))?;

    // ── 5. Spawn signaling-event drain thread (W2-fix-B) ──────────────────
    let stop_flag_s = stop_flag.clone();
    let sig_drain = thread::Builder::new()
        .name("sm-signaling-event-drain".into())
        .spawn(move || {
            run_signaling_drain(
                sig_event_rx,
                recv_ops_for_drain,
                sig_publish_for_drain,
                stop_flag_s,
            );
        })
        .map_err(|e| format!("failed to spawn signaling-event drain: {e}"))?;

    // ── 6. Build SignalingOps for stop_stream ─────────────────────────────
    struct MdnsStopOps(Arc<Mutex<MdnsSignaling>>);
    impl SignalingOps for MdnsStopOps {
        fn stop(&mut self) -> Result<(), sm_domain::signaling::SignalingError> {
            self.0.lock().unwrap().stop()
        }
    }

    Ok(ReceiverBundle {
        receiver: Box::new(recv_ops_for_bridge),
        pkt_rx,
        signaling: Some(Box::new(MdnsStopOps(sig_ops_for_stop))),
        drain_handles: vec![transport_drain, sig_drain],
        _drain_senders: vec![],
    })
}

// ─── Tauri commands ───────────────────────────────────────────────────────────

/// Core of `start_stream` — extracted for unit testing without the Tauri runtime.
///
/// `bridge` is a plain reference (not `tauri::State`) so unit tests can call it
/// directly. `channel` is an `Arc<dyn ChannelLike>` — production code wraps the
/// Tauri `Channel<InvokeResponseBody>` in `TauriChannel`; tests use `FakeChannel`.
///
/// `udp_port` and `service_name` accept `Option<_>` — `None` resolves to the
/// respective default (7889 and "_screen-mirror._tcp.local." respectively).
///
/// Validation order (design §3):
/// 1. Validate `udp_port` (if `Some`) — pure fn, no locks held.
/// 2. Validate `service_name` (if `Some`) — pure fn, no locks held.
/// 3. Resolve defaults for `None` args — static values, not validated (known-good).
/// 4. Acquire `current_args` lock; check `Some(cur)` → `Err(AlreadyRunning)` (PQ-E).
///    Release lock before builder invocation (builder may take seconds).
/// 5. Invoke the `BuilderFn` with resolved `(port, name, stop_flag)`.
/// 6. Acquire session lock; store session.
/// 7. Set `current_args = Some((port, name))`.
///
/// Lock-ordering discipline (design §4):
///   start path: `current_args` FIRST, then `session`.
///   stop path: `session` FIRST, then `current_args` (see `stop_stream_session`).
///
/// Design §10 OQ-A2 (option a): `pub(crate)` so the `#[tauri::command]` wrapper is
/// a thin 4-line forwarder and tests exercise the same code path.
pub(crate) fn start_stream_inner(
    bridge: &StreamBridge,
    channel: Arc<dyn ChannelLike>,
    udp_port: Option<u16>,
    service_name: Option<String>,
) -> Result<(), StartStreamError> {
    // Step 1 — Validate udp_port BEFORE acquiring any lock (design §3 step 1).
    // Pure fn; cannot deadlock. Returns Err(InvalidPort{..}) on 0 or < 1024.
    // Spec R4.1, R4.7; design §5.1.
    if let Some(p) = udp_port {
        validate_udp_port(p)?;
    }

    // Step 2 — Validate service_name BEFORE acquiring any lock (design §3 step 2).
    // Pure fn; cannot deadlock. Returns Err(InvalidServiceName{..}) on RFC 6763 mismatch.
    // Spec R5.1, R5.7; design §5.2.
    if let Some(ref s) = service_name {
        validate_service_name(s)?;
    }

    // Step 3 — Resolve defaults (design §3 step 3).
    // Defaults are static and known-good — not re-validated (saving two fn calls per
    // default-path invocation). Design §3 rationale: if defaults ever change to
    // invalid values, unit tests for validate_* will still pass; only the
    // start_stream_inner integration tests will catch it.
    // Spec R2.2: None udp_port → 7889. Spec R2.3: None service_name → default.
    let resolved_port = udp_port.unwrap_or(7889);
    let resolved_name = service_name.unwrap_or_else(|| "_screen-mirror._tcp.local.".to_string());

    // Step 4 — Acquire current_args lock FIRST (design §4 lock-order: current_args → session).
    //
    // Lock-ordering discipline (design §4, spec R6.6):
    //   start path: current_args FIRST, then session.
    //   stop path:  session FIRST, then current_args (see stop_stream_session).
    // This asymmetry is intentional and MUST NOT be reversed in future changes.
    // The start path needs to atomically check-and-set both; the stop path can
    // release session before current_args because no concurrent start will see
    // the inconsistent state (session is the visible signal of "running").
    {
        let args_guard = bridge.current_args.lock().unwrap();
        if let Some((cur_port, cur_name)) = &*args_guard {
            // PQ-E (spec R6.4): ALWAYS return AlreadyRunning on double-start, regardless
            // of whether the new args match the current args. No silent ignore.
            // Spec R6.5: error MUST carry the CURRENT session's args, NOT the new caller's.
            return Err(StartStreamError::AlreadyRunning {
                current_port: *cur_port,
                current_service_name: cur_name.clone(),
            });
        }
        // Fall through: current_args is None — no active session.
        // Drop args_guard here so the builder (potentially slow: sockets, mDNS) runs
        // without holding the lock. A concurrent start will also see None and enter the
        // builder — acceptable race for V1.1; the session lock below serializes the
        // final wiring. current_args will be set again after session is stored.
    }

    // Step 5 — Clone the builder Arc (cheap atomic increment) and invoke.
    // No borrow of bridge.builder held during the (potentially slow) build.
    // Design §6, OQ-D1.
    let builder = bridge.builder.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));

    let bundle = match (builder)(resolved_port, resolved_name.clone(), stop_flag.clone()) {
        Ok(b) => b,
        Err(e) => {
            // OQ-A1 — PortInUse substring detection shim (B5-4).
            //
            // `build_production_bundle` wraps `std::io::Error` via `format!("...: {e}")`.
            // The OS-level `AddrInUse` message differs per platform:
            //   Linux/macOS : "address already in use"
            //   Windows     : "only one usage of each socket address ..."
            //
            // We lowercase-compare to tolerate capitalisation differences (e.g. macOS
            // `Error { kind: AddrInUse, message: "Address already in use" }`).
            //
            // FIXME(V1.2): replace string-match with typed bundle errors once
            // `build_production_bundle` returns a typed error enum.
            let e_lower = e.to_lowercase();
            if e_lower.contains("address already in use")
                || e_lower.contains("only one usage of each socket address")
            {
                return Err(StartStreamError::PortInUse {
                    port: resolved_port,
                });
            }
            return Err(StartStreamError::BundleBuildFailed(e));
        }
    };

    // Step 6 — Acquire session lock and store the new session.
    let mut guard = bridge.session.lock().unwrap();
    let session = build_stream_session(channel, bundle, stop_flag)
        .map_err(StartStreamError::BundleBuildFailed)?;
    *guard = Some(session);
    drop(guard);

    // Step 7 — Populate current_args AFTER session is successfully stored.
    // Re-acquire current_args lock (released at end of step 4 block).
    // Spec R6.2: "When start_stream completes successfully, current_args MUST
    // be set to Some((resolved_port, resolved_service_name))."
    *bridge.current_args.lock().unwrap() = Some((resolved_port, resolved_name));

    Ok(())
}

/// Start the streaming session.
///
/// Accepts a `Channel<InvokeResponseBody>` from the frontend (OQ-tauri-emit-1 pivot).
/// Builds a `Str0mVideoReceiver` + `MdnsSignaling` pair, starts both, spawns drain
/// threads for transport and signaling events, and spawns the `sm-stream-mux` thread
/// which drains packets, builds fMP4 segments, and delivers them as binary
/// `InvokeResponseBody::Raw` frames through the channel.
///
/// Frame layout (byte 0 = discriminant):
/// - `FRAME_INIT` (`0x00`) = fMP4 init segment (one per session)
/// - `FRAME_SEGMENT` (`0x01`) = fMP4 media segment (one per GOP)
///
/// `udp_port`: UDP port for the receiver socket. `None` → default 7889.
///   Valid range: 1024..=65535 (0 and 1..=1023 are rejected).
///   Spec R2.1, R2.2. Tauri 2 maps absent JS key → Rust `None` for `Option<T>`.
///
/// `service_name`: mDNS service-type name (RFC 6763 format). `None` → default
///   `"_screen-mirror._tcp.local."`.
///   Spec R2.1, R2.3, R2.6: must be owned `String` (not `&str`) for Tauri safety.
///
/// Returns `Ok(())` on success; `Err(StartStreamError)` on validation failure,
/// double-start (B6+), build failure, or OS-level port conflict.
///
/// Back-compat: `invoke("start_stream", { channel: streamChannel })` (no `udpPort`/
/// `serviceName` keys) continues to work — absent keys → `None` → defaults applied.
/// Spec R2.4.
///
/// Thin wrapper over `start_stream_inner` (extracted in B5-2 per design §10 OQ-A2).
/// Return type: `Result<(), StartStreamError>` (migrated from `Result<(), String>` in B2).
#[tauri::command]
pub fn start_stream(
    bridge: tauri::State<StreamBridge>,
    udp_port: Option<u16>,
    service_name: Option<String>,
    channel: tauri::ipc::Channel<InvokeResponseBody>,
) -> Result<(), StartStreamError> {
    let channel_arc: Arc<dyn ChannelLike> = Arc::new(TauriChannel(channel.clone()));
    start_stream_inner(&bridge, channel_arc, udp_port, service_name)
}

/// Core of `stop_stream` — extracted for unit testing without the Tauri runtime.
///
/// Shutdown order (W2-fix-D + B6 current_args clear):
/// 1. Acquire session lock; take the session (guard.take()).
/// 2. Set the stop flag — signals the mux thread and all drain threads.
/// 3. Join the mux thread (it owns pkt_rx; setting stop_flag causes it to exit).
/// 4. Join drain threads (they check stop_flag on every 500 ms timeout).
/// 5. Stop the signaling adapter.
/// 6. receiver and channel are dropped (their Drop impls call stop).
/// 7. Drop session lock FIRST (step 1 guard is dropped here).
/// 8. Acquire current_args lock; clear to None.
///
/// Lock-ordering discipline (design §4, spec R6.6):
///   stop path:  session FIRST, then current_args — this is the COMPLEMENTARY ordering
///   to start_stream_inner which acquires current_args FIRST, then session.
///   The asymmetry is intentional. Clearing current_args AFTER the session guard
///   is released ensures that a racing start_stream_inner which sees current_args=None
///   only enters the builder when the previous session is fully torn down.
///
/// Idempotent: if no session is active, returns immediately (session lock released
/// without touching current_args, which is already None).
fn stop_stream_session(bridge: &StreamBridge) {
    {
        // Lock-order step 1: acquire session lock FIRST (stop path: session → current_args).
        let mut guard = bridge.session.lock().unwrap();
        if let Some(mut session) = guard.take() {
            // 2. Signal stop to the mux thread and all drain threads.
            session.stop_flag.store(true, Ordering::Relaxed);

            // 3. Join the mux thread.
            if let Some(handle) = session.mux_handle.take() {
                let _ = handle.join();
            }

            // 4. Join drain threads.
            for handle in session.drain_handles.drain(..) {
                let _ = handle.join();
            }

            // 5. Stop the signaling adapter.
            if let Some(mut sig) = session.signaling.take() {
                let _ = sig.stop();
            }

            // 6. receiver and channel are dropped here (their Drop impls call stop).
        }
        // 7. Session lock (guard) is released here — explicit via block scope.
        //    Releasing session BEFORE acquiring current_args respects the lock order.
    }

    // 8. Acquire current_args lock AFTER session lock is released (design §4).
    //    Spec R6.3: clear current_args to None AFTER the drain+join+signaling stop
    //    completes. Clearing here ensures a concurrent start_stream_inner sees None
    //    only when the previous teardown is fully done.
    *bridge.current_args.lock().unwrap() = None;
}

/// Stop the streaming session. Idempotent.
#[tauri::command]
pub fn stop_stream(bridge: tauri::State<StreamBridge>) -> Result<(), String> {
    stop_stream_session(&bridge);
    Ok(())
}

/// Attach the frontend MSE consumer and fire a PLI to request an IDR.
///
/// Called from the frontend after `MediaSource` `sourceopen` fires.
/// Rate-limited to 1 PLI per 2-second window.
#[tauri::command]
pub fn attach_stream(bridge: tauri::State<StreamBridge>) -> Result<(), String> {
    let mut guard = bridge.session.lock().unwrap();
    if let Some(session) = guard.as_mut() {
        let now = Instant::now();
        let should_fire = session
            .last_pli
            .map(|t| now.duration_since(t) >= Duration::from_secs(2))
            .unwrap_or(true);

        if should_fire {
            if let Some(recv) = &session.receiver {
                let _ = recv.request_keyframe();
                session
                    .counters
                    .keyframe_requests_fired
                    .fetch_add(1, Ordering::Relaxed);
            }
            session.last_pli = Some(now);
        }
    }
    Ok(())
}

/// Return current streaming diagnostics.
#[tauri::command]
pub fn stream_diagnostics(bridge: tauri::State<StreamBridge>) -> Result<StreamStats, String> {
    let guard = bridge.session.lock().unwrap();
    let (fragments, inits, dropped, receiver_drops, pli_count) =
        if let Some(session) = guard.as_ref() {
            let c = &session.counters;
            (
                c.fragments_emitted.load(Ordering::Relaxed),
                c.init_segments_emitted.load(Ordering::Relaxed),
                c.dropped_segments.load(Ordering::Relaxed),
                session
                    .receiver
                    .as_ref()
                    .map(|r| r.dropped_frames())
                    .unwrap_or(0),
                c.keyframe_requests_fired.load(Ordering::Relaxed),
            )
        } else {
            (0, 0, 0, 0, 0)
        };

    Ok(StreamStats {
        fragments_emitted: fragments,
        init_segments_emitted: inits,
        dropped_segments: dropped,
        receiver_dropped_frames: receiver_drops,
        keyframe_requests_fired: pli_count,
    })
}

// ─── Mux thread — Capabilities B + D + F-fix-2 ───────────────────────────────

/// The `sm-stream-mux` thread body.
///
/// Drains `pkt_rx`, fires PLI on the first packet, buffers non-keyframe
/// packets until the first IDR, builds the fMP4 init segment from SPS+PPS,
/// and sends frames through the `ChannelLike` (F-fix-2: replaces app.emit).
fn mux_thread(
    pkt_rx: Receiver<EncodedPacket>,
    stop_flag: Arc<AtomicBool>,
    counters: Arc<BridgeCounters>,
    channel: Arc<dyn ChannelLike>,
) {
    // Muxer is created lazily on the first IDR (SPS+PPS required for init segment).
    let mut muxer: Option<Mp4Muxer> = None;
    let mut init_emitted = false;
    let mut pli_fired = false;
    // Pre-IDR buffer: accumulate non-keyframe packets until the first IDR.
    let mut pre_idr_buffer: Vec<EncodedPacket> = Vec::new();

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        let pkt = match pkt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(p) => p,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Fire PLI exactly once on the first packet received.
        if !pli_fired {
            pli_fired = true;
            // PLI is fired via attach_stream in V1 (frontend-driven).
            // Counter is incremented there. This marker prevents duplicate fires
            // from the mux thread itself.
        }

        if !pkt.is_keyframe {
            // R9.3: buffer non-keyframe packets until the first IDR.
            if !init_emitted {
                pre_idr_buffer.push(pkt);
                continue;
            }
            // After init is emitted: feed to muxer.
            if let Some(m) = muxer.as_mut() {
                if let Some(segment) = m.append_packet(&pkt) {
                    emit_segment(&channel, &counters, segment);
                }
            }
            continue;
        }

        // This is an IDR (keyframe) packet.
        if !init_emitted {
            // Extract SPS + PPS from this IDR to build the init segment.
            let sps_pps = extract_sps_pps_from_idr(&pkt.data);

            if let Some((sps, pps)) = sps_pps {
                // Determine dimensions (fallback: use a default; real dims come from SPS parser).
                let m = Mp4Muxer::new(1920, 1080, 30, 1);
                match m.build_init_segment(&sps, &pps) {
                    Ok(init_bytes) => {
                        emit_init(&channel, &counters, init_bytes);
                        init_emitted = true;
                        // Drop pre-IDR buffer (those frames are gone — no init to attach them to).
                        pre_idr_buffer.clear();
                        muxer = Some(m);
                    }
                    Err(e) => {
                        // SPS parse failure: keep buffering until a good IDR arrives.
                        eprintln!("[sm-stream-mux] build_init_segment failed: {e}");
                        pre_idr_buffer.push(pkt);
                        continue;
                    }
                }
            } else {
                // No SPS+PPS in this IDR — unusual; keep buffering.
                pre_idr_buffer.push(pkt);
                continue;
            }
        }

        // Feed the IDR to the muxer (may flush the previous GOP).
        if let Some(m) = muxer.as_mut() {
            if let Some(segment) = m.append_packet(&pkt) {
                emit_segment(&channel, &counters, segment);
            }
        }
    }
}

/// Send an fMP4 init segment through the channel (F-fix-2).
///
/// OQ-tauri-emit-1 pivot: uses `InvokeResponseBody::Raw` with discriminant byte
/// `FRAME_INIT` (`0x00`). No JSON encoding — binary delivery to the WebView.
/// JS side reads `data[0]` to distinguish init from segment.
fn emit_init(channel: &Arc<dyn ChannelLike>, counters: &BridgeCounters, bytes: Vec<u8>) {
    match channel.send_raw(FRAME_INIT, bytes) {
        Ok(_) => {
            counters
                .init_segments_emitted
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            counters.dropped_segments.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Send an fMP4 media segment through the channel (F-fix-2).
///
/// Backpressure (Capability D): if the channel send returns an error,
/// increment `dropped_segments` (drop-newest — no queuing, just drop).
fn emit_segment(channel: &Arc<dyn ChannelLike>, counters: &BridgeCounters, bytes: Vec<u8>) {
    match channel.send_raw(FRAME_SEGMENT, bytes) {
        Ok(_) => {
            counters.fragments_emitted.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            counters.dropped_segments.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32; // used in FakeReceiver::pli_count

    // ─── FakeReceiver: minimal VideoReceiver for unit tests ───────────────────

    /// Fake receiver that counts PLI calls and never blocks.
    struct FakeReceiver {
        pli_count: AtomicU32,
    }

    impl FakeReceiver {
        fn new() -> Self {
            Self {
                pli_count: AtomicU32::new(0),
            }
        }
    }

    impl ReceiverOps for FakeReceiver {
        fn request_keyframe(&self) -> Result<(), TransportError> {
            self.pli_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            0
        }
    }

    // ─── FakeChannel: captures raw frames for assertions ─────────────────────

    /// Fake channel that captures all raw frames sent through it.
    struct FakeChannel {
        frames: Mutex<Vec<Vec<u8>>>,
    }

    impl FakeChannel {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Vec::new()),
            })
        }

        /// Returns all captured frames (cloned).
        fn captured(&self) -> Vec<Vec<u8>> {
            self.frames.lock().unwrap().clone()
        }

        /// Returns true if any captured frame starts with the given discriminant.
        fn has_discriminant(&self, disc: u8) -> bool {
            self.frames
                .lock()
                .unwrap()
                .iter()
                .any(|f| f.first() == Some(&disc))
        }
    }

    impl ChannelLike for FakeChannel {
        fn send_raw(&self, discriminant: u8, bytes: Vec<u8>) -> Result<(), String> {
            let mut frame = Vec::with_capacity(1 + bytes.len());
            frame.push(discriminant);
            frame.extend(bytes);
            self.frames.lock().unwrap().push(frame);
            Ok(())
        }
    }

    // ─── BuilderProbe + make_test_builder — C7 test-double infrastructure ────
    //
    // Design #288 §7: `BuilderProbe` records every `(port, name)` pair the
    // `BuilderFn` receives. `make_test_builder` wraps it into a `BuilderFn` that
    // either returns a fake bundle (Ok variant) or a specified error (Err variant).
    //
    // Used by T7.1–T7.5, T7.8 (spec R7.1–R7.4). Matches design §7.1 exactly.
    // No struct, trait, or derive is introduced for the fake (R7.2).
    // No real sockets, no mDNS threads, no I/O (R7.3).

    /// Records every `(port, name)` invocation of a test `BuilderFn`.
    ///
    /// Construct via `BuilderProbe::new()`. Retrieve recorded calls via
    /// `probe.calls()`. The `Arc` wrapper allows the probe to be shared
    /// between the closure captured by `make_test_builder` and the test body.
    ///
    /// Design #288 §7.1: "Records the args every time the builder is invoked."
    #[derive(Default)]
    struct BuilderProbe {
        invocations: Mutex<Vec<(u16, String)>>,
    }

    impl BuilderProbe {
        /// Create a new, empty probe wrapped in an `Arc` for sharing.
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Return a snapshot of recorded `(port, name)` pairs (cloned).
        fn calls(&self) -> Vec<(u16, String)> {
            self.invocations.lock().unwrap().clone()
        }

        /// Return the number of times the builder was invoked.
        ///
        /// Used by T7.3–T7.5 to assert "builder NOT called" (spec R7.4 S7.2):
        /// `assert_eq!(probe.call_count(), 0)`.
        fn call_count(&self) -> usize {
            self.invocations.lock().unwrap().len()
        }

        /// Panic if the builder was not called exactly once.
        ///
        /// Used by T7.8 to prove the builder WAS invoked (validation passed, error
        /// originated inside the builder, not from a validation reject).
        fn assert_called_once(&self) {
            let count = self.invocations.lock().unwrap().len();
            assert_eq!(
                count, 1,
                "expected builder to be called exactly once, but it was called {count} times"
            );
        }
    }

    /// Build a `BuilderFn` that records invocations into `probe` and returns
    /// either a fake bundle (`Ok(())` path) or an error (`Err(&str)` path).
    ///
    /// The fake bundle uses `FakeReceiver`, a disconnected `pkt_rx`, and no
    /// signaling or drain handles. The disconnected `pkt_rx` causes the mux
    /// thread to exit cleanly on `Disconnected` — no real I/O, no sockets.
    ///
    /// Pass `result: Ok(())` for the happy path (T7.1, T7.2) or
    /// `result: Err("msg")` to simulate a builder failure (T7.8).
    ///
    /// Design #288 §7.1: "build a `BuilderFn` that, when invoked, records the args
    /// into `probe` and returns a fake bundle".
    fn make_test_builder(probe: Arc<BuilderProbe>, result: Result<(), &'static str>) -> BuilderFn {
        Arc::new(move |port, name, _stop_flag| {
            probe.invocations.lock().unwrap().push((port, name));
            match result {
                Ok(()) => {
                    let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
                    Ok(ReceiverBundle {
                        receiver: Box::new(FakeReceiver::new()),
                        pkt_rx,
                        signaling: None,
                        drain_handles: Vec::new(),
                        _drain_senders: Vec::new(),
                    })
                }
                Err(msg) => Err(msg.to_string()),
            }
        })
    }

    // ─── Helper: build a StreamBridge with an active session ─────────────────

    fn make_bridge_with_session() -> StreamBridge {
        make_bridge_with_channel(FakeChannel::new())
    }

    fn make_bridge_with_channel(channel: Arc<dyn ChannelLike>) -> StreamBridge {
        let bridge = StreamBridge::new();
        let counters = Arc::new(BridgeCounters::default());
        let stop_flag = Arc::new(AtomicBool::new(false));
        {
            let mut guard = bridge.session.lock().unwrap();
            *guard = Some(StreamSession {
                stop_flag,
                mux_handle: None,
                counters,
                receiver: Some(Box::new(FakeReceiver::new())),
                last_pli: None,
                channel,
                signaling: None,
                drain_handles: Vec::new(),
            });
        }
        bridge
    }

    // ─── F-fix-1 RED: bridge holds ChannelLike ───────────────────────────────

    /// F1.1 — FakeChannel captures frames with correct discriminant byte.
    #[test]
    fn fake_channel_captures_init_discriminant() {
        let ch = FakeChannel::new();
        ch.send_raw(FRAME_INIT, vec![0xAA, 0xBB]).unwrap();
        let frames = ch.captured();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0][0], FRAME_INIT);
        assert_eq!(&frames[0][1..], &[0xAA, 0xBB]);
    }

    /// F1.2 — FakeChannel captures segment discriminant correctly.
    #[test]
    fn fake_channel_captures_segment_discriminant() {
        let ch = FakeChannel::new();
        ch.send_raw(FRAME_SEGMENT, vec![0x01, 0x02, 0x03]).unwrap();
        assert!(ch.has_discriminant(FRAME_SEGMENT));
        let frames = ch.captured();
        assert_eq!(frames[0][0], FRAME_SEGMENT);
        assert_eq!(&frames[0][1..], &[0x01, 0x02, 0x03]);
    }

    /// F1.3 — StreamSession can be constructed with a FakeChannel.
    #[test]
    fn stream_session_holds_channel_like() {
        let ch = FakeChannel::new();
        let bridge = make_bridge_with_channel(ch.clone());
        assert!(bridge.is_running());
    }

    // ─── F-fix-2 RED: emit helpers route through Channel.send_raw ────────────

    /// F2.1 — emit_init sends FRAME_INIT discriminant through channel.
    #[test]
    fn emit_init_sends_init_discriminant() {
        let ch = FakeChannel::new();
        let counters = Arc::new(BridgeCounters::default());
        emit_init(
            &(ch.clone() as Arc<dyn ChannelLike>),
            &counters,
            vec![0xDE, 0xAD],
        );
        assert!(ch.has_discriminant(FRAME_INIT));
        assert_eq!(counters.init_segments_emitted.load(Ordering::Relaxed), 1);
    }

    /// F2.2 — emit_segment sends FRAME_SEGMENT discriminant through channel.
    #[test]
    fn emit_segment_sends_segment_discriminant() {
        let ch = FakeChannel::new();
        let counters = Arc::new(BridgeCounters::default());
        emit_segment(
            &(ch.clone() as Arc<dyn ChannelLike>),
            &counters,
            vec![0xBE, 0xEF],
        );
        assert!(ch.has_discriminant(FRAME_SEGMENT));
        assert_eq!(counters.fragments_emitted.load(Ordering::Relaxed), 1);
    }

    /// F2.3 — channel failure in emit_init increments dropped_segments.
    #[test]
    fn emit_init_channel_failure_increments_dropped() {
        struct FailChannel;
        impl ChannelLike for FailChannel {
            fn send_raw(&self, _: u8, _: Vec<u8>) -> Result<(), String> {
                Err("closed".into())
            }
        }
        let ch: Arc<dyn ChannelLike> = Arc::new(FailChannel);
        let counters = Arc::new(BridgeCounters::default());
        emit_init(&ch, &counters, vec![1, 2, 3]);
        assert_eq!(counters.dropped_segments.load(Ordering::Relaxed), 1);
        assert_eq!(counters.init_segments_emitted.load(Ordering::Relaxed), 0);
    }

    /// F2.4 — channel failure in emit_segment increments dropped_segments.
    #[test]
    fn emit_segment_channel_failure_increments_dropped() {
        struct FailChannel;
        impl ChannelLike for FailChannel {
            fn send_raw(&self, _: u8, _: Vec<u8>) -> Result<(), String> {
                Err("closed".into())
            }
        }
        let ch: Arc<dyn ChannelLike> = Arc::new(FailChannel);
        let counters = Arc::new(BridgeCounters::default());
        emit_segment(&ch, &counters, vec![1, 2, 3]);
        assert_eq!(counters.dropped_segments.load(Ordering::Relaxed), 1);
        assert_eq!(counters.fragments_emitted.load(Ordering::Relaxed), 0);
    }

    // ─── Capability A: bridge state container ────────────────────────────────

    /// A.1 — new bridge has no active session → is_running() is false.
    #[test]
    fn stream_bridge_new_is_not_running() {
        let bridge = StreamBridge::new();
        assert!(!bridge.is_running());
    }

    /// A.2 — bridge with a session (stop_flag = false) → is_running() is true.
    #[test]
    fn stream_bridge_with_session_is_running() {
        let bridge = make_bridge_with_session();
        assert!(bridge.is_running());
    }

    /// A.3 — setting stop_flag stops the session.
    #[test]
    fn stream_bridge_stop_flag_stops_session() {
        let bridge = make_bridge_with_session();
        assert!(bridge.is_running());
        {
            let guard = bridge.session.lock().unwrap();
            guard
                .as_ref()
                .unwrap()
                .stop_flag
                .store(true, Ordering::Relaxed);
        }
        assert!(!bridge.is_running());
    }

    /// A.4 — BridgeCounters default is all zeros.
    #[test]
    fn bridge_counters_default_is_zero() {
        let c = BridgeCounters::default();
        assert_eq!(c.fragments_emitted.load(Ordering::Relaxed), 0);
        assert_eq!(c.init_segments_emitted.load(Ordering::Relaxed), 0);
        assert_eq!(c.dropped_segments.load(Ordering::Relaxed), 0);
        assert_eq!(c.keyframe_requests_fired.load(Ordering::Relaxed), 0);
    }

    // ─── Capability B: init-segment timing guard (R9.3) ─────────────────────

    /// Build an Annex-B slice with 4-byte start codes from raw NAL slices.
    fn make_annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// B.1 — extract_sps_pps_from_idr finds SPS (type 7) and PPS (type 8).
    #[test]
    fn extract_sps_pps_finds_sps_and_pps() {
        // SPS NAL header = 0x67 (forbidden_zero_bit=0, nal_ref_idc=3, nal_unit_type=7).
        let sps_nal = &[0x67u8, 0x42, 0xE0, 0x1E];
        // PPS NAL header = 0x68 (nal_unit_type=8).
        let pps_nal = &[0x68u8, 0xCE, 0x38];
        let annex_b = make_annex_b(&[sps_nal, pps_nal]);

        let result = extract_sps_pps_from_idr(&annex_b);
        assert!(result.is_some(), "should find SPS and PPS");
        let (sps, pps) = result.unwrap();
        assert_eq!(sps, sps_nal);
        assert_eq!(pps, pps_nal);
    }

    /// B.2 — extract_sps_pps_from_idr returns None when SPS is missing.
    #[test]
    fn extract_sps_pps_returns_none_when_sps_missing() {
        let pps_nal = &[0x68u8, 0xCE, 0x38];
        let idr_nal = &[0x65u8, 0x00]; // nal_type = 5 (IDR)
        let annex_b = make_annex_b(&[pps_nal, idr_nal]);
        let result = extract_sps_pps_from_idr(&annex_b);
        assert!(result.is_none());
    }

    /// B.3 — extract_sps_pps_from_idr returns None when PPS is missing.
    #[test]
    fn extract_sps_pps_returns_none_when_pps_missing() {
        let sps_nal = &[0x67u8, 0x42, 0xE0, 0x1E];
        let annex_b = make_annex_b(&[sps_nal]);
        let result = extract_sps_pps_from_idr(&annex_b);
        assert!(result.is_none());
    }

    /// B.4 — extract_sps_pps_from_idr on empty input returns None.
    #[test]
    fn extract_sps_pps_empty_returns_none() {
        let result = extract_sps_pps_from_idr(&[]);
        assert!(result.is_none());
    }

    // ─── Capability C: PLI fire-once on attach ───────────────────────────────

    /// C.1 — attach_stream fires PLI exactly once when receiver is present.
    #[test]
    fn pli_fired_once_on_attach() {
        let bridge = make_bridge_with_session();

        // Simulate attach_stream logic (cannot use Tauri State in unit tests,
        // so we call the logic directly on the session).
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            let now = Instant::now();
            let should_fire = session.last_pli.is_none();
            if should_fire {
                if let Some(recv) = &session.receiver {
                    let _ = recv.request_keyframe();
                    session
                        .counters
                        .keyframe_requests_fired
                        .fetch_add(1, Ordering::Relaxed);
                }
                session.last_pli = Some(now);
            }
        }

        let guard = bridge.session.lock().unwrap();
        let session = guard.as_ref().unwrap();
        assert_eq!(
            session
                .counters
                .keyframe_requests_fired
                .load(Ordering::Relaxed),
            1,
            "PLI should have been fired exactly once"
        );
    }

    /// C.2 — second attach within 2s is rate-limited (no second PLI).
    #[test]
    fn pli_rate_limited_within_2s() {
        let bridge = make_bridge_with_session();

        // First attach.
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            session.last_pli = Some(Instant::now());
            session
                .counters
                .keyframe_requests_fired
                .fetch_add(1, Ordering::Relaxed);
        }

        // Second attach immediately (< 2s later).
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            let now = Instant::now();
            let elapsed = session
                .last_pli
                .map(|t| now.duration_since(t))
                .unwrap_or(Duration::MAX);
            let should_fire = elapsed >= Duration::from_secs(2);
            if should_fire {
                if let Some(recv) = &session.receiver {
                    let _ = recv.request_keyframe();
                    session
                        .counters
                        .keyframe_requests_fired
                        .fetch_add(1, Ordering::Relaxed);
                }
                session.last_pli = Some(now);
            }
        }

        // Should still be 1 — rate-limited.
        let guard = bridge.session.lock().unwrap();
        let session = guard.as_ref().unwrap();
        assert_eq!(
            session
                .counters
                .keyframe_requests_fired
                .load(Ordering::Relaxed),
            1,
            "second PLI within 2s should be rate-limited"
        );
    }

    // ─── Capability D: backpressure / drop-newest ────────────────────────────

    /// D.1 — dropped_segments counter is observable via BridgeCounters.
    #[test]
    fn dropped_segments_counter_increments() {
        let counters = Arc::new(BridgeCounters::default());
        assert_eq!(counters.dropped_segments.load(Ordering::Relaxed), 0);
        counters.dropped_segments.fetch_add(1, Ordering::Relaxed);
        counters.dropped_segments.fetch_add(1, Ordering::Relaxed);
        assert_eq!(counters.dropped_segments.load(Ordering::Relaxed), 2);
    }

    /// D.2 — fragments_emitted counter is observable.
    #[test]
    fn fragments_emitted_counter_increments() {
        let counters = Arc::new(BridgeCounters::default());
        counters.fragments_emitted.fetch_add(5, Ordering::Relaxed);
        assert_eq!(counters.fragments_emitted.load(Ordering::Relaxed), 5);
    }

    // ─── W2-fix-D RED: stop_stream cleans up all threads in correct order ────────

    /// D.STOP.1 — `stop_stream_session` (the extractable core of stop_stream) sets
    ///             stop_flag, joins the mux thread, joins drain threads, and stops
    ///             signaling — all within 500 ms.
    ///
    /// RED: `stop_stream_session` does not exist yet.
    #[test]
    fn stop_stream_joins_all_threads_within_500ms() {
        let ch: Arc<dyn ChannelLike> = FakeChannel::new();
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Build a bundle with two fake drain threads (100 ms sleep each).
        let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        let stop_for_drain1 = stop_flag.clone();
        let stop_for_drain2 = stop_flag.clone();

        let drain1 = thread::spawn(move || {
            // Simulate a drain that checks stop_flag with 50 ms polling.
            while !stop_for_drain1.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let drain2 = thread::spawn(move || {
            while !stop_for_drain2.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let bundle = ReceiverBundle {
            receiver: Box::new(FakeReceiver::new()),
            pkt_rx,
            signaling: None,
            drain_handles: vec![drain1, drain2],
            _drain_senders: Vec::new(),
        };

        let session =
            build_stream_session(ch, bundle, stop_flag).expect("build_stream_session must succeed");

        // Wrap in a bridge and call stop_stream_session.
        let bridge = StreamBridge::new();
        {
            let mut guard = bridge.session.lock().unwrap();
            *guard = Some(session);
        }

        let t0 = std::time::Instant::now();
        stop_stream_session(&bridge);
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "stop_stream_session must join all threads within 500 ms, took {elapsed:?}"
        );
        // Session must be cleared.
        assert!(
            !bridge.is_running(),
            "bridge must not be running after stop_stream_session"
        );
    }

    /// D.STOP.2 — `stop_stream_session` is idempotent: calling it twice on the same
    ///             bridge does not panic and returns promptly.
    #[test]
    fn stop_stream_is_idempotent() {
        let bridge = StreamBridge::new();
        // Call on an empty bridge — must not panic.
        stop_stream_session(&bridge);
        stop_stream_session(&bridge);
    }

    // ─── W2-fix-C RED: transport-event drain absorbs events without blocking ───

    /// C.T.1 — `run_transport_event_drain` processes all sent TransportEvents
    ///          and exits cleanly when stop_flag is set.
    ///          No panics, no blocking beyond the 500 ms timeout.
    #[test]
    fn transport_event_drain_absorbs_events_without_blocking() {
        use sm_domain::transport::TransportEvent;
        use std::sync::mpsc::sync_channel;

        let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();

        let drain = thread::spawn(move || {
            run_transport_event_drain(ev_rx, stop_clone);
        });

        // Send several events — the drain must absorb them without panicking.
        ev_tx.send(TransportEvent::IceConnected).unwrap();
        ev_tx.send(TransportEvent::IceFailed).unwrap();
        ev_tx
            .send(TransportEvent::ConnectionLost {
                reason: "test".to_string(),
            })
            .unwrap();
        ev_tx
            .send(TransportEvent::PacketDropped { count: 5 })
            .unwrap();

        // Allow time to process.
        std::thread::sleep(Duration::from_millis(50));

        // Signal stop.
        stop_flag.store(true, Ordering::Relaxed);
        drop(ev_tx);
        // Must join within a short time (no hang).
        let start = std::time::Instant::now();
        drain.join().expect("drain thread must exit cleanly");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(700),
            "drain must join within 700 ms, took {elapsed:?}"
        );
    }

    /// C.T.2 — When the transport-event channel disconnects (sender dropped),
    ///          the drain exits cleanly without needing stop_flag.
    #[test]
    fn transport_event_drain_exits_on_channel_disconnect() {
        use sm_domain::transport::TransportEvent;
        use std::sync::mpsc::sync_channel;

        let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(4);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();

        let drain = thread::spawn(move || {
            run_transport_event_drain(ev_rx, stop_clone);
        });

        // Drop the sender — the drain should see Disconnected and exit.
        drop(ev_tx);

        let start = std::time::Instant::now();
        drain
            .join()
            .expect("drain must exit after channel disconnect");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(700),
            "drain must join within 700 ms after disconnect, took {elapsed:?}"
        );
    }

    // ─── B1 RED: StreamBridge builder seam (R1.1–R1.5) ──────────────────────────

    /// B1.1 — `StreamBridge::new_with_builder` stores the supplied closure and
    ///         the bridge remains `Send + Sync` (compile-time assertion).
    ///
    /// RED: `BuilderFn`, `new_with_builder`, and the `builder` field do not exist.
    #[test]
    fn test_bridge_new_with_builder_stores_builder() {
        // This compile-time function asserts `StreamBridge: Send + Sync + 'static`.
        fn _assert_send_sync_static<T: Send + Sync + 'static>() {}
        _assert_send_sync_static::<StreamBridge>();

        // Construct a bridge with a closure that panics if called — the test only
        // checks that the constructor compiles and the builder is stored.
        let builder: BuilderFn =
            Arc::new(|_port, _name, _stop_flag| panic!("builder must not be called in this test"));
        let bridge = StreamBridge::new_with_builder(builder);
        // builder field must be populated: Arc strong count is at least 1.
        assert!(Arc::strong_count(&bridge.builder) >= 1);
        // current_args must default to None.
        assert!(bridge.current_args.lock().unwrap().is_none());
    }

    /// B1.2 — `StreamBridge::new()` delegates to `new_with_builder` and keeps the
    ///         builder accessible via `Arc::clone`.
    ///
    /// RED: `builder` field does not exist yet.
    #[test]
    fn test_bridge_new_uses_production_builder() {
        let bridge = StreamBridge::new();
        // The bridge is constructed; builder Arc is valid (strong count >= 1).
        assert!(Arc::strong_count(&bridge.builder) >= 1);
        // current_args defaults to None.
        assert!(bridge.current_args.lock().unwrap().is_none());
    }

    /// B1.3 — `Send + Sync + 'static` compile-time gate (R1.1, S1.3).
    ///
    /// RED: only fails to compile if `StreamBridge` is not `Send + Sync + 'static`.
    /// With the `Arc<dyn Fn + Send + Sync>` field this should always be GREEN
    /// immediately after B1.T2.
    #[test]
    fn test_send_sync_compile_gate() {
        // compile-time gate: the generics fn forces the compiler to verify the bound.
        fn _s<T: Send + Sync + 'static>() {}
        _s::<StreamBridge>();
    }

    // ─── W2-fix-B RED: signaling drain bridges offer → apply_remote_offer → publish_local_answer ──

    /// B.SIG.1 — On `SignalingEvent::OfferReceived`, `run_signaling_drain` calls
    ///            `receiver.apply_remote_offer(offer)` and then
    ///            `signaling.publish_local_answer(answer)`.
    ///
    /// RED: `run_signaling_drain` does not exist yet.
    #[test]
    fn signaling_drain_offer_received_calls_apply_and_publish() {
        use sm_domain::signaling::{SdpAnswer, SdpOffer, SignalingEvent};
        use std::sync::mpsc::sync_channel;

        // FakeReceiverForSig: records the offer it received and returns a canned answer.
        struct FakeReceiverForSig {
            last_offer: Mutex<Option<SdpOffer>>,
        }
        impl FakeReceiverForSig {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    last_offer: Mutex::new(None),
                })
            }
        }
        impl SignalingReceiverOps for FakeReceiverForSig {
            fn apply_remote_offer(&self, offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
                *self.last_offer.lock().unwrap() = Some(offer);
                Ok(SdpAnswer("v=0\r\nanswer".to_string()))
            }
            fn add_remote_candidate(
                &self,
                _cand: sm_domain::signaling::IceCandidate,
            ) -> Result<(), TransportError> {
                Ok(())
            }
        }

        // FakeSignalingForDrain: records the answer it received.
        struct FakeSignalingForDrain {
            last_answer: Mutex<Option<SdpAnswer>>,
        }
        impl FakeSignalingForDrain {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    last_answer: Mutex::new(None),
                })
            }
        }
        impl SignalingPublishOps for FakeSignalingForDrain {
            fn publish_local_answer(
                &self,
                answer: SdpAnswer,
            ) -> Result<(), sm_domain::signaling::SignalingError> {
                *self.last_answer.lock().unwrap() = Some(answer);
                Ok(())
            }
            fn publish_local_candidate(
                &self,
                _cand: sm_domain::signaling::IceCandidate,
            ) -> Result<(), sm_domain::signaling::SignalingError> {
                Ok(())
            }
        }

        let recv = FakeReceiverForSig::new();
        let sig = FakeSignalingForDrain::new();
        let (ev_tx, ev_rx) = sync_channel::<SignalingEvent>(8);
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Spawn the drain.
        let recv_clone = recv.clone();
        let sig_clone = sig.clone();
        let stop_clone = stop_flag.clone();
        let drain = thread::spawn(move || {
            run_signaling_drain(ev_rx, recv_clone, sig_clone, stop_clone);
        });

        // Send an OfferReceived event.
        let test_offer = SdpOffer("v=0\r\noffer".to_string());
        ev_tx
            .send(SignalingEvent::OfferReceived(test_offer.clone()))
            .unwrap();

        // Give the drain a moment to process.
        std::thread::sleep(Duration::from_millis(100));

        // Signal drain to stop.
        stop_flag.store(true, Ordering::Relaxed);
        drop(ev_tx);
        drain.join().unwrap();

        // Assert: receiver.apply_remote_offer was called with the correct offer.
        let received_offer = recv.last_offer.lock().unwrap().clone();
        assert_eq!(
            received_offer.as_ref(),
            Some(&test_offer),
            "apply_remote_offer must be called with the OfferReceived offer"
        );

        // Assert: signaling.publish_local_answer was called with the canned answer.
        let published_answer = sig.last_answer.lock().unwrap().clone();
        assert_eq!(
            published_answer.as_ref().map(|a| a.0.as_str()),
            Some("v=0\r\nanswer"),
            "publish_local_answer must be called with the answer from apply_remote_offer"
        );
    }

    /// B.SIG.2 — On `SignalingEvent::CandidateReceived`, `run_signaling_drain`
    ///            calls `receiver.add_remote_candidate(cand)`.
    #[test]
    fn signaling_drain_candidate_received_calls_add_remote_candidate() {
        use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer, SignalingEvent};
        use std::sync::atomic::AtomicU32;
        use std::sync::mpsc::sync_channel;

        struct FakeReceiverForCand {
            cand_count: AtomicU32,
        }
        impl FakeReceiverForCand {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    cand_count: AtomicU32::new(0),
                })
            }
        }
        impl SignalingReceiverOps for FakeReceiverForCand {
            fn apply_remote_offer(&self, _: SdpOffer) -> Result<SdpAnswer, TransportError> {
                Ok(SdpAnswer("v=0".to_string()))
            }
            fn add_remote_candidate(&self, _: IceCandidate) -> Result<(), TransportError> {
                self.cand_count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        struct NoopSignalingPublish;
        impl SignalingPublishOps for NoopSignalingPublish {
            fn publish_local_answer(
                &self,
                _: SdpAnswer,
            ) -> Result<(), sm_domain::signaling::SignalingError> {
                Ok(())
            }
            fn publish_local_candidate(
                &self,
                _: IceCandidate,
            ) -> Result<(), sm_domain::signaling::SignalingError> {
                Ok(())
            }
        }

        let recv = FakeReceiverForCand::new();
        let (ev_tx, ev_rx) = sync_channel::<SignalingEvent>(8);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let recv_clone = recv.clone();
        let stop_clone = stop_flag.clone();

        let drain = thread::spawn(move || {
            run_signaling_drain(
                ev_rx,
                recv_clone,
                Arc::new(NoopSignalingPublish),
                stop_clone,
            );
        });

        ev_tx
            .send(SignalingEvent::CandidateReceived(IceCandidate(
                "candidate:1".to_string(),
            )))
            .unwrap();
        ev_tx
            .send(SignalingEvent::CandidateReceived(IceCandidate(
                "candidate:2".to_string(),
            )))
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        stop_flag.store(true, Ordering::Relaxed);
        drop(ev_tx);
        drain.join().unwrap();

        assert_eq!(
            recv.cand_count.load(Ordering::Relaxed),
            2,
            "add_remote_candidate must be called once per CandidateReceived event"
        );
    }

    // ─── W2-fix-A RED: start_stream wires a real receiver (Some, not None) ─────

    /// Helper: build a test `ReceiverBundle` with a `FakeReceiver` and a
    /// disconnected `pkt_rx` (the sender end is dropped immediately so the
    /// mux thread sees `Disconnected` and exits cleanly when stop_flag is set).
    fn fake_bundle() -> ReceiverBundle {
        let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        ReceiverBundle {
            receiver: Box::new(FakeReceiver::new()),
            pkt_rx,
            signaling: None,
            drain_handles: Vec::new(),
            _drain_senders: Vec::new(),
        }
    }

    /// W2-A.1 — build_stream_session with a FakeReceiver bundle produces a
    ///           session with receiver = Some(_), not None.
    #[test]
    fn start_stream_wires_receiver_some_not_none() {
        let ch: Arc<dyn ChannelLike> = FakeChannel::new();
        let stop_flag = Arc::new(AtomicBool::new(false));

        let session = build_stream_session(ch, fake_bundle(), stop_flag)
            .expect("build_stream_session must succeed");
        // Eagerly stop the mux thread before the assertion so the test does not hang.
        session.stop_flag.store(true, Ordering::Relaxed);
        assert!(
            session.receiver.is_some(),
            "start_stream must wire receiver (Some), not leave it None"
        );
    }

    /// W2-A.2 — build_stream_session with FakeReceiver bundle spawns the mux
    ///           thread (mux_handle is Some after build).
    #[test]
    fn start_stream_spawns_mux_thread() {
        let ch: Arc<dyn ChannelLike> = FakeChannel::new();
        let stop_flag = Arc::new(AtomicBool::new(false));

        let mut session = build_stream_session(ch, fake_bundle(), stop_flag)
            .expect("build_stream_session must succeed");
        assert!(
            session.mux_handle.is_some(),
            "build_stream_session must spawn the mux thread"
        );
        // Clean up: signal stop and join.
        session.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = session.mux_handle.take() {
            let _ = h.join();
        }
    }

    // ─── Capability E: Tauri command signatures (compile gate) ───────────────

    /// E.1 — StreamStats implements serde::Serialize (compile gate).
    #[test]
    fn stream_stats_is_serializable() {
        let stats = StreamStats {
            fragments_emitted: 10,
            init_segments_emitted: 1,
            dropped_segments: 0,
            receiver_dropped_frames: 0,
            keyframe_requests_fired: 2,
        };
        let json = serde_json::to_string(&stats).expect("should serialize");
        assert!(json.contains("fragments_emitted"));
    }

    /// E.2 — StreamBridge::new() produces a bridge with no active session.
    #[test]
    fn stream_bridge_default_no_session() {
        let bridge = StreamBridge::default();
        assert!(!bridge.is_running());
    }

    // ─── B2-1 RED: PortRejectReason serialization (R3.3) ────────────────────────

    /// B2-1.1 — `PortRejectReason::Privileged` serializes as the bare string
    ///           `"Privileged"` (default serde unit-variant representation).
    ///
    /// RED: `PortRejectReason` does not exist yet.
    #[test]
    fn test_port_reject_reason_serialization() {
        let reason = PortRejectReason::Privileged;
        let json = serde_json::to_string(&reason).expect("PortRejectReason must serialize");
        assert_eq!(json, "\"Privileged\"");
    }

    /// B2-1.2 — `PortRejectReason::Zero` serializes as `"Zero"`.
    ///
    /// RED: `PortRejectReason` does not exist yet.
    #[test]
    fn test_port_reject_reason_zero_serialization() {
        let reason = PortRejectReason::Zero;
        let json = serde_json::to_string(&reason).expect("PortRejectReason::Zero must serialize");
        assert_eq!(json, "\"Zero\"");
    }

    // ─── B2-2 RED: StartStreamError serialization + Display shape (R3.1–R3.4) ──

    /// B2-2.1 — `StartStreamError::InvalidPort` serializes with `"kind"/"data"`
    ///           shape per R3.4: `{"kind":"InvalidPort","data":{"value":80,"reason":"Privileged"}}`.
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_invalid_port_serialization() {
        let err = StartStreamError::InvalidPort {
            value: 80,
            reason: PortRejectReason::Privileged,
        };
        let json = serde_json::to_string(&err).expect("StartStreamError must serialize");
        assert!(
            json.contains("\"kind\":\"InvalidPort\""),
            "expected kind=InvalidPort, got: {json}"
        );
        assert!(
            json.contains("\"reason\":\"Privileged\""),
            "expected reason=Privileged, got: {json}"
        );
        assert!(
            json.contains("\"value\":80"),
            "expected value=80, got: {json}"
        );
    }

    /// B2-2.2 — `StartStreamError::AlreadyRunning` serializes with `"kind":"AlreadyRunning"`.
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_already_running_serialization() {
        let err = StartStreamError::AlreadyRunning {
            current_port: 7889,
            current_service_name: "_screen-mirror._tcp.local.".to_string(),
        };
        let json = serde_json::to_string(&err).expect("AlreadyRunning must serialize");
        assert!(
            json.contains("\"kind\":\"AlreadyRunning\""),
            "expected kind=AlreadyRunning, got: {json}"
        );
        assert!(
            json.contains("\"current_port\":7889"),
            "expected current_port=7889, got: {json}"
        );
    }

    /// B2-2.3 — `StartStreamError::BundleBuildFailed("foo")` serializes with
    ///           `"kind":"BundleBuildFailed"` and `"data":"foo"` (tuple variant).
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_bundle_build_failed_serialization() {
        let err = StartStreamError::BundleBuildFailed("foo".to_string());
        let json = serde_json::to_string(&err).expect("BundleBuildFailed must serialize");
        assert!(
            json.contains("\"kind\":\"BundleBuildFailed\""),
            "expected kind=BundleBuildFailed, got: {json}"
        );
        assert!(
            json.contains("\"data\":\"foo\""),
            "expected data=foo, got: {json}"
        );
    }

    /// B2-2.4 — All 5 `StartStreamError` variants produce non-empty Display strings
    ///           (thiserror `#[error("...")]` derivation, R3.4 + S3.4).
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_display_non_empty_for_all_variants() {
        let variants: Vec<StartStreamError> = vec![
            StartStreamError::AlreadyRunning {
                current_port: 7889,
                current_service_name: "_screen-mirror._tcp.local.".to_string(),
            },
            StartStreamError::InvalidPort {
                value: 80,
                reason: PortRejectReason::Privileged,
            },
            StartStreamError::InvalidServiceName {
                value: "bogus".to_string(),
                reason: "must end with '.local.'".to_string(),
            },
            StartStreamError::PortInUse { port: 7889 },
            StartStreamError::BundleBuildFailed("some error".to_string()),
        ];
        for err in &variants {
            let display = format!("{err}");
            assert!(!display.is_empty(), "Display must be non-empty for {err:?}");
        }
    }

    /// B2-2.5 — `StartStreamError::InvalidServiceName` serializes correctly.
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_invalid_service_name_serialization() {
        let err = StartStreamError::InvalidServiceName {
            value: "bogus".to_string(),
            reason: "must end with '.local.'".to_string(),
        };
        let json = serde_json::to_string(&err).expect("InvalidServiceName must serialize");
        assert!(
            json.contains("\"kind\":\"InvalidServiceName\""),
            "expected kind=InvalidServiceName, got: {json}"
        );
    }

    /// B2-2.6 — `StartStreamError::PortInUse` serializes with `"kind":"PortInUse"`.
    ///
    /// RED: `StartStreamError` does not exist yet.
    #[test]
    fn test_start_stream_error_port_in_use_serialization() {
        let err = StartStreamError::PortInUse { port: 7889 };
        let json = serde_json::to_string(&err).expect("PortInUse must serialize");
        assert!(
            json.contains("\"kind\":\"PortInUse\""),
            "expected kind=PortInUse, got: {json}"
        );
        assert!(
            json.contains("\"port\":7889"),
            "expected port=7889, got: {json}"
        );
    }

    // ─── B3 RED: validate_udp_port boundary scenarios (R4.2–R4.6) ───────────────

    /// B3.1 — `validate_udp_port(0)` returns `Err(InvalidPort { value: 0, reason: Zero })`.
    ///
    /// Spec R4.2: port 0 would silently corrupt the ICE candidate (Risk #1).
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_zero() {
        let result = validate_udp_port(0);
        match result {
            Err(StartStreamError::InvalidPort {
                value: 0,
                reason: PortRejectReason::Zero,
            }) => {}
            other => {
                panic!("expected Err(InvalidPort {{ value: 0, reason: Zero }}), got {other:?}")
            }
        }
    }

    /// B3.2 — `validate_udp_port(1)` returns `Err(InvalidPort { value: 1, reason: Privileged })`.
    ///
    /// Spec R4.3: lower boundary of the privileged range (1..=1023).
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_privileged_lower() {
        let result = validate_udp_port(1);
        match result {
            Err(StartStreamError::InvalidPort {
                value: 1,
                reason: PortRejectReason::Privileged,
            }) => {}
            other => panic!(
                "expected Err(InvalidPort {{ value: 1, reason: Privileged }}), got {other:?}"
            ),
        }
    }

    /// B3.3 — `validate_udp_port(80)` returns `Err(InvalidPort { value: 80, reason: Privileged })`.
    ///
    /// Spec R4.3: canonical privileged port (HTTP).
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_privileged_mid() {
        let result = validate_udp_port(80);
        match result {
            Err(StartStreamError::InvalidPort {
                value: 80,
                reason: PortRejectReason::Privileged,
            }) => {}
            other => panic!(
                "expected Err(InvalidPort {{ value: 80, reason: Privileged }}), got {other:?}"
            ),
        }
    }

    /// B3.4 — `validate_udp_port(1023)` returns `Err(InvalidPort { value: 1023, reason: Privileged })`.
    ///
    /// Spec R4.3: upper boundary of the privileged range (1..=1023).
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_privileged_upper() {
        let result = validate_udp_port(1023);
        match result {
            Err(StartStreamError::InvalidPort {
                value: 1023,
                reason: PortRejectReason::Privileged,
            }) => {}
            other => panic!(
                "expected Err(InvalidPort {{ value: 1023, reason: Privileged }}), got {other:?}"
            ),
        }
    }

    /// B3.5 — `validate_udp_port(1024)` returns `Ok(())`.
    ///
    /// Spec R4.4: port 1024 is the first non-privileged port.
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_first_valid() {
        let result = validate_udp_port(1024);
        assert!(
            result.is_ok(),
            "port 1024 is the first non-privileged port — must return Ok(()), got {result:?}"
        );
    }

    /// B3.6 — `validate_udp_port(7889)` returns `Ok(())`.
    ///
    /// Spec R4.5: the default port must pass its own validation.
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_default() {
        let result = validate_udp_port(7889);
        assert!(
            result.is_ok(),
            "default port 7889 must pass validation, got {result:?}"
        );
    }

    /// B3.7 — `validate_udp_port(65535)` returns `Ok(())`.
    ///
    /// Spec R4.6: maximum valid u16 must be accepted.
    /// RED: `validate_udp_port` does not exist yet.
    #[test]
    fn test_validate_udp_port_max() {
        let result = validate_udp_port(65535);
        assert!(
            result.is_ok(),
            "max u16 port 65535 must pass validation, got {result:?}"
        );
    }

    // ─── B4 RED: validate_service_name accept/reject scenarios (R5.5, R5.6) ─────

    // ── Reject set (R5.5) ────────────────────────────────────────────────────────

    /// B4.1 — `validate_service_name("")` returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: empty string must be rejected.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_empty_rejected() {
        let result = validate_service_name("");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "", "error must carry the rejected value");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B4.2 — `validate_service_name("bogus")` returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: no leading `_`, no protocol segment.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_bogus_rejected() {
        let result = validate_service_name("bogus");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "bogus");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B4.3 — `validate_service_name("_screen-mirror._tcp.local")` (missing trailing dot)
    ///         returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: missing trailing dot is a common mistake — must be rejected.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_missing_trailing_dot_rejected() {
        let result = validate_service_name("_screen-mirror._tcp.local");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "_screen-mirror._tcp.local");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B4.4 — `validate_service_name("_screen-mirror.tcp.local.")` (protocol missing `_`)
    ///         returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: protocol segment must start with `_`.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_protocol_missing_underscore_rejected() {
        let result = validate_service_name("_screen-mirror.tcp.local.");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "_screen-mirror.tcp.local.");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B4.5 — `validate_service_name("screen-mirror._tcp.local.")` (service missing `_`)
    ///         returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: service segment must start with `_`.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_service_missing_underscore_rejected() {
        let result = validate_service_name("screen-mirror._tcp.local.");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "screen-mirror._tcp.local.");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B4.6 — `validate_service_name("_screen-mirror._tcp.local..")` (double trailing dot)
    ///         returns `Err(InvalidServiceName { .. })`.
    ///
    /// Spec R5.5: double trailing dot — must be rejected.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_double_trailing_dot_rejected() {
        let result = validate_service_name("_screen-mirror._tcp.local..");
        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "_screen-mirror._tcp.local..");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    // ── Accept set (R5.6) ────────────────────────────────────────────────────────

    /// B4.7 — `validate_service_name("_screen-mirror._tcp.local.")` returns `Ok(())`.
    ///
    /// Spec R5.3 + R5.6: default value must pass its own validation.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_default_accepted() {
        let result = validate_service_name("_screen-mirror._tcp.local.");
        assert!(
            result.is_ok(),
            "default service name must be accepted, got {result:?}"
        );
    }

    /// B4.8 — `validate_service_name("_my-mirror._tcp.local.")` returns `Ok(())`.
    ///
    /// Spec R5.6: custom service, TCP protocol.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_custom_tcp_accepted() {
        let result = validate_service_name("_my-mirror._tcp.local.");
        assert!(
            result.is_ok(),
            "_my-mirror._tcp.local. must be accepted, got {result:?}"
        );
    }

    /// B4.9 — `validate_service_name("_my-mirror._udp.local.")` returns `Ok(())`.
    ///
    /// Spec R5.6: custom service, UDP protocol.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_custom_udp_accepted() {
        let result = validate_service_name("_my-mirror._udp.local.");
        assert!(
            result.is_ok(),
            "_my-mirror._udp.local. must be accepted, got {result:?}"
        );
    }

    /// B4.10 — `validate_service_name("_a._b.local.")` returns `Ok(())`.
    ///
    /// Spec R5.6: minimal valid form — single-char service and protocol segments.
    /// The pattern `^_[A-Za-z0-9-]+\._[A-Za-z0-9-]+\.local\.$` (R5.2) accepts
    /// any `[A-Za-z0-9-]+` in both segments — not restricted to `tcp`/`udp`.
    /// RED: `validate_service_name` does not exist yet.
    #[test]
    fn test_validate_service_name_minimal_accepted() {
        let result = validate_service_name("_a._b.local.");
        assert!(
            result.is_ok(),
            "_a._b.local. must be accepted (minimal valid form), got {result:?}"
        );
    }

    // ─── B5-1 RED: build_production_bundle signature change ──────────────────

    /// B5-1.1 — `build_production_bundle` must accept `(udp_port: u16, service_name: String,
    ///           stop_flag: Arc<AtomicBool>)` so the resolved args flow through to the
    ///           transport and signaling layers.
    ///
    /// RED: `build_production_bundle` currently takes only `(stop_flag)` — this call
    /// will fail with E0061 (wrong number of arguments).
    ///
    /// Spec R2.5: `start_stream` MUST pass resolved `(u16, String)` to the `BuilderFn`.
    /// Design §1 Glossary: `build_production_bundle(udp_port, service_name, stop_flag)`.
    #[test]
    fn test_build_production_bundle_accepts_udp_port_and_service_name() {
        // We do NOT actually call it (it would try to bind a real socket),
        // but the TYPE SIGNATURE must accept these args. This is a compile-time
        // gate: if the function still takes only (stop_flag), this won't compile.
        //
        // We use a function-pointer coercion to verify the signature at compile time
        // without executing the function.
        let _: fn(u16, String, Arc<AtomicBool>) -> Result<ReceiverBundle, String> =
            build_production_bundle;
    }

    /// B5-1.2 — `StreamBridge::new()` wrapper closure passes udp_port and service_name
    ///           through to `build_production_bundle` instead of ignoring them.
    ///
    /// RED: `new()` currently wraps with `|_port, _name, stop_flag|` (ignoring port/name).
    /// After GREEN the wrapper must be `|port, name, stop_flag| build_production_bundle(port, name, stop_flag)`.
    ///
    /// We verify this indirectly: the wrapper is `BuilderFn` — we call it via a
    /// probe that intercepts the port argument. Because `build_production_bundle`
    /// actually binds sockets, we cannot call `new()` production wrapper in unit tests.
    /// This test is therefore a COMPILE gate only — the signature coercion in B5-1.1
    /// is the meaningful RED assert. This test documents the requirement.
    #[test]
    fn test_new_wrapper_closure_passes_port_and_name_to_build_production_bundle() {
        // Compile gate: verify that `build_production_bundle` can be referred to as
        // a function with the new three-argument signature (redundant with B5-1.1
        // but explicit about the wrapper's contract).
        fn _assert_arity(_f: fn(u16, String, Arc<AtomicBool>) -> Result<ReceiverBundle, String>) {}
        _assert_arity(build_production_bundle);
    }

    // ─── B5-2 RED: start_stream_inner extraction ─────────────────────────────

    /// B5-2.1 — `start_stream_inner` must exist as a `pub(crate)` function with signature:
    ///   `fn start_stream_inner(bridge: &StreamBridge, channel: Arc<dyn ChannelLike>,
    ///       udp_port: Option<u16>, service_name: Option<String>) -> Result<(), StartStreamError>`
    ///
    /// The `#[tauri::command] start_stream` must become a thin wrapper calling it.
    ///
    /// RED: `start_stream_inner` does not exist yet — this call will fail E0425.
    ///
    /// Spec R2.1; design §10 OQ-A2 (option a: extract as pub(crate) fn).
    #[test]
    fn test_start_stream_inner_exists_and_returns_ok_for_valid_args() {
        let probe = Arc::new(Mutex::new(Vec::<(u16, String)>::new()));
        let probe_clone = probe.clone();
        let builder: BuilderFn = Arc::new(move |port, name, _stop_flag| {
            probe_clone.lock().unwrap().push((port, name));
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // RED: start_stream_inner does not exist yet.
        let result = start_stream_inner(&bridge, channel, None, None);
        assert!(
            result.is_ok(),
            "start_stream_inner must return Ok(()) for default args: {result:?}"
        );

        // After a successful call, builder was invoked.
        let calls = probe.lock().unwrap();
        assert_eq!(calls.len(), 1, "builder must be invoked exactly once");
    }

    /// B5-2.2 — After `start_stream_inner` returns `Ok(())`, the bridge session must be `Some`.
    ///
    /// RED: `start_stream_inner` does not exist.
    #[test]
    fn test_start_stream_inner_sets_session_on_success() {
        let builder: BuilderFn = Arc::new(move |_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(&bridge, channel, None, None).unwrap();

        // Session must be populated.
        assert!(
            bridge.is_running(),
            "bridge must be running after start_stream_inner succeeds"
        );
    }

    // ─── B5-3 RED: argument plumbing + validation wiring ─────────────────────

    /// B5-3.1 — `start_stream_inner(bridge, ch, Some(0), None)` returns
    ///           `Err(InvalidPort { value: 0, reason: Zero })` WITHOUT invoking
    ///           the builder (spec R4.8: S4.8 guard pattern).
    ///
    /// RED: validation is NOT yet wired in start_stream_inner — the function
    /// calls the builder regardless (no validate_udp_port call before builder).
    /// This test must FAIL until the validators are wired in B5-3 GREEN.
    #[test]
    fn test_start_stream_inner_invalid_port_zero_rejects_before_builder() {
        // Builder panics if called — ensures validator short-circuits before builder.
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            panic!("builder must NOT be called when port validation fails")
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, Some(0), None);

        match result {
            Err(StartStreamError::InvalidPort {
                value: 0,
                reason: PortRejectReason::Zero,
            }) => {}
            other => {
                panic!("expected Err(InvalidPort {{ value: 0, reason: Zero }}), got {other:?}")
            }
        }
    }

    /// B5-3.2 — `start_stream_inner(bridge, ch, Some(80), None)` returns
    ///           `Err(InvalidPort { value: 80, reason: Privileged })` WITHOUT invoking
    ///           the builder.
    ///
    /// RED: validators not yet wired.
    #[test]
    fn test_start_stream_inner_invalid_port_privileged_rejects_before_builder() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            panic!("builder must NOT be called when port validation fails")
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, Some(80), None);

        match result {
            Err(StartStreamError::InvalidPort {
                value: 80,
                reason: PortRejectReason::Privileged,
            }) => {}
            other => panic!(
                "expected Err(InvalidPort {{ value: 80, reason: Privileged }}), got {other:?}"
            ),
        }
    }

    /// B5-3.3 — `start_stream_inner(bridge, ch, None, Some("bogus".into()))` returns
    ///           `Err(InvalidServiceName { .. })` WITHOUT invoking the builder.
    ///
    /// RED: validators not yet wired.
    #[test]
    fn test_start_stream_inner_invalid_service_name_rejects_before_builder() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            panic!("builder must NOT be called when service-name validation fails")
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, None, Some("bogus".to_string()));

        match result {
            Err(StartStreamError::InvalidServiceName { value, .. }) => {
                assert_eq!(value, "bogus");
            }
            other => panic!("expected Err(InvalidServiceName), got {other:?}"),
        }
    }

    /// B5-3.4 — `start_stream_inner(bridge, ch, None, None)` resolves defaults
    ///           and passes `(7889, "_screen-mirror._tcp.local.")` to the builder.
    ///
    /// GREEN in B5-2 (defaults already resolved), but this test records the contract
    /// explicitly and asserts the exact values (not just "builder called once").
    ///
    /// RED: the assertions in B5-2.1 only check `calls.len() == 1`; this test also
    /// checks the specific port and name values. Already passes in B5-2 because
    /// start_stream_inner already resolves defaults — this is effectively a GREEN
    /// test from the start. Include as documentation-level test.
    #[test]
    fn test_start_stream_inner_none_args_resolve_to_defaults_in_builder() {
        let probe = Arc::new(Mutex::new(Vec::<(u16, String)>::new()));
        let probe_clone = probe.clone();
        let builder: BuilderFn = Arc::new(move |port, name, _stop_flag| {
            probe_clone.lock().unwrap().push((port, name));
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(&bridge, channel, None, None).unwrap();

        let calls = probe.lock().unwrap();
        assert_eq!(calls.len(), 1);
        // Spec R2.2: None → 7889. Spec R2.3: None → "_screen-mirror._tcp.local.".
        assert_eq!(
            calls[0],
            (7889u16, "_screen-mirror._tcp.local.".to_string()),
            "default args must be (7889, \"_screen-mirror._tcp.local.\") when None is passed"
        );
    }

    /// B5-3.5 — `start_stream_inner(bridge, ch, Some(7900), Some("_my-mirror._tcp.local.".into()))`
    ///           passes the exact provided args to the builder.
    ///
    /// Spec R2.5: builder receives resolved (port, service_name, stop_flag) tuple.
    /// Also implicitly tests S2.2.
    ///
    /// RED: validators will reject Some(7900) as valid ONLY if validate_udp_port is wired
    /// correctly. Actually 7900 is valid (> 1023), so this is a GREEN-from-start test once
    /// the extraction is done — but it confirms the plumbing. Keeping here to expose
    /// any regression in plumbing.
    #[test]
    fn test_start_stream_inner_custom_args_reach_builder() {
        let probe = Arc::new(Mutex::new(Vec::<(u16, String)>::new()));
        let probe_clone = probe.clone();
        let builder: BuilderFn = Arc::new(move |port, name, _stop_flag| {
            probe_clone.lock().unwrap().push((port, name));
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(
            &bridge,
            channel,
            Some(7900),
            Some("_my-mirror._tcp.local.".to_string()),
        )
        .unwrap();

        let calls = probe.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            (7900u16, "_my-mirror._tcp.local.".to_string()),
            "custom args must be passed through to the builder unchanged"
        );
    }

    // ─── B5-4 RED: PortInUse substring detection shim (OQ-A1) ───────────────

    /// B5-4.1 — When the builder returns `Err("address already in use")`,
    ///           `start_stream_inner` must return `Err(PortInUse { port: <resolved> })`
    ///           NOT `Err(BundleBuildFailed(...))`.
    ///
    /// Design §3 step 7 + §10 OQ-A1: substring-match on "address already in use"
    /// (Linux/macOS OS message, lowercase-compared).
    ///
    /// RED: `start_stream_inner` currently returns `BundleBuildFailed` for all
    /// builder errors — the substring-detection shim is not yet implemented.
    #[test]
    fn test_start_stream_inner_builder_addr_in_use_returns_port_in_use() {
        let builder: BuilderFn =
            Arc::new(|_port, _name, _stop_flag| Err("address already in use".to_string()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, Some(7900), None);

        match result {
            Err(StartStreamError::PortInUse { port: 7900 }) => {}
            other => panic!("expected Err(PortInUse {{ port: 7900 }}), got {other:?}"),
        }
    }

    /// B5-4.2 — Windows variant: builder returns
    ///           `Err("only one usage of each socket address")` →
    ///           `start_stream_inner` returns `Err(PortInUse { port })`.
    ///
    /// Design §10 OQ-A1: match BOTH substrings for cross-platform coverage.
    ///
    /// RED: shim not yet implemented.
    #[test]
    fn test_start_stream_inner_builder_windows_addr_in_use_returns_port_in_use() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            Err("only one usage of each socket address (protocol/network address/port) is normally permitted.".to_string())
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, None, None);

        match result {
            Err(StartStreamError::PortInUse { port: 7889 }) => {}
            other => panic!("expected Err(PortInUse {{ port: 7889 }}), got {other:?}"),
        }
    }

    /// B5-4.3 — Builder returns an unrelated error `Err("some other failure")` →
    ///           `start_stream_inner` must return `Err(BundleBuildFailed("some other failure"))`,
    ///           NOT `PortInUse`.
    ///
    /// RED: currently all builder errors go to BundleBuildFailed, but B5-4.1 adds
    /// PortInUse detection — this test ensures the non-AddrInUse path still goes
    /// to BundleBuildFailed.
    ///
    /// This test is actually GREEN in the pre-B5-4 code (all errors → BundleBuildFailed).
    /// It serves as a regression guard: once B5-4 shim is added, BundleBuildFailed
    /// must still be returned for non-AddrInUse errors.
    #[test]
    fn test_start_stream_inner_builder_other_error_returns_bundle_build_failed() {
        let builder: BuilderFn =
            Arc::new(|_port, _name, _stop_flag| Err("some unrelated build failure".to_string()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let result = start_stream_inner(&bridge, channel, None, None);

        match result {
            Err(StartStreamError::BundleBuildFailed(msg)) => {
                assert_eq!(msg, "some unrelated build failure");
            }
            other => panic!(
                "expected Err(BundleBuildFailed(\"some unrelated build failure\")), got {other:?}"
            ),
        }
    }

    // ─── B5-fix-A RED: service_name threads to SignalingConfig::service_name ────

    /// B5-fix-A.1 — `build_signaling_config_for_receiver` must set `service_name`
    /// to the string passed in, NOT leave it at the default.
    ///
    /// Spec R2.5: builder receives the resolved `service_name`; explore #284
    /// identified `SignalingConfig::service_name` as the right vehicle.
    ///
    /// RED: `build_signaling_config_for_receiver` does not exist yet → E0425.
    #[test]
    fn test_build_signaling_config_for_receiver_threads_service_name() {
        let cfg = build_signaling_config_for_receiver(7900, "_my-mirror._tcp.local.".to_string());
        assert_eq!(
            cfg.service_name, "_my-mirror._tcp.local.",
            "service_name must be threaded into SignalingConfig::service_name"
        );
        assert_eq!(cfg.control_port, 7900);
        assert_eq!(cfg.role, SignalingRole::Receiver);
    }

    /// B5-fix-A.2 — Passing a non-default `service_name` must reach the field
    /// verbatim; the `..SignalingConfig::default()` spread must NOT shadow it.
    ///
    /// RED: helper does not exist yet → E0425.
    #[test]
    fn test_build_signaling_config_for_receiver_no_default_shadowing() {
        let cfg = build_signaling_config_for_receiver(7889, "_default._tcp.local.".to_string());
        assert_eq!(
            cfg.service_name, "_default._tcp.local.",
            "the default spread must not overwrite the supplied service_name"
        );
        assert_eq!(cfg.control_port, 7889);
    }

    // ─── B6-1 RED: current_args populated on successful start_stream_inner ───────

    /// B6-1.1 — After a successful `start_stream_inner` call with explicit custom
    ///           args, `bridge.current_args` must be `Some((7900, "_my-mirror._tcp.local."))`.
    ///
    /// Spec R6.2: "When `start_stream` completes successfully, `current_args` MUST
    /// be set to `Some((resolved_port, resolved_service_name))`."
    ///
    /// RED: `start_stream_inner` currently does NOT write to `current_args`.
    #[test]
    fn test_current_args_set_on_successful_start_custom_args() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(
            &bridge,
            channel,
            Some(7900),
            Some("_my-mirror._tcp.local.".to_string()),
        )
        .expect("start_stream_inner must succeed with valid args");

        let args = bridge.current_args.lock().unwrap();
        assert_eq!(
            *args,
            Some((7900u16, "_my-mirror._tcp.local.".to_string())),
            "current_args must be Some((7900, \"_my-mirror._tcp.local.\")) after successful start"
        );
    }

    /// B6-1.2 — After a successful `start_stream_inner` call with `None` args
    ///           (defaults), `bridge.current_args` must be
    ///           `Some((7889, "_screen-mirror._tcp.local."))`.
    ///
    /// Spec R6.2; the resolved defaults must be stored (not the raw `None`).
    ///
    /// RED: `start_stream_inner` currently does NOT write to `current_args`.
    #[test]
    fn test_current_args_set_on_successful_start_default_args() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(&bridge, channel, None, None)
            .expect("start_stream_inner must succeed with default args");

        let args = bridge.current_args.lock().unwrap();
        assert_eq!(
            *args,
            Some((7889u16, "_screen-mirror._tcp.local.".to_string())),
            "current_args must be Some((7889, \"_screen-mirror._tcp.local.\")) after successful default start"
        );
    }

    // ─── B6-2 RED: double-start returns AlreadyRunning (T7.6, T7.7) per PQ-E ───

    /// B6-2 / T7.6 — Double-start with SAME args returns
    ///                `Err(AlreadyRunning { current_port: 7889, current_service_name: ... })`.
    ///
    /// Spec R6.4 (PQ-E): "ALWAYS return Err(AlreadyRunning) on double-start regardless
    /// of whether new args match current args."
    /// Spec R6.5: "The AlreadyRunning error MUST carry the CURRENT session's args."
    ///
    /// RED: `start_stream_inner` currently returns `Ok(())` on double-start (is_running()
    /// guard). B6-2 GREEN replaces that with the AlreadyRunning error.
    #[test]
    fn test_double_start_same_args_returns_already_running() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // First start — must succeed.
        start_stream_inner(
            &bridge,
            channel.clone(),
            Some(7889),
            None, // resolves to "_screen-mirror._tcp.local."
        )
        .expect("first start must succeed");

        // Second start with the SAME args — must return AlreadyRunning.
        let err = start_stream_inner(&bridge, channel.clone(), Some(7889), None)
            .expect_err("second start must return AlreadyRunning, not Ok(())");

        match err {
            StartStreamError::AlreadyRunning {
                current_port,
                current_service_name,
            } => {
                assert_eq!(
                    current_port, 7889,
                    "AlreadyRunning must carry the CURRENT port (7889)"
                );
                assert_eq!(
                    current_service_name, "_screen-mirror._tcp.local.",
                    "AlreadyRunning must carry the CURRENT service name"
                );
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    /// B6-2 / T7.7 — Double-start with DIFFERENT args returns
    ///                `Err(AlreadyRunning { current_port: 7889, .. })` where the
    ///                payload carries the CURRENT args (not the new caller's args).
    ///
    /// Spec R6.4, R6.5: CURRENT args must be in the error, not the new caller's args.
    ///
    /// RED: `start_stream_inner` currently returns `Ok(())` on double-start.
    #[test]
    fn test_double_start_different_args_returns_already_running_with_current_args() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // First start with port 7889 and default name.
        start_stream_inner(&bridge, channel.clone(), Some(7889), None)
            .expect("first start must succeed");

        // Second start with DIFFERENT port (7900) and different name.
        let err = start_stream_inner(
            &bridge,
            channel.clone(),
            Some(7900),
            Some("_other-service._tcp.local.".to_string()),
        )
        .expect_err("second start must return AlreadyRunning, not Ok(())");

        match err {
            StartStreamError::AlreadyRunning {
                current_port,
                current_service_name,
            } => {
                // CRITICAL: must carry CURRENT args (7889 / default name), NOT new args (7900).
                assert_eq!(
                    current_port, 7889,
                    "AlreadyRunning must carry the CURRENT port (7889), not the new caller's port (7900)"
                );
                assert_eq!(
                    current_service_name, "_screen-mirror._tcp.local.",
                    "AlreadyRunning must carry the CURRENT service name, not the new caller's"
                );
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    // ─── B6-3 RED: stop_stream_session clears current_args ───────────────────────

    /// B6-3 / T7.9 — start → stop → start with DIFFERENT args → second start
    ///                returns `Ok(())`.
    ///
    /// Spec R6.7: "After stop_stream_session, a subsequent start_stream with any
    /// valid args MUST succeed (assuming no other session is active)."
    /// Spec R6.3: `stop_stream_session` MUST clear `current_args` to `None`.
    ///
    /// RED: `stop_stream_session` currently does NOT clear `current_args`, so the
    /// second `start_stream_inner` call will see `Some((old_port, old_name))` and
    /// return `Err(AlreadyRunning)` instead of `Ok(())`.
    #[test]
    fn test_stop_clears_current_args_and_second_start_succeeds() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // Step 1: start with port 7889.
        start_stream_inner(&bridge, channel.clone(), Some(7889), None)
            .expect("first start must succeed");

        // Step 2: stop.
        stop_stream_session(&bridge);

        // Step 3: verify current_args is None after stop.
        let args = bridge.current_args.lock().unwrap();
        assert!(
            args.is_none(),
            "current_args must be None after stop_stream_session, got {args:?}"
        );
        drop(args);

        // Step 4: start with DIFFERENT args (7900).
        // Must return Ok(()) — stop cleared the state so no AlreadyRunning.
        start_stream_inner(
            &bridge,
            channel.clone(),
            Some(7900),
            Some("_other-service._tcp.local.".to_string()),
        )
        .expect("second start with different args must succeed after stop");

        // Step 5: verify current_args is updated to the new args.
        let args = bridge.current_args.lock().unwrap();
        assert_eq!(
            *args,
            Some((7900u16, "_other-service._tcp.local.".to_string())),
            "current_args must reflect the new args after the second successful start"
        );
    }

    // ─── B7-3 RED: T7.8 — builder error → BundleBuildFailed (probe confirms called) ─

    /// B7-3 / T7.8 — When the builder returns `Err("build failed")`,
    ///                 `start_stream_inner` must return `Err(BundleBuildFailed("build failed"))`.
    ///                 The probe additionally proves the builder WAS invoked (once),
    ///                 confirming that validation passed and the error originates inside
    ///                 the builder, not from a validation reject.
    ///
    /// Spec R7.4 T7.8, R3.2 (BundleBuildFailed variant), design §3 step 7.
    ///
    /// RED: `BuilderProbe::assert_called_once` does not exist yet → E0599.
    #[test]
    fn test_t7_8_builder_error_returns_bundle_build_failed() {
        let probe = BuilderProbe::new();
        // Builder returns a non-AddrInUse error — must map to BundleBuildFailed.
        let builder = make_test_builder(probe.clone(), Err("build failed"));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // Use valid args (port=7889, default name) so validation does NOT reject.
        let err = start_stream_inner(&bridge, channel, None, None)
            .expect_err("T7.8: builder error must cause start_stream_inner to return Err");

        match err {
            StartStreamError::BundleBuildFailed(msg) => {
                assert_eq!(
                    msg, "build failed",
                    "T7.8: BundleBuildFailed must carry the builder's error string verbatim"
                );
            }
            other => panic!("T7.8: expected BundleBuildFailed(\"build failed\"), got {other:?}"),
        }

        // Prove the builder WAS called (validation passed, error came from builder).
        probe.assert_called_once();
    }

    // ─── B7-2 RED: T7.3 + T7.4 + T7.5 (validation-rejection, builder NOT called) ─

    /// B7-2.1 / T7.3 — `start_stream_inner(Some(0), None)` must return
    ///                   `Err(InvalidPort { value: 0, reason: Zero })` and the
    ///                   builder must NOT be called (spec R7.4 T7.3, R4.1, S4.8).
    ///
    /// "Builder NOT called" is asserted by checking `probe.call_count() == 0`.
    ///
    /// RED: `BuilderProbe::call_count()` does not exist yet → E0599.
    #[test]
    fn test_t7_3_port_zero_builder_not_called() {
        let probe = BuilderProbe::new();
        let builder = make_test_builder(probe.clone(), Ok(()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let err = start_stream_inner(&bridge, channel, Some(0), None)
            .expect_err("T7.3: port=0 must return Err(InvalidPort { value: 0, reason: Zero })");

        match err {
            StartStreamError::InvalidPort {
                value: 0,
                reason: PortRejectReason::Zero,
            } => {}
            other => panic!("T7.3: expected InvalidPort(Zero), got {other:?}"),
        }

        assert_eq!(
            probe.call_count(),
            0,
            "T7.3: builder must NOT be called when port=0 is rejected by validation"
        );
    }

    /// B7-2.2 / T7.4 — `start_stream_inner(Some(80), None)` must return
    ///                   `Err(InvalidPort { value: 80, reason: Privileged })` and the
    ///                   builder must NOT be called (spec R7.4 T7.4, R4.3, S4.8).
    ///
    /// RED: `BuilderProbe::call_count()` does not exist yet → E0599.
    #[test]
    fn test_t7_4_privileged_port_builder_not_called() {
        let probe = BuilderProbe::new();
        let builder = make_test_builder(probe.clone(), Ok(()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let err = start_stream_inner(&bridge, channel, Some(80), None).expect_err(
            "T7.4: port=80 must return Err(InvalidPort { value: 80, reason: Privileged })",
        );

        match err {
            StartStreamError::InvalidPort {
                value: 80,
                reason: PortRejectReason::Privileged,
            } => {}
            other => panic!("T7.4: expected InvalidPort(Privileged, 80), got {other:?}"),
        }

        assert_eq!(
            probe.call_count(),
            0,
            "T7.4: builder must NOT be called when port=80 is rejected as Privileged"
        );
    }

    /// B7-2.3 / T7.5 — `start_stream_inner(None, Some("bogus"), channel)` must return
    ///                   `Err(InvalidServiceName { .. })` and the builder must NOT be
    ///                   called (spec R7.4 T7.5, R5.1, S5.8).
    ///
    /// RED: `BuilderProbe::call_count()` does not exist yet → E0599.
    #[test]
    fn test_t7_5_bogus_service_name_builder_not_called() {
        let probe = BuilderProbe::new();
        let builder = make_test_builder(probe.clone(), Ok(()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        let err = start_stream_inner(&bridge, channel, None, Some("bogus".to_string()))
            .expect_err("T7.5: service_name='bogus' must return Err(InvalidServiceName { .. })");

        match err {
            StartStreamError::InvalidServiceName { value, .. } => {
                assert_eq!(
                    value, "bogus",
                    "T7.5: InvalidServiceName must carry the rejected value"
                );
            }
            other => panic!("T7.5: expected InvalidServiceName, got {other:?}"),
        }

        assert_eq!(
            probe.call_count(),
            0,
            "T7.5: builder must NOT be called when service_name='bogus' is rejected"
        );
    }

    // ─── B7-1 RED: BuilderProbe + T7.1 + T7.2 (args flow to builder) ───────────

    /// B7-1.1 / T7.1 — `start_stream_inner(None, None, channel)` with a recording
    ///                   builder must call the builder with the resolved defaults:
    ///                   `(7889, "_screen-mirror._tcp.local.", _)`.
    ///
    /// Spec R7.4 T7.1: `start_stream(None, None, channel)` → `Ok(())`, builder called
    /// with `(7889, "_screen-mirror._tcp.local.", _)`.
    /// Spec R2.2, R2.3, R2.5: defaults resolve to (7889, "_screen-mirror._tcp.local.")
    /// and these MUST be passed to the BuilderFn verbatim.
    ///
    /// RED: `BuilderProbe` and `make_test_builder` do not exist yet → E0422/E0425.
    #[test]
    fn test_t7_1_default_args_builder_called_with_defaults() {
        let probe = BuilderProbe::new();
        let builder = make_test_builder(probe.clone(), Ok(()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(&bridge, channel, None, None)
            .expect("T7.1: start_stream_inner with default args must return Ok(())");

        let calls = probe.calls();
        assert_eq!(
            calls.len(),
            1,
            "T7.1: builder must be called exactly once, got {} calls",
            calls.len()
        );
        assert_eq!(
            calls[0].0, 7889,
            "T7.1: builder must receive resolved default port 7889, got {}",
            calls[0].0
        );
        assert_eq!(
            calls[0].1, "_screen-mirror._tcp.local.",
            "T7.1: builder must receive resolved default service name, got {:?}",
            calls[0].1
        );
    }

    /// B7-1.2 / T7.2 — `start_stream_inner(Some(7900), Some("_my-mirror._tcp.local."), channel)`
    ///                   with a recording builder must call the builder with the custom args:
    ///                   `(7900, "_my-mirror._tcp.local.", _)`.
    ///
    /// Spec R7.4 T7.2: `start_stream(Some(7900), Some("_my-mirror._tcp.local."), channel)` →
    /// `Ok(())`, builder called with `(7900, "_my-mirror._tcp.local.", _)`.
    /// Spec R2.5: "start_stream MUST pass the resolved (u16, String) values to the BuilderFn
    /// as positional parameters".
    ///
    /// RED: `BuilderProbe` and `make_test_builder` do not exist yet → E0422/E0425.
    #[test]
    fn test_t7_2_custom_args_builder_called_with_custom_args() {
        let probe = BuilderProbe::new();
        let builder = make_test_builder(probe.clone(), Ok(()));
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        start_stream_inner(
            &bridge,
            channel,
            Some(7900),
            Some("_my-mirror._tcp.local.".to_string()),
        )
        .expect("T7.2: start_stream_inner with custom args must return Ok(())");

        let calls = probe.calls();
        assert_eq!(
            calls.len(),
            1,
            "T7.2: builder must be called exactly once, got {} calls",
            calls.len()
        );
        assert_eq!(
            calls[0].0, 7900,
            "T7.2: builder must receive custom port 7900, got {}",
            calls[0].0
        );
        assert_eq!(
            calls[0].1, "_my-mirror._tcp.local.",
            "T7.2: builder must receive custom service name '_my-mirror._tcp.local.', got {:?}",
            calls[0].1
        );
    }

    /// B6-3 extra — `stop_stream_session` on an active session sets
    ///               `bridge.current_args` to `None` immediately on return.
    ///
    /// Spec R6.3: "When stop_stream_session completes successfully, current_args
    /// MUST be cleared to None."
    ///
    /// RED: `stop_stream_session` does not clear `current_args`.
    #[test]
    fn test_stop_stream_session_clears_current_args() {
        let builder: BuilderFn = Arc::new(|_port, _name, _stop_flag| {
            let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiver::new()),
                pkt_rx,
                signaling: None,
                drain_handles: Vec::new(),
                _drain_senders: Vec::new(),
            })
        });
        let bridge = StreamBridge::new_with_builder(builder);
        let channel: Arc<dyn ChannelLike> = FakeChannel::new();

        // Start to populate current_args.
        start_stream_inner(&bridge, channel, Some(7889), None)
            .expect("start must succeed to populate current_args");

        // Verify current_args is populated before stop.
        assert!(
            bridge.current_args.lock().unwrap().is_some(),
            "current_args must be Some before stop"
        );

        // Stop.
        stop_stream_session(&bridge);

        // Verify current_args is cleared after stop.
        let args = bridge.current_args.lock().unwrap();
        assert!(
            args.is_none(),
            "current_args must be None after stop_stream_session, got {args:?}"
        );
    }

    // ─── B3.T1 RED: BundleError enum exists and displays correctly (R2.1, R2.2) ──

    #[test]
    fn bundle_error_enum_exists_and_displays() {
        let port_err = BundleError::PortInUse(7889);
        let other_err = BundleError::Other("fail".to_string());
        assert_eq!(
            format!("{port_err}"),
            "UDP port 7889 already in use",
            "PortInUse Display must be 'UDP port 7889 already in use'"
        );
        assert_eq!(
            format!("{other_err}"),
            "bundle build failed: fail",
            "Other Display must be 'bundle build failed: fail'"
        );
    }
}
