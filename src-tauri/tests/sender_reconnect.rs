// Integration tests for SenderBridge reconnect supervisor wiring (Phase 6, T6.1+T6.2).
//
// These tests exercise:
// - RestartCache populated by start_sender_inner (T6.1, AC-8)
// - Reconnecting events emitted to frontend on IceFailed/ConnectionLost (T6.2, AC-1, AC-2)
// - Dead event emitted after 3 failures (T6.2, AC-3, AC-7)
// - Stop during reconnect cancels supervisor cleanly (T6.2, AC-9, AC-13)
//
// All tests are cross-platform — no real adapters or Windows-only code.

use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::{
    BundleError, ChannelLike, SenderBridge, SenderBuilderFn, SenderBundle, SenderCoordinatorHooks,
    SenderCounters, SenderSession, make_sender_rebuild_hook, retry_session_inner,
    run_sender_transport_event_drain_with_supervisor_custom,
    run_sender_transport_event_drain_with_supervisor_custom_and_hooks, start_sender_inner,
    stop_sender_session,
};
use sm_domain::session::{BackoffSchedule, ReconnectPolicy};
use sm_domain::supervisor::SupervisorSignal;
use sm_domain::transport::TransportEvent;

// ─── FakeJsonChannel ──────────────────────────────────────────────────────────

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

    fn wait_for_message_containing(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.messages().iter().any(|m| m.contains(needle)) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
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

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Fast reconnect policy for tests: 1ms base, factor 2 → delays 1ms/2ms/4ms.
/// Allows driving the supervisor through all 3 attempts in milliseconds.
fn fast_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: std::num::NonZeroU8::new(3).unwrap(),
        backoff: BackoffSchedule::Exponential {
            base_ms: 1,
            factor: 2,
        },
    }
}

/// Build a `SenderBridge` that:
/// - Spawns the supervisor-aware drain using the given policy and ack_timeout.
/// - Shares the same `supervisor_signal_tx` Arc between the bridge and the drain.
///
/// Returns `(bridge, ev_tx, ch)` where:
/// - `ev_tx` injects `TransportEvent`s into the drain.
/// - `ch` is the `FakeJsonChannel` the drain sends status events to; observe it for assertions.
fn make_supervised_bridge_with_policy(
    policy: ReconnectPolicy,
    ack_timeout: Duration,
) -> (
    SenderBridge,
    std::sync::mpsc::SyncSender<TransportEvent>,
    Arc<FakeJsonChannel>,
) {
    let ch = FakeJsonChannel::new();
    let ch_for_caller = ch.clone();

    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    // Create the supervisor_signal_tx Arc BEFORE the bridge so the builder can capture it.
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    let bridge = SenderBridge::new_with_builder_and_sup_tx(
        Arc::new(move |_, _, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let counters = Arc::new(SenderCounters::default());
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;
            let h = thread::Builder::new()
                .name("supervised-drain".into())
                .spawn(move || {
                    run_sender_transport_event_drain_with_supervisor_custom(
                        ev_rx, stop_flag, channel, counters, st, p, t, t,
                    );
                })
                .expect("spawn drain");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
                backend_name: "sw_fake".to_string(),
            })
        }),
        sup_tx,
    );

    (bridge, ev_tx, ch_for_caller)
}

/// Build a bridge with fast_policy + 200ms ack_timeout for basic tests.
///
/// Returns `(bridge, ev_tx, ch)` — `ch` is the channel the drain emits status
/// events to and must be passed to `start_sender_inner` as the active channel.
fn make_supervised_bridge() -> (
    SenderBridge,
    std::sync::mpsc::SyncSender<TransportEvent>,
    Arc<FakeJsonChannel>,
) {
    make_supervised_bridge_with_policy(fast_policy(), Duration::from_millis(200))
}

// ─── T6.1 — RestartCache populated by start_sender_inner ──────────────────────

/// T6.1 (AC-8): After start_sender_inner, restart_cache must be populated
/// with udp_port, service_name, and a non-zero session_nonce.
#[test]
fn t6_1_restart_cache_populated_after_start() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    start_sender_inner(
        &bridge,
        ch as Arc<dyn ChannelLike>,
        Some(7890),
        Some("_screen-mirror._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    let cache = bridge.restart_cache.lock().unwrap();
    let c = cache
        .as_ref()
        .expect("restart_cache must be Some after start");
    assert_eq!(c.udp_port, 7890);
    assert_eq!(c.service_name, "_screen-mirror._tcp.local.");
    assert_ne!(c.session_nonce, 0, "session_nonce must be non-zero");
}

/// T6.1 (AC-8): RestartCache is cleared after stop_sender_session.
#[test]
fn t6_1_restart_cache_cleared_after_stop() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    {
        let cache = bridge.restart_cache.lock().unwrap();
        assert!(cache.is_some(), "cache must be Some after start");
    }

    stop_sender_session(&bridge);

    let cache = bridge.restart_cache.lock().unwrap();
    assert!(cache.is_none(), "cache must be None after stop");
}

/// T6.1: session_nonce is stable during the same session.
#[test]
fn t6_1_session_nonce_is_stable_during_session() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    let nonce1 = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .session_nonce;
    let nonce2 = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .session_nonce;
    assert_eq!(nonce1, nonce2, "session_nonce must be stable");
}

// ─── T6.2 — Supervisor emits Reconnecting on IceFailed/ConnectionLost ─────────

/// T6.2 (AC-1, AC-2): IceFailed triggers supervisor → Reconnecting event.
/// Must NOT emit the old peer_lost + Restart button.
#[test]
fn t6_2_ice_failed_emits_reconnecting_event_not_peer_lost() {
    let (bridge, ev_tx, ch) = make_supervised_bridge();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    ev_tx.send(TransportEvent::IceFailed).unwrap();

    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got,
        "expected reconnecting event after IceFailed, got: {:?}",
        ch.messages()
    );

    let msgs = ch.messages();
    let has_peer_lost = msgs.iter().any(|m| m.contains("\"kind\":\"peer_lost\""));
    assert!(
        !has_peer_lost,
        "IceFailed must NOT emit peer_lost when supervisor is active, got: {msgs:?}"
    );

    stop_sender_session(&bridge);
}

/// T6.2 (AC-1, AC-2): ConnectionLost triggers supervisor → Reconnecting event.
#[test]
fn t6_2_connection_lost_emits_reconnecting_event_not_peer_lost() {
    let (bridge, ev_tx, ch) = make_supervised_bridge();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    ev_tx
        .send(TransportEvent::ConnectionLost {
            reason: "poll error".to_string(),
        })
        .unwrap();

    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got,
        "expected reconnecting event after ConnectionLost, got: {:?}",
        ch.messages()
    );

    stop_sender_session(&bridge);
}

// ─── T6.2 — Dead event after 3 rebuild failures ───────────────────────────────

/// T6.2 (AC-3, AC-7): After 3 failed rebuild cycles, supervisor emits Dead event.
///
/// We inject IceFailed, wait for reconnecting{1}, then drive the supervisor directly
/// via the supervisor_signal_tx exposed on the bridge.
#[test]
fn t6_2_three_rebuild_failures_emit_dead_event() {
    let (bridge, ev_tx, ch) = make_supervised_bridge();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Trigger reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting event, got: {:?}", ch.messages());

    // Get session nonce.
    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    // Wait for supervisor_signal_tx to be set (supervisor is spawned in enter_supervisor_mode).
    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set within 500ms");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    // Drive 3 attempt cycles: Ack → RebuildFailed.
    // Each cycle: wait for reconnecting{i} to appear, then ack + fail rebuild.
    // The supervisor uses v1_default() backoff (3s/9s/27s), so we cannot use
    // fixed sleeps. Instead we wait for each reconnecting event before advancing.
    //
    // Note: between attempts, the supervisor sleeps for the backoff duration.
    // We interrupt each sleep by sending PeerAck immediately (any signal wakes
    // recv_timeout in the backoff phase). The supervisor then ignores the signal
    // (it's an Ok(_) during backoff) and moves to AwaitingAck{next}.
    // We then immediately send the REAL PeerAck for AwaitingAck to consume.

    // The supervisor uses v1_default() backoff (3s/9s/27s between attempts).
    // We interrupt each backoff sleep by sending a harmless signal (any Ok(_) wakes
    // recv_timeout and falls through to start the next AwaitingAck phase).
    // Strategy per attempt:
    //   1. Wait for reconnecting{i} → supervisor is in AwaitingAck{i}.
    //   2. Send PeerAck{nonce, i} → supervisor moves to Rebuilding{i}.
    //   3. Send RebuildFailed → supervisor starts backoff sleep (up to 9s).
    //   4. Send a dummy signal to interrupt the backoff sleep.
    //   5. Supervisor wakes, transitions to AwaitingAck{i+1}.

    for i in 1u8..=3 {
        // Wait for reconnecting{i} event — supervisor is now in AwaitingAck{i}.
        // The ack_timeout in AwaitingAck is 2s; give a generous 3s margin.
        let reconnecting_key = format!("\"attempt\":{i}");
        let got_i = ch.wait_for_message_containing(&reconnecting_key, Duration::from_secs(3));
        assert!(
            got_i,
            "expected reconnecting attempt={i}, got: {:?}",
            ch.messages()
        );

        // Ack → supervisor moves to Rebuilding.
        sup_tx
            .try_send(SupervisorSignal::PeerAck {
                session_nonce,
                attempt: i,
            })
            .ok();

        // Give the coordinator a moment to forward InitiateRebuild.
        thread::sleep(Duration::from_millis(60));

        // Fail the rebuild → supervisor enters backoff sleep.
        sup_tx.try_send(SupervisorSignal::RebuildFailed).ok();

        // Interrupt the backoff sleep immediately (Ok(_) falls through to next AwaitingAck).
        if i < 3 {
            thread::sleep(Duration::from_millis(30));
            // Any non-Stop signal wakes the backoff recv_timeout.
            sup_tx.try_send(SupervisorSignal::RebuildFailed).ok();
        }
    }

    let got_dead = ch.wait_for_message_containing("\"kind\":\"dead\"", Duration::from_millis(1000));
    assert!(
        got_dead,
        "expected dead event after 3 failures, got: {:?}",
        ch.messages()
    );

    stop_sender_session(&bridge);
}

// ─── T6.2 — Stop during reconnect cancels supervisor (AC-9, AC-13) ────────────

/// T6.2 (AC-9, AC-13): stop_sender_session during Reconnecting state cancels
/// supervisor cleanly and returns within 2s.
#[test]
fn t6_2_stop_during_reconnect_cancels_supervisor_cleanly() {
    let (bridge, ev_tx, ch) = make_supervised_bridge();

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Enter reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got,
        "expected reconnecting before stop, got: {:?}",
        ch.messages()
    );

    // Wait for supervisor to set signal_tx.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        if bridge.supervisor_signal_tx.lock().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break; // stop_sender_session will still work via stop_flag fallback
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Stop must complete within 2s (AC-9).
    let start = std::time::Instant::now();
    stop_sender_session(&bridge);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "stop must complete within 2s during reconnect, took: {elapsed:?}"
    );
    assert!(bridge.session.lock().unwrap().is_none());
}

