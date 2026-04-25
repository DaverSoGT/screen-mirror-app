//! Port boundary for screen capture sources.

use std::sync::Arc;

/// Pixel format of a captured frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit BGRA, 8 bits per channel. Native format produced by Windows Graphics Capture API.
    Bgra8,
    /// 32-bit RGBA, 8 bits per channel.
    Rgba8,
    /// 64-bit RGBA, 16 bits per channel, half-float. Reserved for future HDR support.
    Rgba16F,
}

impl std::fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PixelFormat::Bgra8 => write!(f, "BGRA8"),
            PixelFormat::Rgba8 => write!(f, "RGBA8"),
            PixelFormat::Rgba16F => write!(f, "RGBA16F"),
        }
    }
}

/// A single captured frame with shared-ownership pixel buffer.
///
/// Cloning a `CaptureFrame` increments the Arc reference count — it does NOT copy pixel data.
/// This allows multiple consumers (encoder, preview) to receive the same frame at zero copy cost.
#[derive(Debug, Clone)]
pub struct CaptureFrame {
    /// Pixel data, BGRA8 format. Shared across clones via Arc.
    pub data: Arc<[u8]>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes (may be > width * 4 due to alignment padding).
    pub stride: u32,
    /// Pixel format of the data buffer.
    pub format: PixelFormat,
    /// Monotonic capture timestamp relative to session start.
    pub timestamp: std::time::Duration,
}

/// Errors produced by capture operations.
///
/// `Internal(String)` accepts a string payload for wrapping lower-level platform errors
/// whose types are not stable enough to warrant a dedicated variant. Platform-specific
/// error types are converted to strings at the adapter boundary to preserve the hexagonal seam.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// Screen capture is not supported on this platform or OS version.
    #[error("capture not supported on this platform or OS version")]
    NotSupported,

    /// The requested monitor was not found. The payload is the monitor identifier string.
    #[error("monitor not found: {0}")]
    MonitorNotFound(String),

    /// The capture item closed unexpectedly (monitor unplugged, session ended externally).
    #[error("capture item closed unexpectedly")]
    ItemClosed,

    /// The graphics capture device was lost (GPU reset, driver update).
    #[error("capture device lost")]
    DeviceLost,

    /// Frame dropped due to channel backpressure. Reserved for future `last_error()` API.
    #[error("backpressure: frame dropped, channel is full")]
    Backpressure,

    /// COM initialization failed. The payload contains the underlying error description.
    #[error("COM initialization failed: {0}")]
    ComInitFailed(String),

    /// Capture session creation failed. The payload contains the underlying error description.
    #[error("capture session creation failed: {0}")]
    SessionCreateFailed(String),

    /// Internal error wrapping a platform-level error string.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Opaque, stable monitor identifier. For the Windows adapter this is a `u64` hash of
/// the display device name. Usable in `MonitorSelector::ById` to select a specific display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorId(pub u64);

/// Describes a single display returned by `CaptureSource::enumerate_monitors()`.
///
/// The `id` field is a stable handle usable in `MonitorSelector::ById` to target this display
/// in a subsequent `CaptureConfig`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// Stable identifier for this display. Derived from the display device name.
    pub id: MonitorId,
    /// Human-readable display label (e.g., `"DISPLAY1"`, `"\\.\DISPLAY1"`).
    pub label: String,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Whether this display is the system's primary display.
    pub is_primary: bool,
}

/// Specifies which monitor to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorSelector {
    /// Capture the system's primary display.
    Primary,
    /// Capture the display at the given zero-based index in the enumeration order.
    ByIndex(usize),
    /// Capture the display with the given stable `MonitorId` (from `enumerate_monitors()`).
    ById(MonitorId),
}

/// Controls whether the WGC system border overlay is drawn around the captured window/monitor.
///
/// The border can only be disabled on Windows 11 22H2 (build ≥ 22621) and later.
/// On older builds, `Auto` and `AlwaysOff` behave identically (border is always on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderPolicy {
    /// Detect OS version at runtime. Disable border on Windows 11 22H2+ (build ≥ 22621).
    Auto,
    /// Always attempt to show the capture border. OS default.
    AlwaysOn,
    /// Always attempt to hide the capture border. Falls back to OS default on unsupported builds.
    AlwaysOff,
}

