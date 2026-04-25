//! Windows Graphics Capture adapter.
//!
//! All code in this module is gated to `cfg(target_os = "windows")` and MUST NOT compile on
//! non-Windows targets. This file implements [`CaptureSource`] for Windows using the
//! `windows-capture` v2 crate via its `start_free_threaded` path.

#![cfg(target_os = "windows")]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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

use sm_domain::{
    CaptureConfig, CaptureError, CaptureFrame, CaptureSource, MonitorId, MonitorInfo,
    MonitorSelector, PixelFormat,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bounded channel capacity for frame delivery (R11.1).
/// At 60 fps this represents ~67 ms of latency tolerance. Value is in [4, 8].
/// Used in the `start` implementation (Batch 4, task 5.6) when wiring the channel.
#[allow(dead_code)]
pub const CAPTURE_CHANNEL_CAPACITY: usize = 4;

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
    let mut hasher = DefaultHasher::new();
    device_name.hash(&mut hasher);
    let id = MonitorId(hasher.finish());

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

/// Internal state carried on the WGC capture thread.
///
/// This type implements [`GraphicsCaptureApiHandler`] and forwards frames
/// to the caller via a bounded `SyncSender`. It is NOT part of the public API.
struct WgcHandler {
    tx: std::sync::mpsc::SyncSender<CaptureFrame>,
    dropped: Arc<AtomicU64>,
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = (Arc<AtomicU64>, std::sync::mpsc::SyncSender<CaptureFrame>);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (dropped, tx) = ctx.flags;
        Ok(Self { tx, dropped })
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

        match self.tx.try_send(capture_frame) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                // Consumer dropped the receiver — tear down the WGC session.
                capture_control.stop();
            }
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
        // Validate max_fps (R5.4).
        if config.max_fps == Some(0) {
            return Err(CaptureError::Internal("max_fps must be > 0".into()));
        }

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
                    let mut hasher = DefaultHasher::new();
                    device_name.hash(&mut hasher);
                    if MonitorId(hasher.finish()) == id {
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

        // Border setting — todo!() placeholder for Batch 4 (Phase 6).
        // Full border-detection logic (RtlGetVersion + BorderPolicy::Auto) lands in task 6.2.
        let border = DrawBorderSettings::Default;
        let _ = &self.config.border; // suppress unused warning until Phase 6

        let settings = Settings::new(
            self.monitor,
            cursor,
            border,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            (Arc::clone(&self.dropped), tx),
        );

        let control = WgcHandler::start_free_threaded(settings)
            .map_err(|e| CaptureError::SessionCreateFailed(format!("{e}")))?;

        self.control = Some(control);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
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
