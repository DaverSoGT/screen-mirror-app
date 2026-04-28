// Integration tests for the SenderBridge command surface.
//
// These tests cover R1–R4, R17 via the BuilderFn injection seam.
// ALL tests in this file are cross-platform — no real adapters.
// Windows-only tests must be marked #[ignore] (not yet present in this file).

use screen_mirror_lib::commands::sender::{
    SenderBridge, SenderStats, StartSenderError,
};

// B2: Verify the command symbols are importable (compile-level test).
#[allow(unused_imports)]
use screen_mirror_lib::commands::sender::{start_sender, stop_sender, sender_diagnostics};

// ─── B1 type assertions ────────────────────────────────────────────────────────

/// Compile-time assertion: SenderBridge is Send + Sync + 'static.
/// Mirrors stream.rs test_bridge_new_with_builder_stores_builder.
#[test]
fn sender_bridge_is_send_sync_static() {
    fn _assert<T: Send + Sync + 'static>() {}
    _assert::<SenderBridge>();
}

/// SenderBridge::new() must have no active session.
#[test]
fn sender_bridge_new_has_no_session() {
    let bridge = SenderBridge::new();
    assert!(bridge.session.lock().unwrap().is_none());
}

/// StartSenderError::AlreadyRunning serializes with kind = "AlreadyRunning".
#[test]
fn start_sender_error_serializes_already_running() {
    let err = StartSenderError::AlreadyRunning {
        udp_port: 7889,
        service_name: "test".to_string(),
    };
    let s = serde_json::to_string(&err).unwrap();
    assert!(
        s.contains("\"kind\":\"AlreadyRunning\""),
        "expected kind=AlreadyRunning in: {s}"
    );
}

/// StartSenderError::PortInUse serializes with kind + data.
#[test]
fn start_sender_error_serializes_port_in_use() {
    let err = StartSenderError::PortInUse { port: 5004 };
    let s = serde_json::to_string(&err).unwrap();
    assert!(
        s.contains("\"kind\":\"PortInUse\""),
        "expected kind=PortInUse in: {s}"
    );
    assert!(s.contains("5004"), "expected port 5004 in: {s}");
}

/// SenderStats round-trips through serde_json.
#[test]
fn sender_stats_serializes_correctly() {
    let stats = SenderStats {
        dropped_frames_encoder: 0,
        dropped_frames_transport: 0,
        keyframe_requests_received: 0,
        running: false,
    };
    let s = serde_json::to_string(&stats).unwrap();
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back["running"], false);
    assert_eq!(back["dropped_frames_encoder"], 0);
}

// ─── B2 command registration assertions ──────────────────────────────────────

/// Compile-level test: start_sender, stop_sender, sender_diagnostics exist as pub items.
/// The #[allow(unused_imports)] use at the top of this file acts as the compile-level gate.
#[test]
fn start_sender_stub_exists() {
    // The import at the top verifies the symbols are accessible.
    // This test body just runs to confirm the binary is built.
    let _bridge = SenderBridge::new();
    assert!(true);
}

/// SenderBridge and StreamBridge can coexist in the same scope.
#[test]
fn sender_bridge_managed_separately_from_stream_bridge() {
    use screen_mirror_lib::commands::stream::StreamBridge;
    let sender = SenderBridge::new();
    let stream = StreamBridge::new();
    assert!(sender.session.lock().unwrap().is_none());
    assert!(!stream.is_running());
}