/// Configuration for a capture session.
///
/// # Defaults
///
/// - `monitor`: `MonitorSelector::Primary`
/// - `cursor`: `true` (cursor is included in the capture)
/// - `max_fps`: `None` (uncapped; WGC delivers frames as they become available)
/// - `border`: `BorderPolicy::Auto` (border disabled automatically on Win11 22H2+)
///
/// # Notes
///
/// `max_fps = Some(0)` is invalid and will cause the adapter's `new()` to return
/// `Err(CaptureError::Internal("max_fps must be > 0"))`.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Which monitor to capture.
    pub monitor: MonitorSelector,
    /// Whether to include the mouse cursor in the captured frames.
    pub cursor: bool,
    /// Optional frame-rate cap. `None` means uncapped. `Some(0)` is rejected by the adapter.
    pub max_fps: Option<u32>,
    /// Whether to show the system capture border overlay.
    pub border: BorderPolicy,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            monitor: MonitorSelector::Primary,
            cursor: true,
            max_fps: None,
            border: BorderPolicy::Auto,
        }
    }
}

/// Port boundary for platform-specific capture adapters.
///
/// Each platform ships a concrete type that implements this trait inside `sm-infra`. The domain
/// crate never references any platform type directly — the trait is the only coupling point.
///
/// # Trait shape
///
/// - `enumerate_monitors` and `new` are `where Self: Sized` to allow future `dyn CaptureSource`
///   usage (object-safe subset covers `start`, `stop`, and `dropped_frames`).
/// - `Send` supertrait: adapters move `SyncSender` into OS threads; the struct itself must be
///   transferable across thread boundaries.
///
/// # Backpressure
///
/// `start` accepts a bounded [`std::sync::mpsc::SyncSender`]. When the channel is full the
/// adapter MUST drop the incoming frame (drop-newest policy) and increment `dropped_frames`.
/// The adapter MUST NOT block the capture callback thread.
pub trait CaptureSource: Send {
    /// Enumerate available capture monitors without constructing a session.
    ///
    /// Returns at least one entry on any system with an active display.
    fn enumerate_monitors() -> Result<Vec<MonitorInfo>, CaptureError>
    where
        Self: Sized;

    /// Construct a new capture session with the given configuration.
    ///
    /// Does NOT start the WGC session. Call [`start`](CaptureSource::start) to begin capture.
    /// `max_fps = Some(0)` is rejected with `Err(CaptureError::Internal(_))`.
    fn new(config: CaptureConfig) -> Result<Self, CaptureError>
    where
        Self: Sized;

    /// Begin delivering frames to `tx`.
    ///
    /// The adapter pushes frames asynchronously on an internal OS thread.
    /// Frames are delivered via `tx.try_send`; a full channel causes the frame to be dropped
    /// (drop-newest backpressure).
    fn start(&mut self, tx: std::sync::mpsc::SyncSender<CaptureFrame>) -> Result<(), CaptureError>;

    /// Stop the capture session.
    ///
    /// MUST be idempotent: calling `stop` on an already-stopped session returns `Ok(())`.
    fn stop(&mut self) -> Result<(), CaptureError>;

