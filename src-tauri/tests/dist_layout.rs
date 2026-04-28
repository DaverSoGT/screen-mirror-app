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
