// Static-grep regression guards for the supervisor-eager-cleanup refactor.
//
// SC-CLEANUP-1: `sup_tx_eager` must be absent from sender.rs after cleanup.
// SC-CLEANUP-2: `_relic_rx_eager` must be absent from sender.rs after cleanup.
// SC-CLEANUP-3: post-cleanup invariant comment marker must be present in sender.rs.
//
// These tests are RED on master 4eb6842 (symbols still exist) and GREEN after WU-2.
// They act as permanent CI regression guards: any future PR that re-introduces the
// obsolete eager pair will fail nextest loudly.

/// SC-CLEANUP-1 — REQ-CLEANUP-1: `sup_tx_eager` must not appear in sender.rs after cleanup.
/// RED on master 4eb6842 (symbol still exists). GREEN after WU-2.
#[test]
fn sc_cleanup_1_no_eager_pair_symbols() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/sender.rs"),
    )
    .expect("sender.rs must be readable");
    assert!(
        !src.contains("sup_tx_eager"),
        "REQ-CLEANUP-1: sup_tx_eager must not appear in sender.rs after cleanup"
    );
    assert!(
        !src.contains("_relic_rx_eager"),
        "REQ-CLEANUP-1: _relic_rx_eager must not appear in sender.rs after cleanup"
    );
}

/// SC-CLEANUP-2 — REQ-CLEANUP-2: `set_supervisor_signal_tx` must not be called at bundle-build
/// time. At most one call site may exist, located exclusively inside `enter_supervisor_mode`.
/// RED on master 4eb6842 (call inside `build_production_sender_bundle`). GREEN after WU-2.
#[test]
fn sc_cleanup_2_no_pre_supervisor_set_supervisor_signal_tx() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/sender.rs"),
    )
    .expect("sender.rs must be readable");

    // Count total occurrences of the call site.
    let call_count = src.matches("signaling.set_supervisor_signal_tx(").count();

    // There must be at most one occurrence globally, and it must be inside
    // enter_supervisor_mode — not inside build_production_sender_bundle.
    assert!(
        call_count <= 1,
        "REQ-CLEANUP-2: signaling.set_supervisor_signal_tx( must appear at most once in sender.rs \
         (found {call_count} occurrences; expected ≤1 — exclusively inside enter_supervisor_mode)"
    );

    // If exactly one occurrence exists, verify it is inside enter_supervisor_mode by
    // checking that the slice between "fn enter_supervisor_mode(" and the next "fn "
    // contains the call.
    if call_count == 1 {
        let enter_marker = "fn enter_supervisor_mode(";
        let enter_pos = src
            .find(enter_marker)
            .expect("enter_supervisor_mode must be present in sender.rs");
        // Find the next "fn " after the supervisor function start to bound the search region.
        let region_end = src[enter_pos + enter_marker.len()..]
            .find("\nfn ")
            .map(|rel| enter_pos + enter_marker.len() + rel)
            .unwrap_or(src.len());
        let supervisor_body = &src[enter_pos..region_end];
        assert!(
            supervisor_body.contains("signaling.set_supervisor_signal_tx("),
            "REQ-CLEANUP-2: the single set_supervisor_signal_tx call must be inside \
             enter_supervisor_mode, not in build_production_sender_bundle"
        );
    }
}

/// SC-CLEANUP-3 — REQ-CLEANUP-3 + NFR-COMMENT-FIDELITY: post-cleanup invariant comment
/// marker must be present. The exact phrase `"bridge_supervisor_signal_tx starts None"` is
/// required by D-CLEAN-5 and used as the SC-CLEANUP-3 marker in tasks #1480.
/// RED on master 4eb6842 (stale comment still present). GREEN after WU-2.
#[test]
fn sc_cleanup_3_no_pre_supervisor_bridge_arc_write() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/sender.rs"),
    )
    .expect("sender.rs must be readable");

    // Verify the new invariant comment is present (D-CLEAN-5 marker).
    assert!(
        src.contains("bridge_supervisor_signal_tx starts None"),
        "NFR-COMMENT-FIDELITY: post-cleanup invariant comment must be present in sender.rs \
         (expected marker: \"bridge_supervisor_signal_tx starts None\")"
    );

    // Verify no bridge Arc write exists outside enter_supervisor_mode.
    let write_pattern = "*bridge_supervisor_signal_tx.lock().unwrap() = Some(";
    let write_count = src.matches(write_pattern).count();

    assert!(
        write_count <= 1,
        "REQ-CLEANUP-3: bridge_supervisor_signal_tx lock-write must appear at most once \
         (found {write_count} occurrences; expected ≤1 — exclusively inside enter_supervisor_mode)"
    );

    if write_count == 1 {
        let enter_marker = "fn enter_supervisor_mode(";
        let enter_pos = src
            .find(enter_marker)
            .expect("enter_supervisor_mode must be present in sender.rs");
        let region_end = src[enter_pos + enter_marker.len()..]
            .find("\nfn ")
            .map(|rel| enter_pos + enter_marker.len() + rel)
            .unwrap_or(src.len());
        let supervisor_body = &src[enter_pos..region_end];
        assert!(
            supervisor_body.contains(write_pattern),
            "REQ-CLEANUP-3: the single bridge Arc write must be inside enter_supervisor_mode, \
             not in build_production_sender_bundle"
        );
    }
}
