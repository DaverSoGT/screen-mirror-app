// dist_layout — static integration tests for dist/ file layout.
//
// These tests verify the structural invariants of the dist/ directory after
// the dual-mode-shell change is applied:
//   R1  — dist/ contains exactly index.html, viewer.html, sender.html (+ mse-client.js)
//   R2  — index.html and sender.html do NOT reference mse-client.js
//   R9  — mse-client.js is referenced ONLY from viewer.html
//
// Uses CARGO_MANIFEST_DIR to resolve dist/ portably (no string-concatenated paths).
// Tests are #[test], not #[ignore] — these are fast static-file checks.

use std::fs;
use std::path::Path;

fn dist_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist")
}

/// R1 — dist/ must contain exactly three HTML files: index.html, viewer.html, sender.html.
#[test]
fn exact_four_files() {
    let dist = dist_dir();
    let html_files: Vec<String> = fs::read_dir(&dist)
        .unwrap_or_else(|e| panic!("cannot read dist/ at {}: {}", dist.display(), e))
        .filter_map(|entry| {
            let entry = entry.expect("dir entry error");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".html") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    let mut sorted = html_files.clone();
    sorted.sort();

    assert_eq!(
        sorted,
        vec![
            "index.html".to_string(),
            "sender.html".to_string(),
            "viewer.html".to_string(),
        ],
        "dist/ must contain exactly index.html, sender.html, viewer.html — found: {:?}",
        sorted
    );
}

/// R2 + R9 — mse-client.js MUST appear only in viewer.html.
/// index.html and sender.html MUST NOT reference mse-client.js.
#[test]
fn mse_client_referenced_only_from_viewer() {
    let dist = dist_dir();

    let html_files: Vec<(String, String)> = fs::read_dir(&dist)
        .unwrap_or_else(|e| panic!("cannot read dist/ at {}: {}", dist.display(), e))
        .filter_map(|entry| {
            let entry = entry.expect("dir entry error");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".html") {
                let path = dist.join(&name);
                let content = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
                Some((name, content))
            } else {
                None
            }
        })
        .collect();

    // Exactly viewer.html must reference mse-client.js; no other .html file may.
    let referencing: Vec<&str> = html_files
        .iter()
        .filter(|(_, content)| content.contains("mse-client.js"))
        .map(|(name, _)| name.as_str())
        .collect();

    assert_eq!(
        referencing,
        vec!["viewer.html"],
        "mse-client.js must be referenced ONLY from viewer.html — found in: {:?}",
        referencing
    );
}

// ─── B9 sender.html shape assertions (R15) ────────────────────────────────────

fn read_sender_html() -> String {
    let path = dist_dir().join("sender.html");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read sender.html: {}", e))
}

/// sender.html must not contain disabled config inputs.
#[test]
fn sender_html_has_no_disabled_config_inputs() {
    let content = read_sender_html();
    assert!(
        !content.contains("id=\"monitor\""),
        "sender.html must not contain #monitor input"
    );
    assert!(
        !content.contains("id=\"fps\""),
        "sender.html must not contain #fps input"
    );
    assert!(
        !content.contains("id=\"bitrate\""),
        "sender.html must not contain #bitrate input"
    );
    assert!(
        !content.contains("disabled"),
        "sender.html must not contain any disabled attributes"
    );
}

/// sender.html must have #start button, #status div, and #error div.
#[test]
fn sender_html_has_start_button_and_status_div() {
    let content = read_sender_html();
    assert!(
        content.contains("id=\"start\""),
        "sender.html must contain #start button"
    );
    assert!(
        content.contains("id=\"status\""),
        "sender.html must contain #status div"
    );
    assert!(
        content.contains("id=\"error\""),
        "sender.html must contain #error div"
    );
}

/// sender.html must have #change-mode link and clear sm.lastMode.
#[test]
fn sender_html_has_change_mode_link() {
    let content = read_sender_html();
    assert!(
        content.contains("id=\"change-mode\""),
        "sender.html must contain #change-mode link"
    );
    assert!(
        content.contains("sm.lastMode"),
        "sender.html must reference sm.lastMode"
    );
}

/// sender.html must reference sender.js.
#[test]
fn sender_html_references_sender_js() {
    let content = read_sender_html();
    assert!(
        content.contains("sender.js"),
        "sender.html must reference sender.js"
    );
}

// ─── CRITICAL-1: Retry/Cancel button elements (AC-7, AC-8, AC-9) ─────────────

/// CRITICAL-1: sender.html MUST have id="retry" button element.
///
/// sender.js uses `document.getElementById("retry")` to show/hide the Retry
/// button on dead events. Without this element the button never appears and
/// retry_session is never invoked. Spec §5.4.
#[test]
fn sender_html_has_retry_button() {
    let content = read_sender_html();
    assert!(
        content.contains("id=\"retry\""),
        "sender.html must contain a button with id=\"retry\" (spec §5.4, AC-7)"
    );
}

