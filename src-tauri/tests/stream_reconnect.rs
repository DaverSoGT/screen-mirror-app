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

use screen_mirror_lib::commands::sender::ChannelLike;
use screen_mirror_lib::commands::stream::{
    ReceiverBundle, StreamBridge, run_stream_transport_event_drain_with_supervisor_custom,
    start_stream_inner, stop_stream_session,
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
                        ev_rx, stop_flag, channel, st, p, t,
                    );
                })
                .expect("spawn stream drain");
            Ok(ReceiverBundle {
                receiver: Box::new(FakeReceiverOps),
                pkt_rx,
                signaling: None,
                drain_handles: vec![h],
                _drain_senders: vec![],
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
