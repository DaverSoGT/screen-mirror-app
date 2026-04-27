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
}
