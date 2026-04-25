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
