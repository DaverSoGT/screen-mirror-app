//! Integration tests for the Windows capture adapter.
//!
//! All tests in this file are `#[ignore]` and gated to Windows. They require an interactive
//! desktop session with Windows Graphics Capture (WGC) support. Run with:
//!
//! ```sh
//! cargo nextest run -p sm-infra --run-ignored only
//! ```
//!
//! Each test begins with a runtime `IsSupported` guard so the test exits cleanly on hosts where
//! WGC is not available (e.g. Windows 10 < 1903, Server Core, headless CI).

#![cfg(target_os = "windows")]

use sm_domain::{CaptureConfig, CaptureError, CaptureSource, MonitorSelector};

#[cfg(target_os = "windows")]
use sm_infra::capture::WindowsCaptureSource;

// ---------------------------------------------------------------------------
// Task 5.1 — IsSupported guard / enumerate_monitors smoke test
// ---------------------------------------------------------------------------

/// Smoke test: enumerate monitors returns at least one entry and exactly one primary.
///
/// This test is `#[ignore]` and requires an interactive desktop with WGC support.
/// The `IsSupported` guard causes it to exit cleanly on unsupported hosts without failing.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_enumerate_monitors_returns_at_least_one() {
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    let monitors = WindowsCaptureSource::enumerate_monitors()
        .expect("enumerate_monitors must not fail on a system with active displays");

    assert!(!monitors.is_empty(), "expected at least one monitor");

    let primary_count = monitors.iter().filter(|m| m.is_primary).count();
    assert_eq!(
        primary_count, 1,
        "exactly one monitor must be marked as primary"
    );

    for m in &monitors {
        assert!(!m.label.is_empty(), "monitor label must not be empty");
    }
}

// ---------------------------------------------------------------------------
// Task 5.3 — new() with bad selector → CaptureError::MonitorNotFound
// ---------------------------------------------------------------------------

/// Constructing a capture source with an out-of-range index selector must return
/// `CaptureError::MonitorNotFound`.
///
/// This test is `#[ignore]` and requires an interactive desktop with WGC support.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_new_bad_index_returns_monitor_not_found() {
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    let config = CaptureConfig {
        monitor: MonitorSelector::ByIndex(9999),
        ..CaptureConfig::default()
    };

    let result = WindowsCaptureSource::new(config);
    match result {
        Err(CaptureError::MonitorNotFound(_)) => { /* expected */ }
        other => panic!("expected MonitorNotFound, got: {other:?}"),
    }
}

/// Constructing a capture source with a valid config (primary monitor) must return `Ok`.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_new_primary_returns_ok() {
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    let config = CaptureConfig::default();
    let result = WindowsCaptureSource::new(config);
    assert!(
        result.is_ok(),
        "WindowsCaptureSource::new with primary monitor must succeed: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 5.5 — frame delivery: start → receive ≥3 frames → stop
// ---------------------------------------------------------------------------

/// End-to-end frame delivery test (R11, R5, R13.4).
///
/// Starts capture on the primary monitor using a channel of capacity
/// `CAPTURE_CHANNEL_CAPACITY` (R11.1). Waits up to 10 seconds to receive
/// at least 3 frames. Validates each frame has non-zero dimensions and the
/// correct pixel format. Then calls `stop()` and asserts `Ok(())`.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_delivers_at_least_3_frames() {
    use sm_infra::capture::CAPTURE_CHANNEL_CAPACITY;
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    let (tx, rx) = std::sync::mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY);

    let mut source = WindowsCaptureSource::new(CaptureConfig::default())
        .expect("WindowsCaptureSource::new must succeed");

    source.start(tx).expect("start must succeed");

    let timeout = std::time::Duration::from_secs(10);
    let deadline = std::time::Instant::now() + timeout;
    let mut frames = Vec::new();

    while frames.len() < 3 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(frame) => frames.push(frame),
            Err(_) => break,
        }
    }

    source.stop().expect("stop must return Ok(())");

    assert!(
        frames.len() >= 3,
        "expected at least 3 frames within 10 s, got {}",
        frames.len()
    );

    use sm_domain::PixelFormat;
    use sm_domain::encode::FramePayload;
    for (i, payload) in frames.iter().enumerate() {
        // No GPU hand-off is wired in this test, so the capture source always emits
        // the CPU-staged variant.
        let FramePayload::Cpu(frame) = payload else {
            panic!("frame {i} must be the Cpu variant (no GPU hand-off wired)");
        };
        assert!(frame.width > 0, "frame {i} width must be > 0");
        assert!(frame.height > 0, "frame {i} height must be > 0");
        assert_eq!(
            frame.format,
            PixelFormat::Bgra8,
            "frame {i} format must be Bgra8"
        );
    }
}

// ---------------------------------------------------------------------------
// Task 5.7 — stop() idempotency: calling stop twice must not panic and
//            must return Ok(()) both times (AC #13)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Task 7.1 / Phase 7 — dropped_frames counter: drop-newest backpressure
// ---------------------------------------------------------------------------

/// Backpressure test: start capture with a tiny channel (capacity 1) and a
/// consumer that pauses 500 ms before draining. After the pause, `dropped_frames()`
/// must be > 0 (R11.2, R11.3).
///
/// This test is `#[ignore]` and requires an interactive desktop with WGC support.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_drops_frames_when_consumer_slow() {
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    // Capacity 1 means the second frame will cause a drop.
    let (tx, rx) = std::sync::mpsc::sync_channel::<sm_domain::encode::FramePayload>(1);

    let mut source = WindowsCaptureSource::new(CaptureConfig::default()).expect("new must succeed");

    source.start(tx).expect("start must succeed");

    // Give the WGC thread time to produce several frames (at 60 fps → ~10 frames in 200 ms).
    // The consumer does NOT read from rx during this window — so frames pile up and drop.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let drops = source.dropped_frames();
    source.stop().expect("stop must succeed");

    // Drain the remaining buffered frame(s).
    while rx.try_recv().is_ok() {}

    assert!(
        drops > 0,
        "expected at least 1 dropped frame with capacity-1 channel and slow consumer, \
         got 0 — did WGC not produce any frames?"
    );
}

// ---------------------------------------------------------------------------
// Task 5.7 — stop() idempotency: calling stop twice must not panic and
//            must return Ok(()) both times (AC #13)
// ---------------------------------------------------------------------------

/// Calling `stop()` on an already-stopped capture source must be idempotent:
/// both calls must return `Ok(())` and neither must panic.
#[test]
#[ignore = "requires interactive desktop with WGC support"]
fn windows_capture_stop_is_idempotent() {
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;

    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        eprintln!("SKIP: Windows Graphics Capture not supported on this host");
        return;
    }

    let (tx, _rx) = std::sync::mpsc::sync_channel::<sm_domain::encode::FramePayload>(4);

    let mut source = WindowsCaptureSource::new(CaptureConfig::default()).expect("new must succeed");

    source.start(tx).expect("start must succeed");

    let first = source.stop();
    assert!(
        first.is_ok(),
        "first stop() must return Ok(()), got: {first:?}"
    );

    let second = source.stop();
    assert!(
        second.is_ok(),
        "second stop() must return Ok(()), got: {second:?}"
    );
}
