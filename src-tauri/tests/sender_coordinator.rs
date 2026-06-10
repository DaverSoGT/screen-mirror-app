// Integration tests for SenderBridge production coordinator wiring (Batch 6, CRITICAL-2).
//
// These tests exercise the PRODUCTION coordinator paths — specifically that the
// coordinator hooks are actually called when the supervisor emits:
//   - InitiateRebuild          → builder closure invoked (counted)
//   - PublishReconnectRequest  → signaling closure invoked (counted)
//   - PublishReconnectAck      → signaling closure invoked (counted)
//   - InitiateMdnsReset        → mdns_reset closure invoked (counted)
//
// The existing sender_reconnect.rs tests cover the event-emission layer (reconnecting/dead
// status messages) but use no-op coordinator hooks. These tests add a second seam:
// a `SenderCoordinatorHooks` struct (or equivalent) accepted by `enter_supervisor_mode`
// so production wiring can be unit-tested without a real Windows capture stack.
//
// TDD cycle (Strict TDD Mode): tests written FIRST (RED), then implementation.
//
// Naming convention: coordinator_invokes_<what>_on_<outcome>

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::{
    ChannelLike, NoopSignalingRefresh, SenderBridge, SenderBundle, SenderCoordinatorHooks,
    SignalingSupervisorRefresh, run_sender_transport_event_drain_with_supervisor_custom_and_hooks,
    start_sender_inner, stop_sender_session,
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

    fn wait_for_message_containing(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self
                .messages
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains(needle))
            {
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

/// Fast reconnect policy for tests: 1ms base, factor 2.
fn fast_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: std::num::NonZeroU8::new(3).unwrap(),
        backoff: BackoffSchedule::Exponential {
            base_ms: 1,
            factor: 2,
        },
    }
}

