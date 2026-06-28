// Integration tests for StreamBridge reconnect supervisor wiring (Phase 7, T7.1+T7.2).
//
// These tests exercise:
// - RestartCache populated by start_stream_inner (T7.1, AC-8)
// - RestartCache cleared by stop_stream_session (T7.1, AC-8)
// - session_nonce is stable during a session (T7.1, AC-10)
// - Reconnecting 0x02 frames emitted on IceFailed/ConnectionLost (T7.2, AC-1, AC-2)
// - Dead 0x02 frame emitted after 3 failures (T7.2, AC-3, AC-7)
// - Stop during reconnect cancels supervisor cleanly (T7.2, AC-9, AC-13)
//
// All tests are cross-platform — no real adapters or Windows-only code.
//
// RECEIVER STATUS TRANSPORT: receiver uses 0x02 discriminant on the binary
// ChannelLike (decision #477). JSON status is sent as:
//   [0x02][json_bytes...]
// NOT via a separate Tauri emit.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicU8, Ordering};

use screen_mirror_lib::commands::sender::ChannelLike;
use screen_mirror_lib::commands::stream::{
    BundleError, ProbeFn, ReceiverBundle, StreamBridge, StreamCoordinatorHooks,
    make_stream_rebuild_hook, run_stream_transport_event_drain_with_supervisor_custom,
    run_stream_transport_event_drain_with_supervisor_custom_and_hooks, start_stream_inner,
    stop_stream_session, stop_stream_session_internal,
};
use sm_domain::session::{BackoffSchedule, ReconnectPolicy};
use sm_domain::supervisor::SupervisorSignal;
use sm_domain::transport::TransportEvent;

// ─── FakeBinaryChannel ────────────────────────────────────────────────────────

/// Records ALL raw frames (discriminant + bytes) sent via ChannelLike::send_raw.
struct FakeBinaryChannel {
    frames: Mutex<Vec<(u8, Vec<u8>)>>,
}

