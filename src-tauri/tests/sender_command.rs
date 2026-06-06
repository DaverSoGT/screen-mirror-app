// Integration tests for the SenderBridge command surface.
//
// These tests cover R1–R4, R17 via the BuilderFn injection seam.
// ALL tests in this file are cross-platform — no real adapters.
// Windows-only tests must be marked #[ignore] (not yet present in this file).

use screen_mirror_lib::commands::sender::{SenderBridge, SenderStats, StartSenderError};

// B2: Verify the command symbols are importable (compile-level test).
#[allow(unused_imports)]
use screen_mirror_lib::commands::sender::{sender_diagnostics, start_sender, stop_sender};

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
        backend_name: String::new(),
    };
    let s = serde_json::to_string(&stats).unwrap();
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back["running"], false);
    assert_eq!(back["dropped_frames_encoder"], 0);
    assert_eq!(back["backend_name"], "");
}

// ─── B2 command registration assertions ──────────────────────────────────────

/// Compile-level test: start_sender, stop_sender, sender_diagnostics exist as pub items.
/// The #[allow(unused_imports)] use at the top of this file acts as the compile-level gate.
#[test]
fn start_sender_stub_exists() {
    // The import at the top verifies the symbols are accessible.
    // This test body just runs to confirm the binary is built.
    let bridge = SenderBridge::new();
    assert!(
        bridge.session.lock().unwrap().is_none(),
        "bridge must start with no session"
    );
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

use screen_mirror_lib::commands::sender::{
    BundleError, ChannelLike, SenderBundle, start_sender_inner,
};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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
    Arc::new(
        move |port: u16, name: String, _stop: Arc<AtomicBool>, _ch: Arc<dyn ChannelLike>, _attempt: u8| {
            probe.calls.lock().unwrap().push((port, name));
            (probe.result)()
        },
    )
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
    assert!(matches!(
        result,
        Err(StartSenderError::InvalidServiceName { .. })
    ));
    assert_eq!(
        probe.call_count(),
        0,
        "builder must not be called on validation error"
    );
}

/// Privileged port returns InvalidPort error.
#[test]
fn start_sender_privileged_port_returns_invalid_port() {
    use screen_mirror_lib::commands::sender::{PortRejectReason, StartSenderError};
    let probe = SenderBuilderProbe::new_ok();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(
        make_sender_test_builder(probe.clone()),
    );
    let ch = FakeJsonChannel::new();
    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, Some(80), None);
    assert!(
        matches!(
            result,
            Err(StartSenderError::InvalidPort {
                reason: PortRejectReason::Privileged,
                ..
            })
        ),
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
    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, Some(0), None);
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
    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(result.is_ok(), "None port must default to 0: {result:?}");
    assert_eq!(probe.call_count(), 1);
    assert_eq!(
        probe.calls()[0].0,
        0,
        "builder must receive port 0 when None"
    );
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
    assert_eq!(
        probe.call_count(),
        1,
        "builder must not be called a second time"
    );
}

// ─── B4 stop_sender tests ─────────────────────────────────────────────────────

use screen_mirror_lib::commands::sender::stop_sender_session;

/// stop_sender with no session returns ok immediately (idempotent).
#[test]
fn stop_sender_with_no_session_returns_ok() {
    let bridge = SenderBridge::new();
    stop_sender_session(&bridge);
    assert!(bridge.session.lock().unwrap().is_none());
}

/// stop_sender twice does not panic.
#[test]
fn stop_sender_twice_does_not_panic() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    stop_sender_session(&bridge);
    stop_sender_session(&bridge); // second call must not panic
}

/// stop_sender clears current_args after teardown.
#[test]
fn stop_sender_clears_current_args() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    {
        let guard = bridge.current_args.lock().unwrap();
        assert!(guard.is_some(), "current_args must be Some after start");
    }

    stop_sender_session(&bridge);

    {
        let guard = bridge.current_args.lock().unwrap();
        assert!(guard.is_none(), "current_args must be None after stop");
    }
}

