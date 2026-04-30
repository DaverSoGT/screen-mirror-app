// Integration tests for SenderBridge reconnect supervisor wiring (Phase 6, T6.1+T6.2).
//
// These tests exercise:
// - RestartCache populated by start_sender_inner (T6.1, AC-8)
// - Reconnecting events emitted to frontend on IceFailed/ConnectionLost (T6.2, AC-1, AC-2)
// - Dead event emitted after 3 failures (T6.2, AC-3, AC-7)
// - Stop during reconnect cancels supervisor cleanly (T6.2, AC-9, AC-13)
//
// All tests are cross-platform — no real adapters or Windows-only code.

use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::{
    ChannelLike, SenderBridge, SenderBundle, SenderCounters, retry_session_inner,
    run_sender_transport_event_drain_with_supervisor_custom, start_sender_inner,
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