impl FakeBinaryChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: Mutex::new(vec![]),
        })
    }

    fn frames(&self) -> Vec<(u8, Vec<u8>)> {
        self.frames.lock().unwrap().clone()
    }

    /// Collect all frames with discriminant 0x02 decoded as JSON strings.
    fn status_messages(&self) -> Vec<String> {
        self.frames()
            .into_iter()
            .filter_map(|(disc, bytes)| {
                if disc == 0x02 {
                    String::from_utf8(bytes).ok()
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

/// Fake receiver that does nothing — satisfies the ReceiverBundle requirement.
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

/// Build a `StreamBridge` that:
/// - Spawns the supervisor-aware drain using the given policy and ack_timeout.
/// - Shares the same `supervisor_signal_tx` Arc between bridge and drain.
///
/// Returns `(bridge, ev_tx, ch)` where:
/// - `ev_tx` injects `TransportEvent`s into the drain.
/// - `ch` is the `FakeBinaryChannel`; observe it for 0x02 status frames.
fn make_supervised_stream_bridge(
    policy: ReconnectPolicy,
    ack_timeout: Duration,
) -> (
    StreamBridge,
    SyncSender<TransportEvent>,
    Arc<FakeBinaryChannel>,
) {
    let ch = FakeBinaryChannel::new();
    let ch_for_caller = ch.clone();

    let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    // Create supervisor_signal_tx BEFORE the bridge so the builder can capture it.
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;
            // Disconnected pkt_rx — mux thread will exit immediately.
            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            let h = thread::Builder::new()
                .name("supervised-stream-drain".into())
                .spawn(move || {
                    run_stream_transport_event_drain_with_supervisor_custom(
                        ev_rx, stop_flag, channel, st, p, t, t,
                    );
                })
                .expect("spawn stream drain");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        sup_tx,
    );

    (bridge, ev_tx, ch_for_caller)
}

// ─── T7.1 — RestartCache populated by start_stream_inner ──────────────────────

/// T7.1 (AC-8): After start_stream_inner, restart_cache must be populated.
#[test]
fn t7_1_restart_cache_populated_after_start() {
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(|_bind_ctx, _port, _name, _stop_flag, _channel| {
            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        sup_tx,
    );
    let ch = FakeBinaryChannel::new();

    // Provide a fake BindCtx via the bridge's test socket injection.
    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9900),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    let cache = bridge.restart_cache.lock().unwrap();
    let c = cache.as_ref().expect("RestartCache must be populated");
    assert_eq!(c.udp_port, 9900);
    assert_eq!(c.service_name, "_sm-test._tcp.local.");
    assert_ne!(c.session_nonce, 0, "session_nonce must be non-zero");
}

/// T7.1 (AC-8): After stop_stream_session, restart_cache must be cleared.
#[test]
fn t7_1_restart_cache_cleared_after_stop() {
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(|_bind_ctx, _port, _name, _stop_flag, _channel| {
            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        sup_tx,
    );
    let ch = FakeBinaryChannel::new();
    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9901),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    stop_stream_session(&bridge);

    let cache = bridge.restart_cache.lock().unwrap();
    assert!(cache.is_none(), "RestartCache must be cleared after stop");
}

/// T7.1 (AC-10): session_nonce must be stable for the lifetime of a session.
#[test]
fn t7_1_session_nonce_is_stable_during_session() {
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(|_bind_ctx, _port, _name, _stop_flag, _channel| {
            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        sup_tx,
    );
    let ch = FakeBinaryChannel::new();
    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9902),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

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
    assert_ne!(nonce1, 0, "session_nonce must be non-zero");

    stop_stream_session(&bridge);
}

// ─── T7.2 — Supervisor wiring with 0x02 emit ─────────────────────────────────

/// T7.2 (AC-1, AC-2): IceFailed → 0x02 Reconnecting frame emitted, NOT PeerLost.
#[test]
fn t7_2_ice_failed_emits_reconnecting_0x02_not_peer_lost() {
    let (bridge, ev_tx, ch) =
        make_supervised_stream_bridge(fast_policy(), Duration::from_millis(200));
    let ch_arc: Arc<dyn ChannelLike> = ch.clone() as Arc<dyn ChannelLike>;
    start_stream_inner(
        &bridge,
        ch_arc,
        Some(9910),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    ev_tx.send(TransportEvent::IceFailed).unwrap();

    let got = ch.wait_for_status_containing("reconnecting", Duration::from_secs(2));
    assert!(got, "0x02 reconnecting frame must be emitted on IceFailed");

    let msgs = ch.status_messages();
    assert!(
        !msgs.iter().any(|m| m.contains("peer_lost")),
        "peer_lost must NOT be emitted when supervisor is wired"
    );

    stop_stream_session(&bridge);
}

/// T7.2 (AC-1, AC-2): ConnectionLost → 0x02 Reconnecting frame emitted.
#[test]
fn t7_2_connection_lost_emits_reconnecting_0x02() {
    let (bridge, ev_tx, ch) =
        make_supervised_stream_bridge(fast_policy(), Duration::from_millis(200));
    let ch_arc: Arc<dyn ChannelLike> = ch.clone() as Arc<dyn ChannelLike>;
    start_stream_inner(
        &bridge,
        ch_arc,
        Some(9911),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    ev_tx
        .send(TransportEvent::ConnectionLost {
            reason: "poll_output error".to_string(),
        })
        .unwrap();

    let got = ch.wait_for_status_containing("reconnecting", Duration::from_secs(2));
    assert!(
        got,
        "0x02 reconnecting frame must be emitted on ConnectionLost"
    );

    stop_stream_session(&bridge);
}

/// T7.2 (AC-3, AC-7): Three rebuild failures → 0x02 dead frame with reason.
#[test]
fn t7_2_three_rebuild_failures_emit_dead_0x02() {
    let (bridge, ev_tx, ch) =
        make_supervised_stream_bridge(fast_policy(), Duration::from_millis(50));
    let ch_arc: Arc<dyn ChannelLike> = ch.clone() as Arc<dyn ChannelLike>;
    start_stream_inner(
        &bridge,
        ch_arc,
        Some(9912),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    // Trigger first failure.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // The supervisor now drives rebuild attempts internally: the noop initiate_rebuild
    // hook immediately signals RebuildFailed, so all 3 attempts exhaust without the
    // test needing to inject more IceFailed events. Just wait for the dead 0x02 frame.
    // (The old approach of sending more IceFailed was needed before the hook was wired;
    //  with the hook in place the coordinator exits before those sends can land anyway.)
    let got = ch.wait_for_status_containing("dead", Duration::from_secs(5));
    assert!(got, "0x02 dead frame must be emitted after 3 failures");

    stop_stream_session(&bridge);
}

/// T7.2 (AC-9, AC-13): Stop during reconnect cancels supervisor cleanly.
#[test]
fn t7_2_stop_during_reconnect_cancels_supervisor_cleanly() {
    let (bridge, ev_tx, ch) =
        make_supervised_stream_bridge(fast_policy(), Duration::from_millis(200));
    let ch_arc: Arc<dyn ChannelLike> = ch.clone() as Arc<dyn ChannelLike>;
    start_stream_inner(
        &bridge,
        ch_arc,
        Some(9913),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .unwrap();

    // Trigger reconnect.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Wait briefly so supervisor enters backoff sleep.
    let got = ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(got, "must be in reconnecting state before stopping");

    // Stop should cancel supervisor and return within 2s (AC-9, AC-13).
    stop_stream_session(&bridge);
    // If we reach here without hanging, the test passes (no orphan threads).
}

// ─── Batch 1 (T1.3) — stop_stream_session_internal extraction contract ────────

/// T1.3 (AC-NR1): `stop_stream_session_internal` tears down the session but
/// does NOT clear `restart_cache` or `current_args`.
///
/// Symmetric to T1.1 for StreamBridge.
#[test]
fn stop_stream_session_internal_leaves_restart_cache_intact() {
    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let bridge = StreamBridge::new_with_builder_and_sup_tx(
        Arc::new(|_bind_ctx, _port, _name, _stop_flag, _channel| {
            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        sup_tx,
    );
    let ch = FakeBinaryChannel::new();

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9920),
        Some("_sm-internal-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Verify preconditions.
    assert!(
        bridge.restart_cache.lock().unwrap().is_some(),
        "restart_cache must be Some before internal stop"
    );
    assert!(
        bridge.current_args.lock().unwrap().is_some(),
        "current_args must be Some before internal stop"
    );

    // Call the internal variant — partial teardown only.
    stop_stream_session_internal(&bridge);

    // restart_cache must still be Some (internal does NOT clear it).
    assert!(
        bridge.restart_cache.lock().unwrap().is_some(),
        "restart_cache must remain Some after stop_stream_session_internal"
    );

    // current_args must still be Some (internal does NOT clear it).
    assert!(
        bridge.current_args.lock().unwrap().is_some(),
        "current_args must remain Some after stop_stream_session_internal"
    );
}

// ─── Batch 3 (T3.x) — Stream rebuild worker ──────────────────────────────────

/// Build a supervised StreamBridge whose `initiate_rebuild` hook is the V2 worker.
///
/// The bridge builder (injected via `StreamBridge.builder`) returns a no-op
/// ReceiverBundle with a single supervised drain thread — cross-platform safe.
///
/// The V2 rebuild hook is constructed via `make_stream_rebuild_hook` and wired
/// into the drain via `run_stream_transport_event_drain_with_supervisor_custom_and_hooks`.
///
/// Returns `(bridge, ev_tx, ch)` identical in shape to `make_supervised_stream_bridge`.
fn make_supervised_stream_bridge_with_rebuild_hook(
    policy: ReconnectPolicy,
    ack_timeout: Duration,
    rebuild_timeout: Duration,
) -> (
    StreamBridge,
    SyncSender<TransportEvent>,
    Arc<FakeBinaryChannel>,
) {
    let ch = FakeBinaryChannel::new();
    let ch_for_caller = ch.clone();

    let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    // Pre-allocate the session and restart_cache arcs BEFORE building the bridge.
    // Both the bridge (via new_with_builder_and_arcs) and the rebuild hook (captured
    // in the builder closure) share the SAME arc pointers.
    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamSession>>> =
        Arc::new(Mutex::new(None));
    let restart_cache_arc: Arc<
        Mutex<Option<screen_mirror_lib::commands::stream::StreamRestartCache>>,
    > = Arc::new(Mutex::new(None));

    let session_for_builder = session_arc.clone();
    let cache_for_builder = restart_cache_arc.clone();

    let bridge = screen_mirror_lib::commands::stream::StreamBridge::new_with_builder_and_arcs(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
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
            let rebuild_hook = make_stream_rebuild_hook(
                // Inner builder: returns a no-op ReceiverBundle — cross-platform safe.
                Arc::new(|_, _, _, _, _| {
                    let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
                    Ok(ReceiverBundle {
                        receiver: Box::new(FakeReceiverOps),
                        pkt_rx,
                        signaling: None,
                        drain_handles: vec![],
                        _drain_senders: vec![],
                        counters: Arc::new(
                            screen_mirror_lib::commands::stream::BridgeCounters::default(),
                        ),
                    })
                }),
                cache_for_builder.clone(),
                session_for_builder.clone(),
                stop_flag.clone(),
                1,    // attempt — fixed at 1 for this helper
                None, // probe_fn — use real bind_probe
            );

            let hooks = StreamCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            let h = thread::Builder::new()
                .name("supervised-stream-drain-v2".into())
                .spawn(move || {
                    run_stream_transport_event_drain_with_supervisor_custom_and_hooks(
                        ev_rx,
                        stop_flag,
                        channel,
                        st,
                        p,
                        ack_t,
                        rebuild_t,
                        hooks,
                        // Media-arrival watchdog disabled — this V2 rebuild-hook
                        // bridge does not exercise the post-rebuild watchdog.
                        None,
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
                        Arc::new(AtomicU8::new(1)), // T1.9: default epoch — test doesn't drive stale-guard
                    );
                })
                .expect("spawn stream drain");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        session_arc,
        restart_cache_arc,
        sup_tx,
    );

    (bridge, ev_tx, ch_for_caller)
}

/// T3.1 (AC-R4, AC-6): Happy path — stream rebuild hook spawns a worker that
/// calls the builder and signals `RebuildSucceeded`, causing the drain to emit
/// a "streaming" 0x02 status frame.
///
/// RED against V1: V1 stub always signals `RebuildFailed` → drain emits "dead",
/// assertion `streaming_before_dead` fails.
#[test]
fn rebuild_hook_calls_builder_and_signals_succeeded() {
    let (bridge, ev_tx, ch) = make_supervised_stream_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9940),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Trigger a reconnect cycle.
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    let got_reconnecting =
        ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting status, got: {:?}",
        ch.status_messages()
    );

    // Obtain session nonce for PeerAck.
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

    // The V2 hook spawns a worker that signals RebuildSucceeded.
    // The drain maps StateChanged(Connected) → emit "streaming" 0x02 frame.
    let got_streaming = ch.wait_for_status_containing("streaming", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming status after successful stream rebuild, got: {:?}",
        ch.status_messages()
    );

    // Must NOT have emitted "dead" before "streaming" (attempt 1 succeeded).
    let msgs = ch.status_messages();
    let streaming_idx = msgs.iter().position(|m| m.contains("streaming"));
    let dead_idx = msgs.iter().position(|m| m.contains("dead"));
    assert!(
        streaming_idx.is_some(),
        "streaming status must be present, got: {msgs:?}"
    );
    if let Some(d) = dead_idx {
        let s = streaming_idx.unwrap();
        assert!(
            s < d,
            "streaming must appear before dead (rebuild succeeded on attempt 1), got: {msgs:?}"
        );
    }

    stop_stream_session(&bridge);
}

/// T3.3 (AC-R4): Builder error — stream rebuild hook signals `RebuildFailed`.
///
/// Verifies the worker actually calls the builder (observable via call_count),
/// which the V1 stub does NOT do.
#[test]
fn rebuild_hook_signals_failed_on_builder_error() {
    use std::sync::atomic::AtomicU32;

    let call_count = Arc::new(AtomicU32::new(0));
    let call_count_for_hook = call_count.clone();

    let ch = FakeBinaryChannel::new();
    let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    // Pre-allocate arcs shared between bridge and rebuild hook.
    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamSession>>> =
        Arc::new(Mutex::new(None));
    let cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamRestartCache>>> =
        Arc::new(Mutex::new(None));
    let session_clone = session_arc.clone();
    let cache_clone = cache_arc.clone();

    let policy = fast_policy();
    let ack_timeout = Duration::from_millis(500);

    let bridge = screen_mirror_lib::commands::stream::StreamBridge::new_with_builder_and_arcs(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
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
            let failing_builder: screen_mirror_lib::commands::stream::BuilderFn =
                Arc::new(move |_, _, _, _, _| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                    Err(screen_mirror_lib::commands::stream::BundleError::Other(
                        "injected failure".to_string(),
                    ))
                });

            let rebuild_hook = make_stream_rebuild_hook(
                failing_builder,
                cache_clone.clone(),
                session_clone.clone(),
                stop_flag.clone(),
                1,
                None, // probe_fn — use real bind_probe
            );

            let hooks = StreamCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            let h = thread::Builder::new()
                .name("failing-stream-drain".into())
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
                        // Media-arrival watchdog disabled for this drain.
                        None,
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
                        Arc::new(AtomicU8::new(1)), // T1.9: default epoch — test doesn't drive stale-guard
                    );
                })
                .expect("spawn");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        session_arc,
        cache_arc,
        sup_tx,
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9941),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting, got: {:?}",
        ch.status_messages()
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

    sup_tx_guard
        .try_send(SupervisorSignal::PeerAck {
            session_nonce,
            attempt: 1,
        })
        .ok();

    // Wait up to 2s for the builder to be called.
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
        "builder must be called by the stream rebuild worker, but call_count={}",
        call_count.load(Ordering::Relaxed)
    );

    stop_stream_session(&bridge);
}

/// T3.5 (AC-R2): Successful rebuild swaps the stream session —
/// new stop_flag differs from old.
///
/// RED against V1: V1 stub never swaps the session, so Arc identity is unchanged.
#[test]
fn rebuild_swaps_session_new_stop_flag_differs_from_old() {
    let (bridge, ev_tx, ch) = make_supervised_stream_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9942),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Capture the original stop_flag Arc before rebuild.
    let original_stop_flag = bridge
        .session
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.stop_flag.clone())
        .expect("session must be Some after start");

    ev_tx.send(TransportEvent::IceFailed).unwrap();
    ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));

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
    let got_streaming = ch.wait_for_status_containing("streaming", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming after stream rebuild, got: {:?}",
        ch.status_messages()
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
        "session must be Some after successful stream rebuild"
    );
    let new_stop_flag = new_stop_flag.unwrap();
    assert!(
        !Arc::ptr_eq(&original_stop_flag, &new_stop_flag),
        "stream rebuild must install a fresh stop_flag Arc (new Arc identity), but got the same pointer"
    );

    stop_stream_session(&bridge);
}

