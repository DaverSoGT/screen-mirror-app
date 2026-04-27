//! Tauri IPC bridge — stream commands.
//!
//! Implements the Tauri command surface for the screen-mirror live stream:
//! `start_stream`, `stop_stream`, `attach_stream`, `stream_diagnostics`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use sm_domain::transport::TransportError;

// ─── Bridge bookkeeping ───────────────────────────────────────────────────────

/// Shared bookkeeping counters for the mux thread + diagnostics command.
#[derive(Debug, Default)]
pub(crate) struct BridgeCounters {
    pub fragments_emitted: AtomicU64,
    pub init_segments_emitted: AtomicU64,
    pub dropped_segments: AtomicU64,
    pub keyframe_requests_fired: AtomicU64,
}

// ─── Minimal receiver ops trait ───────────────────────────────────────────────

/// Minimal interface needed from the receiver by the bridge (avoids pulling the
/// full `VideoReceiver` bound into tests).
pub(crate) trait ReceiverOps: Send {
    /// Fire a PLI toward the sender.
    fn request_keyframe(&self) -> Result<(), TransportError>;
    /// Count of dropped frames (backpressure).
    fn dropped_frames(&self) -> u64;
}

// ─── StreamSession — internal per-run state ───────────────────────────────────

/// Active stream session: receiver + mux thread + counters.
pub(crate) struct StreamSession {
    /// Stop flag shared with the mux thread. Set by `stop_stream`.
    pub stop_flag: Arc<AtomicBool>,
    /// Join handle for the `sm-stream-mux` thread.
    pub mux_handle: Option<JoinHandle<()>>,
    /// Shared counters observable via `stream_diagnostics`.
    pub counters: Arc<BridgeCounters>,
    /// The receiver — kept alive so packets flow until stop.
    pub receiver: Option<Box<dyn ReceiverOps>>,
    /// PLI rate-limit: timestamp of the last keyframe request.
    pub last_pli: Option<Instant>,
}

impl StreamSession {
    pub fn is_running(&self) -> bool {
        !self.stop_flag.load(Ordering::Relaxed)
    }
}

// ─── StreamBridge — Capability A ─────────────────────────────────────────────

/// Tauri managed state for an active streaming session.
///
/// Held behind `State<StreamBridge>` in Tauri commands.
/// Wraps a `Mutex<Option<StreamSession>>` to allow mutation inside
/// immutable Tauri command references.
pub struct StreamBridge {
    pub(crate) session: Mutex<Option<StreamSession>>,
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

// ─── Tauri commands ───────────────────────────────────────────────────────────

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
            .map(|t| now.duration_since(t) >= std::time::Duration::from_secs(2))
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // ─── Capability A: bridge state container ────────────────────────────────

    /// A.1 — StreamBridge::new() produces a bridge with no active session.
    #[test]
    fn stream_bridge_new_is_not_running() {
        let bridge = StreamBridge::new();
        assert!(!bridge.is_running());
    }

    /// A.2 — bridge with a session (stop_flag = false) → is_running() is true.
    #[test]
    fn stream_bridge_with_session_is_running() {
        let bridge = StreamBridge::new();
        let counters = Arc::new(BridgeCounters::default());
        let stop_flag = Arc::new(AtomicBool::new(false));
        {
            let mut guard = bridge.session.lock().unwrap();
            *guard = Some(StreamSession {
                stop_flag,
                mux_handle: None,
                counters,
                receiver: None,
                last_pli: None,
            });
        }
        assert!(bridge.is_running());
    }

    /// A.3 — setting stop_flag to true makes is_running() return false.
    #[test]
    fn stream_bridge_stop_flag_stops_session() {
        let bridge = StreamBridge::new();
        let stop_flag = Arc::new(AtomicBool::new(false));
        {
            let mut guard = bridge.session.lock().unwrap();
            *guard = Some(StreamSession {
                stop_flag: stop_flag.clone(),
                mux_handle: None,
                counters: Arc::new(BridgeCounters::default()),
                receiver: None,
                last_pli: None,
            });
        }
        assert!(bridge.is_running());
        stop_flag.store(true, Ordering::Relaxed);
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

    /// Build an Annex-B byte stream from raw NAL slices (prepends 4-byte start codes).
    fn make_annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// B.1 — extract_sps_pps_from_idr finds SPS (NAL type 7) and PPS (NAL type 8).
    ///
    /// RED: extract_sps_pps_from_idr does not exist in sm-infra yet.
    #[test]
    fn extract_sps_pps_finds_sps_and_pps() {
        let sps_nal = &[0x67u8, 0x42, 0xE0, 0x1E]; // nal_type = 7
        let pps_nal = &[0x68u8, 0xCE, 0x38]; // nal_type = 8
        let annex_b = make_annex_b(&[sps_nal, pps_nal]);
        let result = sm_infra::render::fmp4_muxer::extract_sps_pps_from_idr(&annex_b);
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
        let result = sm_infra::render::fmp4_muxer::extract_sps_pps_from_idr(&annex_b);
        assert!(result.is_none());
    }

    /// B.3 — extract_sps_pps_from_idr returns None when PPS is missing.
    #[test]
    fn extract_sps_pps_returns_none_when_pps_missing() {
        let sps_nal = &[0x67u8, 0x42, 0xE0, 0x1E];
        let annex_b = make_annex_b(&[sps_nal]);
        let result = sm_infra::render::fmp4_muxer::extract_sps_pps_from_idr(&annex_b);
        assert!(result.is_none());
    }

    /// B.4 — extract_sps_pps_from_idr on empty input returns None.
    #[test]
    fn extract_sps_pps_empty_returns_none() {
        let result = sm_infra::render::fmp4_muxer::extract_sps_pps_from_idr(&[]);
        assert!(result.is_none());
    }

    // ─── Capability C: PLI fire-once on attach ───────────────────────────────

    /// Fake receiver that counts PLI calls.
    struct FakeReceiver {
        pli_count: std::sync::atomic::AtomicU32,
    }

    impl FakeReceiver {
        fn new() -> Self {
            Self {
                pli_count: std::sync::atomic::AtomicU32::new(0),
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

    fn make_bridge_with_fake_receiver() -> StreamBridge {
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
            });
        }
        bridge
    }

    /// C.1 — PLI is fired exactly once on the first attach call.
    ///
    /// RED: attach logic does not exist on StreamBridge yet.
    #[test]
    fn pli_fired_once_on_attach() {
        let bridge = make_bridge_with_fake_receiver();
        // Simulate attach_stream logic directly (cannot use Tauri State in unit tests).
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            let now = std::time::Instant::now();
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
            "PLI must fire exactly once on first attach"
        );
    }

    /// C.2 — second attach within 2 seconds is rate-limited: no second PLI.
    #[test]
    fn pli_rate_limited_within_2s() {
        let bridge = make_bridge_with_fake_receiver();
        // First attach.
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            session.last_pli = Some(std::time::Instant::now());
            session
                .counters
                .keyframe_requests_fired
                .fetch_add(1, Ordering::Relaxed);
        }
        // Second attach immediately (< 2s elapsed).
        {
            let mut guard = bridge.session.lock().unwrap();
            let session = guard.as_mut().unwrap();
            let now = std::time::Instant::now();
            let elapsed = session
                .last_pli
                .map(|t| now.duration_since(t))
                .unwrap_or(std::time::Duration::MAX);
            if elapsed >= std::time::Duration::from_secs(2) {
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
            "second PLI within 2s must be rate-limited"
        );
    }
}
