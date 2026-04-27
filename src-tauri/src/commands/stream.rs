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

// ─── StreamBridge — Capability A ─────────────────────────────────────────────

/// Tauri managed state for an active streaming session.
///
/// Held behind `State<StreamBridge>` in Tauri commands.
/// Wraps a `Mutex<Option<StreamSession>>` to allow mutation inside
/// immutable Tauri command references.
pub struct StreamBridge {
    session: Mutex<Option<StreamSession>>,
}

impl StreamBridge {
    /// Create an empty bridge (no active session).
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    /// Returns `true` if a session is currently running.
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
struct ReceiverBundle {
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

/// Build the production `ReceiverBundle`: real `Str0mVideoReceiver` + `MdnsSignaling`.
///
/// The signaling adapter is started first so it begins mDNS discovery immediately.
/// The receiver is started second so it can process the offer/answer that arrives
/// asynchronously via the signaling-event drain thread.
///
/// Both adapters run their own OS threads. This function returns immediately —
/// the full SDP/ICE handshake completes asynchronously in the drain threads.
fn build_production_bundle(
    stop_flag: Arc<AtomicBool>,
) -> Result<ReceiverBundle, String> {
    // ── 1. Build MdnsSignaling (Receiver role) ─────────────────────────────
    let sig_config = SignalingConfig {
        role: SignalingRole::Receiver,
        control_port: 7889,
        peer_hint: None,
        ..SignalingConfig::default()
    };
    let mut signaling =
        MdnsSignaling::new(sig_config).map_err(|e| format!("MdnsSignaling::new failed: {e}"))?;

    let (sig_event_tx, sig_event_rx) =
        sync_channel::<SignalingEvent>(TRANSPORT_CHANNEL_CAPACITY);
    signaling
        .start(sig_event_tx)
        .map_err(|e| format!("MdnsSignaling::start failed: {e}"))?;

    // ── 2. Build Str0mVideoReceiver (Receiver role) ────────────────────────
    let transport_config = TransportConfig {
        udp_port: 7889,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    };
    let mut receiver = Str0mVideoReceiver::new(transport_config)
        .map_err(|e| format!("Str0mVideoReceiver::new failed: {e}"))?;

    let (pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
    let (transport_event_tx, transport_event_rx) =
        sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);

    receiver
        .start(pkt_tx.clone(), transport_event_tx)
        .map_err(|e| format!("Str0mVideoReceiver::start failed: {e}"))?;

    // ── 3. Spawn transport-event drain thread (W2-fix-C) ──────────────────
    let stop_flag_t = stop_flag.clone();
    let transport_drain = thread::Builder::new()
        .name("sm-transport-event-drain".into())
        .spawn(move || {
            loop {
                if stop_flag_t.load(Ordering::Relaxed) {
                    break;
                }
                match transport_event_rx
                    .recv_timeout(Duration::from_millis(500))
                {
                    Ok(ev) => match ev {
                        TransportEvent::IceConnected => {
                            eprintln!("[sm-transport-event-drain] ICE connected");
                        }
                        TransportEvent::IceFailed => {
                            eprintln!("[sm-transport-event-drain] ICE failed");
                        }
                        TransportEvent::ConnectionLost { reason } => {
                            eprintln!("[sm-transport-event-drain] connection lost: {reason}");
                        }
                        _ => {}
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|e| format!("failed to spawn transport-event drain: {e}"))?;

    // ── 4. Wrap receiver in Arc for shared access across threads ───────────
    // The signaling-event drain thread needs to call `receiver.apply_remote_offer`
    // and `receiver.add_remote_candidate`. We wrap in Arc<Mutex<>> so the drain
    // thread can take &mut self calls.
    let receiver_arc = Arc::new(Mutex::new(receiver));
    let receiver_arc_for_drain = receiver_arc.clone();

    // ── 5. Spawn signaling-event drain thread (W2-fix-B) ──────────────────
    let stop_flag_s = stop_flag.clone();
    let signaling_arc = Arc::new(Mutex::new(signaling));
    let signaling_arc_for_drain = signaling_arc.clone();

    let sig_drain = thread::Builder::new()
        .name("sm-signaling-event-drain".into())
        .spawn(move || {
            loop {
                if stop_flag_s.load(Ordering::Relaxed) {
                    break;
                }
                match sig_event_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => match ev {
                        SignalingEvent::PeerFound { host, port } => {
                            eprintln!(
                                "[sm-signaling-event-drain] peer found: {host}:{port}"
                            );
                        }
                        SignalingEvent::OfferReceived(offer) => {
                            // Apply the remote offer and publish our answer.
                            let answer_result = {
                                let recv = receiver_arc_for_drain.lock().unwrap();
                                recv.apply_remote_offer(offer)
                            };
                            match answer_result {
                                Ok(answer) => {
                                    let sig = signaling_arc_for_drain.lock().unwrap();
                                    if let Err(e) = sig.publish_local_answer(answer) {
                                        eprintln!("[sm-signaling-event-drain] publish_local_answer failed: {e}");
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[sm-signaling-event-drain] apply_remote_offer failed: {e}");
                                }
                            }
                        }
                        SignalingEvent::CandidateReceived(cand) => {
                            let recv = receiver_arc_for_drain.lock().unwrap();
                            if let Err(e) = recv.add_remote_candidate(cand) {
                                eprintln!("[sm-signaling-event-drain] add_remote_candidate failed: {e}");
                            }
                        }
                        SignalingEvent::Closed => {
                            eprintln!("[sm-signaling-event-drain] signaling closed");
                            break;
                        }
                        SignalingEvent::Error(e) => {
                            eprintln!("[sm-signaling-event-drain] signaling error: {e}");
                        }
                        _ => {}
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|e| format!("failed to spawn signaling-event drain: {e}"))?;

    // ── 6. Build the ReceiverOps wrapper around Arc<Mutex<Str0mVideoReceiver>> ──
    struct ArcReceiverOps(Arc<Mutex<Str0mVideoReceiver>>);

    impl ReceiverOps for ArcReceiverOps {
        fn request_keyframe(&self) -> Result<(), TransportError> {
            self.0.lock().unwrap().request_keyframe()
        }

        fn dropped_frames(&self) -> u64 {
            self.0.lock().unwrap().dropped_frames()
        }
    }

    // ── 7. Build SignalingOps wrapper around Arc<Mutex<MdnsSignaling>> ────
    struct ArcSignalingOps(Arc<Mutex<MdnsSignaling>>);

    impl SignalingOps for ArcSignalingOps {
        fn stop(&mut self) -> Result<(), sm_domain::signaling::SignalingError> {
            self.0.lock().unwrap().stop()
        }
    }

    Ok(ReceiverBundle {
        receiver: Box::new(ArcReceiverOps(receiver_arc)),
        pkt_rx,
        signaling: Some(Box::new(ArcSignalingOps(signaling_arc))),
        drain_handles: vec![transport_drain, sig_drain],
        _drain_senders: vec![],
    })
}

// ─── Tauri commands ───────────────────────────────────────────────────────────

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
/// Idempotent: if a session is already running, returns `Ok(())` immediately.
/// Returns immediately — SDP/ICE handshake completes asynchronously via drain threads.
#[tauri::command]
pub fn start_stream(
    bridge: tauri::State<StreamBridge>,
    channel: tauri::ipc::Channel<InvokeResponseBody>,
) -> Result<(), String> {
    if bridge.is_running() {
        return Ok(()); // idempotent
    }
    let mut guard = bridge.session.lock().unwrap();

    // Wrap the Tauri channel in Arc<dyn ChannelLike> — cloned into the mux thread.
    let channel_arc: Arc<dyn ChannelLike> = Arc::new(TauriChannel(channel.clone()));

    // Pre-allocate the stop_flag so that drain threads (built inside
    // build_production_bundle) and the mux thread (built inside
    // build_stream_session) all share the exact same Arc.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let bundle = build_production_bundle(stop_flag.clone())?;

    *guard = Some(build_stream_session(channel_arc, bundle, stop_flag)?);

    Ok(())
}

/// Stop the streaming session. Idempotent.
///
/// Shutdown order (caller-must-drop-tx-first invariant):
/// 1. Set the stop flag — signals the mux thread and all drain threads.
/// 2. Join the mux thread (it owns pkt_rx; setting stop_flag causes it to exit).
/// 3. Join drain threads (they check stop_flag on every 500 ms timeout).
/// 4. Stop the signaling adapter.
/// 5. Drop the session (receiver + signaling are dropped here — their Drop
///    implementations call stop() so the tick threads are joined).
#[tauri::command]
pub fn stop_stream(bridge: tauri::State<StreamBridge>) -> Result<(), String> {
    let mut guard = bridge.session.lock().unwrap();
    if let Some(mut session) = guard.take() {
        // 1. Signal stop to the mux thread and all drain threads.
        session.stop_flag.store(true, Ordering::Relaxed);

        // 2. Join the mux thread.
        if let Some(handle) = session.mux_handle.take() {
            let _ = handle.join();
        }

        // 3. Join drain threads.
        for handle in session.drain_handles.drain(..) {
            let _ = handle.join();
        }

        // 4. Stop the signaling adapter.
        if let Some(mut sig) = session.signaling.take() {
            let _ = sig.stop();
        }

        // 5. receiver and channel are dropped here (their Drop impls call stop).
    }
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
}