// ─── Batch 3 fix — multi-generation stream rebuild chain (AC-6 regression guard) ──

/// Regression test: after TWO consecutive stream rebuilds, `bridge.session` holds
/// the second-generation (B2) session — not B1 or the original.
///
/// Mirrors `rebuild_can_chain_across_generations_swaps_bridge_session_each_time`
/// in sender_reconnect.rs. Same structure, same bug-prevention goal.
///
/// AC-6: auto-rebuild after mDNS reset (attempt 2) must observe the real bridge
/// session arcs — NOT dummies — to correctly swap into the live bridge state.
#[test]
fn stream_rebuild_can_chain_across_generations_swaps_bridge_session_each_time() {
    use screen_mirror_lib::commands::stream::{BuilderFn, StreamSession};
    use std::sync::atomic::AtomicU32;

    /// Inner helper: run a two-generation stream rebuild chain.
    /// `use_real_arcs`: if true, B1's hook uses the real bridge arcs (the fix).
    ///                  if false, B1's hook uses dummy arcs (the bug).
    /// Returns `(b1_ptr, b2_ptr)` — the stop_flag Arc pointers after each rebuild.
    fn run_chain(
        use_real_arcs: bool,
    ) -> (
        Arc<std::sync::atomic::AtomicBool>,
        Option<Arc<std::sync::atomic::AtomicBool>>,
    ) {
        let session_arc: Arc<Mutex<Option<StreamSession>>> = Arc::new(Mutex::new(None));
        let cache_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamRestartCache>>> =
            Arc::new(Mutex::new(None));

        let ch = FakeBinaryChannel::new();

        // Three generations of ev_rx + sup_tx (B0, B1, B2).
        let (ev_tx_b0, ev_rx0) = sync_channel::<TransportEvent>(8);
        let (ev_tx_b1, ev_rx1) = sync_channel::<TransportEvent>(8);
        let (_ev_tx_b2, ev_rx2) = sync_channel::<TransportEvent>(8);

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
        let builder_slot: Arc<Mutex<Option<BuilderFn>>> = Arc::new(Mutex::new(None));

        let session_b = session_arc.clone();
        let cache_b = cache_arc.clone();
        let sup_b0_b = sup_tx_b0.clone();
        let sup_b1_b = sup_tx_b1.clone();
        let sup_b2_b = sup_tx_b2.clone();
        let builder_slot_b = builder_slot.clone();
        let session_b2 = session_arc.clone();

        let policy = fast_policy();
        let ack_timeout = Duration::from_millis(500);

        let the_builder: BuilderFn =
            Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
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

                // Inner builder for the hook — the self-referencing builder so each
                // generation spawns a proper supervised drain when rebuilt.
                let inner_builder: BuilderFn = builder_slot_b
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("builder_slot populated");

                // Bridge arcs for the hook — the property under test.
                //
                // B0's hook ALWAYS uses real arcs so the first rebuild (B0→B1) succeeds.
                // B1's hook is where the bug would manifest:
                //   BUGGY (use_real_arcs=false): B1's hook gets dummy arcs — worker reads
                //     cache=None → RebuildFailed → bridge.session stays on B1.
                //   FIXED (use_real_arcs=true): B1's hook gets real arcs — worker reads
                //     real cache → builds B2 → swaps into bridge.session.
                let (hook_session, hook_cache) = if generation == 0 || use_real_arcs {
                    (session_b.clone(), cache_b.clone())
                } else {
                    // Simulate the dummy-arcs bug: inner call got Arc::new(Mutex::new(None)).
                    (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)))
                };

                let rebuild_hook = make_stream_rebuild_hook(
                    inner_builder,
                    hook_cache,
                    hook_session,
                    stop_flag.clone(),
                    generation + 1,
                    None, // probe_fn — use real bind_probe
                );

                let hooks = StreamCoordinatorHooks {
                    publish_reconnect_request: Arc::new(|_, _| {}),
                    publish_reconnect_ack: Arc::new(|_, _| {}),
                    initiate_rebuild: rebuild_hook,
                    initiate_mdns_reset: Arc::new(|| {}),
                };

                let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
                let p = policy.clone();
                let t = ack_timeout;
                let h = thread::Builder::new()
                    .name(format!("stream-chain-g{generation}-drain"))
                    .spawn(move || {
                        run_stream_transport_event_drain_with_supervisor_custom_and_hooks(
                            ev_rx,
                            stop_flag,
                            channel,
                            sup_tx_slot,
                            p,
                            t,
                            t,
                            hooks,
                            // Media-arrival watchdog disabled — this generation-chain
                            // test does not exercise the post-rebuild watchdog. (The
                            // CAP-2-v3 cross-generation cap is covered deterministically
                            // by the in-module SC-WD-CAP tests sharing one counter Arc.)
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
                    signaling: None,
                    drain_handles: vec![h],
                    _drain_senders: vec![],
                    counters: Arc::new(
                        screen_mirror_lib::commands::stream::BridgeCounters::default(),
                    ),
                })
            });

        *builder_slot.lock().unwrap() = Some(the_builder.clone());

        let bridge = screen_mirror_lib::commands::stream::StreamBridge::new_with_builder_and_arcs(
            the_builder,
            session_arc.clone(),
            cache_arc.clone(),
            sup_tx_b0.clone(),
        );

        // Phase 0: start B0.
        start_stream_inner(
            &bridge,
            ch.clone() as Arc<dyn ChannelLike>,
            Some(9943),
            Some("_sm-test._tcp.local.".to_string()),
        )
        .expect("start B0");

        // Phase 1: first rebuild B0 → B1.
        ev_tx_b0.send(TransportEvent::IceFailed).unwrap();
        let got_rc1 = ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
        assert!(got_rc1, "phase1 reconnecting missing");

        // Let AwaitingAck time out (ack_timeout=500ms) → supervisor fires
        // InitiateRebuild naturally (no PeerAck needed).
        let got_streaming1 =
            ch.wait_for_status_containing("streaming", Duration::from_millis(3000));
        assert!(
            got_streaming1,
            "phase1 streaming missing, status: {:?}",
            ch.status_messages()
        );

        let b1_stop_flag = session_b2
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.stop_flag.clone())
            .expect("B1 session after first rebuild");

        // Phase 2: second rebuild B1 → B2.
        let streaming_before = ch
            .status_messages()
            .iter()
            .filter(|m| m.contains("streaming"))
            .count();
        let reconnecting_before = ch
            .status_messages()
            .iter()
            .filter(|m| m.contains("reconnecting"))
            .count();

        ev_tx_b1.send(TransportEvent::IceFailed).unwrap();

        // Wait for a NEW reconnecting event (B1's supervisor entered AwaitingAck).
        let got_rc2 = {
            let dl = std::time::Instant::now() + Duration::from_millis(1000);
            loop {
                let cnt = ch
                    .status_messages()
                    .iter()
                    .filter(|m| m.contains("reconnecting"))
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
        //   REAL arcs: rebuild succeeds → "streaming"
        //   DUMMY arcs: rebuild fails (cache=None) → 3 attempts → "dead"
        let resolved = {
            let dl = std::time::Instant::now() + Duration::from_millis(4000);
            loop {
                let msgs = ch.status_messages();
                let new_streaming =
                    msgs.iter().filter(|m| m.contains("streaming")).count() > streaming_before;
                let dead = msgs.iter().any(|m| m.contains("dead"));
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
            "phase2 did not resolve within 4s, status: {:?}",
            ch.status_messages()
        );

        // Small yield to let the worker finish the swap (step 11 happens before step 13).
        thread::sleep(Duration::from_millis(50));

        // Read bridge.session AFTER rebuild resolves.
        let b2_stop_flag = session_b2
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.stop_flag.clone());

        stop_stream_session(&bridge);

        (b1_stop_flag, b2_stop_flag)
    }

    // ── RED check: dummy arcs (simulates the pre-fix bug) ──────────────────
    // With dummy arcs for B1's hook, the worker reads cache=None → RebuildFailed
    // immediately, without touching bridge.session.
    {
        let (b1, b2_opt) = run_chain(false /* dummy arcs = pre-fix bug */);
        let b2 = b2_opt.expect("bridge.session must be Some even with dummy arcs (still holds B1)");
        assert!(
            Arc::ptr_eq(&b1, &b2),
            "DUMMY arcs: expected bridge.session still holds B1 after second rebuild \
             (worker read cache=None from dummy arc → RebuildFailed without touching session). \
             Got different Arcs — test setup is incorrect."
        );
    }

    // ── GREEN check: real arcs (the fix) ─────────────────────────────────
    // With real arcs for B1's hook, the worker reads the actual cache, builds B2,
    // and swaps it into bridge.session → b2_stop_flag is a NEW Arc distinct from B1.
    {
        let (b1, b2_opt) = run_chain(true /* real arcs = post-fix */);
        let b2 = b2_opt.expect("bridge.session must be Some after second rebuild");
        assert!(
            !Arc::ptr_eq(&b1, &b2),
            "REAL arcs: expected bridge.session updated to B2 after second rebuild \
             (worker swapped into real bridge.session). Got same Arc — production builder \
             is NOT passing real arcs to make_stream_rebuild_hook inner call (Batch 2 bug repeated)."
        );
    }
}