/// stop_sender with a session that has drain handles completes without blocking.
#[test]
fn stop_sender_fake_session_drains_handles() {
    use std::time::{Duration, Instant};

    // Build a bundle that has a real (trivial) drain thread.
    let probe = Arc::new(SenderBuilderProbe {
        calls: Mutex::new(vec![]),
        result: Box::new(|| {
            // Spawn a thread that does nothing and exits immediately.
            let h = std::thread::spawn(|| {});
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
                backend_name: "sw_fake".to_string(),
            })
        }),
    });

    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();
    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    let start_time = Instant::now();
    stop_sender_session(&bridge);
    let elapsed = start_time.elapsed();

    assert!(
        elapsed < Duration::from_millis(200),
        "stop_sender must complete quickly (< 200ms), took {elapsed:?}"
    );
}

// ─── B5 sender_diagnostics tests ─────────────────────────────────────────────

use screen_mirror_lib::commands::sender::sender_diagnostics_impl;

/// No session → Err("not running").
#[test]
fn sender_diagnostics_no_session_returns_err_not_running() {
    let bridge = SenderBridge::new();
    let result = sender_diagnostics_impl(&bridge);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "not running");
}

/// Session present → Ok(SenderStats) with correct counter values.
#[test]
fn sender_diagnostics_with_fake_session_returns_stats() {
    use std::sync::atomic::Ordering;

    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();
    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Set counters on the live session.
    {
        let guard = bridge.session.lock().unwrap();
        if let Some(s) = guard.as_ref() {
            s.counters
                .dropped_frames_encoder
                .store(3, Ordering::Relaxed);
        }
    }

    let stats = sender_diagnostics_impl(&bridge).expect("should return stats");
    assert_eq!(stats.dropped_frames_encoder, 3);
    assert!(stats.running);
    // T.D.4 RED→GREEN: backend_name must be "sw_fake" when session uses test stub
    // (SenderBundle::test_stub() sets backend_name = "sw_fake").
    assert_eq!(
        stats.backend_name, "sw_fake",
        "backend_name must equal the test-stub sentinel"
    );
}

/// No session → Err("not running") — R7: backend_name not surfaced on Err path.
///
/// Covers R7: when no session is active, `sender_diagnostics` returns Err,
/// meaning `backend_name` is not surfaced at all (the field lives only in Ok).
#[test]
fn sender_diagnostics_no_session_err_path_omits_backend_name() {
    let bridge = SenderBridge::new();
    let result = sender_diagnostics_impl(&bridge);
    assert!(result.is_err(), "no session must return Err");
    assert_eq!(
        result.unwrap_err(),
        "not running",
        "error message must be 'not running'"
    );
}

/// running field reflects session presence.
#[test]
fn sender_stats_running_field_reflects_session_presence() {
    let bridge = SenderBridge::new();
    let result = sender_diagnostics_impl(&bridge);
    assert!(result.is_err(), "no session = not running");

    let probe = SenderBuilderProbe::new_ok();
    let bridge2 = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();
    start_sender_inner(&bridge2, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");
    let stats = sender_diagnostics_impl(&bridge2).expect("session exists = running");
    assert!(stats.running);
}

// ─── B6 start_sender_inner bring-up tests ────────────────────────────────────

/// Happy path: builder OK, bridge has session, current_args set.
#[test]
fn start_sender_inner_happy_path_sets_current_args() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe.clone()));
    let ch = FakeJsonChannel::new();

    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(result.is_ok(), "expected Ok: {result:?}");
    assert!(bridge.current_args.lock().unwrap().is_some());
    assert!(bridge.session.lock().unwrap().is_some());
    assert_eq!(probe.call_count(), 1);
}

/// Builder receives port=0 when udp_port is None (Amendment A).
#[test]
fn start_sender_inner_builder_receives_port_zero_when_none() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe.clone()));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None).expect("should succeed");

    assert_eq!(probe.calls()[0].0, 0, "builder must receive port=0");
}

/// Builder failure: bridge stays clean.
#[test]
fn start_sender_inner_builder_failure_leaves_bridge_clean() {
    use screen_mirror_lib::commands::sender::StartSenderError;
    let probe = SenderBuilderProbe::new_err(BundleError::Other("boom".to_string()));
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();

    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(
        matches!(result, Err(StartSenderError::BundleBuildFailed(_))),
        "expected BundleBuildFailed: {result:?}"
    );
    assert!(bridge.session.lock().unwrap().is_none());
    assert!(bridge.current_args.lock().unwrap().is_none());
}

