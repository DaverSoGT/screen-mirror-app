// Integration tests for StreamBridge production coordinator wiring (Batch 6, CRITICAL-2).
//
// Symmetric to sender_coordinator.rs — tests that StreamCoordinatorHooks are invoked
// by the stream coordinator when the supervisor emits the corresponding outcomes.
//
// TDD cycle (Strict TDD Mode): tests written FIRST (RED), then implementation.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::ChannelLike;
use screen_mirror_lib::commands::stream::{
    ReceiverBundle, StreamBridge, StreamCoordinatorHooks,
    run_stream_transport_event_drain_with_supervisor_custom_and_hooks, start_stream_inner,
    stop_stream_session,
};
use sm_domain::session::{BackoffSchedule, ReconnectPolicy};
use sm_domain::supervisor::SupervisorSignal;
use sm_domain::transport::TransportEvent;

// ─── FakeBinaryChannel ────────────────────────────────────────────────────────

struct FakeBinaryChannel {
    frames: Mutex<Vec<(u8, Vec<u8>)>>,
}

impl FakeBinaryChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::new(vec![]),
        })
    }

    fn status_messages(&self) -> Vec<String> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(disc, bytes)| {
                if *disc == 0x02 {
                    String::from_utf8(bytes.clone()).ok()
                } else {
                    None
                }
            })
            .collect()
    }

    fn wait_for_status_containing(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.status_messages().iter().any(|m| m.contains(needle)) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl ChannelLike for FakeBinaryChannel {
    fn send_raw(&self, discriminant: u8, bytes: Vec<u8>) -> Result<(), String> {
        self.frames.lock().unwrap().push((discriminant, bytes));
        Ok(())
    }
}

// ─── Fake receiver for ReceiverBundle ─────────────────────────────────────────

struct FakeReceiverOps;
impl screen_mirror_lib::commands::stream::ReceiverOps for FakeReceiverOps {
    fn request_keyframe(&self) -> Result<(), sm_domain::transport::TransportError> {
        Ok(())
    }
    fn dropped_frames(&self) -> u64 {
        0
    }
    fn stop(&mut self) -> Result<(), sm_domain::transport::TransportError> {
        Ok(())
    }
}

struct FakeSignalingOps;
impl screen_mirror_lib::commands::stream::SignalingOps for FakeSignalingOps {
    fn stop(&mut self) -> Result<(), sm_domain::signaling::SignalingError> {
        Ok(())
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn fast_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: std::num::NonZeroU8::new(3).unwrap(),
        backoff: BackoffSchedule::Exponential {
            base_ms: 1,
            factor: 2,
        },
    }
}

fn wait_for_sup_tx(bridge: &StreamBridge, timeout: Duration) -> SyncSender<SupervisorSignal> {
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

/// Build a `StreamBridge` with custom coordinator hooks (counters) and a fast policy.
///
/// `ack_timeout` is parametrized because tests have conflicting needs:
///   - The PeerRequest-driven InitiateRebuild test wants a GENEROUS timeout (2s) so the
///     deterministic loser path is the ONLY way InitiateRebuild can fire — never the
///     AwaitingAck wall-clock fallback.
///   - The InitiateMdnsReset test exercises the ack-timeout branch itself and needs a
///     SHORT timeout (200ms) so the timeout actually fires within the test deadline.
///
/// `rebuild_result_signal` is the `SupervisorSignal` the counting `initiate_rebuild`
/// hook feeds back after incrementing its counter. Tests that exercise the retry /
/// ack-timeout path pass `RebuildFailed` (the supervisor re-enters `AwaitingAck` and
/// keeps escalating). The InitiateRebuild test passes `RebuildSucceeded` so the
/// supervisor transitions `Rebuilding` → `Connected` and parks on a blocking
/// `signal_rx.recv()` with NO second `AwaitingAck` timeout — making its `reset_count==0`
/// assert structurally guaranteed instead of bounded by the wall-clock `ack_timeout`.
#[allow(clippy::type_complexity)]
fn make_stream_bridge_with_counting_hooks(
    ack_timeout: Duration,
    rebuild_result_signal: SupervisorSignal,
) -> (
    StreamBridge,
    SyncSender<TransportEvent>,
    Arc<FakeBinaryChannel>,
    Arc<AtomicU32>, // rebuild
    Arc<AtomicU32>, // publish_req
    Arc<AtomicU32>, // publish_ack
    Arc<AtomicU32>, // mdns_reset
) {
    let ch = FakeBinaryChannel::new();
    let ch_for_caller = ch.clone();

    let rebuild_count = Arc::new(AtomicU32::new(0));
    let publish_req_count = Arc::new(AtomicU32::new(0));
    let publish_ack_count = Arc::new(AtomicU32::new(0));
    let reset_count = Arc::new(AtomicU32::new(0));

    let rb_c = rebuild_count.clone();
    let pr_c = publish_req_count.clone();
    let pa_c = publish_ack_count.clone();
    let re_c = reset_count.clone();

    let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    let policy = fast_policy();

    let (dummy_pkt_tx, dummy_pkt_rx) = sync_channel(1);
    let dummy_pkt_rx_slot: Arc<
        Mutex<Option<std::sync::mpsc::Receiver<sm_domain::encode::EncodedPacket>>>,
    > = Arc::new(Mutex::new(Some(dummy_pkt_rx)));
    let dummy_pkt_rx_slot_c = dummy_pkt_rx_slot.clone();
    drop(dummy_pkt_tx); // disconnect it so mux thread exits fast

    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let pkt_rx = dummy_pkt_rx_slot_c
                .lock()
                .unwrap()
                .take()
                .expect("pkt_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;

            let rb = rb_c.clone();
            let pr = pr_c.clone();
            let pa = pa_c.clone();
            let re = re_c.clone();
            let rebuild_signal = rebuild_result_signal.clone();
            let hooks = StreamCoordinatorHooks {
                publish_reconnect_request: Arc::new(move |_attempt, _nonce| {
                    pr.fetch_add(1, Ordering::Relaxed);
                }),
                publish_reconnect_ack: Arc::new(move |_attempt, _nonce| {
                    pa.fetch_add(1, Ordering::Relaxed);
                }),
                initiate_rebuild: Arc::new(move |signal_tx| {
                    rb.fetch_add(1, Ordering::Relaxed);
                    // Feed back the parametrized rebuild result (no real bundle) so the
                    // supervisor advances. RebuildFailed → retry/escalate; RebuildSucceeded
                    // → Connected (no second AwaitingAck timeout).
                    let _ = signal_tx.try_send(rebuild_signal.clone());
                }),
                initiate_mdns_reset: Arc::new(move || {
                    re.fetch_add(1, Ordering::Relaxed);
                }),
            };

            let h = thread::Builder::new()
                .name("stream-coord-test-drain".into())
                .spawn(move || {
                    run_stream_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx,
                        stop_flag,
                        channel,
                        st,
                        p,
                        t,
                        t,
                        hooks,
                        // Media-arrival watchdog disabled — this coordinator test
                        // does not exercise the post-rebuild watchdog.
                        None,
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
                        Arc::new(AtomicU8::new(1)), // T1.9: default epoch — test doesn't drive stale-guard
                    );
                })
                .expect("spawn drain");

            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: Some(Box::new(FakeSignalingOps)),
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
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

/// CRITICAL-2 (stream): InitiateRebuild invokes the initiate_rebuild hook.
///
/// De-flake: uses the deterministic PeerRequest-loser drive. The supervisor is a Receiver
/// and the peer_role sent is Sender (role-differ), so the Receiver always defers →
/// PublishReconnectAck + InitiateRebuild emitted immediately, signal-driven,
/// nonce-independent (no ack_timeout dependency, and NO SRR suppression on the stream
/// `SupervisorOutcome::InitiateRebuild` handling in `handle_supervisor_outcome`).
/// See `ReconnectSupervisor`'s AwaitingAck `is_active_reconnector` false branch.
#[test]
fn coordinator_invokes_builder_on_initiate_rebuild() {
    // 2s ack_timeout: generous enough that the deterministic PeerRequest-loser path is the
    // ONLY way InitiateRebuild can fire — NOT the AwaitingAck wall-clock fallback (which
    // would also emit InitiateMdnsReset). The publish_ack==1 / reset==0 asserts below pin
    // that we took the loser path, not the timeout fallback.
    // De-flake (wall-clock decoupling): the counting initiate_rebuild hook feeds back
    // RebuildSucceeded (not RebuildFailed) so the supervisor goes Rebuilding → Connected
    // and parks on a blocking signal_rx.recv() — it never re-enters a timeout-bearing
    // AwaitingAck{attempt=2}. That makes the reset_count==0 assert STRUCTURALLY guaranteed
    // rather than bounded by the 2s ack_timeout (the prior RebuildFailed path re-armed
    // AwaitingAck and could, under pathological starvation, emit InitiateMdnsReset before
    // the assert ran). rebuild_count is still incremented inside the hook BEFORE the signal.
    let (bridge, ev_tx, ch, rebuild_count, _pr, publish_ack_count, reset_count) =
        make_stream_bridge_with_counting_hooks(
            Duration::from_secs(2),
            SupervisorSignal::RebuildSucceeded,
        );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(19999),
        None,
    )
    .expect("start must succeed");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got =
        ch.wait_for_status_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting 0x02 after IceFailed");

    let sup_tx = wait_for_sup_tx(&bridge, Duration::from_millis(500));

    // De-flake (engram #1171): the stream supervisor has role=Receiver. Sending PeerRequest
    // with peer_role=Sender creates a role-differ scenario: the Receiver always defers →
    // supervisor emits PublishReconnectAck AND InitiateRebuild immediately, signal-driven,
    // nonce-independent (no ack_timeout dependency). The stream coordinator's
    // `SupervisorOutcome::InitiateRebuild` handling in `handle_supervisor_outcome` has NO
    // SRR suppression (unlike the sender), so the rebuild hook fires unconditionally.
    sup_tx
        .send(SupervisorSignal::PeerRequest {
            peer_nonce: 0,
            peer_role: sm_domain::signaling::SignalingRole::Sender,
            attempt: 1,
        })
        .expect("supervisor signal channel must accept PeerRequest");

    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while rebuild_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        rebuild_count.load(Ordering::Relaxed) >= 1,
        "stream initiate_rebuild hook must be called on InitiateRebuild outcome"
    );

    // Pin the DETERMINISTIC loser path (not the ack_timeout fallback): the loser path
    // publishes exactly one ReconnectAck, and the AwaitingAck timeout fallback (which emits
    // InitiateMdnsReset + InitiateRebuild) did NOT run.
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

    stop_stream_session(&bridge);
}

/// CRITICAL-2 (stream): PublishReconnectRequest invokes the hook.
#[test]
fn coordinator_calls_publish_reconnect_request_hook_on_outcome() {
    // Generous timeout: the request hook fires on entering AwaitingAck, well before any
    // ack_timeout would matter.
    let (bridge, ev_tx, ch, _rb, publish_req_count, _pa, _re) =
        make_stream_bridge_with_counting_hooks(
            Duration::from_secs(2),
            SupervisorSignal::RebuildFailed,
        );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(19998),
        None,
    )
    .expect("start must succeed");

    ev_tx.send(TransportEvent::IceFailed).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while publish_req_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        publish_req_count.load(Ordering::Relaxed) >= 1,
        "stream publish_reconnect_request hook must be called on PublishReconnectRequest outcome"
    );

    stop_stream_session(&bridge);
}

/// CRITICAL-2 (stream): InitiateMdnsReset invokes the hook (ack timeout path).
#[test]
fn coordinator_calls_mdns_reset_hook_on_initiate_mdns_reset() {
    // Short timeout: this test exercises the ack-timeout branch itself, so the 200ms
    // AwaitingAck timeout MUST fire within the test deadline.
    let (bridge, ev_tx, ch, _rb, _pr, _pa, reset_count) = make_stream_bridge_with_counting_hooks(
        Duration::from_millis(200),
        SupervisorSignal::RebuildFailed,
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(19997),
        None,
    )
    .expect("start must succeed");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got =
        ch.wait_for_status_containing("\"kind\":\"reconnecting\"", Duration::from_millis(500));
    assert!(got, "expected reconnecting 0x02");

    // No PeerAck — let ack_timeout (200ms) fire → InitiateMdnsReset.
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    while reset_count.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert!(
        reset_count.load(Ordering::Relaxed) >= 1,
        "stream initiate_mdns_reset hook must be called on InitiateMdnsReset outcome"
    );

    stop_stream_session(&bridge);
}