// ─── Batch 4 — bind_probe retry (stream rebuild, AC-R5) ──────────────────────

/// T4.1 (AC-R5): Stream rebuild worker retries `bind_probe` up to 3× on
/// `PortInUse` before signaling `RebuildFailed`.
///
/// Design §6: "bind_probe with retries: 3 attempts × 100ms sleep between attempts.
/// If all 3 retries return Err(BundleError::PortInUse(_)), signal RebuildFailed."
///
/// This test injects a `ProbeFn` that always returns `PortInUse` so the supervisor
/// will observe `RebuildFailed`. It exercises the RETRY COUNT in one worker invocation,
/// not the success path (the success path is exercised by the ack-timeout chain tests).
///
/// RED evidence (MAX_PROBE_ATTEMPTS = 1, no retry loop):
///   - probe called exactly 1 time per worker invocation
///   - assertion `probe_call_count == 3` fails (got 1)
///
/// GREEN after T4.2 (MAX_PROBE_ATTEMPTS = 3, retry loop):
///   - probe called 3 times per worker: attempts 0, 1, 2 all return PortInUse
///   - retry loop runs twice (for calls 1 and 2) with 100ms sleep each
///   - RebuildFailed is signaled after all 3 attempts exhausted
///   - `probe_call_count == 3` passes
///
/// The test uses `ack_timeout=300ms` so the supervisor fires `InitiateMdnsReset` →
/// Rebuilding automatically, without requiring a PeerAck (whose nonce would not match
/// the drain's randomly-generated `my_nonce` anyway).
#[test]
fn rebuild_releases_udp_port_before_rebind() {
    use std::sync::atomic::AtomicU32;

    // probe_fn: always returns PortInUse — exercises the retry counter.
    let probe_call_count = Arc::new(AtomicU32::new(0));
    let probe_count_for_fn = probe_call_count.clone();

    let probe_fn: ProbeFn = Arc::new(move |port| {
        probe_count_for_fn.fetch_add(1, Ordering::Relaxed);
        Err(BundleError::PortInUse(port))
    });

    // Build a bridge where the rebuild hook uses the injectable probe_fn.
    let ch = FakeBinaryChannel::new();
    let ch_for_caller = ch.clone();

    let (ev_tx, ev_rx) = sync_channel::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx.clone();

    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamSession>>> =
        Arc::new(Mutex::new(None));
    let restart_cache_arc: Arc<
        Mutex<Option<screen_mirror_lib::commands::stream::StreamRestartCache>>,
    > = Arc::new(Mutex::new(None));

    let session_for_builder = session_arc.clone();
    let cache_for_builder = restart_cache_arc.clone();

    // Policy with max_attempts=1: supervisor fires ONE rebuild attempt and then Dead.
    // This isolates the retry count to a single worker invocation — if the probe is
    // called 3 times, the retry loop ran 3 times IN THE SAME WORKER.
    //
    // Without retry (MAX_PROBE_ATTEMPTS=1): probe called 1×, RebuildFailed → Dead.
    //   calls == 1 → assertion `>= 3` FAILS (RED).
    // With retry (MAX_PROBE_ATTEMPTS=3): probe called 3× in ONE worker, RebuildFailed → Dead.
    //   calls == 3 → assertion `>= 3` PASSES (GREEN).
    let policy = ReconnectPolicy {
        max_attempts: std::num::NonZeroU8::new(1).unwrap(),
        backoff: fast_policy().backoff,
    };
    let ack_timeout = Duration::from_millis(300);
    let probe_fn_for_builder = probe_fn.clone();

    let bridge = screen_mirror_lib::commands::stream::StreamBridge::new_with_builder_and_arcs(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;

            let rebuild_hook = make_stream_rebuild_hook(
                // Inner builder: always fails — we only test the probe retry count.
                Arc::new(|_, _, _, _, _| {
                    Err(BundleError::Other(
                        "intentional builder failure for probe retry test".to_string(),
                    ))
                }),
                cache_for_builder.clone(),
                session_for_builder.clone(),
                stop_flag.clone(),
                1,
                Some(probe_fn_for_builder.clone()), // inject the always-PortInUse probe
            );

            let hooks = StreamCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
            let h = thread::Builder::new()
                .name("retry-probe-stream-drain".into())
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
                        // Media-arrival watchdog disabled for this drain.
                        None,
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
                        Arc::new(AtomicU8::new(1)), // T1.9: default epoch — test doesn't drive stale-guard
                    );
                })
                .expect("spawn stream drain");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        session_arc,
        restart_cache_arc,
        sup_tx,
    );

    // Start bridge on an arbitrary port (probe_fn ignores the port value).
    start_stream_inner(
        &bridge,
        ch_for_caller.clone() as Arc<dyn ChannelLike>,
        Some(9944),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Trigger reconnect cycle → supervisor enters AwaitingAck(1).
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch_for_caller.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting status, got: {:?}",
        ch_for_caller.status_messages()
    );

    // Wait for the supervisor to auto-fire Rebuilding after ack_timeout (300ms),
    // then for the worker to complete its retry loop (~3 × 100ms = 300ms).
    // Total: ~600ms. We wait up to 1500ms to be safe on slow CI.
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    loop {
        let calls = probe_call_count.load(Ordering::Relaxed);
        if calls >= 3 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    // Primary assertion: the retry loop must exhaust all MAX_PROBE_ATTEMPTS (3) attempts
    // before signaling RebuildFailed. With no retry (MAX_PROBE_ATTEMPTS=1), calls == 1.
    let calls = probe_call_count.load(Ordering::Relaxed);
    assert!(
        calls >= 3,
        "bind_probe must be called at least MAX_PROBE_ATTEMPTS (3) times per worker \
         invocation (design §6: retry loop). Got {calls} calls — retry loop is missing."
    );

    stop_stream_session(&bridge);
}