/// Builder returns PortInUse → StartSenderError::PortInUse.
#[test]
fn start_sender_inner_port_in_use_error_propagates() {
    use screen_mirror_lib::commands::sender::StartSenderError;
    let probe = SenderBuilderProbe::new_err(BundleError::PortInUse(5004));
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();

    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(
        matches!(result, Err(StartSenderError::PortInUse { port: 5004 })),
        "expected PortInUse(5004): {result:?}"
    );
}

/// Double-start: second call returns AlreadyRunning; builder called only once.
#[test]
fn start_sender_inner_already_running_blocks_second_start() {
    use screen_mirror_lib::commands::sender::StartSenderError;
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe.clone()));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("first start must succeed");

    let result = start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None);
    assert!(
        matches!(result, Err(StartSenderError::AlreadyRunning { .. })),
        "expected AlreadyRunning: {result:?}"
    );
    assert_eq!(
        probe.call_count(),
        1,
        "builder must not be called second time"
    );
}

/// After start_sender_inner, channel receives a Connecting status event.
#[test]
fn start_sender_inner_bring_up_emits_connecting_status() {
    let probe = SenderBuilderProbe::new_ok();
    let bridge = SenderBridge::new_with_builder(make_sender_test_builder(probe));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();

    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    let messages = ch_clone.messages();
    assert!(
        !messages.is_empty(),
        "channel must have received at least one message"
    );

    let has_connecting = messages
        .iter()
        .any(|m| m.contains("\"kind\":\"connecting\""));
    assert!(
        has_connecting,
        "expected connecting status, got: {messages:?}"
    );
}

// ─── B7 signaling drain tests ─────────────────────────────────────────────────

use screen_mirror_lib::commands::sender::{SignalingSenderOps, run_sender_signaling_drain};
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer, SignalingEvent};
use sm_domain::transport::TransportError;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

/// Fake sender that records apply_remote_answer / add_remote_candidate calls.
struct FakeSender {
    answer_calls: Mutex<Vec<SdpAnswer>>,
    candidate_calls: Mutex<Vec<IceCandidate>>,
}

impl FakeSender {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer_calls: Mutex::new(vec![]),
            candidate_calls: Mutex::new(vec![]),
        })
    }
}

impl SignalingSenderOps for FakeSender {
    fn apply_remote_answer(&self, ans: SdpAnswer) -> Result<(), TransportError> {
        self.answer_calls.lock().unwrap().push(ans);
        Ok(())
    }
    fn add_remote_candidate(&self, c: IceCandidate) -> Result<(), TransportError> {
        self.candidate_calls.lock().unwrap().push(c);
        Ok(())
    }
}

/// PeerFound: log + emit Connecting; no offer publish (Amendment B).
#[test]
fn signaling_drain_peer_found_logs_and_emits_connecting() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender: Arc<dyn SignalingSenderOps> = FakeSender::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender, stop_clone, ch_clone, none_slot);
    });

    ev_tx
        .send(SignalingEvent::PeerFound {
            host: "127.0.0.1".to_string(),
            port: 7889,
        })
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let msgs = ch.messages();
    let has_connecting = msgs.iter().any(|m| m.contains("\"kind\":\"connecting\""));
    assert!(has_connecting, "expected connecting event, got: {msgs:?}");
}

/// AnswerReceived calls apply_remote_answer.
#[test]
fn signaling_drain_answer_received_calls_apply_remote_answer() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender = FakeSender::new();
    let fake_sender_ops: Arc<dyn SignalingSenderOps> = fake_sender.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender_ops, stop_clone, ch, none_slot);
    });

    ev_tx
        .send(SignalingEvent::AnswerReceived(SdpAnswer(
            "test-answer".to_string(),
        )))
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let calls = fake_sender.answer_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "apply_remote_answer must be called once");
    assert_eq!(calls[0].0, "test-answer");
}

/// CandidateReceived calls add_remote_candidate.
#[test]
fn signaling_drain_candidate_received_calls_add_remote_candidate() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender = FakeSender::new();
    let fake_sender_ops: Arc<dyn SignalingSenderOps> = fake_sender.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender_ops, stop_clone, ch, none_slot);
    });

    ev_tx
        .send(SignalingEvent::CandidateReceived(IceCandidate(
            "test-cand".to_string(),
        )))
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let calls = fake_sender.candidate_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "add_remote_candidate must be called once");
}