    /// Return the cumulative count of frames dropped due to channel backpressure since `start`.
    ///
    /// Thread-safe: MAY be called from a different thread than `start`.
    fn dropped_frames(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Phase 2 tests (2.1 + 2.3) ─────────────────────────────────────────────

    /// A minimal in-test stub that implements `CaptureSource`.
    /// Exercises all 5 trait methods (4 from R1.2 + `dropped_frames` from design §5).
    struct FakeCaptureSource {
        frames: Vec<CaptureFrame>,
        dropped: u64,
    }

    impl FakeCaptureSource {
        fn with_frames(frames: Vec<CaptureFrame>) -> Self {
            Self { frames, dropped: 0 }
        }
    }

    impl CaptureSource for FakeCaptureSource {
        fn enumerate_monitors() -> Result<Vec<MonitorInfo>, CaptureError>
        where
            Self: Sized,
        {
            Ok(vec![MonitorInfo {
                id: MonitorId(1),
                label: "FAKE_DISPLAY1".into(),
                width: 1920,
                height: 1080,
                is_primary: true,
            }])
        }

        fn new(_config: CaptureConfig) -> Result<Self, CaptureError>
        where
            Self: Sized,
        {
            Ok(FakeCaptureSource::with_frames(vec![]))
        }

        fn start(
            &mut self,
            tx: std::sync::mpsc::SyncSender<CaptureFrame>,
        ) -> Result<(), CaptureError> {
            for f in self.frames.drain(..) {
                tx.try_send(f).ok();
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped
        }
    }

    /// 2.1 — trait shape: `FakeCaptureSource` can implement all 5 CaptureSource methods.
    #[test]
    fn capture_source_fake_impl_all_methods() {
        let monitors = FakeCaptureSource::enumerate_monitors().unwrap();
        assert_eq!(monitors.len(), 1);
        assert!(monitors[0].is_primary);

        let mut src = FakeCaptureSource::new(CaptureConfig::default()).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        src.start(tx).unwrap();
        drop(rx);
        src.stop().unwrap();
        assert_eq!(src.dropped_frames(), 0);
    }

    /// 2.1 — `FakeCaptureSource` emits frames via `start` and they are receivable.
    #[test]
    fn capture_source_fake_emits_frames_via_start() {
        let frame = CaptureFrame {
            data: std::sync::Arc::from(&[0u8; 4][..]),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8,
            timestamp: std::time::Duration::ZERO,
        };
        let frames = vec![frame.clone(), frame.clone(), frame];
        let mut src = FakeCaptureSource::with_frames(frames);
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        src.start(tx).unwrap();
        let received: Vec<_> = rx.try_iter().collect();
        assert_eq!(received.len(), 3);
    }

    /// 2.3 — `FakeCaptureSource` (which impl CaptureSource: Send) satisfies Send bound.
    #[test]
    fn capture_source_impl_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<FakeCaptureSource>();
    }

    /// 2.3 — trait bound: a concrete type implementing CaptureSource satisfies `Send + 'static`.
    #[test]
    fn capture_source_trait_send_bound_satisfied() {
        // This compiles only if `CaptureSource: Send` is a supertrait.
        fn takes_send_capture<T: CaptureSource + Send + 'static>(_: T) {}
        let src = FakeCaptureSource::new(CaptureConfig::default()).unwrap();
        takes_send_capture(src);
    }

    // ── Phase 1 tests (unchanged) ─────────────────────────────────────────────

    #[test]
    fn pixel_format_bgra8_display_string() {
        assert_eq!(format!("{}", PixelFormat::Bgra8), "BGRA8");
        assert_eq!(format!("{}", PixelFormat::Rgba8), "RGBA8");
        assert_eq!(format!("{}", PixelFormat::Rgba16F), "RGBA16F");
    }

    #[test]
    fn capture_frame_clone_shares_buffer() {
        let data: Arc<[u8]> = Arc::from(&[0u8; 4][..]);
        let a = CaptureFrame {
            data: data.clone(),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8,
            timestamp: std::time::Duration::ZERO,
        };
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.data, &b.data));
    }

    #[test]
    fn capture_frame_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CaptureFrame>();
    }

    #[test]
    fn capture_frame_debug_is_non_empty() {
        let f = CaptureFrame {
            data: Arc::from(&[0u8; 4][..]),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8,
            timestamp: std::time::Duration::ZERO,
        };
        assert!(!format!("{:?}", f).is_empty());
    }

    #[test]
    fn capture_error_not_supported_display() {
        let e = CaptureError::NotSupported;
        assert!(format!("{e}").to_lowercase().contains("not supported"));
    }

    #[test]
    fn capture_error_monitor_not_found_display() {
        let e = CaptureError::MonitorNotFound("DISPLAY1".to_string());
        assert!(format!("{e}").contains("DISPLAY1"));
    }

    #[test]
    fn capture_error_all_variants_debug_roundtrip() {
        use CaptureError::*;
        let variants: &[CaptureError] = &[
            NotSupported,
            MonitorNotFound("X".into()),
            ItemClosed,
            DeviceLost,
            Backpressure,
            ComInitFailed("c".into()),
            SessionCreateFailed("s".into()),
            Internal("i".into()),
        ];
        for v in variants {
            assert!(!format!("{v:?}").is_empty());
        }
    }

    #[test]
    fn capture_config_default_cursor_is_true() {
        let c = CaptureConfig::default();
        assert!(c.cursor);
        assert!(c.max_fps.is_none());
        assert!(matches!(c.border, BorderPolicy::Auto));
        assert!(matches!(c.monitor, MonitorSelector::Primary));
    }

    #[test]
    fn capture_config_is_debug_and_clone() {
        let c = CaptureConfig::default();
        let _ = format!("{c:?}");
        let _ = c.clone();
    }

    #[test]
    fn monitor_info_shape_and_primary_flag() {
        let m = MonitorInfo {
            id: MonitorId(1),
            label: "DISPLAY1".into(),
            width: 1920,
            height: 1080,
            is_primary: true,
        };
        assert_eq!(m.id, MonitorId(1));
        assert!(m.is_primary);
    }
}