// ─── Batch 6 (T6.5) — Stream: concurrent stop + stop-after-rebuild (AC-R1/AC-R2) ──

/// T6.5a (AC-R1): `stop_stream_session` called concurrently while a stream rebuild
/// worker is in flight does NOT deadlock; both the stop and the worker complete.
///
/// Symmetric to `rebuild_does_not_deadlock_during_concurrent_stop` in sender_reconnect.rs.
/// Uses a blocking builder and std::thread::scope to prove no deadlock path exists.
#[test]
fn stream_rebuild_does_not_deadlock_during_concurrent_stop() {
    use std::sync::mpsc::sync_channel as sc;

    // Blocking builder: waits for the release signal before returning a bundle.
    let (release_tx, release_rx) = sc::<()>(1);
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));

    // Completion signal: builder signals "called" (worker reached step 9).
    let (worker_done_tx, worker_done_rx) = sc::<()>(1);
    let worker_done_tx = Arc::new(Mutex::new(Some(worker_done_tx)));

    // stop_done: stop_stream_session returned.
    let (stop_done_tx, stop_done_rx) = sc::<()>(1);

    let release_rx_clone = release_rx.clone();
    let worker_done_tx_clone = worker_done_tx.clone();

    let ch = FakeBinaryChannel::new();

    let (ev_tx, ev_rx) = sc::<TransportEvent>(8);
    let ev_rx_slot: Arc<Mutex<Option<std::sync::mpsc::Receiver<TransportEvent>>>> =
        Arc::new(Mutex::new(Some(ev_rx)));
    let ev_rx_slot_clone = ev_rx_slot.clone();

    let sup_tx_arc: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>> = Arc::new(Mutex::new(None));
    let sup_tx_for_drain = sup_tx_arc.clone();

    let session_arc: Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamSession>>> =
        Arc::new(Mutex::new(None));
    let restart_cache_arc: Arc<
        Mutex<Option<screen_mirror_lib::commands::stream::StreamRestartCache>>,
    > = Arc::new(Mutex::new(None));

    let session_for_builder = session_arc.clone();
    let cache_for_builder = restart_cache_arc.clone();

    let policy = fast_policy();
    let ack_timeout = Duration::from_millis(500);

    // Blocking inner builder: waits for release.
    let blocking_inner: screen_mirror_lib::commands::stream::BuilderFn =
        Arc::new(move |_, _, _, _, _| {
            if let Some(rx) = release_rx_clone.lock().unwrap().take() {
                let _ = rx.recv_timeout(Duration::from_millis(1000));
            }
            if let Some(tx) = worker_done_tx_clone.lock().unwrap().take() {
                let _ = tx.try_send(());
            }
            let (_pkt_tx, pkt_rx) = sc::<sm_domain::encode::EncodedPacket>(1);
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        });

    let bridge = screen_mirror_lib::commands::stream::StreamBridge::new_with_builder_and_arcs(
        Arc::new(move |_bind_ctx, _port, _name, stop_flag, channel| {
            let ev_rx = ev_rx_slot_clone
                .lock()
                .unwrap()
                .take()
                .expect("ev_rx taken once");
            let st = sup_tx_for_drain.clone();
            let p = policy.clone();
            let t = ack_timeout;

            let rebuild_hook = make_stream_rebuild_hook(
                blocking_inner.clone(),
                cache_for_builder.clone(),
                session_for_builder.clone(),
                stop_flag.clone(),
                1,
                None,
            );

            let hooks = StreamCoordinatorHooks {
                publish_reconnect_request: Arc::new(|_, _| {}),
                publish_reconnect_ack: Arc::new(|_, _| {}),
                initiate_rebuild: rebuild_hook,
                initiate_mdns_reset: Arc::new(|| {}),
            };

            let (_pkt_tx, pkt_rx) = sc::<sm_domain::encode::EncodedPacket>(1);
            let h = thread::Builder::new()
                .name("t65-stream-drain".into())
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
                        // Media-arrival watchdog disabled for this drain.
                        None,
                        // CAP-2-v3: watchdog inert here — no cap, throwaway counter, no arm.
                        None,
                        Arc::new(AtomicU8::new(0)),
                        false,
                        Arc::new(AtomicU8::new(1)), // T1.9: default epoch — test doesn't drive stale-guard
                    );
                })
                .expect("spawn");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
                counters: Arc::new(screen_mirror_lib::commands::stream::BridgeCounters::default()),
            })
        }),
        session_arc,
        restart_cache_arc,
        sup_tx_arc.clone(),
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9960),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Trigger reconnect cycle.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting, got: {:?}",
        ch.status_messages()
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

    // Advance supervisor to Rebuilding.
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

    // Wait briefly for the worker to be spawned and blocking in the builder.
    thread::sleep(Duration::from_millis(20));

    let stop_done_tx_in_scope = stop_done_tx;

    // Scoped threads: both borrow `bridge` safely.
    std::thread::scope(|s| {
        let _stop_handle = s.spawn(|| {
            thread::sleep(Duration::from_millis(10));
            stop_stream_session(&bridge);
            let _ = stop_done_tx_in_scope.try_send(());
        });

        // Release the blocking builder after stop has had a chance to fire.
        thread::sleep(Duration::from_millis(30));
        let _ = release_tx.try_send(());
    });

    // Poll for worker done (500ms ceiling).
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    let _worker_done = loop {
        if worker_done_rx
            .recv_timeout(Duration::from_millis(10))
            .is_ok()
        {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
    };

    let _stop_done = stop_done_rx.recv_timeout(Duration::from_millis(50)).is_ok();

    // Reaching here without timeout proves no deadlock (scope join returned).
    assert!(
        bridge.session.lock().unwrap().is_none(),
        "after concurrent stop, bridge.session must be None"
    );
}

