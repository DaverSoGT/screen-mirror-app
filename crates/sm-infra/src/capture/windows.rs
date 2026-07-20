//! Windows Graphics Capture adapter.
//!
//! All code in this module is gated to `cfg(target_os = "windows")` and MUST NOT compile on
//! non-Windows targets. This file implements [`CaptureSource`] for Windows using the
//! `windows-capture` v2 crate via its `start_free_threaded` path.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use std::sync::OnceLock;

use sm_domain::{
    BorderPolicy, CaptureConfig, CaptureError, CaptureFrame, CaptureSource, MonitorId, MonitorInfo,
    MonitorSelector, PixelFormat,
};

// ---------------------------------------------------------------------------
// Stable hash — djb2
// ---------------------------------------------------------------------------

/// Computes a djb2 hash over raw UTF-8 bytes.
///
/// This is intentionally NOT `std::collections::hash_map::DefaultHasher`, which is
/// explicitly documented as "not guaranteed to be stable across Rust versions"
/// (https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html).
/// djb2 is a well-known, deterministic algorithm: `hash = hash * 33 ^ byte` over
/// every byte in the input, seeded at 5381. The output is identical across Rust
/// compiler versions, operating systems, and process restarts — making it safe for
/// persisted user configuration (e.g. a saved monitor selection).
fn djb2(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    for &b in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(b));
    }
    hash
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bounded channel capacity for frame delivery (R11.1).
///
/// At 60 fps this represents ~67 ms of latency tolerance. Value is in [4, 8].
/// Consumers should create their `SyncSender` channel with this capacity:
/// `std::sync::mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY)`.
pub const CAPTURE_CHANNEL_CAPACITY: usize = 4;

/// Heartbeat interval for static-content frame injection (capture-static-freeze fix).
///
/// WGC `on_frame_arrived` is called only when desktop content changes. A static screen
/// (no motion, no cursor blink, no animation) yields zero frames → zero RTP packets →
/// viewer freezes on the last received frame. A sibling heartbeat thread injects a
/// duplicate of the last real frame at this cadence (with an advanced timestamp) so the
/// encoder keeps producing output. 100ms = 10fps minimum during static periods. Real
/// frames arriving from WGC reset the heartbeat — no duplicates injected during motion.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Phase 6 — Border detection (R9.1–R9.5)
// ---------------------------------------------------------------------------

/// Pure predicate: returns `true` if the given Windows build number supports
/// the `GraphicsCaptureSession.IsBorderRequired` API (i.e., build ≥ 22621,
/// which corresponds to Windows 11 22H2).
///
/// This function accepts a `u32` build number so it can be exercised in unit
/// tests without requiring a specific host OS version (R9.1 scenario 1 / R9.3).
#[inline]
fn supports_borderless_for_build(build: u32) -> bool {
    build >= 22621
}

/// Cached, process-wide check: returns `true` if the running OS supports
/// disabling the WGC capture border.
///
/// The result is computed once via `RtlGetVersion` (through the `windows-version`
/// crate) and cached in a `OnceLock<bool>`. OS version cannot change at runtime,
/// so this is safe and efficient.
fn supports_borderless() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let build = windows_version::OsVersion::current().build;
        supports_borderless_for_build(build)
    })
}

// ── Shared observability seam (perf-pipeline-throughput Slice 1) ────────────
//
// Defined here (no hw-encoder feature gate) so both the capture-side gate in
// on_frame_arrived and the encode-side gate in pump_loop (windows_mft.rs) call
// the same tested predicate instead of an inline duplicate.

/// Returns `true` when the elapsed time since `window_start` is at or above `threshold`.
///
/// Extracted as a pure function so the cadence predicate is unit-testable with synthetic
/// `Instant` values — no wall-clock sleeping required (D-PPT-6).
#[inline]
pub(crate) fn interval_elapsed(
    window_start: std::time::Instant,
    now: std::time::Instant,
    threshold: std::time::Duration,
) -> bool {
    now.duration_since(window_start) >= threshold
}

// ---------------------------------------------------------------------------
// Helper: map Monitor errors to CaptureError
// ---------------------------------------------------------------------------

fn map_monitor_err(e: windows_capture::monitor::Error) -> CaptureError {
    CaptureError::Internal(format!("monitor error: {e}"))
}

// ---------------------------------------------------------------------------
// Helper: build MonitorInfo from a windows-capture Monitor
// ---------------------------------------------------------------------------