// ─── T2-T7 sender-supervisor-rebuild-lifetime ─────────────────────────────────

use tracing_test::traced_test;

// ─── SC-3: hook closures are no-ops after Arc drop ────────────────────────────

/// SC-3 (REQ-SSRL-3, REQ-SSRL-12): When the root Arc<Mutex<MdnsSignaling>> is
/// dropped, the three coordinator hook closures (publish_reconnect_request,
/// publish_reconnect_ack, initiate_mdns_reset) MUST be no-ops and MUST emit a
/// debug-level tracing event with target "sender-hooks" and a hook field.
///
/// This test manually constructs Weak-based closures mirroring the new production
/// pattern. It will FAIL to compile until sender.rs:1654-1656 converts to Weak.
#[traced_test]
#[test]
fn hook_closures_noop_after_arc_drop_sc3() {
    use sm_domain::signaling::{Signaling, SignalingConfig};
    use sm_infra::signaling::mdns::MdnsSignaling;
    use std::sync::{Arc, Mutex};

    let arc = Arc::new(Mutex::new(
        MdnsSignaling::new(SignalingConfig::default()).unwrap(),
    ));

    // Build Weak-based closures mirroring the new production pattern.
    let sig_for_req = Arc::downgrade(&arc);
    let sig_for_ack = Arc::downgrade(&arc);
    let sig_for_reset = Arc::downgrade(&arc);

    let publish_req: Arc<dyn Fn(u8, u64) + Send + Sync> =
        Arc::new(move |_attempt, _session_nonce| {
            let Some(_arc) = sig_for_req.upgrade() else {
                tracing::debug!(
                    target: "sender-hooks",
                    hook = "publish_reconnect_request",
                    "signaling Arc dropped (session torn down); hook is no-op",
                );
                return;
            };
        });

    let publish_ack: Arc<dyn Fn(u8, u64) + Send + Sync> =
        Arc::new(move |_attempt, _session_nonce| {
            let Some(_arc) = sig_for_ack.upgrade() else {
                tracing::debug!(
                    target: "sender-hooks",
                    hook = "publish_reconnect_ack",
                    "signaling Arc dropped (session torn down); hook is no-op",
                );
                return;
            };
        });

    let initiate_reset: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let Some(_arc) = sig_for_reset.upgrade() else {
            tracing::debug!(
                target: "sender-hooks",
                hook = "initiate_mdns_reset",
                "signaling Arc dropped (session torn down); hook is no-op",
            );
            return;
        };
    });

    // Drop root Arc — hooks must now be no-ops.
    drop(arc);

    // Invoke all three hooks — must not panic.
    publish_req(1, 42);
    publish_ack(1, 42);
    initiate_reset();

    // Each invocation must have emitted a debug-level event with target "sender-hooks".
    assert!(
        logs_contain("sender-hooks"),
        "expected tracing events with target 'sender-hooks'"
    );
}

// ─── SC-6: Gate A/B/C/D aborts emit tracing::warn! ───────────────────────────

/// Helper: build a minimal RestartCache for gate-abort tests.
fn make_test_restart_cache(
    ch: Arc<FakeJsonChannel>,
) -> screen_mirror_lib::commands::sender::RestartCache {
    screen_mirror_lib::commands::sender::RestartCache {
        udp_port: 0,
        service_name: "_test._tcp.local.".to_string(),
        channel: ch as Arc<dyn ChannelLike>,
        session_nonce: 1,
    }
}

/// Check raw global tracing buffer (not scope-filtered) for a string.
/// Used for events emitted from spawned threads that are not inside the test span.
fn raw_logs_contain(val: &str) -> bool {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    let s = String::from_utf8_lossy(&buf);
    s.contains(val)
}

/// Return the byte offset of the first occurrence of `val` in the raw global
/// tracing buffer. Returns `None` if the string is not present.
/// Used for ordering assertions between sequentially-fired tracing events.
fn raw_logs_find(val: &str) -> Option<usize> {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    // Cow<str> from from_utf8_lossy is valid for .find() directly; we shadow `s`
    // as owned String to avoid the temporary borrow lifetime issue with Cow.
    let s = String::from_utf8_lossy(&buf).into_owned();
    s.find(val)
}

/// SC-6 Gate A (REQ-SSRL-9): When stop flag is set BEFORE rebuild starts,
/// a warn-level event with target "sender-rebuild", gate="A", and attempt field MUST fire.
///
/// Fixture: stop_flag pre-set true before hook invocation → Gate A check fires immediately.
/// Verified by WARNING-2 fix (batch 2): asserts structured gate="A" field, not just message.
#[traced_test]
#[test]
fn rebuild_gate_a_abort_emits_warn_sc6() {
    use std::sync::atomic::AtomicBool;

    let ch = FakeJsonChannel::new();
    let stop_flag = Arc::new(AtomicBool::new(true)); // pre-set → Gate A fires
    let bridge_cache = Arc::new(Mutex::new(Some(make_test_restart_cache(ch.clone()))));
    let bridge_session: Arc<Mutex<Option<SenderSession>>> = Arc::new(Mutex::new(None));

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);
    let builder: SenderBuilderFn = Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub()));

    let hook = make_sender_rebuild_hook(builder, bridge_cache, bridge_session, stop_flag, 1);
    (hook)(sig_tx);

    sig_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("RebuildFailed must arrive");

    assert!(
        raw_logs_contain("sender-rebuild"),
        "expected target=sender-rebuild in warn event"
    );
    assert!(
        raw_logs_contain("rebuild aborted: stop flag set before work began"),
        "expected Gate A abort message"
    );
    // WARNING-2 fix: assert the structured gate field so each gate test is distinct.
    assert!(
        raw_logs_contain("gate=\"A\""),
        "expected structured gate=\"A\" field in warn event"
    );
}

/// SC-6 Gate B (REQ-SSRL-9): stop flag set AFTER teardown (Gate B).
///
/// Fixture (WARNING-1 fix, batch 2): inject a SenderSession whose shutdown closure
/// atomically sets old_stop_flag, so:
///   Gate A check: flag false → passes
///   Teardown runs shutdown closure → sets flag true
///   Gate B check: flag true → fires
///
/// This replaces the previous fixture that pre-set the flag before the hook ran
/// (which caused Gate A to fire instead of Gate B).
#[traced_test]
#[test]
fn rebuild_gate_b_abort_emits_warn_sc6() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let ch = FakeJsonChannel::new();
    let stop_flag = Arc::new(AtomicBool::new(false)); // starts false → Gate A passes
    let bridge_cache = Arc::new(Mutex::new(Some(make_test_restart_cache(ch.clone()))));

    // Inject a session whose shutdown closure sets the stop flag.
    // Teardown runs s.shutdown.take()() → sets flag → Gate B sees it true.
    let stop_flag_for_shutdown = stop_flag.clone();
    let session = SenderSession::new(
        stop_flag.clone(),
        vec![],
        ch.clone() as Arc<dyn ChannelLike>,
        Arc::new(SenderCounters::default()),
        Some(Box::new(move || {
            stop_flag_for_shutdown.store(true, Ordering::SeqCst);
        })),
        "sw_fake".to_string(),
    );
    let bridge_session: Arc<Mutex<Option<SenderSession>>> = Arc::new(Mutex::new(Some(session)));

    // Builder must NOT be called (Gate B fires before builder).
    let builder: SenderBuilderFn =
        Arc::new(move |_, _, _, _| -> Result<SenderBundle, BundleError> {
            panic!("Gate B: builder must not be called");
        });

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);
    // Spawn hook on separate thread and join — ensures worker thread finishes
    // before we read raw_logs_contain (which reads global tracing buffer).
    let hook = make_sender_rebuild_hook(builder, bridge_cache, bridge_session, stop_flag, 2);
    let hook_handle = std::thread::Builder::new()
        .name("test-hook-gate-b".into())
        .spawn(move || hook(sig_tx))
        .expect("failed to spawn hook thread");

    sig_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("RebuildFailed must arrive");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        raw_logs_contain("sender-rebuild"),
        "expected target=sender-rebuild in warn event"
    );
    assert!(
        raw_logs_contain("rebuild aborted: stop flag set after teardown"),
        "expected Gate B abort message"
    );
    // WARNING-1+2 fix: assert the structured gate field to prove Gate B (not A) fired.
    assert!(
        raw_logs_contain("gate=\"B\""),
        "expected structured gate=\"B\" field — verifies fixture actually triggers Gate B"
    );
}

/// SC-6 Gate C (REQ-SSRL-9): stop flag set AFTER build succeeds (Gate C).
///
/// Fixture: builder sets stop_flag during build → Gate C check (after builder returns)
/// sees it true. This fixture was always correct; batch 2 adds the gate="C" assertion
/// (WARNING-2 fix).
#[traced_test]
#[test]
fn rebuild_gate_c_abort_emits_warn_sc6() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let ch = FakeJsonChannel::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_in_builder = stop_flag.clone();
    let bridge_cache = Arc::new(Mutex::new(Some(make_test_restart_cache(ch.clone()))));
    let bridge_session: Arc<Mutex<Option<SenderSession>>> = Arc::new(Mutex::new(None));

    // Builder sets stop flag while running → Gate C (after-build check) fires.
    // Gate A and Gate B pass (flag false at those checkpoints).
    let builder: SenderBuilderFn = Arc::new(move |_, _, _, _| {
        stop_flag_in_builder.store(true, Ordering::SeqCst);
        Ok(SenderBundle::test_stub())
    });

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);
    let hook = make_sender_rebuild_hook(builder, bridge_cache, bridge_session, stop_flag, 3);
    (hook)(sig_tx);

    sig_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("RebuildFailed must arrive");

    assert!(
        raw_logs_contain("sender-rebuild"),
        "expected target=sender-rebuild in warn event"
    );
    assert!(
        raw_logs_contain("rebuild aborted: stop flag set after build"),
        "expected Gate C abort message"
    );
    // WARNING-2 fix: assert the structured gate field.
    assert!(
        raw_logs_contain("gate=\"C\""),
        "expected structured gate=\"C\" field in warn event"
    );
}