/// T6.5b (AC-R2): After a successful stream rebuild, calling `stop_stream_session`
/// completes within 1 second and does NOT panic.
///
/// Symmetric to `stop_after_successful_rebuild_completes_cleanly` in sender_reconnect.rs.
/// Verifies design §5 invariant for StreamBridge: `supervisor_signal_tx` is not
/// updated by the rebuild worker; subsequent stop works regardless.
#[test]
fn stop_after_successful_stream_rebuild_completes_cleanly() {
    let (bridge, ev_tx, ch) = make_supervised_stream_bridge_with_rebuild_hook(
        fast_policy(),
        Duration::from_millis(500),
        Duration::from_millis(500),
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9961),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Trigger a reconnect cycle.
    ev_tx.send(TransportEvent::IceFailed).unwrap();
    let got_reconnecting =
        ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting, got: {:?}",
        ch.status_messages()
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

    // Wait for rebuild to succeed — "streaming" 0x02 frame confirms RebuildSucceeded processed.
    let got_streaming = ch.wait_for_status_containing("streaming", Duration::from_millis(2000));
    assert!(
        got_streaming,
        "expected streaming after rebuild, got: {:?}",
        ch.status_messages()
    );

    // Brief pause to let the OLD drain finish step 14 (sets stop_flag, drains outcomes,
    // clears supervisor_signal_tx). Tests the window described in design §5.
    thread::sleep(Duration::from_millis(50));

    // Stop must complete within 1 second and must NOT panic.
    let start = std::time::Instant::now();
    stop_stream_session(&bridge);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "stop_stream_session after rebuild must complete within 1s (AC-R2 budget), took: {elapsed:?}"
    );

    assert!(
        bridge.session.lock().unwrap().is_none(),
        "bridge.session must be None after stop"
    );
    assert!(
        bridge.current_args.lock().unwrap().is_none(),
        "bridge.current_args must be None after stop"
    );
}