/// OfferReceived is silently ignored.
#[test]
fn signaling_drain_offer_received_is_silently_ignored() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender = FakeSender::new();
    let fake_sender_ops: Arc<dyn SignalingSenderOps> = fake_sender.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender_ops, stop_clone, ch, none_slot);
    });

    ev_tx
        .send(SignalingEvent::OfferReceived(
            SdpOffer("test-offer".to_string()),
            1,
        ))
        .unwrap();

    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    // No method called on FakeSender
    assert!(fake_sender.answer_calls.lock().unwrap().is_empty());
    assert!(fake_sender.candidate_calls.lock().unwrap().is_empty());
}

/// Closed event emits peer_lost and drain exits.
#[test]
fn signaling_drain_closed_emits_peer_lost_and_exits() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender: Arc<dyn SignalingSenderOps> = FakeSender::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender, stop_clone, ch_clone, none_slot);
    });

    ev_tx.send(SignalingEvent::Closed).unwrap();
    drain.join().expect("drain must exit after Closed");

    let msgs = ch.messages();
    let has_peer_lost = msgs.iter().any(|m| m.contains("\"kind\":\"peer_lost\""));
    assert!(has_peer_lost, "expected peer_lost event, got: {msgs:?}");
}

/// Disconnected rx causes drain to exit cleanly.
#[test]
fn signaling_drain_disconnected_rx_exits_cleanly() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender: Arc<dyn SignalingSenderOps> = FakeSender::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender, stop_clone, ch, none_slot);
    });

    drop(ev_tx); // disconnect

    let start_time = std::time::Instant::now();
    drain.join().expect("drain must exit on disconnect");
    assert!(
        start_time.elapsed() < Duration::from_secs(1),
        "drain must exit within 1s on disconnect"
    );
}

/// stop_flag causes drain to exit within 1s.
#[test]
fn signaling_drain_stop_flag_exits_loop() {
    let (_ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(4);
    let fake_sender: Arc<dyn SignalingSenderOps> = FakeSender::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let stop_clone = stop_flag.clone();

    let none_slot = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>,
    ));
    let drain = thread::spawn(move || {
        run_sender_signaling_drain(ev_rx, fake_sender, stop_clone, ch, none_slot);
    });

    stop_flag.store(true, Ordering::Relaxed);

    let start_time = std::time::Instant::now();
    drain.join().expect("drain must exit on stop_flag");
    assert!(
        start_time.elapsed() < Duration::from_millis(600),
        "drain must exit within 600ms on stop_flag"
    );
}

// ─── B8 transport event drain tests ──────────────────────────────────────────

use screen_mirror_lib::commands::sender::{SenderCounters, run_sender_transport_event_drain};
use sm_domain::transport::TransportEvent;

/// IceConnected emits streaming + "Stop streaming" button.
#[test]
fn transport_drain_ice_connected_emits_streaming_and_button() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch_clone, counters);
    });

    ev_tx.send(TransportEvent::IceConnected).unwrap();
    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let msgs = ch.messages();
    assert!(
        msgs.iter().any(|m| m.contains("\"kind\":\"streaming\"")),
        "expected streaming event, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("Stop streaming")),
        "expected Stop streaming button, got: {msgs:?}"
    );
}

/// IceFailed emits peer_lost + Restart button.
#[test]
fn transport_drain_ice_failed_emits_disconnected_and_restart_button() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch_clone, counters);
    });

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let msgs = ch.messages();
    assert!(
        msgs.iter().any(|m| m.contains("\"kind\":\"peer_lost\"")),
        "expected peer_lost event, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("Restart")),
        "expected Restart button, got: {msgs:?}"
    );
}

/// ConnectionLost emits peer_lost + Restart button.
#[test]
fn transport_drain_connection_lost_emits_disconnected_and_restart_button() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch_clone, counters);
    });

    ev_tx
        .send(TransportEvent::ConnectionLost {
            reason: "test-disconnect".to_string(),
        })
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    let msgs = ch.messages();
    assert!(
        msgs.iter().any(|m| m.contains("\"kind\":\"peer_lost\"")),
        "expected peer_lost event, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("Restart")),
        "expected Restart button, got: {msgs:?}"
    );
}