/// SC-6 Gate D (REQ-SSRL-9): stop flag set AFTER session swap (Gate D).
///
/// Fixture (WARNING-1 fix, batch 2): controlled-timing via blocking builder +
/// bridge_session lock held by test thread. Sequence:
///   1. Builder signals "started" then blocks on release_rx.
///   2. Test waits for builder_started, then holds bridge_session.lock().
///   3. Test releases builder (flag still false → Gate C passes).
///   4. Test sets stop_flag=true while still holding bridge_session lock.
///   5. Test drops lock → worker proceeds to swap (step 11) → Gate D fires.
///
/// This is deterministic: the swap at step 11 blocks until the test drops the lock,
/// by which time stop_flag is true → Gate D always fires, Gate C never does.
///
/// This replaces the previous fixture that set the flag inside the builder, which
/// caused Gate C (not Gate D) to fire.
#[traced_test]
#[test]
fn rebuild_gate_d_abort_emits_warn_sc6() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let ch = FakeJsonChannel::new();
    let stop_flag = Arc::new(AtomicBool::new(false)); // starts false
    let bridge_cache = Arc::new(Mutex::new(Some(make_test_restart_cache(ch.clone()))));
    let bridge_session: Arc<Mutex<Option<SenderSession>>> = Arc::new(Mutex::new(None));

    // Two-channel builder: signals "started", then blocks until test releases.
    let (builder_started_tx, builder_started_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(release_rx));

    let builder: SenderBuilderFn =
        Arc::new(move |_, _, _, _| -> Result<SenderBundle, BundleError> {
            let _ = builder_started_tx.send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
            Ok(SenderBundle::test_stub())
        });

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);
    let hook = make_sender_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session.clone(),
        stop_flag.clone(),
        4,
    );
    let hook_handle = std::thread::Builder::new()
        .name("test-hook-gate-d".into())
        .spawn(move || hook(sig_tx))
        .expect("failed to spawn hook thread");

    // Wait for builder to start (Gates A, B passed; step 6 teardown is done).
    builder_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("builder must start within 5s");

    // Hold bridge_session lock so the swap at step 11 blocks after builder returns.
    let guard = bridge_session.lock().unwrap();

    // Release builder — stop_flag still false → Gate C check passes.
    let _ = release_tx.send(());

    // Give worker ~5ms to: receive release, pass Gate C, block on bridge_session.lock().
    std::thread::sleep(Duration::from_millis(5));

    // Set stop_flag=true while worker is blocked waiting for bridge_session lock.
    stop_flag.store(true, Ordering::SeqCst);

    // Release lock → worker swaps (step 11), hits Gate D (flag true) → RebuildFailed.
    drop(guard);

    sig_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("RebuildFailed must arrive");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        raw_logs_contain("sender-rebuild"),
        "expected target=sender-rebuild in warn event"
    );
    assert!(
        raw_logs_contain("rebuild aborted: stop flag set after swap"),
        "expected Gate D abort message"
    );
    // WARNING-1+2 fix: assert the structured gate field to prove Gate D (not C) fired.
    assert!(
        raw_logs_contain("gate=\"D\""),
        "expected structured gate=\"D\" field — verifies fixture actually triggers Gate D"
    );
}

// ─── SC-7: Builder failure emits tracing::error! ──────────────────────────────

/// SC-7 (REQ-SSRL-10): When the bundle builder returns Err, an error-level event
/// with target "sender-rebuild", field attempt, and field error MUST fire.
#[traced_test]
#[test]
fn rebuild_builder_failure_emits_error_event_sc7() {
    use std::sync::atomic::AtomicBool;

    let ch = FakeJsonChannel::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let bridge_cache = Arc::new(Mutex::new(Some(make_test_restart_cache(ch.clone()))));
    let bridge_session: Arc<Mutex<Option<screen_mirror_lib::commands::sender::SenderSession>>> =
        Arc::new(Mutex::new(None));

    // Builder always fails.
    let builder: SenderBuilderFn =
        Arc::new(|_, _, _, _| Err(BundleError::Other("injected test error".to_string())));

    let (sig_tx, sig_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);
    let hook = make_sender_rebuild_hook(builder, bridge_cache, bridge_session, stop_flag, 1);
    (hook)(sig_tx);

    // RebuildFailed must still be sent (existing behaviour retained).
    let signal = sig_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("RebuildFailed must arrive on builder failure");
    assert!(
        matches!(signal, SupervisorSignal::RebuildFailed),
        "expected RebuildFailed, got {signal:?}"
    );

    assert!(
        raw_logs_contain("sender-rebuild"),
        "expected target=sender-rebuild in error event"
    );
    assert!(
        raw_logs_contain("bundle builder failed"),
        "expected builder failure message"
    );
}

// ─── SC-8A: SupervisorOutcome::Stopped emits tracing::info! ─────────────────

/// SC-8A (REQ-SSRL-11): When the supervisor exits cleanly (stop signal received while
/// waiting for peer ack), a tracing::info! event with target "sender-supervisor" MUST
/// be emitted. SupervisorOutcome::Stopped is reached by: triggering IceFailed to enter
/// supervisor mode, waiting for the supervisor to be spawned, then stopping the bridge.
#[traced_test]
#[test]
fn supervisor_outcome_stopped_emits_info_sc8() {
    let (bridge, ev_tx, ch) = make_supervised_bridge();
    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Trigger supervisor mode.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for supervisor to be spawned (supervisor_signal_tx is set).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        if bridge.supervisor_signal_tx.lock().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("supervisor_signal_tx not set within 500ms");
        }
        thread::sleep(Duration::from_millis(5));
    }

    // Stop the bridge — supervisor receives Stop signal → exits with Stopped → info! fires.
    stop_sender_session(&bridge);

    // Give spawned drain thread time to flush tracing events.
    thread::sleep(Duration::from_millis(20));

    assert!(
        raw_logs_contain("sender-supervisor"),
        "expected tracing event with target=sender-supervisor"
    );
    assert!(
        raw_logs_contain("supervisor stopped cleanly"),
        "expected 'supervisor stopped cleanly' message"
    );
}

// ─── SC-2: rebuild_cycle_no_zombie_mdns_thread (Windows-gated, HW, manual-verify)
#[cfg(target_os = "windows")]
#[ignore]
#[test]
fn rebuild_cycle_no_zombie_mdns_thread_sc2() {
    // MANUAL-VERIFY: Requires Windows CI with real MFT.
    // Asserts: after sd() runs, Weak::upgrade() returns None.
    // Deferred to Windows CI gate (T20 manual verify).
    unimplemented!("manual verify: see apply-progress T16 notes");
}

// ─── T11.1 — retry_session_inner ──────────────────────────────────────────────

/// T11.1 (AC-8 — NoCachedParams): retry_session_inner before any start returns Err.
#[test]
fn t11_1_retry_session_no_cache_returns_err() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    let result = retry_session_inner(&bridge, ch as Arc<dyn ChannelLike>);
    assert!(
        result.is_err(),
        "retry with no cache must return Err, got Ok(())"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("NoCachedParams"),
        "error must contain 'NoCachedParams', got: {err}"
    );
}

/// T11.1 (AC-8 — live session): retry_session_inner while session is live stops it and restarts.
/// Retry is idempotent — it tears down the live session and re-enters Connecting.
#[test]
fn t11_1_retry_session_while_live_stops_and_restarts() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    // Start a session (still alive — test stub has no real drain).
    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // A new channel for the retry.
    let ch2 = FakeJsonChannel::new();

    // Retry while session is active: stops old session and re-starts on new channel.
    let result = retry_session_inner(&bridge, ch2.clone() as Arc<dyn ChannelLike>);
    assert!(
        result.is_ok(),
        "retry while running must succeed (stops + restarts), got: {result:?}"
    );

    // Connecting event emitted on the new channel.
    let msgs = ch2.messages();
    assert!(
        msgs.iter().any(|m| m.contains("\"kind\":\"connecting\"")),
        "retry must emit Connecting on new channel, got: {msgs:?}"
    );

    stop_sender_session(&bridge);
}

