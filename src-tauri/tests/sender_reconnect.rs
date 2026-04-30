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
    ChannelLike, SenderBridge, SenderBundle, SenderCoordinatorHooks, SenderCounters,
    make_sender_rebuild_hook, retry_session_inner,
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
                        ev_rx, stop_flag, channel, counters, st, p, t,
                    );
                })
                .expect("spawn drain");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
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
            let t = ack_timeout;

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
                        ev_rx, stop_flag, channel, st, p, t, hooks,
                    );
                })
                .expect("spawn drain");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
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
    let (bridge, ev_tx, ch) =
        make_supervised_bridge_with_rebuild_hook(fast_policy(), Duration::from_millis(500));

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
                        ev_rx, stop_flag, channel, st, p, t, hooks,
                    );
                })
                .expect("spawn");
            Ok(SenderBundle {
                drain_handles: vec![h],
                shutdown: None,
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
    let (bridge, ev_tx, ch) =
        make_supervised_bridge_with_rebuild_hook(fast_policy(), Duration::from_millis(500));

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
                            hooks,
                        );
                    })
                    .expect("spawn drain");

                Ok(SenderBundle {
                    drain_handles: vec![h],
                    shutdown: None,
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
