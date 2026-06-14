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
                suppress_bye_on_rebuild: None,
                stop_signaling_on_rebuild: None,
                disarm_escalation_on_rebuild: None,
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
/// Sequence: start → IceConnected (latches ice_connected=true) → IceFailed →
/// await AwaitingAck → send PeerRequest (loser path) → supervisor emits
/// PublishReconnectAck AND InitiateRebuild signal-driven → hooks.initiate_rebuild called.
///
/// De-flake: uses the deterministic PeerRequest-loser drive (a role-equal Sender
/// tie with peer_nonce=0 resolves to "peer wins" via `decide_tiebreak`, so the
/// supervisor emits PublishReconnectAck + InitiateRebuild immediately, without
/// depending on the ack_timeout). See `ReconnectSupervisor` AwaitingAck handling
/// (the `is_active_reconnector` false branch → `SupervisorOutcome::InitiateRebuild`).
#[test]
fn coordinator_invokes_builder_on_initiate_rebuild() {
    // 2s ack_timeout (per make_bridge_with_counting_hooks' doc for PeerRequest-driven
    // tests): generous enough that the multi-step deterministic setup (latch
    // ice_connected → IceFailed → wait sup_tx → dispatch PeerRequest) completes while
    // the supervisor is still in AwaitingAck. With this timeout the ONLY way
    // InitiateRebuild can fire is the deterministic PeerRequest-loser path — NOT the
    // AwaitingAck wall-clock fallback (which would also emit InitiateMdnsReset). The
    // publish_ack==1 / reset==0 asserts below pin that we took the loser path, not the
    // timeout fallback.
    let (bridge, ev_tx, ch, rebuild_count, _pr, publish_ack_count, reset_count) =
        make_bridge_with_counting_hooks(Duration::from_secs(2));

    start_sender_inner(&bridge, ch.clone() as Arc<dyn ChannelLike>, None, None)
        .expect("start must succeed");

    // De-flake (REQ-SRR-1 guard): send IceConnected first so ice_connected latches true,
    // making the sender SRR guard (`!ice_connected && peer_ack_seen`) INERT — the
    // PeerRequest-loser InitiateRebuild fires the rebuild hook immediately, signal-driven,
    // with NO dependency on the ack_timeout. The role-equal tie-break is deterministic:
    // the peer_nonce passed below is 0, and the tie-break evaluates `my_nonce < peer_nonce`
    // (i.e. `my_nonce < 0`), which is ALWAYS false for a u64 — so the sender ALWAYS Defers
    // (peer wins), for every possible my_nonce, with zero collision case. The PeerRequest-loser
    // path then emits InitiateRebuild via the supervisor, which is NOT suppressed once
    // ice_connected=true.
    ev_tx.send(TransportEvent::IceConnected).unwrap();
    // Wait for the drain to process IceConnected (it emits a streaming status frame).
    let streaming =
        ch.wait_for_message_containing("\"kind\":\"streaming\"", Duration::from_millis(500));
    assert!(
        streaming,
        "expected streaming event after IceConnected (ice_connected latched)"
    );

    // Trigger reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait for reconnecting{1} event — supervisor is now in AwaitingAck.
    let got =
        ch.wait_for_message_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting event after IceFailed");

    let sup_tx = wait_for_sup_tx(&bridge, Duration::from_millis(500));

    // De-flake (engram #1171): the supervisor's my_nonce is an INDEPENDENT
    // rand::random(), NOT restart_cache.session_nonce, so a PeerAck keyed on the
    // cache nonce is rejected as stale and the rebuild only fired via the ack_timeout
    // — flaky under CI load. Instead drive the deterministic PeerRequest-loser path:
    // with peer_nonce=0 the role-equal (Sender) tie-break (`decide_tiebreak`,
    // `my_nonce < peer_nonce`) is ALWAYS Defer ("peer wins"), so the supervisor emits
    // PublishReconnectAck AND InitiateRebuild immediately, signal-driven, with zero
    // wall-clock-timeout dependency (the `is_active_reconnector` false branch in
    // `ReconnectSupervisor`'s AwaitingAck handling). IceConnected was sent first so the
    // sender SRR guard is INERT and InitiateRebuild fires unconditionally.
    sup_tx
        .send(SupervisorSignal::PeerRequest {
            peer_nonce: 0,
            peer_role: sm_domain::signaling::SignalingRole::Sender,
            attempt: 1,
        })
        .expect("supervisor signal channel must accept PeerRequest");

    // Wait for rebuild hook to be called.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while rebuild_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        rebuild_count.load(Ordering::Relaxed) >= 1,
        "initiate_rebuild hook must be called at least once on InitiateRebuild outcome"
    );

    // Pin the DETERMINISTIC loser path (not the ack_timeout fallback): the loser path
    // publishes exactly one ReconnectAck, and the AwaitingAck timeout fallback (which
    // emits InitiateMdnsReset + InitiateRebuild) did NOT run. If these fail the test was
    // previously green only via the wall-clock fallback.
    assert_eq!(
        publish_ack_count.load(Ordering::Relaxed),
        1,
        "loser path must publish exactly one ReconnectAck (proves the deterministic \
         PeerRequest-loser path drove InitiateRebuild, not the ack_timeout fallback)"
    );
    assert_eq!(
        reset_count.load(Ordering::Relaxed),
        0,
        "initiate_mdns_reset must NOT fire — the AwaitingAck wall-clock timeout fallback \
         (which emits InitiateMdnsReset) must not have run"
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

    // Use peer_nonce = 0 to deterministically lose the race. The role-equal
    // (Sender vs Sender) tie-break in `decide_tiebreak` is `my_nonce < peer_nonce`
    // ⇒ ActiveReconnector, else Defer. With peer_nonce = 0 the comparison is
    // `my_nonce < 0`, which is false for EVERY u64 (including my_nonce == 0, since
    // 0 < 0 is false), so the supervisor ALWAYS takes the Defer / "peer wins" loser
    // branch and emits PublishReconnectAck — deterministic for every my_nonce, with
    // no collision case. The supervisor's own nonce (generated via rand::random in
    // the drain thread) and the independent cache nonce from `start_sender_inner`
    // are both irrelevant to the outcome. Use blocking send so the signal is not
    // silently dropped if the supervisor's signal channel buffer is momentarily full.
    let peer_nonce: u64 = 0;
    // Role-equal (both Sender) ⇒ the nonce fallback decides: `my_nonce < 0` is always
    // false ⇒ the sender supervisor always Defers to the "peer wins" / loser branch.
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
