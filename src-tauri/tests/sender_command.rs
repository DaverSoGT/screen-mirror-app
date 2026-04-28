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

// ─── B3 arg validation tests ──────────────────────────────────────────────────
// These tests call start_sender_inner or validation helpers directly.
// SenderBuilderProbe is defined here (integration test cannot access stream.rs's
// BuilderProbe which is #[cfg(test)] and in a different crate unit).

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use screen_mirror_lib::commands::sender::{
    BundleError, ChannelLike, SenderBundle, SenderBuilderFn, start_sender_inner,
};

/// Fake channel that captures JSON messages sent via send_raw(0, bytes).
struct FakeJsonChannel {
    messages: Mutex<Vec<String>>,
}

impl FakeJsonChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            messages: Mutex::new(vec![]),
        })
    }

    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

impl ChannelLike for FakeJsonChannel {
    fn send_raw(&self, _discriminant: u8, bytes: Vec<u8>) -> Result<(), String> {
        if let Ok(s) = String::from_utf8(bytes) {
            self.messages.lock().unwrap().push(s);
        }
        Ok(())
    }
}

/// Builder probe: records calls made to it.
struct SenderBuilderProbe {
    calls: Mutex<Vec<(u16, String)>>,
    result: Box<dyn Fn() -> Result<SenderBundle, BundleError> + Send + Sync>,
}

impl SenderBuilderProbe {
    fn new_ok() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(vec![]),
            result: Box::new(|| Ok(SenderBundle::test_stub())),
        })
    }

    fn new_err(err: BundleError) -> Arc<Self> {
        let err = Mutex::new(Some(err));
        Arc::new(Self {
            calls: Mutex::new(vec![]),
            result: Box::new(move || {
                let e = err.lock().unwrap().take().expect("error already consumed");
                Err(e)
            }),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn calls(&self) -> Vec<(u16, String)> {
        self.calls.lock().unwrap().clone()
    }
}

fn make_sender_test_builder(
    probe: Arc<SenderBuilderProbe>,
) -> screen_mirror_lib::commands::sender::SenderBuilderFn {
    Arc::new(move |port: u16, name: String, _stop: Arc<AtomicBool>, _ch: Arc<dyn ChannelLike>| {
        probe.calls.lock().unwrap().push((port, name));
        (probe.result)()
    })
}

/// Invalid service name returns error before builder is called.
#[test]
fn start_sender_invalid_service_name_returns_error() {
    use screen_mirror_lib::commands::sender::StartSenderError;
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();
    let result = start_sender_inner(
        &bridge,
        ch as Arc<dyn ChannelLike>,
        None,
        Some("bogus".to_string()),
    );
    assert!(matches!(result, Err(StartSenderError::InvalidServiceName { .. })));
    assert_eq!(probe.call_count(), 0, "builder must not be called on validation error");
}

/// Privileged port returns InvalidPort error.
#[test]
fn start_sender_privileged_port_returns_invalid_port() {
    use screen_mirror_lib::commands::sender::{StartSenderError, PortRejectReason};
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();
    let result = start_sender_inner(
        &bridge,
        ch as Arc<dyn ChannelLike>,
        Some(80),
        None,
    );
    assert!(
        matches!(result, Err(StartSenderError::InvalidPort { reason: PortRejectReason::Privileged, .. })),
        "expected InvalidPort::Privileged, got: {result:?}"
    );
    assert_eq!(probe.call_count(), 0);
}

/// Port 0 is allowed (ephemeral) for sender (Amendment A).
#[test]
fn start_sender_port_zero_is_allowed() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();
    let result = start_sender_inner(
        &bridge,
        ch as Arc<dyn ChannelLike>,
        Some(0),
        None,
    );
    assert!(result.is_ok(), "port 0 must be accepted: {result:?}");
    assert_eq!(probe.call_count(), 1);
    assert_eq!(probe.calls()[0].0, 0, "builder must receive port 0");
}

/// None udp_port defaults to ephemeral (0). Builder receives port=0.
#[test]
fn start_sender_port_none_defaults_to_ephemeral() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();
    let result = start_sender_inner(
        &bridge,
        ch as Arc<dyn ChannelLike>,
        None,
        None,
    );
    assert!(result.is_ok(), "None port must default to 0: {result:?}");
    assert_eq!(probe.call_count(), 1);
    assert_eq!(probe.calls()[0].0, 0, "builder must receive port 0 when None");
}

/// Double-start returns AlreadyRunning.
#[test]
fn start_sender_already_running_returns_error() {
    use screen_mirror_lib::commands::sender::StartSenderError;
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();

    // First start — should succeed.
    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("first start must succeed");

    // Second start — should return AlreadyRunning.
    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(
        matches!(result, Err(StartSenderError::AlreadyRunning { .. })),
        "expected AlreadyRunning, got: {result:?}"
    );
    assert_eq!(probe.call_count(), 1, "builder must not be called a second time");
}