/// KeyframeRequested increments counter; no channel message.
#[test]
fn transport_drain_keyframe_requested_increments_counter() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let counters = Arc::new(SenderCounters::default());
    let counters_clone = counters.clone();
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch_clone, counters_clone);
    });

    ev_tx.send(TransportEvent::KeyframeRequested).unwrap();
    thread::sleep(Duration::from_millis(50));
    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");

    assert_eq!(
        counters.keyframe_requests_received.load(Ordering::Relaxed),
        1,
        "keyframe counter must be 1"
    );
    // No channel messages expected for KeyframeRequested.
    let msgs = ch.messages();
    assert!(
        msgs.is_empty(),
        "no channel messages expected for KeyframeRequested, got: {msgs:?}"
    );
}

/// stop_flag causes drain to exit within 600ms.
#[test]
fn transport_drain_stop_flag_exits() {
    let (_ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch, counters);
    });

    stop_flag.store(true, Ordering::Relaxed);

    let start_time = std::time::Instant::now();
    drain.join().expect("drain must exit on stop_flag");
    assert!(
        start_time.elapsed() < Duration::from_millis(600),
        "drain must exit within 600ms"
    );
}

/// Disconnected rx causes drain to exit cleanly.
#[test]
fn transport_drain_disconnected_rx_exits_cleanly() {
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch, counters);
    });

    drop(ev_tx);

    let start_time = std::time::Instant::now();
    drain.join().expect("drain must exit on disconnect");
    assert!(
        start_time.elapsed() < Duration::from_secs(1),
        "drain must exit within 1s on disconnect"
    );
}

// ─── C1 regression test — production arcs must live until stop_sender_session ─

/// C1 (CRITICAL, verify-report #362): the original `build_production_sender_bundle`
/// dropped capture/encoder/sender/signaling arcs at the end of bundle construction,
/// which on the Windows production path stops the signaling thread before ICE
/// negotiation can complete.
///
/// The fix introduces a `shutdown` closure on `SenderBundle` that takes ownership
/// of the production resources and is invoked by `stop_sender_session`.
///
/// This regression test plants a `DropTracker` into the shutdown closure to
/// assert resources stay alive between `start_sender_inner` and `stop_sender_session`.
#[test]
fn fix_c1_session_keeps_production_arcs_alive_until_stop() {
    use std::sync::atomic::{AtomicUsize, Ordering as O};

    let drop_count = Arc::new(AtomicUsize::new(0));

    struct DropTracker(Arc<AtomicUsize>);
    impl Drop for DropTracker {
        fn drop(&mut self) {
            self.0.fetch_add(1, O::SeqCst);
        }
    }

    let dc = drop_count.clone();
    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder(Arc::new(
        move |_, _, _, _, _| {
            let tracker = DropTracker(dc.clone());
            Ok(SenderBundle {
                drain_handles: vec![],
                shutdown: Some(Box::new(move || {
                    drop(tracker);
                })),
                backend_name: "sw_fake".to_string(),
            })
        },
    ));

    let ch = FakeJsonChannel::new();
    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start_sender_inner must succeed");

    assert_eq!(
        drop_count.load(O::SeqCst),
        0,
        "C1 regressed: production resources dropped before stop_sender_session"
    );

    screen_mirror_lib::commands::sender::stop_sender_session(&bridge);

    assert_eq!(
        drop_count.load(O::SeqCst),
        1,
        "shutdown closure must drop production resources during stop_sender_session"
    );
}

// ─── T6.3/T6.4 (streaming-emit-on-ice-connect): drain-level gate tests ────────
//
// These tests verify that the drain thread does NOT emit a "streaming" event
// to FakeJsonChannel before the transport emits TransportEvent::IceConnected.
//
// The test approach:
// - Manually wire a SyncSender<TransportEvent> to a drain thread
// - Assert no "streaming" in FakeJsonChannel before injecting IceConnected
// - Inject IceConnected, assert "streaming" arrives
//
// This directly replicates what FakeVideoSender::inject_ice_connected_for_test()
// would do (send TransportEvent::IceConnected on the event channel), without
// needing access to the private FakeVideoSender test struct in sm-domain.

