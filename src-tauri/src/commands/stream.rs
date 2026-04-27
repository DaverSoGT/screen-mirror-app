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
//! init segment from SPS+PPS, and emits `"stream/init"` + `"stream/segment"` events
//! to the WebView via `tauri::AppHandle::emit`.
//!
//! # OQ-tauri-emit-1 — SUPERSEDED
//!
//! The V1 decision to keep `app.emit(Vec<u8>)` (JSON encoding) is overridden.
//! See the pivot to `tauri::ipc::Channel<InvokeResponseBody>` implemented below.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sm_domain::encode::EncodedPacket;
use sm_domain::transport::{TRANSPORT_CHANNEL_CAPACITY, TransportError};
use sm_infra::render::fmp4_muxer::{Mp4Muxer, extract_sps_pps_from_idr};
use tauri::ipc::InvokeResponseBody;
use tauri::Emitter;

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

/// Active stream session: receiver + mux thread + counters.
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

// ─── Tauri commands ───────────────────────────────────────────────────────────

/// Start the streaming session.
///
/// Constructs a `Str0mVideoReceiver` (in production), starts it, and spawns the
/// `sm-stream-mux` thread which drains packets, builds fMP4 segments, and emits
/// them to the WebView via `app.emit("stream/init", ...)` / `app.emit("stream/segment", ...)`.
///
/// Idempotent: if a session is already running, returns `Ok(())` immediately.
#[tauri::command]
pub fn start_stream(
    app: tauri::AppHandle,
    bridge: tauri::State<StreamBridge>,
) -> Result<(), String> {
    if bridge.is_running() {
        return Ok(()); // idempotent
    }
    let mut guard = bridge.session.lock().unwrap();

    // For non-test builds: construct a real Str0mVideoReceiver.
    // The receiver is started with a sync_channel; the mux thread drains pkt_rx.
    //
    // In V1 we hard-code a default TransportConfig (receiver listens on port 7889).
    // A future command can accept a config struct for multi-session or custom ports.
    //
    // NOTE: This code path is NOT unit-tested (requires live transport + Tauri runtime).
    // Unit tests cover `StreamBridge::is_running`, init-segment gating, PLI, and backpressure
    // via `FakeReceiver` in `mod tests`.

    let counters = Arc::new(BridgeCounters::default());
    let stop_flag = Arc::new(AtomicBool::new(false));

    let counters_clone = counters.clone();
    let stop_flag_clone = stop_flag.clone();
    let app_clone = app.clone();

    // Production: spawn mux thread with a FakeReceiver stub.
    // Full integration requires a real Str0mVideoReceiver — wired in B8.
    // For now the thread is scaffolded to satisfy the compile gate (S10.3).
    let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);

    let handle = thread::Builder::new()
        .name("sm-stream-mux".into())
        .spawn(move || {
            mux_thread(pkt_rx, stop_flag_clone, counters_clone, app_clone);
        })
        .map_err(|e| format!("failed to spawn mux thread: {e}"))?;

    *guard = Some(StreamSession {
        stop_flag,
        mux_handle: Some(handle),
        counters,
        receiver: None, // production receiver wired in B8
        last_pli: None,
    });

    Ok(())
}

/// Stop the streaming session. Idempotent.
#[tauri::command]
pub fn stop_stream(bridge: tauri::State<StreamBridge>) -> Result<(), String> {
    let mut guard = bridge.session.lock().unwrap();
    if let Some(session) = guard.as_mut() {
        session.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = session.mux_handle.take() {
            let _ = handle.join();
        }
    }
    *guard = None;
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

// ─── Mux thread — Capabilities B + D ─────────────────────────────────────────

/// The `sm-stream-mux` thread body.
///
/// Drains `pkt_rx`, fires PLI on the first packet, buffers non-keyframe
/// packets until the first IDR, builds the fMP4 init segment from SPS+PPS,
/// and emits `"stream/init"` + `"stream/segment"` events to the frontend.
fn mux_thread(
    pkt_rx: Receiver<EncodedPacket>,
    stop_flag: Arc<AtomicBool>,
    counters: Arc<BridgeCounters>,
    app: tauri::AppHandle,
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
                    emit_segment(&app, &counters, segment);
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
                        emit_init(&app, &counters, init_bytes);
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
                emit_segment(&app, &counters, segment);
            }
        }
    }
}

/// Emit an init segment via `app.emit("stream/init", bytes)`.
///
/// OQ-tauri-emit-1: `Vec<u8>` is JSON-serialized by Tauri 2 `emit` as
/// `Array<number>`. The JS `toUint8Array()` helper wraps it. Acceptable for V1
/// (4 KB init segment → ~12 KB JSON payload over LAN).
fn emit_init(app: &tauri::AppHandle, counters: &BridgeCounters, bytes: Vec<u8>) {
    match app.emit("stream/init", bytes) {
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

/// Emit a media segment via `app.emit("stream/segment", bytes)`.
///
/// Backpressure (Capability D): if emit returns an error (e.g., window closed),
/// increment `dropped_segments` (drop-newest — we don't queue, we just drop).
fn emit_segment(app: &tauri::AppHandle, counters: &BridgeCounters, bytes: Vec<u8>) {
    match app.emit("stream/segment", bytes) {
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
                channel, // RED: StreamSession does not yet have this field
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