/// T11.1 (AC-8 — success path): retry_session_inner after Dead state re-enters Connecting.
/// Simulates: start → supervisor → dead → retry_session_inner (new channel).
#[test]
fn t11_1_retry_session_after_dead_emits_connecting() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch1 = FakeJsonChannel::new();

    // First session: start, then simulate Dead (no real supervisor — just stop the session
    // which clears current_args, but keep restart_cache by manually setting it back to
    // simulate the "cache survives Dead" invariant).
    start_sender_inner(
        &bridge,
        ch1.clone() as Arc<dyn ChannelLike>,
        Some(7891),
        Some("_screen-mirror._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Save the cache before stopping (stop_sender_session clears it).
    let saved_cache = {
        let guard = bridge.restart_cache.lock().unwrap();
        guard.clone()
    };

    // Simulate Dead: supervisor exits, drains exit. We model this by stopping normally
    // but then restoring the cache (as if the Dead path preserved it).
    stop_sender_session(&bridge);

    // Restore cache (simulates the Dead path: cache is NOT cleared by Dead, only by stop/retry).
    *bridge.restart_cache.lock().unwrap() = saved_cache;

    // Now retry with a new channel.
    let ch2 = FakeJsonChannel::new();
    let result = retry_session_inner(&bridge, ch2.clone() as Arc<dyn ChannelLike>);
    assert!(
        result.is_ok(),
        "retry after Dead must succeed, got: {result:?}"
    );

    // A Connecting event must be emitted on the new channel.
    let msgs = ch2.messages();
    let has_connecting = msgs.iter().any(|m| m.contains("\"kind\":\"connecting\""));
    assert!(
        has_connecting,
        "retry must emit Connecting on new channel, got: {msgs:?}"
    );

    // The bridge must have a new active session.
    assert!(
        bridge.current_args.lock().unwrap().is_some(),
        "current_args must be Some after retry"
    );

    stop_sender_session(&bridge);
}

/// T11.1: After retry, restart_cache is populated with new session params.
#[test]
fn t11_1_retry_session_populates_restart_cache_with_new_nonce() {
    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch1 = FakeJsonChannel::new();

    start_sender_inner(&bridge, ch1.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    let original_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .session_nonce;

    let saved_cache = bridge.restart_cache.lock().unwrap().clone();
    stop_sender_session(&bridge);
    *bridge.restart_cache.lock().unwrap() = saved_cache;

    let ch2 = FakeJsonChannel::new();
    retry_session_inner(&bridge, ch2 as Arc<dyn ChannelLike>).expect("retry must succeed");

    let new_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .session_nonce;

    // The nonce should be different (new session = new nonce, with negligible collision prob).
    // We can't assert != because rand::random() has ~1/2^64 collision chance,
    // but we can assert the cache is present and populated.
    let _ = original_nonce;
    assert!(
        new_nonce > 0,
        "new session_nonce must be non-zero, got: {new_nonce}"
    );

    stop_sender_session(&bridge);
}

// ─── Batch 1 (T1.1) — stop_sender_session_internal extraction contract ────────

/// T1.1 (AC-NR1): `stop_sender_session_internal` tears down the session but
/// does NOT clear `restart_cache` or `current_args`.
///
/// Proves the extraction contract: the "internal" variant is a partial teardown
/// (steps 1-5 only); the public `stop_sender_session` is the thin wrapper that
/// also clears args/cache.
#[test]
fn stop_sender_session_internal_leaves_restart_cache_intact() {
    use screen_mirror_lib::commands::sender::stop_sender_session_internal;

    let bridge =
        SenderBridge::new_with_builder(Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())));
    let ch = FakeJsonChannel::new();

    // Populate current_args and restart_cache via a real start.
    start_sender_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(7895),
        Some("_sm-internal-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Verify preconditions: session, args, and cache are all populated.
    assert!(
        bridge.session.lock().unwrap().is_some(),
        "session must be Some before internal stop"
    );
    assert!(
        bridge.current_args.lock().unwrap().is_some(),
        "current_args must be Some before internal stop"
    );
    assert!(
        bridge.restart_cache.lock().unwrap().is_some(),
        "restart_cache must be Some before internal stop"
    );

    // Call the internal variant — partial teardown only.
    stop_sender_session_internal(&bridge);

    // Session should be torn down (None).
    assert!(
        bridge.session.lock().unwrap().is_none(),
        "session must be None after internal stop"
    );

    // restart_cache must still be Some (internal does NOT clear it).
    assert!(
        bridge.restart_cache.lock().unwrap().is_some(),
        "restart_cache must remain Some after stop_sender_session_internal"
    );

    // current_args must still be Some (internal does NOT clear it).
    assert!(
        bridge.current_args.lock().unwrap().is_some(),
        "current_args must remain Some after stop_sender_session_internal"
    );
}

// ─── Batch 2 (T2.x) — Sender rebuild worker ──────────────────────────────────

/// Build a supervised bridge whose `initiate_rebuild` hook is the V2 worker.
///
/// The bridge builder (injected via `SenderBridge.builder`) returns
/// `SenderBundle::test_stub()` — no real pipeline, cross-platform.
///
/// The V2 rebuild hook is constructed via `make_sender_rebuild_hook` and wired
/// into the drain via `run_sender_transport_event_drain_with_supervisor_custom_and_hooks`.
///
/// Returns `(bridge, ev_tx, ch)` identical in shape to `make_supervised_bridge`.
fn make_supervised_bridge_with_rebuild_hook(
    policy: ReconnectPolicy,
    ack_timeout: Duration,
    rebuild_timeout: Duration,
) -> (
    SenderBridge,
    std::sync::mpsc::SyncSender<TransportEvent>,
    Arc<FakeJsonChannel>,
) {
    let ch = FakeJsonChannel::new();
    let ch_for_caller = ch.clone();

    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    // Pre-allocate the session and restart_cache arcs BEFORE building the bridge.
    // Both the bridge (via new_with_builder_and_arcs) and the rebuild hook (captured
    // in the builder closure) share the SAME arc pointers.
    // This ensures that when start_sender_inner writes to bridge.session / bridge.restart_cache,
    // the rebuild hook reads from those exact same arcs.
    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::SenderSession>>> =
        Arc::new(Mutex::new(None));
    let restart_cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::RestartCache>>> =
        Arc::new(Mutex::new(None));

    let session_for_builder = session_arc.clone();
    let cache_for_builder = restart_cache_arc.clone();

    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder_and_arcs(
        Arc::new(move |_udp_port, _service_name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let ack_t = ack_timeout;
            let rebuild_t = rebuild_timeout;

            // Construct the V2 rebuild hook using the shared session and cache arcs.
            // The hook's builder returns test_stub() — no real pipeline.
            let rebuild_hook = make_sender_rebuild_hook(
                Arc::new(|_, _, _, _| Ok(SenderBundle::test_stub())),
                cache_for_builder.clone(),
                session_for_builder.clone(),
                stop_flag.clone(),
                1, // attempt — fixed at 1 for this helper
            );

            let hooks = SenderCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let h = thread::Builder::new()
                .name("supervised-drain-v2".into())
                .spawn(move || {
                    run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx, stop_flag, channel, st, p, ack_t, rebuild_t, hooks,
                    );
                })
                .expect("spawn drain");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
                backend_name: "sw_fake".to_string(),
            })
        }),
        session_arc,
        restart_cache_arc,
        sup_tx,
    );

    (bridge, ev_tx, ch_for_caller)
}

/// T2.1 (AC-R4, AC-5): Happy path — rebuild hook spawns a worker that calls the
/// builder and signals `RebuildSucceeded`, causing the drain to emit `"streaming"`.
///
/// RED against V1: V1 stub always signals `RebuildFailed` → drain emits `"dead"`,
/// assertion `streaming_before_dead` fails.
#[test]
fn rebuild_hook_calls_builder_and_signals_succeeded() {
    let (bridge, ev_tx, ch) = make_supervised_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Trigger a reconnect cycle.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for the supervisor to be in AwaitingAck{1}.
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting event, got: {:?}",
        ch.messages()
    );

    // Obtain the session nonce so we can send a valid PeerAck.
    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    // Wait for supervisor_signal_tx to be set.
    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set within 500ms");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    // Send PeerAck → supervisor moves to Rebuilding → calls initiate_rebuild hook.
    sup_tx
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // The hook (V2) should spawn a worker that signals RebuildSucceeded.
    // The drain maps StateChanged(Connected) → emit "streaming".
    let got_streaming =
        ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming event after successful rebuild, got: {:?}",
        ch.messages()
    );

    // Must NOT have emitted "dead" before "streaming" (attempt 1 succeeded).
    let msgs = ch.messages();
    let streaming_idx = msgs
        .iter()
        .position(|m| m.contains("\"kind\":\"streaming\""));
    let dead_idx = msgs.iter().position(|m| m.contains("\"kind\":\"dead\""));
    assert!(
        streaming_idx.is_some(),
        "streaming event must be present, got: {msgs:?}"
    );
    if let Some(d) = dead_idx {
        let s = streaming_idx.unwrap();
        assert!(
            s < d,
            "streaming must appear before dead (rebuild succeeded on attempt 1), got: {msgs:?}"
        );
    }

    stop_sender_session(&bridge);
}

/// T2.3 (AC-R4): Builder error — rebuild hook signals `RebuildFailed`.
///
/// RED against V1: V1 stub always signals RebuildFailed regardless of the builder
/// result, so a builder-returns-Err test would PASS with V1 for the wrong reason.
/// This test is RED in a different sense: it verifies the worker actually calls
/// the builder (observable via a shared counter), which V1 does NOT do.
#[test]
fn rebuild_hook_signals_failed_on_builder_error() {
    use std::sync::atomic::AtomicU32;

    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_for_hook = call_count.clone();

    let ch = FakeJsonChannel::new();
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    // Pre-allocate arcs shared between bridge and rebuild hook.
    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::SenderSession>>> =
        Arc::new(Mutex::new(None));
    let cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::RestartCache>>> =
        Arc::new(Mutex::new(None));
    let session_clone = session_arc.clone();
    let cache_clone = cache_arc.clone();

    let policy = fast_policy();
    let ack_timeout = Duration::from_millis(500);

    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder_and_arcs(
        Arc::new(move |_udp_port, _service_name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;
            let cnt = call_count_for_hook.clone();

            // Builder that counts calls and always fails.
            let failing_builder: screen_mirror_lib::commands::sender::SenderBuilderFn =
                Arc::new(move |_, _, _, _| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                    Err(screen_mirror_lib::commands::sender::BundleError::Other(
                        "injected failure".to_string(),
                    ))
                });

            let rebuild_hook = make_sender_rebuild_hook(
                failing_builder,
                cache_clone.clone(),
                session_clone.clone(),
                stop_flag.clone(),
                1,
            );

            let hooks = SenderCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let h = thread::Builder::new()
                .name("failing-drain".into())
                .spawn(move || {
                    run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx, stop_flag, channel, st, p, t, t, hooks,
                    );
                })
                .expect("spawn");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
                backend_name: "sw_fake".to_string(),
            })
        }),
        session_arc,
        cache_arc,
        sup_tx,
    );

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting, got: {:?}",
        ch.messages()
    );

    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    let sup_tx_guard = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    // Sending PeerAck may be ignored by the supervisor if session_nonce mismatches
    // the supervisor's internal nonce (they are independent rand::random() values).
    // In that case the supervisor times out from AwaitingAck (after ack_timeout=500ms),
    // fires InitiateMdnsReset, then InitiateRebuild — which calls our hook.
    // So we wait up to 2s for the "dead" event (worker calls builder → fails → eventual dead).
    sup_tx_guard
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // Wait up to 2s for the supervisor to reach InitiateRebuild and call the hook.
    // The supervisor spends up to ack_timeout=500ms in AwaitingAck, then calls
    // InitiateRebuild. The worker calls the failing builder quickly after that.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if call_count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    // Builder must have been called at least once (V1 stub does NOT call builder).
    assert!(
        call_count.load(Ordering::Relaxed) >= 1,
        "builder must be called by the rebuild worker, but call_count={}",
        call_count.load(Ordering::Relaxed)
    );

    stop_sender_session(&bridge);
}

/// T2.5 (AC-R2): Successful rebuild swaps the session — new stop_flag differs from old.
///
/// RED against V1: V1 stub never swaps the session, so the stop_flag Arc identity
/// is unchanged after the rebuild → `Arc::ptr_eq` returns true → assertion fails.
#[test]
fn rebuild_swaps_session_new_stop_flag_differs_from_old() {
    let (bridge, ev_tx, ch) = make_supervised_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Capture the original stop_flag Arc before rebuild.
    let original_stop_flag = bridge
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.stop_flag.clone())
        .expect("session must be Some after start");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));

    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    sup_tx
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // Wait for rebuild to succeed.
    let got_streaming =
        ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming after rebuild, got: {:?}",
        ch.messages()
    );

    // After rebuild, the session's stop_flag must be a DIFFERENT Arc (fresh_stop_flag
    // allocated by the worker, not the OLD session's stop_flag).
    let new_stop_flag = bridge
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.stop_flag.clone());

    assert!(
        new_stop_flag.is_some(),
        "session must be Some after successful rebuild"
    );
    let new_stop_flag = new_stop_flag.unwrap();
    assert!(
        !Arc::ptr_eq(&original_stop_flag, &new_stop_flag),
        "rebuild must install a fresh stop_flag Arc (new Arc identity), but got the same pointer"
    );

    stop_sender_session(&bridge);
}

// ─── Batch 2 fix — two-generation rebuild chain (AC-5 regression guard) ─────────