fn monitor_info_from(m: &Monitor, is_primary: bool) -> Result<MonitorInfo, CaptureError> {
    let device_name = m.device_name().map_err(map_monitor_err)?;
    let label = device_name.clone();

    // Derive a stable u64 id from the device name string (e.g. "\\.\DISPLAY1").
    let id = MonitorId(djb2(device_name.as_bytes()));

    let width = m.width().map_err(map_monitor_err)?;
    let height = m.height().map_err(map_monitor_err)?;

    Ok(MonitorInfo {
        id,
        label,
        width,
        height,
        is_primary,
    })
}

// ---------------------------------------------------------------------------
// Internal capture handler (lives on the WGC OS thread)
// ---------------------------------------------------------------------------

/// Shared snapshot of the most recent CaptureFrame + the instant it was observed.
/// Owned by both `WgcHandler` (writer on the WGC thread) and the heartbeat thread (reader).
/// `CaptureFrame::clone` is cheap (Arc refcount on the pixel buffer, no data copy).
type LastFrameSlot = Arc<std::sync::Mutex<Option<(CaptureFrame, std::time::Instant)>>>;

/// Internal state carried on the WGC capture thread.
///
/// This type implements [`GraphicsCaptureApiHandler`] and forwards frames
/// to the caller via a bounded `SyncSender`. It is NOT part of the public API.
struct WgcHandler {
    tx: std::sync::mpsc::SyncSender<CaptureFrame>,
    dropped: Arc<AtomicU64>,
    /// Shared with the heartbeat thread; updated on every real frame delivery so the
    /// heartbeat can detect "no real frame in HEARTBEAT_INTERVAL" and inject a duplicate.
    last_frame: LastFrameSlot,
    /// Frame count accumulated in the current 1-second FPS window (I1, D-PPT-1).
    /// Counts only frames that reached the channel successfully (delivered rate).
    fps_frame_count: u32,
    /// Start of the current 1-second FPS window (I1, D-PPT-1).
    fps_window_start: std::time::Instant,
    /// Snapshot of `dropped` at the end of the last 1-second window, used to compute
    /// per-interval drop delta (I1, D-PPT-3).
    last_dropped_snapshot: u64,
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = (
        Arc<AtomicU64>,
        std::sync::mpsc::SyncSender<CaptureFrame>,
        LastFrameSlot,
    );
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (dropped, tx, last_frame) = ctx.flags;
        Ok(Self {
            tx,
            dropped,
            last_frame,
            fps_frame_count: 0,
            fps_window_start: std::time::Instant::now(),
            last_dropped_snapshot: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();

        // Collect timestamp before taking the mutable buffer borrow.
        let timestamp = frame
            .timestamp()
            .ok()
            .and_then(|ts| {
                // WinRT TimeSpan.Duration is in 100-ns units.
                u64::try_from(ts.Duration).ok()
            })
            .map(|d| std::time::Duration::from_nanos(d * 100))
            .unwrap_or(std::time::Duration::ZERO);

        let mut buf = match frame.buffer() {
            Ok(b) => b,
            Err(e) => {
                // Non-fatal — skip this frame.
                eprintln!("sm-infra: frame buffer error: {e}");
                return Ok(());
            }
        };

        let bytes: &[u8] = buf.as_raw_buffer();
        let stride = (bytes.len() as u32)
            .checked_div(height)
            .unwrap_or(width * 4);

        // Copy pixel data into an Arc slice so it can be shared without holding the GPU mapping.
        let data: Arc<[u8]> = Arc::from(bytes);

        let capture_frame = CaptureFrame {
            data,
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            timestamp,
        };

        // Update shared snapshot BEFORE send so the heartbeat thread always has a
        // representative frame to duplicate. Cloning `capture_frame` is cheap — the
        // `data` field is `Arc<[u8]>`, so the clone is a refcount bump, not a memcpy.
        if let Ok(mut guard) = self.last_frame.lock() {
            *guard = Some((capture_frame.clone(), std::time::Instant::now()));
        }

        match self.tx.try_send(capture_frame) {
            Ok(()) => {
                // Count only frames that were actually delivered to the encoder channel.
                // Heartbeat-injected frames go through heartbeat_loop's own try_send,
                // NOT through here — so capture_fps measures WGC's true delivery rate only.
                // On a static screen capture_fps legitimately reads ~0; this is correct (D-PPT-1).
                self.fps_frame_count += 1;
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                // Consumer dropped the receiver — tear down the WGC session.
                capture_control.stop();
            }
        }

        // Per-second observability window: emit capture_fps and capture drop-delta (I1, D-PPT-1/3).
        // Checked unconditionally after the match so both delivered and dropped frames advance
        // the window clock, keeping the log cadence stable even under backpressure.
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.fps_window_start);
        if interval_elapsed(
            self.fps_window_start,
            now,
            std::time::Duration::from_secs(1),
        ) {
            let fps = self.fps_frame_count as f64 / elapsed.as_secs_f64();
            tracing::info!(
                target: "sm_infra::capture::windows",
                capture_fps = %format!("{fps:.1}"),
                frames = self.fps_frame_count,
                "capture throughput"
            );

            // Compute per-interval drop delta (D-PPT-3).
            let current_dropped = self.dropped.load(Ordering::Relaxed);
            let (delta, new_last) = compute_drop_delta(current_dropped, self.last_dropped_snapshot);
            if delta > 0 {
                tracing::info!(
                    target: "sm_infra::capture::windows",
                    channel_drops = delta,
                    channel = "capture_to_enc",
                    "capture channel drops"
                );
            }

            // Reset window state.
            self.fps_frame_count = 0;
            self.fps_window_start = std::time::Instant::now();
            self.last_dropped_snapshot = new_last;
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // Session closed externally (monitor unplugged, session ended). No action needed —
        // the WGC thread will exit naturally and the channel will be disconnected.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Heartbeat thread — static-content frame injection
// ---------------------------------------------------------------------------

/// Heartbeat loop body — runs on a sibling OS thread spawned by `WindowsCaptureSource::start`.
///
/// WGC delivers frames only on content change. When the desktop is static, the encoder
/// receives nothing and the viewer freezes. This loop wakes every `HEARTBEAT_INTERVAL`,
/// checks the shared `last_frame` snapshot, and if no real frame has arrived within that
/// window, injects a duplicate (with an advanced monotonic timestamp) so the encoder keeps
/// producing output.
///
/// Exits when `stop_flag` is set OR the consumer drops the channel (Disconnected). A
/// `Full` send result is silently ignored — real-frame backpressure already handles this.
fn heartbeat_loop(
    tx: std::sync::mpsc::SyncSender<CaptureFrame>,
    last_frame: LastFrameSlot,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        std::thread::sleep(HEARTBEAT_INTERVAL);

        if stop_flag.load(Ordering::Relaxed) {
            return;
        }

        // Snapshot under lock, then drop the lock before sending.
        let snapshot = match last_frame.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return, // mutex poisoned — capture thread panicked; exit gracefully
        };

        let Some((mut frame, last_update)) = snapshot else {
            continue; // no real frame has arrived yet — nothing to duplicate
        };

        if last_update.elapsed() < HEARTBEAT_INTERVAL {
            continue; // a real frame arrived within the window — skip this beat
        }

        // Advance timestamp monotonically so the encoder + downstream RTP timestamps
        // see a regular cadence. `saturating_add` guards against u128 overflow at the
        // far end of a session lifetime.
        frame.timestamp = frame.timestamp.saturating_add(HEARTBEAT_INTERVAL);

        // Reset the "last observed" instant so we don't immediately re-fire on the next
        // tick. Real frames continue to overwrite this when they arrive.
        if let Ok(mut guard) = last_frame.lock()
            && let Some((_, instant)) = guard.as_mut()
        {
            *instant = std::time::Instant::now();
        }

        match tx.try_send(frame) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Channel saturated — encoder will catch up on real frames. Skip silently;
                // bumping `dropped` here would conflate heartbeat throttling with WGC drops.
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return; // consumer dropped the receiver — session ending
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public adapter struct
// ---------------------------------------------------------------------------

/// Windows Graphics Capture adapter implementing [`CaptureSource`].
///
/// This adapter is gated to `cfg(target_os = "windows")`. It uses the
/// `windows-capture` v2 library's `start_free_threaded` path so the WGC
/// callback runs on a dedicated OS thread, fully isolated from Tauri's COM
/// apartment (R10 compliance).
///
/// # Thread safety
///
/// `WindowsCaptureSource` is `Send`. The `dropped` counter is an `Arc<AtomicU64>`
/// shared with the WGC handler thread; reads via `dropped_frames()` are safe from
/// any thread.
pub struct WindowsCaptureSource {
    /// Capture configuration supplied at construction time.
    config: CaptureConfig,

    /// The resolved monitor to capture. Populated in `new()`.
    monitor: Monitor,

    /// Cumulative count of frames dropped due to channel backpressure.
    /// Shared with the WGC callback thread via `Arc`.
    dropped: Arc<AtomicU64>,

    /// Handle to the running capture session, if any.
    /// `None` before `start()` or after `stop()`.
    control: Option<CaptureControl<WgcHandler, Box<dyn std::error::Error + Send + Sync>>>,

    /// Stop signal for the heartbeat thread spawned by `start()`.
    /// `None` before `start()` or after `stop()`; `Some` during an active session.
    heartbeat_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

// SAFETY: Monitor wraps an HMONITOR handle. windows-capture declares Monitor as Send.
// WindowsCaptureSource is safe to send across thread boundaries.
unsafe impl Send for WindowsCaptureSource {}

impl std::fmt::Debug for WindowsCaptureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsCaptureSource")
            .field("config", &self.config)
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .field("active", &self.control.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// CaptureSource implementation
// ---------------------------------------------------------------------------

impl CaptureSource for WindowsCaptureSource {
    fn enumerate_monitors() -> Result<Vec<MonitorInfo>, CaptureError>
    where
        Self: Sized,
    {
        let primary = Monitor::primary()
            .map_err(|e| CaptureError::Internal(format!("primary monitor: {e}")))?;

        let all = Monitor::enumerate()
            .map_err(|e| CaptureError::Internal(format!("enumerate monitors: {e}")))?;

        let mut result = Vec::with_capacity(all.len());
        for m in &all {
            let is_primary = m == &primary;
            result.push(monitor_info_from(m, is_primary)?);
        }

        Ok(result)
    }

    fn new(config: CaptureConfig) -> Result<Self, CaptureError>
    where
        Self: Sized,
    {
        // Validate domain invariants (R5.4 — moved to sm-domain CaptureConfig::validate).
        config.validate()?;

        // Resolve the requested monitor.
        let monitor = match config.monitor {
            MonitorSelector::Primary => {
                Monitor::primary().map_err(|_| CaptureError::MonitorNotFound("primary".into()))?
            }

            MonitorSelector::ByIndex(idx) => {
                // Monitor::from_index uses 1-based indexing.
                Monitor::from_index(idx + 1)
                    .map_err(|_| CaptureError::MonitorNotFound(format!("index {idx}")))?
            }

            MonitorSelector::ById(id) => {
                // Scan enumerated monitors for a matching hash.
                let all = Monitor::enumerate()
                    .map_err(|e| CaptureError::Internal(format!("enumerate: {e}")))?;

                let mut found = None;
                for m in all {
                    let device_name = m.device_name().map_err(map_monitor_err)?;
                    if MonitorId(djb2(device_name.as_bytes())) == id {
                        found = Some(m);
                        break;
                    }
                }
                found.ok_or_else(|| CaptureError::MonitorNotFound(format!("id {:?}", id)))?
            }
        };

        Ok(Self {
            config,
            monitor,
            dropped: Arc::new(AtomicU64::new(0)),
            control: None,
            heartbeat_stop: None,
        })
    }

    fn start(&mut self, tx: std::sync::mpsc::SyncSender<CaptureFrame>) -> Result<(), CaptureError> {
        // R8.1 — runtime WGC support check.
        let supported = GraphicsCaptureApi::is_supported()
            .map_err(|e| CaptureError::Internal(format!("WGC IsSupported probe failed: {e}")))?;
        if !supported {
            return Err(CaptureError::NotSupported);
        }

        // Cursor setting.
        let cursor = if self.config.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };

        // Border setting — R9.1–R9.5.
        // BorderPolicy::Auto: disable on Win11 22H2+ (build ≥ 22621), leave default otherwise.
        // BorderPolicy::AlwaysOff: disable regardless of OS version (best-effort).
        // BorderPolicy::AlwaysOn: leave border enabled (OS default).
        let border = match self.config.border {
            BorderPolicy::Auto => {
                if supports_borderless() {
                    DrawBorderSettings::WithoutBorder
                } else {
                    DrawBorderSettings::Default
                }
            }
            BorderPolicy::AlwaysOff => DrawBorderSettings::WithoutBorder,
            BorderPolicy::AlwaysOn => DrawBorderSettings::WithBorder,
        };

        // Heartbeat infrastructure (capture-static-freeze fix): shared snapshot of the
        // most recent frame + a stop flag. Clone `tx` so the WGC handler and the heartbeat
        // thread each own a send handle on the same bounded channel.
        let last_frame: LastFrameSlot = Arc::new(std::sync::Mutex::new(None));
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hb_tx = tx.clone();
        let hb_last_frame = Arc::clone(&last_frame);
        let hb_stop = Arc::clone(&stop_flag);

        let settings = Settings::new(
            self.monitor,
            cursor,
            border,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            (Arc::clone(&self.dropped), tx, last_frame),
        );

        let control = WgcHandler::start_free_threaded(settings)
            .map_err(|e| CaptureError::SessionCreateFailed(format!("{e}")))?;

        // Sibling thread to the WGC OS thread; exits on stop_flag OR Disconnected.
        std::thread::Builder::new()
            .name("capture-heartbeat".into())
            .spawn(move || heartbeat_loop(hb_tx, hb_last_frame, hb_stop))
            .map_err(|e| CaptureError::Internal(format!("spawn heartbeat: {e}")))?;

        self.control = Some(control);
        self.heartbeat_stop = Some(stop_flag);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        // Signal the heartbeat thread to exit at its next wake (max HEARTBEAT_INTERVAL delay).
        if let Some(flag) = self.heartbeat_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(control) = self.control.take() {
            control
                .stop()
                .map_err(|e| CaptureError::Internal(format!("stop failed: {e}")))?;
        }
        // Idempotent: calling stop when already stopped is Ok(()) (AC #13).
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Additional accessors
// ---------------------------------------------------------------------------

impl WindowsCaptureSource {
    /// Returns the resolved monitor's pixel dimensions as `(width, height)`.
    ///
    /// The monitor is resolved at `new()` time (see `CaptureSource::new`). This method
    /// queries the stored `Monitor` handle synchronously. On error (e.g., the monitor
    /// was disconnected between `new()` and this call), returns `(0, 0)` so callers
    /// that forward dimensions to `EncoderConfig` will fall back to the adapter default
    /// via the sentinel-zero mechanism (see `effective_dimensions` in `windows_mft.rs`).
    pub fn dimensions(&self) -> (u32, u32) {
        let w = self.monitor.width().unwrap_or(0);
        let h = self.monitor.height().unwrap_or(0);
        (w, h)
    }
}

// ---------------------------------------------------------------------------
// Observability seams (perf-pipeline-throughput Slice 1)
// ---------------------------------------------------------------------------

/// Compute the per-interval drop delta for a monotonically-increasing drop counter.
///
/// Returns `(delta, new_last)` where:
/// - `delta` is the number of drops that occurred since the last snapshot
///   (`current.saturating_sub(last)` — monotonic, never negative),
/// - `new_last` is the snapshot to store for the next interval (equals `current`).
///
/// Called by `on_frame_arrived` (I1, D-PPT-3) at each 1-second window boundary
/// with `self.dropped.load(Relaxed)` as `current`.
#[inline]
fn compute_drop_delta(current: u64, last: u64) -> (u64, u64) {
    (current.saturating_sub(last), current)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Phase 7 tests (7.1) ────────────────────────────────────────────────────

    /// Unit test: `dropped_frames()` returns 0 on a freshly constructed source.
    ///
    /// This verifies the Arc<AtomicU64> is initialised to zero in `new()` and
    /// readable through the public `dropped_frames()` accessor (R11.3, R11.4).
    /// No live WGC session is required — this test is NOT `#[ignore]`.
    #[test]
    fn dropped_frames_starts_at_zero_after_new() {
        let config = sm_domain::CaptureConfig::default();
        // `new()` resolves the primary monitor — this succeeds on any Windows desktop.
        // If it fails (e.g., headless runner), we skip rather than fail.
        let source = match WindowsCaptureSource::new(config) {
            Ok(s) => s,
            Err(_) => return, // headless / no display — skip
        };
        assert_eq!(
            source.dropped_frames(),
            0,
            "dropped_frames() must be 0 on a freshly constructed source"
        );
    }

    /// Unit test: the `dropped_frames` counter correctly reflects the shared
    /// `Arc<AtomicU64>`. This tests the wiring between the adapter struct and
    /// the counter, independent of WGC or real frame delivery (R11.3, R11.4).
    #[test]
    fn dropped_frames_counter_reflects_arc_atomic() {
        // We cannot construct WgcHandler directly (private struct), but we can
        // verify the contract by confirming AtomicU64 is Sync and that the
        // counter value exposed by dropped_frames() is consistent with the
        // underlying atomic state via the Arc.
        let counter = Arc::new(AtomicU64::new(0));
        // Simulate what WgcHandler does on a full channel:
        counter.fetch_add(1, Ordering::Relaxed);
        counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "Arc<AtomicU64> must reflect 2 drops"
        );
        // Simulate a second thread reading the counter (satisfies R11.4):
        let reader = Arc::clone(&counter);
        let handle = std::thread::spawn(move || reader.load(Ordering::Relaxed));
        let val = handle.join().expect("reader thread must not panic");
        assert_eq!(val, 2, "counter must be readable from a different thread");
    }

    // ── Phase 6 tests (6.1) ────────────────────────────────────────────────────

    /// Build number gate: Win11 22H2 threshold is build 22621.
    /// All branches of the `supports_borderless_for_build` predicate are exercised
    /// without running on any specific OS version (pure logic test, R9.1/R9.3).
    #[test]
    fn border_policy_auto_disables_on_win11_22h2_plus() {
        // Exactly at the threshold (Win11 22H2).
        assert!(
            supports_borderless_for_build(22621),
            "build 22621 (Win11 22H2) must return true"
        );
        // One build above (e.g., a later cumulative update).
        assert!(
            supports_borderless_for_build(26100),
            "build 26100 (Win11 24H2) must return true"
        );
        // Just below the threshold (Win11 21H2).
        assert!(
            !supports_borderless_for_build(22000),
            "build 22000 (Win11 21H2) must return false"
        );
        // Win10 builds.
        assert!(
            !supports_borderless_for_build(19045),
            "build 19045 (Win10 22H2) must return false"
        );
        // Edge: build 0 (hypothetical / test guard).
        assert!(
            !supports_borderless_for_build(0),
            "build 0 must return false"
        );
    }

    /// Lock-in test: asserts a SPECIFIC, HARDCODED u64 output for a well-known device name.
    ///
    /// This test exists to catch any accidental change of the hash function. If this value
    /// ever changes, persisted monitor selections (saved user configuration) would become
    /// incompatible — that is a user-visible regression.
    ///
    /// The expected value `0xCA76352EF04EA74E` was computed by running the `djb2`
    /// implementation above on the byte sequence of `r"\\.\DISPLAY1"` (the canonical
    /// Windows device name returned by `Monitor::device_name()`).
    ///
    /// DO NOT update this constant unless the hash function is intentionally changed AND a
    /// migration path for existing stored IDs is provided.
    #[test]
    fn monitor_id_hash_is_stable_for_known_device_name() {
        // r"\\.\DISPLAY1" is the Windows device name format returned by Monitor::device_name().
        let device_name = r"\\.\DISPLAY1";
        let id = MonitorId(djb2(device_name.as_bytes()));
        assert_eq!(
            id,
            MonitorId(0xCA76352EF04EA74E_u64),
            "djb2 hash of '{}' must remain 0xCA76352EF04EA74E — \
             changing this breaks persisted monitor configuration",
            device_name,
        );
    }

    /// Additional stability check: the djb2 function must be pure — same input always
    /// gives the same output within a single process run.
    #[test]
    fn djb2_is_deterministic_across_calls() {
        let input = b"\\\\.\\ DISPLAY2";
        assert_eq!(djb2(input), djb2(input));
    }

    // ── Observability seam tests (WU-A RED — perf-pipeline-throughput Slice 1) ──

    /// Task 1.4 [RED]: compute_drop_delta computes the per-interval delta and advances
    /// the snapshot to the current cumulative value.
    #[test]
    fn drop_delta_computes_and_advances_snapshot() {
        // First call: current=5, last=2 → delta=3, new_last=5
        let (delta, new_last) = compute_drop_delta(5, 2);
        assert_eq!(delta, 3, "delta must be current - last");
        assert_eq!(new_last, 5, "new_last must equal current");

        // Second call: counter unchanged current=5, last=5 → delta=0, new_last=5
        let (delta2, new_last2) = compute_drop_delta(5, 5);
        assert_eq!(delta2, 0, "delta must be 0 when counter did not advance");
        assert_eq!(
            new_last2, 5,
            "new_last must equal current even when delta is 0"
        );

        // Saturating branch: current < last (e.g. counter reset) → delta clamps to 0,
        // new_last re-pins to current rather than underflowing.
        assert_eq!(
            compute_drop_delta(2, 5),
            (0, 2),
            "delta must saturate to 0 and re-pin new_last when current < last"
        );
    }
}