/// Wait for the supervisor_signal_tx to be populated by the drain thread.
fn wait_for_sup_tx(bridge: &SenderBridge, timeout: Duration) -> SyncSender<SupervisorSignal> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(tx) = bridge.supervisor_signal_tx.lock().unwrap().clone() {
            return tx;
        }
        if std::time::Instant::now() >= deadline {
            panic!("supervisor_signal_tx not set within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Build a `SenderBridge` with custom coordinator hooks (counters) and a fast policy.
///
/// `ack_timeout` is parametrized because tests have conflicting needs:
///   - Tests that drive the supervisor through PeerAck/PeerRequest before the
///     timeout fires want a generous timeout (2s) so that multi-step setup
///     (reading nonce + waiting for sup_tx + dispatching the signal) finishes
///     while the supervisor is still in AwaitingAck.
///   - Tests that exercise the ack-timeout branch itself (InitiateMdnsReset)
///     need a short timeout (200ms) so the timeout actually fires within the
///     test deadline.
///
/// Returns `(bridge, ev_tx, ch, rebuild_count, publish_req_count, publish_ack_count, reset_count)`.
#[allow(clippy::type_complexity)]
fn make_bridge_with_counting_hooks(
    ack_timeout: Duration,
) -> (
    SenderBridge,
    std::sync::mpsc::SyncSender<TransportEvent>,
    Arc<FakeJsonChannel>,
    Arc<AtomicU32>, // rebuild invocation count
    Arc<AtomicU32>, // publish_reconnect_request invocation count
    Arc<AtomicU32>, // publish_reconnect_ack invocation count
    Arc<AtomicU32>, // mdns_reset invocation count
) {
    let ch = FakeJsonChannel::new();
    let ch_for_caller = ch.clone();

    let rebuild_count = Arc::new(AtomicU32::new(0));
    let publish_req_count = Arc::new(AtomicU32::new(0));
    let publish_ack_count = Arc::new(AtomicU32::new(0));
    let reset_count = Arc::new(AtomicU32::new(0));

    let rebuild_count_c = rebuild_count.clone();
    let publish_req_count_c = publish_req_count.clone();
    let publish_ack_count_c = publish_ack_count.clone();
    let reset_count_c = reset_count.clone();

    let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    let policy = fast_policy();

    let bridge = SenderBridge::new_with_builder_and_sup_tx(
        Arc::new(move |_, _, stop_flag, channel, _attempt| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;

            // Coordinator hooks: counting closures.
            let rb = rebuild_count_c.clone();
            let pr = publish_req_count_c.clone();
            let pa = publish_ack_count_c.clone();
            let re = reset_count_c.clone();
            let hooks = SenderCoordinatorHooks {
                publish_reconnect_request: Arc::new(move |_attempt, _nonce| {
                    pr.fetch_add(1, Ordering::Relaxed);
                }),
                publish_reconnect_ack: Arc::new(move |_attempt, _nonce| {
                    pa.fetch_add(1, Ordering::Relaxed);
                }),
                initiate_rebuild: Arc::new(move |signal_tx| {
                    rb.fetch_add(1, Ordering::Relaxed);
                    // Immediately signal failure (no real bundle) so supervisor advances.
                    let _ = signal_tx.try_send(SupervisorSignal::RebuildFailed);
                }),
                initiate_mdns_reset: Arc::new(move || {
                    re.fetch_add(1, Ordering::Relaxed);
                }),
                sender_attempt: Arc::new(AtomicU8::new(1)), // T1.10: default epoch — test doesn't drive epoch
            };

            let h = thread::Builder::new()
                .name("coord-test-drain".into())
                .spawn(move || {
                    run_sender_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx,
                        stop_flag,
                        channel,
                        st,
                        p,
                        t,
                        t,
                        hooks,
                        Arc::new(NoopSignalingRefresh) as Arc<dyn SignalingSupervisorRefresh>,
                        None, // watchdog disabled in coordinator tests
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
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

    (
        bridge,
        ev_tx,
        ch_for_caller,
        rebuild_count,
        publish_req_count,
        publish_ack_count,
        reset_count,
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// CRITICAL-2: When the supervisor emits `InitiateRebuild`, the coordinator MUST
/// invoke the `initiate_rebuild` hook.
///
/// Sequence: start → IceFailed → await AwaitingAck → send PeerAck → coordinator
/// should receive InitiateRebuild from supervisor → hooks.initiate_rebuild called.
#[test]
fn coordinator_invokes_builder_on_initiate_rebuild() {
    let (bridge, ev_tx, ch, rebuild_count, _pr, _pa, _re) =
        make_bridge_with_counting_hooks(Duration::from_millis(200));

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Trigger reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for reconnecting{1} event — supervisor is now in AwaitingAck.
    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting event after IceFailed");

    // Get session nonce from restart_cache.
    let nonce = bridge
        .restart_cache
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.session_nonce)
        .unwrap_or(1);

    let sup_tx = wait_for_sup_tx(&bridge, Duration::from_millis(500));

    // Send PeerAck to advance supervisor from AwaitingAck → Rebuilding → InitiateRebuild.
    sup_tx
        .try_send(SupervisorSignal::PeerAck {
            session_nonce: nonce,
            attempt: 1,
        })
        .ok();

    // Wait for rebuild hook to be called.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while rebuild_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        rebuild_count.load(Ordering::Relaxed) >= 1,
        "initiate_rebuild hook must be called at least once on InitiateRebuild outcome"
    );

    stop_sender_session(&bridge);
}

/// CRITICAL-2: When the supervisor emits `PublishReconnectRequest`, the coordinator
/// MUST invoke the `publish_reconnect_request` hook.
///
/// Sequence: start → IceFailed → supervisor enters AwaitingAck → emits PublishReconnectRequest.
/// The hook should be invoked before any PeerAck arrives.
#[test]
fn coordinator_calls_publish_reconnect_request_hook_on_outcome() {
    let (bridge, ev_tx, ch, _rb, publish_req_count, _pa, _re) =
        make_bridge_with_counting_hooks(Duration::from_millis(200));

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Trigger reconnect — supervisor emits PublishReconnectRequest immediately.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for the hook to be called (should happen within ~100ms of IceFailed).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while publish_req_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        publish_req_count.load(Ordering::Relaxed) >= 1,
        "publish_reconnect_request hook must be called when supervisor emits PublishReconnectRequest"
    );

    stop_sender_session(&bridge);
}

/// CRITICAL-2: When the supervisor emits `InitiateMdnsReset`, the coordinator MUST
/// invoke the `initiate_mdns_reset` hook.
///
/// Sequence: start → IceFailed → AwaitingAck → ack_timeout fires → supervisor emits
/// InitiateMdnsReset (TCP fallback path, ack_timeout = 200ms).
#[test]
fn coordinator_calls_mdns_reset_hook_on_initiate_mdns_reset() {
    let (bridge, ev_tx, ch, _rb, _pr, _pa, reset_count) =
        make_bridge_with_counting_hooks(Duration::from_millis(200));

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Trigger reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for reconnecting event.
    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting event");

    // Do NOT send PeerAck — let the ack_timeout (200ms) fire, which causes the
    // supervisor to emit InitiateMdnsReset.
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    while reset_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert!(
        reset_count.load(Ordering::Relaxed) >= 1,
        "initiate_mdns_reset hook must be called when supervisor emits InitiateMdnsReset (ack timeout)"
    );

    stop_sender_session(&bridge);
}

/// CRITICAL-2: When the supervisor emits `PublishReconnectAck` (loser side of race),
/// the coordinator MUST invoke the `publish_reconnect_ack` hook.
///
/// Sequence: start → IceFailed (we are winner/loser) → PeerRequest → supervisor
/// emits PublishReconnectAck for the losing side.
#[test]
fn coordinator_calls_publish_reconnect_ack_hook_on_outcome() {
    // ack_timeout = 2s — production value. Setup races below the timeout in <50ms.
    let (bridge, ev_tx, ch, _rb, publish_req_count, publish_ack_count, _re) =
        make_bridge_with_counting_hooks(Duration::from_millis(2000));

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // Trigger reconnect so supervisor enters AwaitingAck state.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for publish_reconnect_request hook to fire — confirms the supervisor
    // observed IceFailed and is now in AwaitingAck. Polling the hook counter is
    // more robust than waiting on the JSON channel under heavy nextest concurrency.
    let req_deadline = std::time::Instant::now() + Duration::from_millis(2000);
    while publish_req_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < req_deadline
    {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        publish_req_count.load(Ordering::Relaxed) >= 1,
        "publish_reconnect_request must fire before peer race injection (supervisor not in AwaitingAck)"
    );

    // sup_tx is populated by the drain thread once the supervisor exists.
    let sup_tx = wait_for_sup_tx(&bridge, Duration::from_millis(1000));

    // Use peer_nonce = 0 to deterministically lose the race: the supervisor's
    // own nonce is generated via rand::random::<u64>() inside the drain thread
    // (sender.rs line 655) and is NOT the same as restart_cache.session_nonce
    // (sender.rs line 933 — start_sender_inner generates an independent nonce
    // for the cache). Since rand::random::<u64>() returns 0 with probability
    // 2^-64, peer_nonce=0 is virtually always strictly less than the
    // supervisor's nonce, so the supervisor will deterministically take the
    // "peer wins" branch and emit PublishReconnectAck. Use blocking send so
    // the signal is not silently dropped if the supervisor's signal channel
    // buffer is momentarily full.
    let peer_nonce: u64 = 0;
    // Role-equal (both Sender) so the legacy nonce fallback decides: peer_nonce=0
    // < my_nonce ⇒ the sender supervisor takes the "peer wins" / loser branch.
    sup_tx
        .send(SupervisorSignal::PeerRequest {
            peer_nonce,
            peer_role: sm_domain::signaling::SignalingRole::Sender,
            attempt: 1,
        })
        .expect("supervisor signal channel must accept PeerRequest");

    // Wait for ack hook to fire.
    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    while publish_ack_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }

    assert!(
        publish_ack_count.load(Ordering::Relaxed) >= 1,
        "publish_reconnect_ack hook must be called when supervisor emits PublishReconnectAck"
    );

    stop_sender_session(&bridge);
}