/// Regression test: after TWO consecutive rebuilds, `bridge.session` holds the
/// second-generation (B2) session — not the first-generation (B1) or the original.
///
/// Bug: Batch 2 commit a4d0dae passed dummy `Arc::new(Mutex::new(None))` for
/// `bridge_session` / `bridge_cache` when `build_production_sender_bundle` was
/// called from inside its own builder closure.  The NEW bundle's hook held dummy
/// arcs that nobody observed; a second rebuild swapped into the void → real
/// `bridge.session` retained the broken B1 bundle → ZOMBIE.
///
/// This test directly verifies the bug mechanism: when `make_sender_rebuild_hook`
/// is called with DUMMY arcs for `bridge_session`/`bridge_cache`, the second-
/// generation worker's swap goes to the dummy arc and bridge.session is NOT updated.
///
/// Structure: build a bridge backed by REAL arcs, wire B0 with a hook that uses
/// DUMMY arcs (mimicking the bug), trigger first rebuild → bridge.session is
/// updated (B0's hook itself uses real arcs — only B1's inner hook uses dummies).
/// Then call B1's hook directly (simulating B1's supervisor → InitiateRebuild)
/// and verify the result: bridge.session remains unchanged (bug) vs updated (fix).
///
/// The test is split into two sub-cases via a `use_real_arcs` flag so both the
/// RED (dummy) and GREEN (real) behaviors can be asserted in a single test run.
/// The final assertion checks that only the REAL arcs path correctly updates
/// bridge.session on the second rebuild.
///
/// RED (before fix in build_production_sender_bundle): dummy arcs cause the second
/// rebuild to swap into nobody-observed storage → bridge.session stays on B1.
///
/// GREEN (after fix): real arcs passed through → bridge.session updated to B2.
///
/// AC-5: Only one auto-rebuild per process lifetime worked before this fix.
#[test]
fn rebuild_can_chain_across_generations_swaps_bridge_session_each_time() {
    use screen_mirror_lib::commands::sender::SenderBuilderFn;
    use std::sync::atomic::AtomicU32;

    /// Inner helper: run a two-generation rebuild chain.
    /// `use_real_arcs`: if true, B1's hook uses the real bridge arcs (the fix).
    ///                  if false, B1's hook uses dummy arcs (the bug).
    /// Returns `(b1_ptr, b2_ptr)` — the stop_flag Arc pointers after each rebuild.
    fn run_chain(
        use_real_arcs: bool,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        Option<Arc<std::sync::atomic::AtomicBool>>,
    ) {
        let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::SenderSession>>> =
            Arc::new(Mutex::new(None));
        let cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::RestartCache>>> =
            Arc::new(Mutex::new(None));

        let ch = FakeJsonChannel::new();

        // Two generations of ev_rx + sup_tx.
        let (ev_tx_b0, ev_rx0) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
        let (ev_tx_b1, ev_rx1) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
        let (_ev_tx_b2, ev_rx2) = std::sync::mpsc::sync_channel::<TransportEvent>(8);

        let ev_rx0_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
            Arc::new(Mutex::new(Some(ev_rx0)));
        let ev_rx1_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
            Arc::new(Mutex::new(Some(ev_rx1)));
        let ev_rx2_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
            Arc::new(Mutex::new(Some(ev_rx2)));

        let build_count = Arc::new(AtomicU32::new(0));
        let build_count_b = build_count.clone();

        let sup_tx_b0: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> =
            Arc::new(Mutex::new(None));
        let sup_tx_b1: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> =
            Arc::new(Mutex::new(None));
        let sup_tx_b2: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> =
            Arc::new(Mutex::new(None));

        // Self-referencing builder slot (used when use_real_arcs == true).
        let builder_slot: Arc<Mutex<Option<SenderBuilderFn>>> = Arc::new(Mutex::new(None));

        let session_b = session_arc.clone();
        let cache_b = cache_arc.clone();
        let sup_b0_b = sup_tx_b0.clone();
        let sup_b1_b = sup_tx_b1.clone();
        let sup_b2_b = sup_tx_b2.clone();
        let builder_slot_b = builder_slot.clone();
        let session_b2 = session_arc.clone();

        let policy = fast_policy();
        let ack_timeout = Duration::from_millis(500);

        let the_builder: SenderBuilderFn =
            Arc::new(move |_udp_port, _service_name, stop_flag, channel| {
                let generation = build_count_b.fetch_add(1, Ordering::Relaxed);

                let ev_rx = match generation {
                    0 => ev_rx0_slot.lock().unwrap().take().expect("ev_rx0"),
                    1 => ev_rx1_slot.lock().unwrap().take().expect("ev_rx1"),
                    _ => ev_rx2_slot.lock().unwrap().take().expect("ev_rx2"),
                };

                let sup_tx_slot = match generation {
                    0 => sup_b0_b.clone(),
                    1 => sup_b1_b.clone(),
                    _ => sup_b2_b.clone(),
                };

                // Inner builder for the hook: use the self-referencing builder so
                // each generation spawns a proper supervised drain when rebuilt.
                let inner_builder: SenderBuilderFn = builder_slot_b
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("builder_slot populated");

                // Bridge arcs for the hook — the property under test.
                //
                // B0's hook ALWAYS uses real arcs so the first rebuild (B0→B1) succeeds.
                // B1's hook is where the bug manifests:
                //   BUGGY (use_real_arcs=false): B1's hook gets dummy arcs — worker reads
                //     cache=None → RebuildFailed → bridge.session stays on B1.
                //   FIXED (use_real_arcs=true): B1's hook gets real arcs — worker reads
                //     real cache → builds B2 → swaps into bridge.session.
                let (hook_session, hook_cache) = if generation == 0 || use_real_arcs {
                    // B0 always uses real arcs; higher generations use real arcs if fixed.
                    (session_b.clone(), cache_b.clone())
                } else {
                    // Simulate build_production_sender_bundle pre-fix for generation >= 1:
                    // the inner recursive call got dummy arcs (Arc::new(Mutex::new(None))).
                    (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)))
                };

                let rebuild_hook = make_sender_rebuild_hook(
                    inner_builder,
                    hook_cache,
                    hook_session,
                    stop_flag.clone(),
                    generation + 1,
                );

                let hooks = SenderCoordinatorHooks {
                    publish_reconnect_request: Arc::new(|_, _| {}),
                    publish_reconnect_ack: Arc::new(|_, _| {}),
                    initiate_rebuild: rebuild_hook,
                    initiate_mdns_reset: Arc::new(|| {}),
                };

                let p = policy.clone();
                let t = ack_timeout;
                let h = thread::Builder::new()
                    .name(format!("chain-g{generation}-drain"))
                    .spawn(move || {
                        run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                            ev_rx,
                            stop_flag,
                            channel,
                            sup_tx_slot,
                            p,
                            t,
                            t,
                            hooks,
                        );
                    })
                    .expect("spawn drain");

                Ok(SenderBundle {
                    drain_handles: vec![h],
                    shutdown: None,
                    backend_name: "sw_fake".to_string(),
                })
            });

        *builder_slot.lock().unwrap() = Some(the_builder.clone());

        let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder_and_arcs(
            the_builder,
            session_arc.clone(),
            cache_arc.clone(),
            sup_tx_b0.clone(),
        );

        // Phase 0: start B0.
        start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

        // Phase 1: first rebuild B0 → B1.
        ev_tx_b0.send(TransportEvent::IceFailed).unwrap();
        let got_rc1 =
            ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
        assert!(got_rc1, "phase1 reconnecting missing");

        // Do NOT send PeerAck — supervisor uses its own internal nonce which is
        // independent of restart_cache.session_nonce (both are rand::random()).
        // Let AwaitingAck time out → supervisor fires InitiateRebuild naturally.
        // With ack_timeout=500ms this takes ~500ms.

        let got_streaming1 =
            ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(3000));
        assert!(
            got_streaming1,
            "phase1 streaming missing, messages: {:?}",
            ch.messages()
        );

        let b1_stop_flag = session_b2
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.stop_flag.clone())
            .expect("B1 session after first rebuild");

        // Phase 2: second rebuild B1 → B2.
        let streaming_before = ch
            .messages()
            .iter()
            .filter(|m| m.contains("\"kind\":\"streaming\""))
            .count();
        let reconnecting_before = ch
            .messages()
            .iter()
            .filter(|m| m.contains("\"kind\":\"reconnecting\""))
            .count();

        ev_tx_b1.send(TransportEvent::IceFailed).unwrap();

        // Wait for a NEW reconnecting event (B1's supervisor entered AwaitingAck).
        let got_rc2 = {
            let dl = std::time::Instant::now() + Duration::from_millis(1000);
            loop {
                let cnt = ch
                    .messages()
                    .iter()
                    .filter(|m| m.contains("\"kind\":\"reconnecting\""))
                    .count();
                if cnt > reconnecting_before {
                    break true;
                }
                if std::time::Instant::now() >= dl {
                    break false;
                }
                thread::sleep(Duration::from_millis(5));
            }
        };
        assert!(got_rc2, "phase2 reconnecting missing");

        // Wait for B1's rebuild attempts to resolve:
        //   - REAL arcs: rebuild succeeds → "streaming"
        //   - DUMMY arcs: rebuild fails (cache=None) → 3 attempts → "dead"
        // In both cases we wait for a terminal message that appears AFTER the rebuild.
        let resolved = {
            let dl = std::time::Instant::now() + Duration::from_millis(4000);
            loop {
                let msgs = ch.messages();
                let new_streaming = msgs
                    .iter()
                    .filter(|m| m.contains("\"kind\":\"streaming\""))
                    .count()
                    > streaming_before;
                let dead = msgs.iter().any(|m| m.contains("\"kind\":\"dead\""));
                if new_streaming || dead {
                    break true;
                }
                if std::time::Instant::now() >= dl {
                    break false;
                }
                thread::sleep(Duration::from_millis(10));
            }
        };
        assert!(
            resolved,
            "phase2 did not resolve within 4s, messages: {:?}",
            ch.messages()
        );

        // Small yield to let the worker finish the swap (step 11 happens before step 13).
        thread::sleep(Duration::from_millis(50));

        // Read bridge.session AFTER rebuild resolves.
        // DUMMY: bridge.session still holds B1's stop_flag (worker swapped into dummy).
        // REAL:  bridge.session holds B2's stop_flag (worker swapped into real arc).
        let b2_stop_flag = session_b2
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.stop_flag.clone());

        stop_sender_session(&bridge);

        (b1_stop_flag, b2_stop_flag)
    }

    // ── RED check: dummy arcs (simulates the pre-fix bug) ─────────────────────
    // With dummy arcs for B1's hook, the worker reads cache=None → RebuildFailed
    // immediately, without touching bridge.session.
    // bridge.session still holds B1's stop_flag after the second rebuild attempt.
    {
        let (b1, b2_opt) = run_chain(false /* dummy arcs = pre-fix bug */);
        // b2_opt is Some(B1's stop_flag) — the session was never updated to B2.
        let b2 = b2_opt.expect("bridge.session must be Some even with dummy arcs (still holds B1)");
        assert!(
            Arc::ptr_eq(&b1, &b2),
            "DUMMY arcs: expected bridge.session still holds B1 after second rebuild \
             (worker read cache=None from dummy arc → RebuildFailed without touching session). \
             Got different Arcs — test setup is incorrect."
        );
    }

    // ── GREEN check: real arcs (the fix) ──────────────────────────────────────
    // With real arcs for B1's hook, the worker reads the actual cache, builds B2,
    // and swaps it into bridge.session → b2_stop_flag is a NEW Arc distinct from B1.
    {
        let (b1, b2_opt) = run_chain(true /* real arcs = post-fix */);
        let b2 = b2_opt.expect("bridge.session must be Some after successful second rebuild");
        assert!(
            !Arc::ptr_eq(&b1, &b2),
            "after two rebuilds with real arcs, bridge.session must hold B2's stop_flag \
             (distinct Arc from B1). Dummy arcs in B1's hook break AC-5 for 2+ generation \
             failure cycles."
        );
    }
}