/// T6.3 (TST-S-1, AC-6) — No "streaming" event fires before IceConnected.
/// After IceConnected, "streaming" arrives within 100ms.
#[test]
fn streaming_event_does_not_fire_before_ice_connected() {
    // Wire a drain thread directly with a manually-controlled TransportEvent channel.
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone = ch.clone();
    let counters = Arc::new(SenderCounters::default());
    let stop_clone = stop_flag.clone();

    let drain = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx, stop_clone, ch_clone, counters);
    });

    // Phase 1: Do NOT send IceConnected. Wait 50ms and assert no "streaming".
    thread::sleep(Duration::from_millis(50));
    let msgs_before = ch.messages();
    assert!(
        !msgs_before
            .iter()
            .any(|m| m.contains("\"kind\":\"streaming\"")),
        "streaming must NOT fire before IceConnected; got: {msgs_before:?}"
    );

    // Phase 2: Inject IceConnected (mirrors inject_ice_connected_for_test behavior).
    ev_tx.send(TransportEvent::IceConnected).unwrap();
    thread::sleep(Duration::from_millis(100));

    let msgs_after = ch.messages();
    assert!(
        msgs_after
            .iter()
            .any(|m| m.contains("\"kind\":\"streaming\"")),
        "streaming must fire after IceConnected; got: {msgs_after:?}"
    );

    stop_flag.store(true, Ordering::Relaxed);
    drop(ev_tx);
    drain.join().expect("drain must exit");
}

/// T6.4 (TST-S-2, AC-5, AC-6) — After a rebuild (simulated by stopping the drain
/// and starting a fresh one with a new event channel), the second generation MUST
/// NOT emit "streaming" until IceConnected arrives on the new channel.
#[test]
fn rebuild_streaming_event_fires_only_after_new_ice_connected() {
    // ── Generation 1 ─────────────────────────────────────────────────────────────
    let (ev_tx1, ev_rx1) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag1 = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_clone1 = ch.clone();
    let counters1 = Arc::new(SenderCounters::default());
    let stop_clone1 = stop_flag1.clone();

    let drain1 = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx1, stop_clone1, ch_clone1, counters1);
    });

    // Gen 1 gets IceConnected → streaming fires.
    ev_tx1.send(TransportEvent::IceConnected).unwrap();
    thread::sleep(Duration::from_millis(100));

    let msgs_gen1 = ch.messages();
    assert!(
        msgs_gen1
            .iter()
            .any(|m| m.contains("\"kind\":\"streaming\"")),
        "gen1 streaming must fire after IceConnected; got: {msgs_gen1:?}"
    );

    // Stop gen 1 (simulate rebuild teardown).
    stop_flag1.store(true, Ordering::Relaxed);
    drop(ev_tx1);
    drain1.join().expect("gen1 drain must exit");

    // ── Generation 2 ─────────────────────────────────────────────────────────────
    // Fresh drain thread with a new event channel — simulates what build_production_sender_bundle
    // does for each rebuild generation.
    let (ev_tx2, ev_rx2) = std::sync::mpsc::sync_channel::<TransportEvent>(4);
    let stop_flag2 = Arc::new(AtomicBool::new(false));
    let ch_clone2 = ch.clone();
    let counters2 = Arc::new(SenderCounters::default());
    let stop_clone2 = stop_flag2.clone();

    // Record message count before gen2 starts.
    let msgs_count_before_gen2 = ch.messages().len();

    let drain2 = thread::spawn(move || {
        run_sender_transport_event_drain(ev_rx2, stop_clone2, ch_clone2, counters2);
    });

    // Gen 2: Do NOT inject IceConnected. Assert no NEW streaming event.
    thread::sleep(Duration::from_millis(50));
    let msgs_gen2_before = ch.messages();
    assert!(
        !msgs_gen2_before[msgs_count_before_gen2..]
            .iter()
            .any(|m| m.contains("\"kind\":\"streaming\"")),
        "gen2 must NOT emit streaming before its own IceConnected; \
         new messages: {:?}",
        &msgs_gen2_before[msgs_count_before_gen2..]
    );

    // Now inject IceConnected on gen2 channel.
    ev_tx2.send(TransportEvent::IceConnected).unwrap();
    thread::sleep(Duration::from_millis(100));

    let msgs_gen2_after = ch.messages();
    assert!(
        msgs_gen2_after[msgs_count_before_gen2..]
            .iter()
            .any(|m| m.contains("\"kind\":\"streaming\"")),
        "gen2 streaming must fire after its own IceConnected; \
         new messages: {:?}",
        &msgs_gen2_after[msgs_count_before_gen2..]
    );

    stop_flag2.store(true, Ordering::Relaxed);
    drop(ev_tx2);
    drain2.join().expect("gen2 drain must exit");
}