/// CRITICAL-1: sender.html MUST have id="cancel" button element.
///
/// sender.js uses `document.getElementById("cancel")` to show/hide the Cancel
/// button on dead events. Without this element cancel/stop_sender is never
/// callable. Spec §5.4.
#[test]
fn sender_html_has_cancel_button() {
    let content = read_sender_html();
    assert!(
        content.contains("id=\"cancel\""),
        "sender.html must contain a button with id=\"cancel\" (spec §5.4, AC-9)"
    );
}

/// CRITICAL-1: viewer.html MUST have a reconnecting-overlay element.
///
/// mse-client.js handleStatus("reconnecting") should show the overlay
/// so the user sees feedback during MSE teardown. Spec §5.4.
#[test]
fn viewer_html_has_reconnecting_overlay() {
    let dist = dist_dir();
    let path = dist.join("viewer.html");
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read viewer.html: {}", e));
    assert!(
        content.contains("id=\"reconnecting-overlay\""),
        "viewer.html must contain id=\"reconnecting-overlay\" element (spec §5.4)"
    );
}

/// CRITICAL-1: viewer.html MUST have a dead-session modal with receiver-retry button.
///
/// The mse-client.js handleStatus("dead") should show this modal. Spec §5.4.
#[test]
fn viewer_html_has_dead_modal_with_retry_and_cancel() {
    let dist = dist_dir();
    let path = dist.join("viewer.html");
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read viewer.html: {}", e));
    assert!(
        content.contains("id=\"dead-modal\""),
        "viewer.html must contain id=\"dead-modal\" element (spec §5.4)"
    );
    assert!(
        content.contains("id=\"receiver-retry\""),
        "viewer.html must contain id=\"receiver-retry\" button (spec §5.4)"
    );
    assert!(
        content.contains("id=\"receiver-cancel\""),
        "viewer.html must contain id=\"receiver-cancel\" button (spec §5.4)"
    );
}

// ─── B11-S5 regression: mux thread MUST parse dimensions from SPS ─────────

/// B11-S5 — `src-tauri/src/commands/stream.rs` MUST NOT hardcode the
/// muxer dimensions. The `Mp4Muxer::new(...)` call site for the init
/// segment must derive width/height from the parsed SPS, otherwise the
/// `tkhd` and `avc1` boxes contain dimensions that disagree with the SPS
/// embedded in `avcC` and Chromium MSE rejects the init segment by
/// closing the MediaSource — exactly the same surface symptom as the
/// codec mismatch (B11-S4) but caused by dimensions instead.
///
/// The grep guard rejects the literal pattern that the predecessor
/// shipped. A passing alternative MUST go through `parse_sps`.
#[test]
fn stream_rs_mux_thread_does_not_hardcode_1920x1080_b11_s5() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("commands")
        .join("stream.rs");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read stream.rs at {}: {}", path.display(), e));

    assert!(
        !content.contains("Mp4Muxer::new(1920, 1080, 30, 1)"),
        "stream.rs must not hardcode Mp4Muxer::new(1920, 1080, 30, 1); dimensions must be parsed from the SPS via avcc::parse_sps"
    );
    assert!(
        content.contains("parse_sps"),
        "stream.rs must call avcc::parse_sps to derive init-segment dimensions"
    );
}

// ─── B11-S4 regression: codec string derived from init segment ────────────

/// B11-S4 — mse-client.js MUST derive the codec string from the init
/// segment's avcC box rather than hardcoding `avc1.42E01E` (Baseline 3.0).
/// Hardcoding broke MSE on streams above 720p30 because the avcC level
/// (e.g. 4.0 for 1080p) did not match the codec string, which Chromium
/// surfaces by closing the MediaSource and removing the SourceBuffer
/// mid-append. The fix scans for the `avcC` box and synthesises
/// `avc1.<profile><compat><level>` at runtime.
#[test]
fn mse_client_derives_codec_from_init_segment_b11_s4() {
    let dist = dist_dir();
    let path = dist.join("mse-client.js");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read mse-client.js at {}: {}", path.display(), e));

    assert!(
        content.contains("deriveCodecFromInitSegment"),
        "mse-client.js must define deriveCodecFromInitSegment to parse the avcC box"
    );
    // The function must search for the four-byte ASCII tag "avcC".
    assert!(
        content.contains("0x61")
            && content.contains("0x76")
            && content.contains("0x63")
            && content.contains("0x43"),
        "deriveCodecFromInitSegment must scan for the 'avcC' ASCII tag (0x61 0x76 0x63 0x43)"
    );
    // addSourceBuffer must be called AFTER the init segment arrives, not at
    // sourceopen time — this is what guarantees the codec string matches.
    assert!(
        content.contains("ms.addSourceBuffer(derived)"),
        "addSourceBuffer must use the codec derived from the init segment"
    );
}