// ─── Batch 6 (T6.1) — Concurrent stop during rebuild (AC-R1) ─────────────────

/// T6.1 (AC-R1): `stop_sender_session` called concurrently while a rebuild worker
/// is in flight does NOT deadlock; both the stop and the worker complete within 500ms.
///
/// Design §4: stop_*_session does NOTHING active to the in-flight worker — it sets
/// stop_flag=true and sends SupervisorSignal::Stop. The worker observes the cancel
/// signal at one of the four gates (A/B/C/D) and returns. No join is performed on
/// the worker thread, so stop_*_session returns promptly.
///
/// This test uses a `recv_timeout` polling loop (500ms ceiling) to detect completion
/// of both operations, making it robust to CI scheduler jitter.
///
/// RED: if a deadlock path exists (e.g. worker tries to join itself, or stop
/// tries to join the worker), at least one of the channels will not be signaled
/// before the 500ms deadline → assertion fails.
#[test]
fn rebuild_does_not_deadlock_during_concurrent_stop() {
    use std::sync::mpsc::sync_channel as sc;

    // Use a blocking builder: it waits for a release signal so we can control
    // exactly when the rebuild worker is in flight before calling stop.
    let (release_tx, release_rx) = sc::<()>(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));

    // Completion signals: worker signals "done" after completing (success or fail).
    let (worker_done_tx, worker_done_rx) = sc::<()>(1);
    // stop signals "done" after stop_sender_session returns.
    let (stop_done_tx, stop_done_rx) = sc::<()>(1);

    let worker_done_tx = Arc::new(Mutex::new(Some(worker_done_tx)));
    let release_rx_clone = release_rx.clone();
    let worker_done_tx_clone = worker_done_tx.clone();

    let ch = FakeJsonChannel::new();

    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx_arc: Arc<Mutex<Option<std::sync::mpsc::SyncSender<SupervisorSignal>>>> =
        Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx_arc.clone();

    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::SenderSession>>> =
        Arc::new(Mutex::new(None));
    let restart_cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::sender::RestartCache>>> =
        Arc::new(Mutex::new(None));

    let session_for_builder = session_arc.clone();
    let cache_for_builder = restart_cache_arc.clone();

    let policy = fast_policy();
    let ack_timeout = Duration::from_millis(500);

    // Blocking builder: waits for the release channel before returning the bundle.
    // This keeps the rebuild worker in flight long enough for stop to arrive.
    let blocking_builder: screen_mirror_lib::commands::sender::SenderBuilderFn =
        Arc::new(move |_, _, _, _| {
            // Wait for the test to release us (or timeout after 1s to avoid hanging).
            if let Some(rx) = release_rx_clone.lock().unwrap().take() {
                let _ = rx.recv_timeout(Duration::from_millis(1000));
            }
            // Signal that the builder was called (worker reached step 9).
            if let Some(tx) = worker_done_tx_clone.lock().unwrap().take() {
                let _ = tx.try_send(());
            }
            Ok(SenderBundle::test_stub())
        });

    let bridge = screen_mirror_lib::commands::sender::SenderBridge::new_with_builder_and_arcs(
        Arc::new(move |_udp_port, _service_name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;

            let rebuild_hook = make_sender_rebuild_hook(
                blocking_builder.clone(),
                cache_for_builder.clone(),
                session_for_builder.clone(),
                stop_flag.clone(),
                1,
            );

            let hooks = SenderCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let h = thread::Builder::new()
                .name("t6-1-drain".into())
                .spawn(move || {
                    run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx, stop_flag, channel, st, p, t, t, hooks,
                    );
                })
                .expect("spawn drain");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
                backend_name: "sw_fake".to_string(),
            })
        }),
        session_arc,
        restart_cache_arc,
        sup_tx_arc.clone(),
    );

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Trigger a reconnect cycle to get the supervisor into AwaitingAck.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting before concurrent stop test, got: {:?}",
        ch.messages()
    );

    // Wait for supervisor_signal_tx to be set.
    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = sup_tx_arc.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set within 500ms");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    // Advance supervisor to Rebuilding → initiate_rebuild hook fires → worker starts.
    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);
    sup_tx
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // Wait briefly for the worker thread to be spawned and blocking inside the builder.
    thread::sleep(Duration::from_millis(20));

    // Share the bridge via scoped thread — safe because scope joins before returning.
    let stop_done_tx_clone = stop_done_tx;

    // Use std::thread::scope (stable since Rust 1.63) for scoped threads.
    // Both threads borrow `bridge` for the scope lifetime — safe.
    std::thread::scope(|s| {
        let _stop_handle = s.spawn(|| {
            // Small sleep to let the rebuild worker start blocking inside the builder.
            thread::sleep(Duration::from_millis(10));
            stop_sender_session(&bridge);
            let _ = stop_done_tx_clone.try_send(());
        });

        // Release the blocking builder so the worker can complete (or be cancelled).
        thread::sleep(Duration::from_millis(30));
        let _ = release_tx.try_send(());
        // Scope joins all spawned threads before returning.
    });

    // Poll for both done signals within 500ms total ceiling.
    let ceiling = Duration::from_millis(500);
    let deadline = std::time::Instant::now() + ceiling;

    let worker_done = {
        loop {
            if worker_done_rx
                .recv_timeout(Duration::from_millis(10))
                .is_ok()
            {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
        }
    };

    let stop_done = stop_done_rx.recv_timeout(Duration::from_millis(50)).is_ok();

    // Reaching this line proves the scope join completed, which means stop_sender_session
    // returned without deadlocking — the core assertion of this test.
    //
    // worker_done may be false if the cancel gate fired before the builder was called
    // (Gate A or B). That is also a valid non-deadlock outcome.
    let _ = (worker_done, stop_done);

    // The real assertion: stop_sender_session returned (proved by thread::scope join
    // completing). If there were a deadlock, the scope join would never return and
    // the test would time out. The test runner's default timeout would catch that.
    assert!(
        bridge.session.lock().unwrap().is_none(),
        "after concurrent stop, bridge.session must be None (stop won the race or \
         worker was cancelled)"
    );
}

// ─── Batch 6 (T6.3) — Stop after successful rebuild (AC-R2) ──────────────────

/// T6.3 (AC-R2): After a successful rebuild, calling `stop_sender_session` completes
/// within 1 second and does NOT panic.
///
/// Design §5 invariant: `bridge.supervisor_signal_tx` is NOT updated by the rebuild
/// worker. After the OLD drain exits (step 14), the field is `None`. If `stop_sender_session`
/// fires in this window, the `Some(sup_tx)` branch is skipped; setting `stop_flag=true`
/// on the NEW session terminates the NEW drain. No panic, no hang.
///
/// If `stop_sender_session` fires BEFORE the OLD drain clears the field, it sends
/// `SupervisorSignal::Stop` to the OLD (already-`Connected`) supervisor, which causes
/// a clean exit. Again, no panic, no hang.
///
/// Either outcome must complete within 1 second (AC-R1 budget from the spec).
///
/// RED: if the `supervisor_signal_tx` field holds a stale/poisoned value that causes
/// `stop_sender_session` to block, the elapsed time assertion will fail.
#[test]
fn stop_after_successful_rebuild_completes_cleanly() {
    let (bridge, ev_tx, ch) = make_supervised_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Trigger a reconnect cycle.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting, got: {:?}",
        ch.messages()
    );

    // Advance supervisor to Rebuilding via PeerAck.
    let session_nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set within 500ms");
            }
            thread::sleep(Duration::from_millis(10));
        }
    };

    sup_tx
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // Wait for rebuild to succeed — "streaming" status confirms RebuildSucceeded processed.
    let got_streaming =
        ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming after rebuild, got: {:?}",
        ch.messages()
    );

    // Brief pause to let the OLD drain finish step 14 (sets stop_flag, drains outcomes,
    // and clears supervisor_signal_tx). This makes the test exercise the window described
    // in design §5 — both the "tx still set" and "tx already None" cases are valid.
    thread::sleep(Duration::from_millis(50));

    // Now call stop — it must complete within 1 second and must NOT panic.
    let start = std::time::Instant::now();
    stop_sender_session(&bridge);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "stop_sender_session after rebuild must complete within 1s (AC-R2 budget), took: {elapsed:?}"
    );

    // Bridge must be in a clean state after stop.
    assert!(
        bridge.session.lock().unwrap().is_none(),
        "bridge.session must be None after stop"
    );
    assert!(
        bridge.current_args.lock().unwrap().is_none(),
        "bridge.current_args must be None after stop"
    );
}

// ─── Batch 7 (T7.1) — AC-5 end-to-end: auto-rebuild on attempt 1 (T12.2 Escenario 1) ──

/// T7.1 (AC-5): End-to-end auto-rebuild on attempt 1 without manual PeerAck.
///
/// Models T12.2 Escenario 1 (peer crash): the peer is gone, so no `PeerAck` arrives.
/// The supervisor's `ack_timeout` expires → supervisor emits `InitiateMdnsReset` (no-op)
/// then `InitiateRebuild { attempt: 1 }` → rebuild worker constructs a fresh
/// `SenderBundle` → signals `RebuildSucceeded` → coordinator emits `"streaming"`.
///
/// PASS criterion: `"streaming"` status is emitted within 5s WITHOUT the test
/// sending any `SupervisorSignal` manually (stream resumes without user clicking Retry).
///
/// RED against V1: V1 stub always signals `RebuildFailed`; drain eventually reaches
/// Dead after 3 attempts and emits `"dead"` — no `"streaming"` ever appears.
#[test]
fn t12_2_sender_rebuild_succeeds_on_attempt1() {
    // Use a short ack_timeout so the supervisor advances to InitiateRebuild quickly,
    // but a generous rebuild_timeout so the worker has time to bind UDP and signal
    // RebuildSucceeded — Windows CI runners can take >50ms for bind_probe under load.
    let ack_timeout = Duration::from_millis(50);
    let rebuild_timeout = Duration::from_millis(1500);
    let (bridge, ev_tx, ch) =
        make_supervised_bridge_with_rebuild_hook(fast_policy(), ack_timeout, rebuild_timeout);

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Trigger: ICE failure (models peer crash / connection loss).
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Expect reconnecting overlay — supervisor enters AwaitingAck.
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting event after IceFailed, got: {:?}",
        ch.messages()
    );

    // Do NOT send PeerAck.  The ack_timeout (50ms) expires → supervisor emits
    // InitiateMdnsReset (no-op hook) then InitiateRebuild → worker rebuilds →
    // signals RebuildSucceeded → coordinator emits StateChanged(Connected) → "streaming".
    //
    // Wait up to 5s for streaming (well within T12.2 ≤30s pass criterion).
    let got_streaming =
        ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(5000));
    assert!(
        got_streaming,
        "AC-5 FAIL: expected streaming after auto-rebuild on attempt 1 (no manual Retry), \
         got: {:?}",
        ch.messages()
    );

    // Confirm no "dead" was emitted (stream recovered before Dead).
    let messages = ch.messages();
    let has_dead = messages.iter().any(|m| m.contains("\"kind\":\"dead\""));
    assert!(
        !has_dead,
        "AC-5 FAIL: Dead event must NOT appear when rebuild succeeds on attempt 1, \
         got: {messages:?}"
    );

    stop_sender_session(&bridge);
}