// ─── Batch 7 (T7.2) — AC-6 end-to-end: auto-rebuild on attempt 1 (T12.2 Escenario 1) ──

/// T7.2 (AC-6): End-to-end auto-rebuild on attempt 1 without manual PeerAck.
///
/// Models T12.2 Escenario 1 (peer crash) for the StreamBridge: the peer is gone,
/// so no `PeerAck` arrives.  The supervisor's `ack_timeout` expires → supervisor emits
/// `InitiateMdnsReset` (no-op) then `InitiateRebuild { attempt: 1 }` → worker
/// constructs a fresh `ReceiverBundle` → signals `RebuildSucceeded` → coordinator
/// emits a `"streaming"` 0x02 status frame.
///
/// PASS criterion: `"streaming"` 0x02 frame is received within 5s WITHOUT the test
/// sending any `SupervisorSignal` manually (stream resumes without user clicking Retry).
///
/// RED against V1: V1 stub always signals `RebuildFailed`; drain eventually reaches
/// Dead after 3 attempts and emits `"dead"` — no `"streaming"` ever appears.
#[test]
fn t12_2_stream_rebuild_succeeds_on_attempt1() {
    // Use a short ack_timeout so the supervisor advances to InitiateRebuild quickly,
    // but a generous rebuild_timeout so the worker has time to bind UDP and signal
    // RebuildSucceeded — Windows CI runners can take >50ms for bind_probe under load.
    let ack_timeout = Duration::from_millis(50);
    let rebuild_timeout = Duration::from_millis(1500);
    let (bridge, ev_tx, ch) = make_supervised_stream_bridge_with_rebuild_hook(
        fast_policy(),
        ack_timeout,
        rebuild_timeout,
    );

    start_stream_inner(
        &bridge,
        ch.clone() as Arc<dyn ChannelLike>,
        Some(9962),
        Some("_sm-test._tcp.local.".to_string()),
    )
    .expect("start must succeed");

    // Trigger: ICE failure (models peer crash / connection loss).
    ev_tx.send(TransportEvent::IceFailed).unwrap();

    // Expect reconnecting overlay — supervisor enters AwaitingAck.
    let got_reconnecting =
        ch.wait_for_status_containing("reconnecting", Duration::from_millis(500));
    assert!(
        got_reconnecting,
        "expected reconnecting status after IceFailed, got: {:?}",
        ch.status_messages()
    );

    // Do NOT send PeerAck.  The ack_timeout (50ms) expires → supervisor emits
    // InitiateMdnsReset (no-op hook) then InitiateRebuild → worker rebuilds →
    // signals RebuildSucceeded → coordinator emits StateChanged(Connected) → "streaming".
    //
    // Wait up to 5s for streaming (well within T12.2 ≤30s pass criterion).
    let got_streaming = ch.wait_for_status_containing("streaming", Duration::from_millis(5000));
    assert!(
        got_streaming,
        "AC-6 FAIL: expected streaming after auto-rebuild on attempt 1 (no manual Retry), \
         got: {:?}",
        ch.status_messages()
    );

    // Confirm no "dead" was emitted (stream recovered before Dead).
    let status_messages = ch.status_messages();
    let has_dead = status_messages.iter().any(|m| m.contains("dead"));
    assert!(
        !has_dead,
        "AC-6 FAIL: Dead event must NOT appear when rebuild succeeds on attempt 1, \
         got: {status_messages:?}"
    );

    stop_stream_session(&bridge);
}