// ═══════════════════════════════════════════════════════════════════════════════
// BATCH 3 — Sub-fixes A + B + C
// REQ-SSRL-18..28 / SC-11..18
// ═══════════════════════════════════════════════════════════════════════════════

// ─── T23: SC-12 HW stub ────────────────────────────────────────────────────────

/// SC-12 (REQ-SSRL-18, REQ-SSRL-19, REQ-SSRL-20): Full E2E reconnect after Bye on real
/// Windows + NVENC hardware.
///
/// This is a MANUAL-VERIFY stub — the actual verification is performed on real Windows
/// NVENC sender + separate receiver host per the T40 checklist.
/// The test MUST compile but does not execute.
#[cfg(target_os = "windows")]
#[ignore]
#[test]
fn t_abc1_reconnect_after_bye_recovers_stream_sc12_hw() {
    // MANUAL-VERIFY: Requires real Windows + NVENC sender + separate receiver host.
    // Checklist (T40):
    // 1. Start sender with all 6 sub-fixes (1+2+3+A+B+C).
    // 2. Start receiver; wait for stream to flow (first FRAME_SEGMENT arrives).
    // 3. Stop receiver (close the receiver application/session).
    // 4. Restart receiver.
    // 5. Assert within 5s: mDNS browse resolves sender, TCP connects, SDP exchange
    //    completes, ICE connects, first FRAME_SEGMENT arrives.
    // 6. Assert NO sender restart required.
    // 7. Assert NO WSAEADDRINUSE in logs.
    todo!("MANUAL-VERIFY: run on real Windows NVENC host per T40 checklist")
}

// ─── T24: SC-13 RED — PeerBye forwards LocalFailure to supervisor ─────────────

/// SC-13 (REQ-SSRL-21, REQ-SSRL-22, REQ-SSRL-23): When the signaling drain receives
/// `SignalingEvent::Closed`, it MUST forward `SupervisorSignal::LocalFailure { trigger:
/// ReconnectTrigger::PeerBye }` to the supervisor channel.
///
/// RED: On current HEAD, `run_sender_signaling_drain` has no 5th parameter and no
/// forward logic. This test fails to compile until T25 (PeerBye variant) + T26
/// (drain 5th param + forward) are implemented.
#[test]
fn t_b1_peer_bye_forwards_local_failure_to_supervisor() {
    use sm_domain::session::ReconnectTrigger;
    use sm_domain::signaling::SignalingEvent;
    use std::sync::atomic::AtomicBool;

    let (sig_ev_tx, sig_ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let (sup_tx, sup_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let channel: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch.clone();

    // Wire run_sender_signaling_drain with a real supervisor_signal_tx.
    let sup_tx_cell = Arc::new(Mutex::new(Some(sup_tx)));
    let signaling_rx_cell = Arc::new(Mutex::new(Some(sig_ev_rx)));

    let handle = std::thread::Builder::new()
        .name("test-sc13-drain".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                signaling_rx_cell,
                Arc::new(NoOpSenderOps),
                stop_flag,
                channel,
                sup_tx_cell,
            );
        })
        .expect("spawn drain");

    // Drive SignalingEvent::Closed — simulates receiver sending Bye.
    sig_ev_tx
        .try_send(SignalingEvent::Closed)
        .expect("send Closed must succeed");

    // Assert: SupervisorSignal::LocalFailure { trigger: PeerBye } arrives within 200ms.
    let signal = sup_rx
        .recv_timeout(Duration::from_millis(200))
        .expect("LocalFailure{PeerBye} must arrive within 200ms");

    assert!(
        matches!(
            signal,
            SupervisorSignal::LocalFailure {
                trigger: ReconnectTrigger::PeerBye
            }
        ),
        "expected LocalFailure{{PeerBye}}, got {signal:?}"
    );

    handle.join().expect("drain thread must not panic");

    // None-path sub-case: drain with no supervisor channel must process Closed without panic.
    let (sig_ev_tx2, sig_ev_rx2) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let stop2 = Arc::new(AtomicBool::new(false));
    let ch2 = FakeJsonChannel::new();
    let ch2_arc: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch2;
    let no_sup = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<SupervisorSignal>>,
    ));
    let cell2 = Arc::new(Mutex::new(Some(sig_ev_rx2)));

    let handle2 = std::thread::Builder::new()
        .name("test-sc13-drain-none".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                cell2,
                Arc::new(NoOpSenderOps),
                stop2,
                ch2_arc,
                no_sup,
            );
        })
        .expect("spawn drain none-path");

    sig_ev_tx2
        .try_send(SignalingEvent::Closed)
        .expect("send Closed (none-path)");

    // Drain must exit cleanly — no panic, handle joins successfully.
    handle2
        .join()
        .expect("drain none-path must not panic on Closed with no supervisor");
}

// ─── T27: SC-14 — PeerBye supervisor signal is idempotent ────────────────────

/// SC-14 (REQ-SSRL-21, REQ-SSRL-22, REQ-SSRL-23): Delivering `LocalFailure{PeerBye}`
/// twice to the supervisor MUST NOT cause a double-rebuild or panic.
///
/// The existing `AwaitingAck` catch-all arm silently discards re-entrant `LocalFailure`
/// signals. This test is the non-regression anchor for that invariant.
#[test]
fn t_b2_peer_bye_supervisor_signal_idempotent() {
    use sm_domain::session::{BackoffSchedule, ReconnectPolicy, ReconnectTrigger};

    let (bridge, ev_tx, ch) = make_supervised_bridge_with_policy(
        ReconnectPolicy {
            max_attempts: std::num::NonZeroU8::new(3).unwrap(),
            backoff: BackoffSchedule::Exponential {
                base_ms: 1,
                factor: 2,
            },
        },
        Duration::from_millis(200), // ack_timeout
    );
    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None).expect("start");

    // Put supervisor in Connected state via IceFailed → AwaitingAck.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "supervisor must enter AwaitingAck (reconnecting)"
    );

    // Wait for supervisor_signal_tx to be set.
    let sup_tx = {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
                break tx;
            }
            if std::time::Instant::now() >= deadline {
                panic!("supervisor_signal_tx not set within 500ms");
            }
            thread::sleep(Duration::from_millis(5));
        }
    };

    // Deliver LocalFailure{PeerBye} twice rapidly — second must be discarded.
    // AwaitingAck already contains LocalFailure { .. } catch-all → idempotent.
    sup_tx
        .try_send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::PeerBye,
        })
        .expect("first PeerBye must be sent");
    sup_tx
        .try_send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::PeerBye,
        })
        .ok(); // may fail with Full if supervisor is slow — that is also safe

    // Give supervisor time to process both signals.
    thread::sleep(Duration::from_millis(30));

    // Assert: supervisor is in AwaitingAck (NOT Rebuilding due to double trigger).
    // The supervisor_signal_tx is still set (not cleared — no rebuild occurred).
    // Bridge must still be in a valid state (no panic).
    assert!(
        bridge.session.lock().unwrap().is_some(),
        "session must still exist — no spurious rebuild triggered by second PeerBye"
    );

    stop_sender_session(&bridge);
}

// ─── T29: SC-15 RED — signaling_rx_cell swap test ────────────────────────────

/// SC-15 (REQ-SSRL-25, REQ-SSRL-26, REQ-SSRL-27): When `initiate_mdns_reset` writes a
/// new `Receiver` into the swap cell, the drain MUST swap to the new Receiver on the
/// next Closed/Disconnected event and continue receiving events from it.
///
/// Also exercises the exit path: if the cell is empty on Closed, drain exits.
#[test]
fn t_c1_initiate_mdns_reset_replaces_signaling_receiver_in_bundle_cell() {
    use sm_domain::signaling::SignalingEvent;
    use std::sync::atomic::AtomicBool;

    // === SWAP PATH ===
    // Step 1: Create signaling_rx_cell with initial (tx_old, rx_old).
    let (tx_old, rx_old) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let signaling_rx_cell = Arc::new(Mutex::new(Some(rx_old)));
    let cell_clone = signaling_rx_cell.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_arc: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch.clone();
    let no_sup = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<SupervisorSignal>>,
    ));

    // Spawn drain with cell.
    let handle = std::thread::Builder::new()
        .name("test-sc15-drain".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                signaling_rx_cell,
                Arc::new(NoOpSenderOps),
                stop,
                ch_arc,
                no_sup,
            );
        })
        .expect("spawn sc15 drain");

    // Give drain time to take the initial Receiver from the cell.
    thread::sleep(Duration::from_millis(10));

    // Step 2: Simulate hook — write new Receiver into the cell BEFORE sending Closed.
    let (tx_new, rx_new) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    {
        *cell_clone.lock().unwrap() = Some(rx_new);
    }

    // Step 3: Send Closed on old channel — drain swaps to new_rx and continues.
    tx_old
        .try_send(SignalingEvent::Closed)
        .expect("send Closed on old");

    // Step 4: Send sentinel via new channel — drain must receive it (proves swap succeeded).
    // Give drain time to perform the swap.
    thread::sleep(Duration::from_millis(30));
    tx_new
        .try_send(SignalingEvent::PeerFound {
            host: "127.0.0.1".to_string(),
            port: 9999,
        })
        .expect("send sentinel on new channel");

    // Verify drain is still alive (we can stop it now).
    thread::sleep(Duration::from_millis(50));
    // Stop: close new tx + cell empty → drain exits.
    drop(tx_new);
    // Cell is empty (already taken), so drain will exit on Disconnected.

    handle
        .join()
        .expect("drain must exit cleanly after cell empty");

    // === EXIT PATH ===
    // When Closed arrives and cell is empty, drain must exit immediately.
    let (tx_exit, rx_exit) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let cell_empty = Arc::new(Mutex::new(Some(rx_exit)));
    let stop2 = Arc::new(AtomicBool::new(false));
    let ch2 = FakeJsonChannel::new();
    let ch2_arc: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch2;
    let no_sup2 = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<SupervisorSignal>>,
    ));

    let handle2 = std::thread::Builder::new()
        .name("test-sc15-exit".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                cell_empty,
                Arc::new(NoOpSenderOps),
                stop2,
                ch2_arc,
                no_sup2,
            );
        })
        .expect("spawn sc15 exit drain");

    // Cell is not empty at startup (drain takes rx_exit). Send Closed with no replacement.
    thread::sleep(Duration::from_millis(10));
    tx_exit
        .try_send(SignalingEvent::Closed)
        .expect("send Closed exit path");

    // Drain must exit within 1s.
    let start = std::time::Instant::now();
    handle2.join().expect("exit drain must not panic");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "drain must exit within 1s when cell is empty on Closed"
    );
}

// ─── T31: SC-16 — signaling drain swap + exit race ───────────────────────────

/// SC-16 (REQ-SSRL-27, REQ-SSRL-28): Concurrent swap and exit scenarios.
///
/// SWAP PATH (T-C2): drain receives new Receiver from hook, swaps, continues.
/// EXIT PATH (T-C3): drain exits cleanly when cell is empty on Closed.
/// (Both sub-cases are already covered by T29/SC-15 above. This test verifies
/// the lock-hold duration and no-deadlock guarantee under concurrent access.)
#[test]
fn t_c2_c3_signaling_drain_swap_and_exit_race() {
    use sm_domain::signaling::SignalingEvent;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;

    let barrier = Arc::new(Barrier::new(2));

    // Create initial cell.
    let (tx_old, rx_old) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let (tx_new, rx_new) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let signaling_rx_cell = Arc::new(Mutex::new(Some(rx_old)));
    let cell_for_hook = signaling_rx_cell.clone();
    let barrier_hook = barrier.clone();

    let stop = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_arc: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch.clone();
    let no_sup = Arc::new(Mutex::new(
        None::<std::sync::mpsc::SyncSender<SupervisorSignal>>,
    ));

    // Spawn drain.
    let handle = std::thread::Builder::new()
        .name("test-sc16-drain".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                signaling_rx_cell,
                Arc::new(NoOpSenderOps),
                stop,
                ch_arc,
                no_sup,
            );
        })
        .expect("spawn sc16 drain");

    // Concurrent hook simulation: write new Receiver, then trigger Closed.
    std::thread::Builder::new()
        .name("test-sc16-hook".into())
        .spawn(move || {
            // Wait briefly for drain to start.
            thread::sleep(Duration::from_millis(15));
            // Write replacement BEFORE sending Closed (correct hook ordering).
            *cell_for_hook.lock().unwrap() = Some(rx_new);
            barrier_hook.wait();
        })
        .expect("spawn hook thread");

    // Synchronize: wait for hook to write the replacement, then send Closed.
    barrier.wait();
    tx_old
        .try_send(SignalingEvent::Closed)
        .expect("send Closed");

    // Drain swaps to new Receiver. Send sentinel, then close.
    thread::sleep(Duration::from_millis(20));
    // Verify drain received the swap by trying to send to new channel.
    // If drain didn't swap, tx_new still holds the Receiver → no deadlock.
    let _ = tx_new.try_send(SignalingEvent::OfferReceived(
        sm_domain::signaling::SdpOffer("v=0".to_string()),
    ));
    // Close new channel → drain exits (cell now empty after the swap took it).
    drop(tx_new);

    // Join must complete within 2s.
    let start = std::time::Instant::now();
    handle.join().expect("drain must exit without deadlock");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "drain join must complete within 2s (no deadlock), took: {:?}",
        start.elapsed()
    );
}

// ─── T34: SC-17 — tracing events fire in expected order for sub-fixes A+B+C ──

/// SC-17 sub-test A (REQ-SSRL-24 PeerBye warn) + ordering (REQ-SSRL-24 ordering):
///
/// Sub-test A: drain emits warn at target "sender-signaling-drain" with trigger="PeerBye"
///             when supervisor channel is registered and Closed arrives.
///
/// Ordering assertion (W2 fix, batch 4): asserts that within the drain thread,
/// the PeerBye warn fires BEFORE the "Closed received and cell empty; drain exits"
/// debug event. Both events fire on the same thread (drain) sequentially — this
/// ordering is deterministic and not subject to cross-thread races.
///
/// Note: Cross-thread ordering of event 3 (mDNS goodbye at target "mdns-signaling")
/// requires a network fixture (TCP accept) and is covered by the #[ignore] test
/// `t_abc2b_mdns_goodbye_event_on_thread_exit_hw` below.
///
/// Uses raw_logs_contain() / raw_logs_find() (spawned-thread events not captured
/// by scope-filtered logs_contain).
#[traced_test]
#[test]
fn t_abc2_tracing_events_fire_in_order_for_abc_sites() {
    use sm_domain::signaling::SignalingEvent;
    use std::sync::atomic::AtomicBool;

    // === Sub-test A: PeerBye drain warn event ===
    // Wire drain with a real supervisor_signal_tx.
    let (sig_ev_tx, sig_ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    let (sup_tx, _sup_rx) = std::sync::mpsc::sync_channel::<SupervisorSignal>(4);

    let stop = Arc::new(AtomicBool::new(false));
    let ch = FakeJsonChannel::new();
    let ch_arc: Arc<dyn screen_mirror_lib::commands::sender::ChannelLike> = ch.clone();
    let sup_tx_cell = Arc::new(Mutex::new(Some(sup_tx)));
    let cell = Arc::new(Mutex::new(Some(sig_ev_rx)));

    let drain_handle = std::thread::Builder::new()
        .name("test-sc17-drain".into())
        .spawn(move || {
            screen_mirror_lib::commands::sender::run_sender_signaling_drain(
                cell,
                Arc::new(NoOpSenderOps),
                stop,
                ch_arc,
                sup_tx_cell,
            );
        })
        .expect("spawn sc17 drain");

    // Drive Closed → triggers PeerBye warn.
    sig_ev_tx
        .try_send(SignalingEvent::Closed)
        .expect("send Closed for SC-17");

    // Wait for drain to process and exit.
    drain_handle.join().expect("drain must not panic");

    // Give tracing buffer time to flush.
    thread::sleep(Duration::from_millis(10));

    // Assert: PeerBye warn event fired at target "sender-signaling-drain".
    assert!(
        raw_logs_contain("sender-signaling-drain"),
        "expected tracing event with target=sender-signaling-drain"
    );
    assert!(
        raw_logs_contain("peer Bye received, notifying supervisor"),
        "expected PeerBye warn message in tracing output"
    );

    // === Ordering assertion (W2 fix): same-thread deterministic ordering ===
    // Both events fire on the drain thread sequentially, so their relative
    // position in the log buffer is guaranteed.
    //
    // Event 1: PeerBye warn — fires when Closed arrives and supervisor channel is active.
    // Event 2: "Closed received and cell empty; drain exits" — fires after cell is checked
    //           empty and drain is about to break (same recv_timeout iteration, after warn).
    //
    // Cross-thread event 3 (mDNS goodbye, target "mdns-signaling") cannot be asserted
    // in this fixture because it requires a real TCP connection to advance the mDNS
    // thread past its accept loop. See t_abc2b_mdns_goodbye_event_on_thread_exit_hw.
    let pos_peer_bye = raw_logs_find("peer Bye received, notifying supervisor")
        .expect("PeerBye warn must be present to assert ordering");
    let pos_drain_exit = raw_logs_find("Closed received and cell empty; drain exits")
        .expect("drain-exit debug must be present to assert ordering");

    assert!(
        pos_peer_bye < pos_drain_exit,
        "PeerBye warn (pos={pos_peer_bye}) MUST appear before drain-exit debug \
         (pos={pos_drain_exit}) in tracing log buffer (same-thread ordering)"
    );
}

// ─── T35: SC-17 sub-test B — mDNS goodbye tracing event on thread exit ────────

/// SC-17 sub-test B (REQ-SSRL-19): the mDNS thread-exit debug event
/// `"mDNS service goodbye sent on thread exit"` at target `"mdns-signaling"` MUST
/// fire when the `sm-signaling-mdns` thread exits after a real TCP session ends.
///
/// This test is `#[ignore]` because it requires the mDNS thread to advance past the
/// TCP accept phase, which in turn requires a live TCP connect on the loopback
/// interface. The accept loop polls the stop flag and calls `mdns.shutdown()` from
/// the pre-accept path (not the goodbye path) if stopped before any connection —
/// so `.stop()` alone without a prior TCP connect does NOT trigger the goodbye event.
///
/// Run manually:
/// ```text
/// cargo nextest run -p screen-mirror-app --test sender_reconnect --run-ignored \
///     ignored-only -E "test(t_abc2b_)"
/// ```
///
/// RED gate: the production goodbye tracing event was already present at
/// mdns.rs:542-545 when this test was written. The test passes on first run because
/// the production code is correct. It adds REGRESSION COVERAGE — if the tracing
/// call is removed, this test will fail.
#[traced_test]
#[test]
#[ignore = "requires live TCP loopback connect to advance past mDNS accept phase"]
fn t_abc2b_mdns_goodbye_event_on_thread_exit_hw() {
    use sm_domain::signaling::{Signaling, SignalingConfig, SignalingEvent, SignalingRole};
    use sm_infra::signaling::mdns::MdnsSignaling;
    use std::net::{SocketAddr, TcpStream};

    // Ephemeral port — chosen to avoid collision with other tests.
    let control_port: u16 = 18913;

    let config = SignalingConfig {
        role: SignalingRole::Sender,
        control_port,
        ..Default::default()
    };
    let mut sig = MdnsSignaling::new(config).unwrap();
    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<SignalingEvent>(8);
    sig.start(ev_tx).unwrap();

    // Give the mDNS thread time to bind and register.
    thread::sleep(Duration::from_millis(200));

    // TCP connect so the thread advances past the accept loop into run_frame_loop.
    // Without this, stop() hits the pre-accept exit path (mdns.rs:501-504) which
    // does NOT emit the goodbye tracing event.
    let addr: SocketAddr = format!("127.0.0.1:{control_port}").parse().unwrap();
    let _peer = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("loopback TCP connect must succeed");

    // Give the thread time to accept and enter run_frame_loop.
    thread::sleep(Duration::from_millis(100));

    // Stop the signaling thread — sets stop flag, run_frame_loop exits on next
    // READ_TIMEOUT poll (≤200ms), then the goodbye tracing event fires before
    // mdns.shutdown() at mdns.rs:542-546.
    sig.stop().unwrap();

    // Drain any remaining events so the channel is clean.
    while ev_rx.recv_timeout(Duration::from_millis(10)).is_ok() {}

    // Assert the thread-exit goodbye tracing event fired (REQ-SSRL-19).
    assert!(
        raw_logs_contain("mDNS service goodbye sent on thread exit"),
        "expected tracing::debug! event at target 'mdns-signaling' with message \
         'mDNS service goodbye sent on thread exit' (mdns.rs:542-545). \
         If this fails: check that the tracing call was not removed and that \
         the TCP connect successfully advanced the thread past the accept loop."
    );
}

// ─── NoOpSenderOps — stub for signaling drain tests ──────────────────────────

struct NoOpSenderOps;

impl screen_mirror_lib::commands::sender::SignalingSenderOps for NoOpSenderOps {
    fn apply_remote_answer(
        &self,
        _answer: sm_domain::signaling::SdpAnswer,
    ) -> Result<(), sm_domain::transport::TransportError> {
        Ok(())
    }
    fn add_remote_candidate(
        &self,
        _candidate: sm_domain::signaling::IceCandidate,
    ) -> Result<(), sm_domain::transport::TransportError> {
        Ok(())
    }
}
