//! Transport-loopback integration tests.
//!
//! These tests exercise the full signaling + transport wiring using
//! [`LoopbackSignaling`] and the str0m-backed sender/receiver pair on loopback
//! UDP sockets. No real network is required; both sides bind `127.0.0.1:0`
//! (ephemeral port) and communicate over localhost.
//!
//! # Known limitation — DTLS/ICE loopback
//!
//! A single-process loopback WebRTC session (sender + receiver on the same host,
//! both running in the same Rust test process) can complete DTLS handshake and
//! ICE connectivity checks, but the media flow depends on the str0m stack
//! processing STUN/DTLS/SRTP packets correctly across the two `UdpSocket`
//! instances. In a multi-process scenario this is straightforward; in-process
//! loopback requires careful timing of the tick loops.
//!
//! The conservative tests in this file always pass — they verify that:
//! 1. The full signaling exchange (offer → answer → candidates) completes without errors.
//! 2. Both sender and receiver start cleanly and stop idempotently.
//! 3. `dropped_frames()` is observable on both sides.
//! 4. No threads are leaked after stop.
//!
//! Tests that depend on actual `MediaData` events flowing end-to-end are marked
//! `#[ignore]` with a clear rationale. These are S9.2 / S11.2 style tests — they
//! document the *intent* of the system and serve as manually-verified smoke tests
//! rather than automated CI gates.
//!
//! # Test layout
//!
//! - `transport_loopback_signaling_exchange_completes` — conservative; always runs.
//! - `transport_loopback_sender_receiver_start_stop` — conservative; always runs.
//! - `transport_loopback_dropped_frames_observable` — conservative; always runs.
//! - `transport_loopback_stop_is_idempotent` — conservative; always runs.
//! - `transport_loopback_media_flow_end_to_end` — `#[ignore]`; requires DTLS to complete.
//! - `transport_loopback_rtcp_pli_reaches_encoder` — `#[ignore]`; requires DTLS + media flow.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sm_domain::encode::{EncodedPacket, EncoderConfig, VideoEncoder};
use sm_domain::signaling::{IceCandidate, Signaling, SignalingEvent, SignalingRole};
use sm_domain::transport::{
    TransportConfig, TransportEvent, TransportRole, VideoReceiver, VideoSender,
};
use sm_infra::diagnostics::qsv_ledger::{LedgerPositions, TransportLedgerProbe};
use sm_infra::signaling::loopback::LoopbackSignaling;
use sm_infra::transport::{Str0mVideoReceiver, Str0mVideoSender};

// ─── Fake encoder for PLI tests ──────────────────────────────────────────────

/// Minimal in-test encoder that emits synthetic Annex-B IDR + P-frames.
///
/// Used to supply a real `Arc<dyn VideoEncoder + Send + Sync>` to the sender,
/// and to observe `request_keyframe()` calls for PLI round-trip testing.
struct FakeLoopbackEncoder {
    keyframe_called: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    /// When `Some`, the start() call will pump synthetic packets onto this channel.
    #[allow(dead_code)]
    pkt_tx: Option<std::sync::mpsc::SyncSender<EncodedPacket>>,
}

impl FakeLoopbackEncoder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            keyframe_called: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            pkt_tx: None,
        })
    }
}

impl VideoEncoder for FakeLoopbackEncoder {
    fn new(_config: EncoderConfig) -> Result<Self, sm_domain::encode::EncoderError>
    where
        Self: Sized,
    {
        Ok(Self {
            keyframe_called: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            pkt_tx: None,
        })
    }

    fn start(
        &mut self,
        _rx: std::sync::mpsc::Receiver<sm_domain::CaptureFrame>,
        _tx: std::sync::mpsc::SyncSender<EncodedPacket>,
    ) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn request_keyframe(&self) {
        self.keyframe_called.store(true, Ordering::Release);
    }

    fn set_bitrate(&self, _bps: u32) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn backend_name(&self) -> &'static str {
        "sw_fake"
    }
}

// ─── Helper: build a minimal synthetic IDR frame in Annex-B format ───────────

/// Build a minimal Annex-B IDR frame: SPS + PPS + IDR slice.
///
/// The NAL payloads are trivially minimal (single-byte each). This is enough to
/// verify `is_keyframe` detection and `data[0..4] == [0, 0, 0, 1]` on the
/// receiver side — the str0m packetizer will accept any Annex-B payload.
fn synthetic_idr_frame() -> Arc<[u8]> {
    let mut buf = Vec::new();
    // SPS (NAL type 7 = 0x67)
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]);
    // PPS (NAL type 8 = 0x68)
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68]);
    // IDR slice (NAL type 5 = 0x65)
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
    Arc::from(buf.as_slice())
}

// ─── Conservative test 1: signaling exchange completes cleanly ───────────────

/// R11.2–R11.4, S11.2 (conservative path) — The full signaling exchange
/// (offer → answer) over `LoopbackSignaling` MUST complete without errors.
///
/// This test does NOT start the transport tick threads; it verifies only the
/// signaling plane, which is network-free and always deterministic.
#[test]
fn transport_loopback_signaling_exchange_completes() {
    let (mut sender_sig, mut receiver_sig) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

    let (sender_sig_event_tx, sender_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    let (receiver_sig_event_tx, receiver_sig_event_rx) = sync_channel::<SignalingEvent>(8);

    sender_sig.start(sender_sig_event_tx).unwrap();
    receiver_sig.start(receiver_sig_event_tx).unwrap();

    // Build sender (pre-negotiation only — no start()).
    let sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new must succeed");

    let receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new must succeed");

    // Step 1: Get the local SDP offer from sender.
    let offer = sender
        .create_local_offer()
        .expect("create_local_offer must return Ok");

    assert!(
        offer.0.contains("v=0"),
        "SDP offer must contain v=0; got: {}",
        offer.0
    );

    // Step 2: Sender publishes offer via loopback signaling.
    sender_sig
        .publish_local_offer(offer.clone(), 1)
        .expect("publish_local_offer must succeed");

    // Step 3: Receiver side receives OfferReceived.
    let offer_received = match receiver_sig_event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SignalingEvent::OfferReceived(o, _attempt)) => o,
        Ok(other) => panic!("expected OfferReceived, got {other:?}"),
        Err(e) => panic!("recv_timeout for OfferReceived: {e}"),
    };

    assert_eq!(
        offer.0, offer_received.0,
        "offer payload must be preserved through LoopbackSignaling"
    );

    // Step 4: Receiver applies the offer and gets an SDP answer.
    let answer = receiver
        .apply_remote_offer(offer_received)
        .expect("apply_remote_offer must return Ok(SdpAnswer)");

    assert!(
        answer.0.contains("v=0"),
        "SDP answer must contain v=0; got: {}",
        answer.0
    );

    // Step 5: Receiver publishes the answer.
    receiver_sig
        .publish_local_answer(answer.clone())
        .expect("publish_local_answer must succeed");

    // Step 6: Sender receives AnswerReceived.
    let answer_received = match sender_sig_event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SignalingEvent::AnswerReceived(a)) => a,
        Ok(other) => panic!("expected AnswerReceived, got {other:?}"),
        Err(e) => panic!("recv_timeout for AnswerReceived: {e}"),
    };

    assert_eq!(
        answer.0, answer_received.0,
        "answer payload must be preserved through LoopbackSignaling"
    );

    // Step 7: Sender applies the remote answer (posts to control inbox — not
    // started yet so returns Err(NotRunning); this is the pre-start path).
    // We only test that the control method API is reachable; the actual
    // offer/answer wiring in the tick thread is tested by the DTLS test below.
    // (After start(), apply_remote_answer posts to the control inbox and the
    // tick thread will process it; in the not-yet-started state it returns
    // NotRunning which is expected here.)
    let apply_result = sender.apply_remote_answer(answer_received);
    // NotRunning is expected since we haven't called start() yet.
    assert!(
        apply_result.is_err(),
        "apply_remote_answer before start must return Err; got Ok(())"
    );

    sender_sig.stop().unwrap();
    receiver_sig.stop().unwrap();
}

// ─── Conservative test 2: start + stop lifecycle over loopback ───────────────

/// R11.2, R11.4 (conservative path) — Both sender and receiver MUST start and
/// stop cleanly when wired via `LoopbackSignaling`. No media flow required.
///
/// This test verifies the channel wiring and lifecycle invariants:
/// - `start()` returns `Ok(())` for both sides.
/// - `stop()` returns `Ok(())` and is idempotent on both sides.
/// - No threads are leaked.
#[test]
fn transport_loopback_sender_receiver_start_stop() {
    let (mut sender_sig, mut receiver_sig) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

    let (sender_sig_event_tx, _sender_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    let (receiver_sig_event_tx, _receiver_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    sender_sig.start(sender_sig_event_tx).unwrap();
    receiver_sig.start(receiver_sig_event_tx).unwrap();

    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new must succeed");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new must succeed");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, _sender_event_rx) = sync_channel::<TransportEvent>(4);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(4);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(4);

    sender
        .start(pkt_rx, sender_event_tx)
        .expect("sender start must succeed");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start must succeed");

    // Give threads a moment to enter tick loops.
    std::thread::sleep(Duration::from_millis(20));

    // Sender must report dropped_frames() = 0 (no packets sent yet).
    assert_eq!(
        sender.dropped_frames(),
        0,
        "sender dropped_frames must be 0"
    );
    // Receiver must report dropped_frames() = 0 (no packets received yet).
    assert_eq!(
        receiver.dropped_frames(),
        0,
        "receiver dropped_frames must be 0"
    );

    // Stop in correct order: drop sender's packet tx first (so tick thread
    // is unblocked), then stop sender, then stop receiver.
    drop(pkt_tx);
    sender.stop().expect("sender stop must succeed");
    receiver.stop().expect("receiver stop must succeed");

    // Idempotent stop must not panic.
    sender.stop().expect("sender second stop must succeed");
    receiver.stop().expect("receiver second stop must succeed");

    sender_sig.stop().unwrap();
    receiver_sig.stop().unwrap();
}

// ─── Conservative test 3: dropped_frames() observable on both sides ──────────

/// R11.4, R14.3, S11.2 (conservative path) — `dropped_frames()` counter MUST
/// be observable (readable without panicking) on both sender and receiver at any
/// point during their lifecycle.
#[test]
fn transport_loopback_dropped_frames_observable() {
    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Before start: dropped_frames() must be 0.
    assert_eq!(
        sender.dropped_frames(),
        0,
        "sender pre-start dropped must be 0"
    );
    assert_eq!(
        receiver.dropped_frames(),
        0,
        "receiver pre-start dropped must be 0"
    );

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, _) = sync_channel::<TransportEvent>(4);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(4);
    let (receiver_event_tx, _) = sync_channel::<TransportEvent>(4);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    // Send a few synthetic packets. Pre-negotiation, they will be dropped by
    // the sender tick loop (ICE/DTLS not ready). dropped_frames() must increase
    // or at least remain non-panicking.
    let idr = EncodedPacket {
        data: synthetic_idr_frame(),
        is_keyframe: true,
        timestamp: Duration::from_millis(0),
        sequence: 0,
    };
    for _ in 0..3 {
        let _ = pkt_tx.try_send(idr.clone());
    }

    std::thread::sleep(Duration::from_millis(100));

    // dropped_frames() must be readable (non-panicking).
    let _ = sender.dropped_frames();
    let _ = receiver.dropped_frames();

    drop(pkt_tx);
    sender.stop().expect("sender stop");
    receiver.stop().expect("receiver stop");
}

// ─── Conservative test 4: stop() is idempotent for both sides ────────────────

/// R1.5, R2.5, R12.4 — `stop()` MUST be idempotent: second call MUST return
/// `Ok(())` without panicking for both sender and receiver.
#[test]
fn transport_loopback_stop_is_idempotent() {
    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, _) = sync_channel::<TransportEvent>(4);
    let (pkt_out_tx, _) = sync_channel::<EncodedPacket>(4);
    let (receiver_event_tx, _) = sync_channel::<TransportEvent>(4);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    drop(pkt_tx);

    // First stop.
    sender.stop().expect("sender first stop");
    receiver.stop().expect("receiver first stop");

    // Second stop — MUST NOT panic.
    sender.stop().expect("sender second stop (idempotent)");
    receiver.stop().expect("receiver second stop (idempotent)");

    // Third stop — still no panic.
    sender.stop().expect("sender third stop");
    receiver.stop().expect("receiver third stop");
}

// ─── Conservative test 5: full loopback signaling + transport start ──────────

/// R11.2–R11.4, S11.2 (end-to-end wire-up conservative) — Wire sender and
/// receiver through the full signaling exchange then start both transport threads.
///
/// Verifies that the signaling exchange + transport start work without error
/// when using ephemeral ports on 127.0.0.1.
#[test]
fn transport_loopback_full_wire_up_no_error() {
    let (mut sender_sig, mut receiver_sig) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

    let (sender_sig_event_tx, _sender_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    let (receiver_sig_event_tx, receiver_sig_event_rx) = sync_channel::<SignalingEvent>(8);

    sender_sig.start(sender_sig_event_tx).unwrap();
    receiver_sig.start(receiver_sig_event_tx).unwrap();

    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Get offer BEFORE start (pre-neg path).
    let offer = sender
        .create_local_offer()
        .expect("create_local_offer must succeed");

    // Apply offer to receiver (pre-start path A).
    let answer = receiver
        .apply_remote_offer(offer.clone())
        .expect("apply_remote_offer (pre-start) must succeed");

    // Start both.
    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    // Now send the answer to the sender via the control inbox (post-start path).
    sender
        .apply_remote_answer(answer)
        .expect("apply_remote_answer must succeed after start()");

    // Publish offer/answer through signaling for observability (not strictly needed
    // since we applied them directly, but exercises the signaling path).
    sender_sig
        .publish_local_offer(offer, 1)
        .expect("publish_local_offer");

    // Drain the OfferReceived from receiver_sig.
    match receiver_sig_event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SignalingEvent::OfferReceived(_, _)) => {}
        Ok(other) => panic!("expected OfferReceived; got {other:?}"),
        Err(e) => panic!("recv_timeout: {e}"),
    }

    // Let ICE/DTLS run for a bit.
    std::thread::sleep(Duration::from_millis(500));

    // Observe dropped_frames on both sides — non-panicking reads.
    let s_dropped = sender.dropped_frames();
    let r_dropped = receiver.dropped_frames();

    // Pre-negotiation packets (sent before ICE connected) are dropped.
    // The counter may or may not be > 0 depending on timing — we only assert
    // that the read itself succeeds (no panic).
    let _ = s_dropped;
    let _ = r_dropped;

    // Observe any transport events (IceConnected, etc.) without blocking long.
    let mut ice_connected = false;
    let wait_until = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if std::time::Instant::now() >= wait_until {
            break;
        }
        match sender_event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TransportEvent::IceConnected) => {
                ice_connected = true;
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // ICE connection is a BONUS if it completes; we do NOT assert it here
    // because it requires DTLS + str0m loopback plumbing to fully complete.
    // The test passes regardless of whether ICE connected.
    let _ = ice_connected;

    drop(pkt_tx);
    sender.stop().expect("sender stop");
    receiver.stop().expect("receiver stop");
    sender_sig.stop().unwrap();
    receiver_sig.stop().unwrap();
}

// ─── Ignored test: end-to-end media flow ─────────────────────────────────────

/// S9.2, S11.2 — End-to-end loopback: sender emits packets → receiver emits
/// `EncodedPacket` with `data[0..4] == [0x00, 0x00, 0x00, 0x01]` and
/// `is_keyframe == true`.
///
/// # Why this is `#[ignore]`
///
/// This test requires the str0m DTLS handshake to complete over two loopback
/// `UdpSocket` instances in the same OS process. The DTLS handshake involves
/// certificate exchange and cryptographic operations that are non-trivial to
/// orchestrate in a unit-test context:
///
/// - Both sockets need to exchange DTLS `ClientHello`/`ServerHello` via
///   `Output::Transmit → UdpSocket::send_to` → peer's `recv_from`.
/// - Both tick loops must be running simultaneously to make progress.
/// - The ephemeral ports chosen by the OS must be routable to each other
///   (127.0.0.1 → 127.0.0.1 always works, but the candidates must be set up).
///
/// In a full integration environment (e.g. separate processes, or with explicit
/// remote candidate injection), this test would pass. For CI-safe testing,
/// the conservative tests above (which test signaling, lifecycle, and
/// dropped_frames observability) are the automated gate.
///
/// Run manually with:
/// ```text
/// cargo nextest run -p sm-infra --run-ignored --test transport_loopback
/// ```
#[test]
fn transport_loopback_media_flow_end_to_end() {
    let (mut sender_sig, mut receiver_sig) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

    let (sender_sig_event_tx, _sender_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    let (receiver_sig_event_tx, _receiver_sig_event_rx) = sync_channel::<SignalingEvent>(8);

    sender_sig.start(sender_sig_event_tx).unwrap();
    receiver_sig.start(receiver_sig_event_tx).unwrap();

    let enc = FakeLoopbackEncoder::new();
    let keyframe_called = Arc::clone(&enc.keyframe_called);

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Pre-start offer/answer exchange.
    let offer = sender.create_local_offer().expect("offer");
    let answer = receiver.apply_remote_offer(offer).expect("answer");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(8);
    let (sender_event_tx, sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);

    let ledger_probe = Arc::new(TransportLedgerProbe::collecting());
    sender.install_transport_ledger_probe_for_test(Arc::clone(&ledger_probe));

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    let sender_addr = sender.local_addr().expect("sender local_addr");
    let receiver_addr = receiver.local_addr().expect("receiver local_addr");

    sender.apply_remote_answer(answer).expect("apply answer");

    let sender_candidate =
        str0m::Candidate::host(sender_addr, "udp").expect("sender host candidate");
    let receiver_candidate =
        str0m::Candidate::host(receiver_addr, "udp").expect("receiver host candidate");

    receiver
        .add_remote_candidate(IceCandidate(
            serde_json::to_string(&sender_candidate).expect("serialize sender candidate"),
        ))
        .expect("receiver add_remote_candidate");
    sender
        .add_remote_candidate(IceCandidate(
            serde_json::to_string(&receiver_candidate).expect("serialize receiver candidate"),
        ))
        .expect("sender add_remote_candidate");

    // Wait for ICE to connect (up to 10 s).
    let ice_connected = {
        let mut connected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match sender_event_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(TransportEvent::IceConnected) => {
                    connected = true;
                    break;
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        connected
    };

    assert!(
        ice_connected,
        "ICE must connect within 10 s for loopback loopback"
    );

    // Pump a few synthetic IDR frames.
    for i in 0..5u64 {
        let _ = pkt_tx.try_send(EncodedPacket {
            data: synthetic_idr_frame(),
            is_keyframe: true,
            timestamp: Duration::from_millis(i * 33),
            sequence: i,
        });
    }

    // Wait for at least one EncodedPacket on the receiver side (up to 5 s).
    let first_pkt = pkt_out_rx.recv_timeout(Duration::from_secs(5));
    let first_pkt = first_pkt.expect("at least one EncodedPacket must arrive within 5 s");

    // Assert 1: starts with Annex-B start code.
    assert_eq!(
        &first_pkt.data[0..4],
        &[0x00, 0x00, 0x00, 0x01],
        "first packet data must start with Annex-B start code"
    );

    // Assert 2: first emitted packet has is_keyframe == true (IDR).
    assert!(
        first_pkt.is_keyframe,
        "first received packet must be a keyframe"
    );

    let writer_witnesses = ledger_probe.writer_witnesses();
    let transmit_witnesses = ledger_probe.udp_transmit_witnesses();
    assert!(
        !writer_witnesses.is_empty(),
        "an accepted active-loop writer frame must record a ledger witness"
    );
    assert!(
        !transmit_witnesses.is_empty(),
        "a full active-loop UDP transmit must record a ledger witness"
    );
    assert_eq!(
        writer_witnesses[0].source().session(),
        transmit_witnesses[0].identity().source().session(),
        "writer and transmit witnesses must share the canonical sender session"
    );

    // Assert 3: request_keyframe() reaches encoder via RTCP PLI.
    receiver
        .request_keyframe()
        .expect("request_keyframe must succeed");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        keyframe_called.load(Ordering::Acquire),
        "RTCP PLI must have reached encoder.request_keyframe()"
    );

    drop(pkt_tx);
    sender.stop().expect("sender stop");
    receiver.stop().expect("receiver stop");
    sender_sig.stop().unwrap();
    receiver_sig.stop().unwrap();
}

/// S9.2 (isolated PLI test) — When the receiver calls `request_keyframe()` and
/// the transport is running, the sender's encoder receives `request_keyframe()`.
///
/// # Why `#[ignore]`
///
/// Same rationale as `transport_loopback_media_flow_end_to_end`: requires DTLS
/// loopback to complete so that RTCP PLI travels from receiver → sender.
#[test]
#[ignore = "Requires DTLS/ICE loopback — depends on media_flow test prerequisites"]
fn transport_loopback_rtcp_pli_reaches_encoder() {
    // Test body intentionally minimal — relies on the same setup as the media
    // flow test above. Run both together via --run-ignored.
    //
    // The canonical PLI test without full DTLS is the unit test
    // `sender_pli_calls_encoder_request_keyframe_s9_1` in str0m_sender.rs,
    // which uses `inject_keyframe_request_for_test()` to bypass the network path.
}

// ─── RED test: full loopback ICE connectivity ─────────────────────────────────

/// R11.2–R11.4 (ambitious path) — Assert that the str0m sender and receiver
/// can exchange ICE STUN connectivity checks over loopback UDP and complete
/// the ICE `Connected` state within 5 seconds.
///
/// This test is NOT `#[ignore]`d — it is designed to be the RED gate for
/// task 6.2. If the loopback ICE connectivity check is not wired correctly
/// (e.g. remote candidates not exchanged), it will fail.
///
/// # What needs to happen for this to pass (task 6.2)
///
/// 1. Both sender and receiver bind ephemeral UDP sockets (udp_port: 0).
/// 2. After `start()`, both expose their effective local address via `local_addr()`.
/// 3. Their addresses are serialized as JSON and exchanged via `add_remote_candidate`.
/// 4. str0m's ICE layer sends STUN binding requests to the peer's address.
/// 5. Both tick loops process the incoming STUN and send responses.
/// 6. ICE transitions to `Connected`.
///
/// The test wires all of these together and waits for
/// `TransportEvent::IceConnected` within 5 seconds.
#[test]
fn transport_loopback_ice_connects_over_loopback_r11_2() {
    let enc = FakeLoopbackEncoder::new();

    // Step 1: Build sender and receiver with ephemeral ports.
    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Step 2: Pre-start signaling exchange.
    let offer = sender.create_local_offer().expect("offer");
    let answer = receiver.apply_remote_offer(offer).expect("answer");

    // Step 3: Start both.
    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    // Step 4: Retrieve effective local addresses from both adapters.
    let sender_addr = sender
        .local_addr()
        .expect("sender must expose local_addr after start()");
    let receiver_addr = receiver
        .local_addr()
        .expect("receiver must expose local_addr after start()");

    // Step 5: Apply the answer to sender (post-start path via control inbox).
    sender.apply_remote_answer(answer).expect("apply answer");

    // Step 6: Exchange ICE candidates — tell each peer where the other is listening.
    // str0m's Candidate implements serde::Serialize, so we JSON-serialize the candidate.
    let sender_candidate =
        str0m::Candidate::host(sender_addr, "udp").expect("sender host candidate");
    let receiver_candidate =
        str0m::Candidate::host(receiver_addr, "udp").expect("receiver host candidate");

    let sender_cand_json =
        serde_json::to_string(&sender_candidate).expect("serialise sender candidate");
    let receiver_cand_json =
        serde_json::to_string(&receiver_candidate).expect("serialise receiver candidate");

    // Tell receiver about sender's address.
    receiver
        .add_remote_candidate(IceCandidate(sender_cand_json))
        .expect("receiver add_remote_candidate");

    // Tell sender about receiver's address.
    sender
        .add_remote_candidate(IceCandidate(receiver_cand_json))
        .expect("sender add_remote_candidate");

    // Step 7: Wait for IceConnected event on sender (up to 5 s).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut ice_connected = false;
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        match sender_event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TransportEvent::IceConnected) => {
                ice_connected = true;
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Clean up before asserting (so we don't leak threads on failure).
    drop(pkt_tx);
    sender.stop().unwrap();
    receiver.stop().unwrap();

    assert!(
        ice_connected,
        "ICE must reach Connected state within 5 s over 127.0.0.1 loopback \
         (sender_addr={sender_addr}, receiver_addr={receiver_addr}); \
         check that both tick loops process STUN binding requests correctly"
    );
}

// ─── T6.1 (streaming-emit-on-ice-connect): loopback pre-ICE packets are dropped ─

/// T6.1 (TST-L-1) — Pre-ICE packets pushed to the sender before any ICE
/// candidate exchange are counted in `dropped_frames()`.
///
/// This test withholds candidate exchange entirely — the sender starts but
/// ICE never completes. Packets arrive at the tick loop, the ice_ready gate
/// is false, and they must be dropped and counted.
///
/// ACs: AC-1, AC-2 (REQ-EMIT-1, REQ-EMIT-4, REQ-EMIT-8)
#[test]
fn loopback_pre_ice_packets_are_dropped() {
    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Exchange offer/answer so mid is eventually set (simulates MediaAdded scenario).
    let offer = sender.create_local_offer().expect("offer");
    let answer = receiver.apply_remote_offer(offer).expect("answer");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(8);
    let (sender_event_tx, _sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    // Apply the answer (so SDP is done and mid may be set by MediaAdded).
    sender.apply_remote_answer(answer).expect("apply answer");

    // Deliberately withhold candidate exchange — ICE never completes.
    // Push 4 packets into the sender. With ice_ready=false, all must be dropped.
    for i in 0..4u64 {
        let _ = pkt_tx.send(EncodedPacket {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65].into(),
            is_keyframe: true,
            timestamp: Duration::from_millis(i * 33),
            sequence: i,
        });
    }
    // 250ms > max tick timeout (200ms): guarantees at least one full iteration.
    std::thread::sleep(Duration::from_millis(250));

    let dropped = sender.dropped_frames();
    // Clean up before asserting.
    drop(pkt_tx);
    sender.stop().unwrap();
    receiver.stop().unwrap();

    assert!(
        dropped >= 4,
        "dropped_frames must be >= 4 when ICE never completes (ice_ready=false), got {dropped}"
    );
}

// ─── T6.2 (streaming-emit-on-ice-connect): ICE-Completed also opens gate ──────

/// T6.2 (TST-L-2) — After a full ICE handshake completes and IceConnected
/// arrives, `dropped_frames()` does not increase for subsequently-sent packets
/// (the gate is open and frames flow, even if DTLS doesn't complete for writing).
///
/// # Why this may be `#[ignore]`d
///
/// This test requires full loopback ICE to complete. In CI with a single-process
/// loopback setup, ICE completing is reliable (verified by
/// `transport_loopback_ice_connects_over_loopback_r11_2`). However the assertion
/// that `dropped_frames` doesn't increase post-ICE depends on mid being resolved
/// via MediaAdded, which needs DTLS to complete — that is NOT guaranteed.
/// The test is therefore `#[ignore]`d per the loopback file's pattern.
///
/// ACs: AC-3 (REQ-EMIT-2, REQ-EMIT-3)
#[test]
#[ignore = "Requires full DTLS loopback to verify mid+ice_ready write path — \
             use transport_loopback_ice_connects_over_loopback_r11_2 for ICE gate smoke"]
fn loopback_ice_completed_also_opens_gate() {
    let enc = FakeLoopbackEncoder::new();

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    let offer = sender.create_local_offer().expect("offer");
    let answer = receiver.apply_remote_offer(offer).expect("answer");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(8);
    let (sender_event_tx, sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    let sender_addr = sender.local_addr().expect("sender local_addr");
    let receiver_addr = receiver.local_addr().expect("receiver local_addr");

    sender.apply_remote_answer(answer).expect("apply answer");

    let sender_candidate =
        str0m::Candidate::host(sender_addr, "udp").expect("sender host candidate");
    let receiver_candidate =
        str0m::Candidate::host(receiver_addr, "udp").expect("receiver host candidate");

    receiver
        .add_remote_candidate(IceCandidate(
            serde_json::to_string(&sender_candidate).expect("serialize"),
        ))
        .expect("receiver add_remote_candidate");
    sender
        .add_remote_candidate(IceCandidate(
            serde_json::to_string(&receiver_candidate).expect("serialize"),
        ))
        .expect("sender add_remote_candidate");

    // Wait for IceConnected (up to 5s).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut ice_connected = false;
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        match sender_event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TransportEvent::IceConnected) => {
                ice_connected = true;
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    assert!(ice_connected, "ICE must connect for T6.2 to be meaningful");

    let dropped_at_ice = sender.dropped_frames();

    // Send 2 packets post-ICE. With ice_ready=true, they should NOT be dropped
    // (assuming mid is also resolved). If DTLS isn't done, mid may still be None,
    // in which case they're dropped — but that's the DTLS path, not the ICE gate.
    for i in 0..2u64 {
        let _ = pkt_tx.send(EncodedPacket {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65].into(),
            is_keyframe: true,
            timestamp: Duration::from_millis((4 + i) * 33),
            sequence: 4 + i,
        });
    }
    std::thread::sleep(Duration::from_millis(250));
    let dropped_after = sender.dropped_frames();

    drop(pkt_tx);
    sender.stop().unwrap();
    receiver.stop().unwrap();

    assert_eq!(
        dropped_at_ice, dropped_after,
        "dropped_frames must NOT increase after IceConnected when mid is resolved; \
         before={dropped_at_ice} after={dropped_after}"
    );
}

// ─── S-CT-2: candidate_addr() returns None before start() ────────────────────

/// S-CT-2 (sender variant) — `candidate_addr()` MUST return `None` before
/// `start()` is called. R-CT-1 pre-condition.
#[test]
fn transport_sender_candidate_addr_is_none_pre_start_s_ct_2() {
    let sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    assert!(
        sender.candidate_addr().is_none(),
        "candidate_addr() must return None before start() is called"
    );
}

/// S-CT-2 (receiver variant) — `candidate_addr()` MUST return `None` before
/// `start_with_socket()` is called. R-CT-2 pre-condition.
#[test]
fn transport_receiver_candidate_addr_is_none_pre_start_s_ct_2() {
    let receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    assert!(
        receiver.candidate_addr().is_none(),
        "candidate_addr() must return None before start_with_socket() is called"
    );
}

// ─── S-CT-3: candidate_addr() returns None when no non-loopback NIC ──────────

/// S-CT-3 (sender variant) — `candidate_addr()` MUST return `None` when the NIC
/// enumeration returns an empty list (simulating a machine with no usable LAN
/// adapter). R-CT-3 / R-CT-8: no panic, no TransportError.
///
/// The NicOverrideGuard injects an empty NIC list for this thread only. Even
/// though the sender is started and `local_addr` is `Some(127.0.0.1:port)` (the
/// loopback-fallback effective_local_addr), `enumerate_local_ipv4()` returns `[]`
/// so `candidate_addr()` short-circuits to `None`.
///
/// This test passes immediately after B1-GREEN because B1 already implements the
/// correct NIC substitution path. The RED commit documents the invariant and guards
/// against future regressions.
#[test]
fn transport_sender_candidate_addr_is_none_when_no_usable_nic_s_ct_3() {
    use sm_infra::transport::NicOverrideGuard;

    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let enc = FakeLoopbackEncoder::new();
    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (event_tx, _event_rx) = sync_channel::<TransportEvent>(8);
    sender.start(pkt_rx, event_tx).expect("sender start");

    // Inject empty NIC list: simulates no usable LAN adapter.
    // The guard restores the default on drop at end of scope.
    let _guard = NicOverrideGuard::new(vec![]);

    assert!(
        sender.candidate_addr().is_none(),
        "candidate_addr() must return None when no non-loopback NIC is available"
    );

    // Clean up (guard drops here, restoring the override to None).
    drop(pkt_tx);
    sender.stop().unwrap();
}

/// S-CT-3 (receiver variant) — `candidate_addr()` MUST return `None` when the NIC
/// enumeration returns an empty list after `start()`. Mirror of the sender variant.
#[test]
fn transport_receiver_candidate_addr_is_none_when_no_usable_nic_s_ct_3() {
    use sm_infra::transport::NicOverrideGuard;

    let sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new for offer");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Exchange offer/answer to allow start() to succeed on the receiver.
    let offer = sender.create_local_offer().expect("offer");
    let _answer = receiver.apply_remote_offer(offer).expect("answer");

    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (event_tx, _event_rx) = sync_channel::<TransportEvent>(8);
    receiver
        .start(pkt_out_tx, event_tx)
        .expect("receiver start");

    // Inject empty NIC list: simulates no usable LAN adapter.
    let _guard = NicOverrideGuard::new(vec![]);

    assert!(
        receiver.candidate_addr().is_none(),
        "candidate_addr() must return None when no non-loopback NIC is available"
    );

    receiver.stop().unwrap();
}

// ─── S-CT-4: Candidate JSON round-trip through IceCandidate ──────────────────

/// S-CT-4 — The `publish_host_candidate` helper serialises a `str0m::Candidate`
/// via `serde_json::to_string` and the resulting `IceCandidate(String)` MUST be
/// accepted by `add_remote_candidate` on a peer adapter without error.
///
/// This is the regression gate for R-PROP-4 (design D-CT-3): the wire codec is
/// `serde_json`, NOT `Candidate::to_string()` (which produces SDP-attribute text
/// that `serde_json::from_str::<Candidate>` cannot parse).
///
/// The test uses `LoopbackSignaling` as the signaling channel so we can observe
/// the `CandidateReceived` event and confirm the JSON round-trips without panic.
///
/// This test passes immediately after B1-GREEN because `publish_host_candidate`
/// was implemented there. The RED commit documents the serialisation invariant.
#[test]
fn transport_candidate_serde_json_round_trips_through_add_remote_candidate_s_ct_4() {
    use sm_infra::transport::publish_host_candidate;

    // Use a known LAN-style address (does not need to be a real bound socket for
    // round-trip purposes; we are testing serialisation only, not connectivity).
    let addr: std::net::SocketAddr = "192.168.1.100:5004".parse().expect("valid addr");

    // Build a sender + receiver pair to exercise the full round-trip.
    let enc = FakeLoopbackEncoder::new();
    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    // Exchange offer/answer so both adapters are in the negotiated state.
    let offer = sender.create_local_offer().expect("offer");
    let _answer = receiver.apply_remote_offer(offer).expect("answer");

    // Start both adapters.
    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, _sender_event_rx) = sync_channel::<TransportEvent>(8);
    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(8);
    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    // Build a LoopbackSignaling pair; use the sender side for publishing.
    let (mut sig_a, mut sig_b) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
    let (sig_a_ev_tx, _sig_a_ev_rx) = sync_channel::<SignalingEvent>(4);
    let (sig_b_ev_tx, sig_b_ev_rx) = sync_channel::<SignalingEvent>(4);
    sig_a.start(sig_a_ev_tx).expect("sig_a start");
    sig_b.start(sig_b_ev_tx).expect("sig_b start");

    // Publish the host candidate from sig_a.
    publish_host_candidate(&sig_a, addr).expect("publish_host_candidate must not error");

    // The peer (sig_b) should receive a CandidateReceived event.
    let event = sig_b_ev_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("CandidateReceived must arrive within 5 s");

    let candidate_json = match event {
        SignalingEvent::CandidateReceived(IceCandidate(json)) => json,
        other => panic!("Expected CandidateReceived, got {other:?}"),
    };

    // Round-trip: the JSON must be accepted by add_remote_candidate on the sender
    // without error. This exercises the existing consume path at str0m_sender.rs:446:
    //   serde_json::from_str::<Candidate>(&cand.0)
    sender
        .add_remote_candidate(IceCandidate(candidate_json))
        .expect("add_remote_candidate must accept serde_json-encoded Candidate");

    // Clean up.
    drop(pkt_tx);
    sender.stop().unwrap();
    receiver.stop().unwrap();
}

// ─── S-CT-1: signaling-plane candidate publish integration test ───────────────

/// S-CT-1 (PRIMARY integration test) — After `start()`, calling `candidate_addr()`
/// on the sender (with a `NicOverrideGuard` injecting a known LAN address) and then
/// `publish_host_candidate(&signaling, addr)` MUST deliver a `CandidateReceived`
/// event to the peer's signaling event channel within 5 seconds. The event's JSON
/// payload MUST deserialise (via `serde_json::from_str::<Candidate>`) to a host
/// candidate with the injected IP address.
///
/// This is the primary automated gate for the trickle-ICE signaling path (R-CT-4,
/// R-CT-5, AC-CT-1). It exercises the full publish→channel→receive→add loop
/// without requiring a real LAN or Windows-specific bundle code.
///
/// The test calls `publish_host_candidate` directly (not via the production bundle
/// builder). The production-bundle gate is in B5 (sender) and B6 (receiver).
///
/// This test passes immediately after B1-GREEN because the helper and NicOverrideGuard
/// were implemented there. The RED commit documents the S-CT-1 contract.
#[test]
fn transport_candidate_publishes_to_peer_via_loopback_signaling_s_ct_1() {
    use sm_infra::transport::{NicOverrideGuard, publish_host_candidate};

    let injected_ip = std::net::Ipv4Addr::new(192, 168, 1, 13);

    // Set up sender + signaling.
    let enc = FakeLoopbackEncoder::new();
    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (sender_event_tx, _sender_event_rx) = sync_channel::<TransportEvent>(8);
    sender.start(pkt_rx, sender_event_tx).expect("sender start");

    // Wire signaling pair.
    let (mut sig_sender, mut sig_receiver) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
    let (sig_s_ev_tx, _sig_s_ev_rx) = sync_channel::<SignalingEvent>(4);
    let (sig_r_ev_tx, sig_r_ev_rx) = sync_channel::<SignalingEvent>(4);
    sig_sender.start(sig_s_ev_tx).expect("sig_sender start");
    sig_receiver.start(sig_r_ev_tx).expect("sig_receiver start");

    // Inject a known LAN address so candidate_addr() returns a deterministic value.
    let _guard = NicOverrideGuard::new(vec![injected_ip]);

    let cand_addr = sender
        .candidate_addr()
        .expect("candidate_addr() must return Some after start() with NIC override");

    // Verify the substituted IP matches the injected NIC.
    assert_eq!(
        cand_addr.ip(),
        std::net::IpAddr::V4(injected_ip),
        "candidate_addr() must substitute injected NIC IP"
    );

    // Publish the candidate from the sender side.
    publish_host_candidate(&sig_sender, cand_addr).expect("publish_host_candidate must not error");

    // Drop the NIC override before draining events (no longer needed).
    drop(_guard);

    // The receiver side should see CandidateReceived within 5 s.
    let event = sig_r_ev_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("CandidateReceived must arrive on receiver's signaling channel within 5 s");

    let candidate_json = match event {
        SignalingEvent::CandidateReceived(IceCandidate(json)) => json,
        other => panic!("Expected CandidateReceived, got {other:?}"),
    };

    // Verify the JSON deserialises to a Candidate with the injected IP.
    let parsed: str0m::Candidate =
        serde_json::from_str(&candidate_json).expect("Candidate JSON must be valid serde_json");
    let expected_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(injected_ip), cand_addr.port());
    assert_eq!(
        parsed.addr(),
        expected_addr,
        "Deserialised Candidate must carry the injected IP and bound port"
    );

    // Clean up.
    drop(pkt_tx);
    sender.stop().unwrap();
}

// ─── B5/B6 bundle-sequence regression tests ──────────────────────────────────

/// Bundle-sequence regression gate (sender side, B5) — Asserts that the
/// production-bundle sequence (start → publish_offer → publish_host_candidate)
/// delivers CandidateReceived to the peer's signaling channel.
///
/// This test mirrors what `build_production_sender_bundle` SHOULD do after B5-GREEN.
/// It calls `publish_host_candidate` directly rather than via the production bundle
/// builder (which is #[cfg(target_os="windows")] and requires real adapters).
/// The test documents the contract and guards against regressions.
///
/// The test passes immediately because the helper was implemented in B1-GREEN.
/// The production-bundle wiring (B5-GREEN) is verified by grep in B7.4 and the
/// sdd-verify phase.
#[test]
fn transport_sender_bundle_sequence_publishes_candidate_b5() {
    use sm_infra::transport::{NicOverrideGuard, publish_host_candidate};

    let injected_ip = std::net::Ipv4Addr::new(10, 0, 0, 5);

    // Simulate the bundle-builder sequence.
    let enc = FakeLoopbackEncoder::new();
    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new");

    sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    let (pkt_tx, pkt_rx) = sync_channel(4);
    let (event_tx, _event_rx) = sync_channel::<TransportEvent>(8);
    sender.start(pkt_rx, event_tx).expect("sender start");

    let (mut sig_sender, mut sig_receiver) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
    let (sig_s_ev_tx, _sig_s_ev_rx) = sync_channel::<SignalingEvent>(4);
    let (sig_r_ev_tx, sig_r_ev_rx) = sync_channel::<SignalingEvent>(4);
    sig_sender.start(sig_s_ev_tx).expect("sig_sender start");
    sig_receiver.start(sig_r_ev_tx).expect("sig_receiver start");

    // Step matching build_production_sender_bundle:
    // 1. publish_local_offer (already done in real bundle; here we skip for simplicity)
    // 2. publish_host_candidate AFTER offer
    let _guard = NicOverrideGuard::new(vec![injected_ip]);
    if let Some(addr) = sender.candidate_addr() {
        publish_host_candidate(&sig_sender, addr).expect("publish ok");
    }
    drop(_guard);

    // Assert CandidateReceived arrives on the receiver side.
    let event = sig_r_ev_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("CandidateReceived must arrive on receiver-side signaling channel");

    assert!(
        matches!(event, SignalingEvent::CandidateReceived(_)),
        "Expected CandidateReceived, got {event:?}"
    );

    drop(pkt_tx);
    sender.stop().unwrap();
}

/// Bundle-sequence regression gate (receiver side, B6) — Mirror of the sender
/// variant: after `receiver.start()`, publishing the host candidate via
/// `publish_host_candidate` MUST deliver `CandidateReceived` to the peer.
#[test]
fn transport_receiver_bundle_sequence_publishes_candidate_b6() {
    use sm_infra::transport::{NicOverrideGuard, publish_host_candidate};

    let injected_ip = std::net::Ipv4Addr::new(10, 0, 0, 6);

    let sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("sender new for offer");

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("receiver new");

    let offer = sender.create_local_offer().expect("offer");
    let _answer = receiver.apply_remote_offer(offer).expect("answer");

    let (pkt_out_tx, _pkt_out_rx) = sync_channel::<EncodedPacket>(8);
    let (event_tx, _event_rx) = sync_channel::<TransportEvent>(8);
    receiver
        .start(pkt_out_tx, event_tx)
        .expect("receiver start");

    let (mut sig_receiver_side, mut sig_sender_side) =
        LoopbackSignaling::pair(SignalingRole::Receiver, SignalingRole::Sender);
    let (sig_r_ev_tx, _sig_r_ev_rx) = sync_channel::<SignalingEvent>(4);
    let (sig_s_ev_tx, sig_s_ev_rx) = sync_channel::<SignalingEvent>(4);
    sig_receiver_side
        .start(sig_r_ev_tx)
        .expect("sig_receiver_side start");
    sig_sender_side
        .start(sig_s_ev_tx)
        .expect("sig_sender_side start");

    // Step matching build_production_bundle:
    // After receiver.start_with_socket(), publish the candidate.
    let _guard = NicOverrideGuard::new(vec![injected_ip]);
    if let Some(addr) = receiver.candidate_addr() {
        publish_host_candidate(&sig_receiver_side, addr).expect("publish ok");
    }
    drop(_guard);

    // Assert CandidateReceived arrives on the sender side (peer of receiver).
    let event = sig_s_ev_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("CandidateReceived must arrive on sender-side signaling channel");

    assert!(
        matches!(event, SignalingEvent::CandidateReceived(_)),
        "Expected CandidateReceived, got {event:?}"
    );

    receiver.stop().unwrap();
}

// ─── PR4R-harness RED: deterministic relay contracts ─────────────────────────

#[derive(Debug, Eq, PartialEq)]
struct RtpHeaderFields {
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
}

fn parse_rtp_header(packet: &[u8]) -> Result<RtpHeaderFields, &'static str> {
    const RTP_FIXED_HEADER_LEN: usize = 12;
    const RTP_VERSION: u8 = 2;

    if packet.len() < RTP_FIXED_HEADER_LEN {
        return Err("RTP packet is shorter than its fixed header");
    }

    if packet[0] >> 6 != RTP_VERSION {
        return Err("RTP packet does not use version 2");
    }

    Ok(RtpHeaderFields {
        payload_type: packet[1] & 0x7f,
        marker: packet[1] & 0x80 != 0,
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
    })
}

struct DeterministicUdpRelay {
    local_addr: SocketAddr,
    held_rx: Receiver<Result<(), &'static str>>,
    release_tx: SyncSender<()>,
    delivered_rx: Receiver<Result<(), &'static str>>,
    shutdown_tx: SyncSender<()>,
    state: Arc<AtomicU8>,
    worker: Option<JoinHandle<()>>,
}

const RELAY_CHECKPOINT_TIMEOUT: Duration = Duration::from_millis(250);
const RELAY_WAITING_FOR_PACKET: u8 = 0;
const RELAY_HOLDING_PACKET: u8 = 1;
const RELAY_RELEASED_PACKET: u8 = 2;

impl DeterministicUdpRelay {
    fn bind(destination: SocketAddr) -> Self {
        let socket =
            UdpSocket::bind("127.0.0.1:0").expect("relay binds an ephemeral loopback port");
        let local_addr = socket
            .local_addr()
            .expect("relay exposes its local address");
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .expect("relay configures a bounded shutdown poll");

        let (held_tx, held_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let (delivered_tx, delivered_rx) = sync_channel(1);
        let (shutdown_tx, shutdown_rx) = sync_channel(1);
        let state = Arc::new(AtomicU8::new(RELAY_WAITING_FOR_PACKET));
        let worker_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || {
            let mut buffer = [0_u8; 65_535];
            let packet = loop {
                match socket.recv_from(&mut buffer) {
                    Ok((length, _)) => break buffer[..length].to_vec(),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        match shutdown_rx.try_recv() {
                            Ok(()) | Err(TryRecvError::Disconnected) => return,
                            Err(TryRecvError::Empty) => continue,
                        }
                    }
                    Err(_) => {
                        let _ = held_tx.send(Err("relay failed before holding a packet"));
                        return;
                    }
                }
            };

            worker_state.store(RELAY_HOLDING_PACKET, Ordering::Release);
            if held_tx.send(Ok(())).is_err() || !wait_for_relay_signal(&release_rx, &shutdown_rx) {
                return;
            }

            let result = socket
                .send_to(&packet, destination)
                .map(|_| ())
                .map_err(|_| "relay failed to deliver the held packet");
            let _ = delivered_tx.send(result);
        });

        Self {
            local_addr,
            held_rx,
            release_tx,
            delivered_rx,
            shutdown_tx,
            state,
            worker: Some(worker),
        }
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn wait_until_held(&self) -> Result<(), &'static str> {
        match self.held_rx.recv_timeout(RELAY_CHECKPOINT_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err("relay timed out before holding a packet"),
            Err(RecvTimeoutError::Disconnected) => {
                Err("relay worker exited before holding a packet")
            }
        }
    }

    fn release(&self) -> Result<(), &'static str> {
        match self.state.compare_exchange(
            RELAY_HOLDING_PACKET,
            RELAY_RELEASED_PACKET,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(RELAY_WAITING_FOR_PACKET) => return Err("relay has not held a packet"),
            Err(RELAY_RELEASED_PACKET) => return Err("relay has already released the held packet"),
            Err(_) => return Err("relay worker exited before accepting the release signal"),
        }

        match self.release_tx.try_send(()) {
            Ok(()) => {}
            Err(TrySendError::Full(())) => {
                return Err("relay already has a pending release signal");
            }
            Err(TrySendError::Disconnected(())) => {
                return Err("relay worker exited before accepting the release signal");
            }
        }

        match self.delivered_rx.recv_timeout(RELAY_CHECKPOINT_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err("relay timed out before delivering the held packet")
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err("relay worker exited before delivering the held packet")
            }
        }
    }
}

impl Drop for DeterministicUdpRelay {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(worker) = self.worker.take() {
            worker.join().expect("relay worker must shut down cleanly");
        }
    }
}

fn wait_for_relay_signal(release_rx: &Receiver<()>, shutdown_rx: &Receiver<()>) -> bool {
    loop {
        match release_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(()) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => match shutdown_rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => {}
            },
        }
    }
}

fn rtp_packet(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut packet = [0_u8; 12];
    packet[0] = 0x80;
    packet[1] = 0x80 | 96;
    packet[2..4].copy_from_slice(&sequence.to_be_bytes());
    packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
    packet
}

#[test]
fn pr4r_harness_parses_minimum_rtp_header_fields() {
    let packet = rtp_packet(7, 90_000, 0x1020_3040);

    assert_eq!(
        parse_rtp_header(&packet),
        Ok(RtpHeaderFields {
            payload_type: 96,
            marker: true,
            sequence: 7,
            timestamp: 90_000,
            ssrc: 0x1020_3040,
        })
    );
}

#[test]
fn pr4r_harness_rejects_short_or_malformed_rtp_packets_deterministically() {
    let short_packet = [0x80, 96, 0, 7, 0, 1, 95, 144, 16, 32, 48];
    let malformed_version = [0x40, 96, 0, 7, 0, 1, 95, 144, 16, 32, 48, 64];

    assert!(parse_rtp_header(&short_packet).is_err());
    assert!(parse_rtp_header(&malformed_version).is_err());
}

#[test]
fn pr4r_harness_relay_holds_udp_until_release_then_delivers() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("receiver binds");
    receiver
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("receiver timeout configures");
    let relay = DeterministicUdpRelay::bind(receiver.local_addr().expect("receiver address"));
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("sender binds");
    let packet = rtp_packet(8, 180_000, 0x1020_3040);

    sender
        .send_to(&packet, relay.local_addr())
        .expect("sender writes UDP packet to relay");
    relay
        .wait_until_held()
        .expect("relay worker must hold one packet successfully");
    assert!(
        receiver.recv_from(&mut [0_u8; 32]).is_err(),
        "a held packet must not arrive before release"
    );

    relay
        .release()
        .expect("relay worker must deliver the held packet");
    let mut delivered = [0_u8; 32];
    let (length, _) = receiver
        .recv_from(&mut delivered)
        .expect("released packet must arrive");
    assert_eq!(&delivered[..length], &packet);
}

#[test]
fn pr4r_harness_relay_reports_a_bounded_error_and_joins_when_no_packet_arrives() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("receiver binds");
    let relay = DeterministicUdpRelay::bind(receiver.local_addr().expect("receiver address"));

    assert_eq!(
        relay.wait_until_held(),
        Err("relay timed out before holding a packet")
    );

    drop(relay);
}

#[test]
fn pr4r_harness_relay_rejects_release_before_holding_without_a_stale_signal() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("receiver binds");
    receiver
        .set_read_timeout(Some(RELAY_CHECKPOINT_TIMEOUT))
        .expect("receiver configures bounded read");
    let relay = DeterministicUdpRelay::bind(receiver.local_addr().expect("receiver address"));

    assert_eq!(
        relay.release(),
        Err("relay has not held a packet"),
        "release-before-held must fail without queuing a future release"
    );

    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("sender binds");
    let packet = [0x80, 0x60, 0x00, 0x01];
    sender
        .send_to(&packet, relay.local_addr())
        .expect("sender writes UDP packet to relay");
    relay
        .wait_until_held()
        .expect("relay holds the packet after the rejected release");

    assert!(
        receiver.recv_from(&mut [0_u8; 32]).is_err(),
        "the rejected release must not auto-deliver a later packet"
    );

    relay
        .release()
        .expect("an explicit release must still deliver the held packet");
    let mut delivered = [0_u8; 32];
    let (length, _) = receiver
        .recv_from(&mut delivered)
        .expect("receiver reads the explicitly released packet");
    assert_eq!(&delivered[..length], &packet);

    drop(relay);
}

#[test]
fn pr4r_harness_relay_rejects_a_repeated_release_after_delivery() {
    let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("receiver binds");
    let relay = DeterministicUdpRelay::bind(receiver.local_addr().expect("receiver address"));
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("sender binds");

    sender
        .send_to(&[0x80, 0x60, 0x00, 0x01], relay.local_addr())
        .expect("sender writes UDP packet to relay");
    relay
        .wait_until_held()
        .expect("relay worker must hold one packet successfully");
    relay
        .release()
        .expect("the first explicit release must deliver the held packet");

    assert_eq!(
        relay.release(),
        Err("relay has already released the held packet")
    );

    drop(relay);
}

// ─── SC-MLO-4: real-str0m m-line conflict reproduction (#[ignore]) ───────────
//
// REQ-MLO-2: A real-str0m integration test that documents the m-line order
// failure when an OLD Str0mVideoReceiver processes a NEW Str0mVideoSender's
// SDP offer. This is the root cause of Bug #2 (reconnect-rebuild-fixes cycle).
//
// Test is tagged #[ignore] — it confirms the BUG EXISTS (str0m's Rtc rejects the
// second offer). The fix (stop_flag guard in run_signaling_drain) prevents the OLD
// drain from ever calling apply_remote_offer on the NEW offer. This test remains
// as a PROTOCOL SEMANTICS ANCHOR: if it ever starts PASSING without code changes,
// str0m's upstream behavior changed.
//
// Run manually: cargo nextest run --workspace --run-ignored -E 'test(sc_mlo_4)'

/// SC-MLO-4 — Real-str0m: OLD receiver rejects second offer with m-line order error.
///
/// Protocol anchor (D-RDF-6, reconnect-reset-drain-fix): documents that str0m's
/// `accept_offer` enforces m-line ordering across renegotiations (RFC 8843 /
/// RFC 8829 semantics). When a fresh `Str0mVideoSender::new()` is created (new
/// `Rtc` instance, different m-line internals) and its offer is applied to the
/// ORIGINAL receiver (unmodified `Rtc` state from the first session), str0m
/// rejects it with "Changed order for m-line with mid: X".
///
/// **Post-fix meaning (reconnect-reset-drain-fix / REQ-RRD-1):** The receiver
/// code path no longer triggers this scenario. After REQ-RRD-1 the reset drain
/// (`DrainRole::ResetSignalingOnly`, D-RDF-2) no longer applies offers to the
/// stale Rtc — the rebuild worker's fresh Rtc (DrainRole::Primary drain) is the
/// sole offer-application owner. This test now serves as an upstream str0m
/// behavior anchor only: if str0m ever changes such that a second offer on the
/// SAME Rtc succeeds, document the behavioral change before removing this test.
///
/// Run with `--run-ignored` to verify str0m still rejects (should still fail).
///
/// ARCHIVE GATE note: if this test starts PASSING unexpectedly, str0m behavior
/// changed upstream — document it before removing the test. This note MUST be
/// preserved.
///
/// No network, no mDNS, no TCP — purely in-process `Rtc` instantiation.
#[test]
#[ignore = "Protocol anchor (D-RDF-6): documents that str0m rejects a second SDP offer \
             on the SAME (stale) Rtc with 'Changed order for m-line'. \
             After REQ-RRD-1 (reconnect-reset-drain-fix), the receiver code path no \
             longer triggers this: the reset drain (DrainRole::ResetSignalingOnly) no \
             longer applies offers to the stale Rtc. \
             Run with --run-ignored to verify str0m still rejects (should still fail). \
             If this test PASSES without code changes, str0m behavior changed upstream -- \
             document before removing. ARCHIVE-GATE: see doc-comment above."]
fn sc_mlo_4_str0m_rejects_offer2_with_mline_conflict() {
    use sm_domain::transport::{TransportError, VideoSender};

    // First sender + receiver session: offer/answer exchange succeeds.
    let sender_1 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-MLO-4: sender_1 new must succeed");

    let receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })
    .expect("SC-MLO-4: receiver new must succeed");

    let offer_1 = sender_1
        .create_local_offer()
        .expect("SC-MLO-4: sender_1 create_local_offer must succeed");

    // First apply_remote_offer: MUST succeed (baseline session established).
    let _answer_1 = receiver
        .apply_remote_offer(offer_1)
        .expect("SC-MLO-4: first apply_remote_offer must return Ok(SdpAnswer)");

    // Simulate a rebuild: NEW sender with fresh Rtc, NEW offer.
    // This mirrors what happens during a reconnect: Str0mVideoSender::new() creates
    // a fresh Rtc instance with a different m-line ordering than the original sender.
    let sender_2 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-MLO-4: sender_2 new must succeed");

    let offer_2 = sender_2
        .create_local_offer()
        .expect("SC-MLO-4: sender_2 create_local_offer must succeed");

    // Apply second offer to the ORIGINAL (unmodified, non-rebuilt) receiver.
    // str0m MUST reject this with a "Changed order for m-line" or similar error.
    let result = receiver.apply_remote_offer(offer_2);

    assert!(
        result.is_err(),
        "SC-MLO-4 (REQ-MLO-2): expected Err from apply_remote_offer on second fresh-sender \
         offer, but got Ok. str0m behavior may have changed upstream."
    );

    let err_str = match result.unwrap_err() {
        TransportError::Internal(s) => s,
        other => format!("{other:?}"),
    };

    // str0m error message contains "Changed order for m-line" or "accept_offer failed".
    // The exact message depends on str0m version but both variants indicate the same
    // protocol-level rejection.
    assert!(
        err_str.contains("Changed order for m-line")
            || err_str.contains("accept_offer failed")
            || err_str.contains("m-line"),
        "SC-MLO-4 (REQ-MLO-2): error must reference m-line order conflict; got: {err_str}"
    );
}

// ─── SC-RRD-2: fresh-Rtc offer acceptance anchor (reconnect-reset-drain-fix) ──

/// SC-RRD-2 — A FRESH receiver Rtc MUST accept a FRESH sender Rtc's offer
/// without m-line conflict (REQ-RRD-2).
///
/// This is the CORRECT code path that the fix enables: after a sender-process
/// restart the rebuild worker creates a brand-new `Str0mVideoReceiver` (fresh
/// Rtc) and the `DrainRole::Primary` drain on that fresh receiver applies the
/// fresh sender's offer without any "Changed order for m-line" error.
///
/// **Contrast with sc_mlo_4:** sc_mlo_4 (still `#[ignore]`, D-RDF-6) shows
/// that applying a fresh sender's offer to the STALE receiver Rtc FAILS. This
/// test shows that applying it to a FRESH receiver Rtc SUCCEEDS — confirming
/// that the fix (DrainRole::ResetSignalingOnly suppresses the stale path while
/// DrainRole::Primary on the fresh path remains the sole offer-application owner)
/// is correct end-to-end.
///
/// **Note:** This test passes even on the baseline (it exercises the correct
/// path, not the buggy one). Its purpose is to document the invariant and guard
/// against future regressions in the fresh-Rtc acceptance flow.
///
/// No network, no mDNS, no TCP — purely in-process `Rtc` instantiation.
///
/// Satisfies: SC-RRD-2, REQ-RRD-2.
#[test]
fn sc_rdf_3_fresh_rtc_accepts_fresh_sender_offer() {
    use sm_domain::transport::VideoSender;

    // ── First session: sender_1 + receiver_1 complete initial offer/answer ──
    let sender_1 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-2: sender_1 new must succeed");

    let receiver_1 = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-2: receiver_1 new must succeed");

    let offer_1 = sender_1
        .create_local_offer()
        .expect("SC-RRD-2: sender_1 create_local_offer must succeed");

    // First apply_remote_offer: MUST succeed (baseline session established).
    let _answer_1 = receiver_1
        .apply_remote_offer(offer_1)
        .expect("SC-RRD-2: first apply_remote_offer (sender_1 -> receiver_1) must return Ok");
    // Note: Str0mVideoReceiver does not expose apply_local_answer — the answer
    // is used by the signaling layer externally; the receiver's Rtc state is
    // updated internally by apply_remote_offer itself.

    // ── Second session: fresh sender_2 + fresh receiver_2 (rebuild worker path) ──
    // Simulates: sender process fully restarted (new Rtc, new m-lines) AND
    // rebuild worker produced a fresh Str0mVideoReceiver (DrainRole::Primary drain).
    let sender_2 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-2: sender_2 new must succeed");

    let receiver_2 = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-2: receiver_2 (fresh Rtc) new must succeed");

    let offer_2 = sender_2
        .create_local_offer()
        .expect("SC-RRD-2: sender_2 create_local_offer must succeed");

    // Apply fresh sender's offer to the FRESH receiver — MUST succeed (no m-line conflict).
    // This is the DrainRole::Primary path in the fix. The stale-Rtc rejection is documented
    // by sc_mlo_4 (still #[ignore], D-RDF-6). A fresh Rtc MUST accept a fresh sender's offer.
    let result = receiver_2.apply_remote_offer(offer_2);

    assert!(
        result.is_ok(),
        "SC-RRD-2: fresh receiver Rtc MUST accept a fresh sender's Offer without \
         m-line conflict (REQ-RRD-2). This is the DrainRole::Primary path. \
         Got Err: {:?}",
        result.err()
    );
}

/// SC-RRD-C — NO-COMPETE invariant: the restarted sender's Offer MUST land only on
/// the fresh (gen-G+1) Rtc; the stale (gen-G) Rtc MUST NEVER receive it.
///
/// This test closes the blind-spot that PR-B's `sc_srr_4` left open: that test used
/// a single `CountingReceiverOps` spy and asserted `apply_remote_offer` call count==1.
/// It could not distinguish *which* Rtc received the offer — a stale Rtc with the right
/// count is structurally identical to a fresh Rtc with the right count in that test.
///
/// This test uses TWO DISTINCT REAL `Str0mVideoReceiver` instances (no spy):
/// - `stale_1` simulates the gen-G receiver Rtc (pre-reset, has prior m-line state).
/// - `fresh_2` simulates the gen-G+1 receiver Rtc (rebuild's Primary drain target).
/// - `sender_2` simulates the RESTARTED sender process (fresh Offer, new m-lines).
///
/// ## Assertions
///
/// 1. `fresh_2.apply_remote_offer(offer_b)` MUST return `Ok` — the fresh Rtc accepts
///    the restarted sender's Offer (no prior m-line state conflict). This is the target
///    state that Approach C (NO-COMPETE) achieves: only the fresh Primary drain receives
///    the Offer.
///
/// 2. `stale_1.apply_remote_offer(offer_b)` MUST return `Err` — the stale Rtc REJECTS
///    the same Offer because it already has m-line state from `offer_a` (a different
///    sender's session). This is the exact defect PR-B's bypass triggered: when the reset
///    drain (bound to `stale_1`) applied `offer_b`, str0m returned "Changed order for
///    m-line" (issue #870). The ONLY fix is to ensure `stale_1` NEVER sees `offer_b`.
///
/// Together the two assertions prove: applying a fresh sender's Offer to a stale Rtc
/// is FATAL (Err), therefore the routing MUST guarantee stale_1 is never offered.
/// Approach C achieves this by construction — no competing reset browse, so only the
/// fresh Primary drain can receive the restarted sender's single Offer.
///
/// No network, no mDNS, no TCP — purely in-process `Rtc` instantiation.
///
/// Satisfies: REQ-SRR-4 (NO-COMPETE redefined), REQ-SRR-5 INV-5b.
#[test]
fn sc_rrd_c_fresh_offer_lands_on_fresh_rtc_only() {
    use sm_domain::transport::VideoSender;

    // ── Session 1: stale gen-G Rtc — sender_1 + stale_1 ──────────────────────
    // Simulates the established session BEFORE sender restart. stale_1 is the
    // receiver Rtc that the reset hook's ResetSignalingOnly drain is bound to.
    let sender_1 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-C: sender_1 new must succeed");

    let stale_1 = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-C: stale_1 (gen-G Rtc) new must succeed");

    let offer_a = sender_1
        .create_local_offer()
        .expect("SC-RRD-C: sender_1 create_local_offer must succeed");

    // Establish m-line state on stale_1 — this is what makes stale_1 unable to
    // accept a new sender's offer (m-line ordering conflict = issue #870).
    stale_1
        .apply_remote_offer(offer_a)
        .expect("SC-RRD-C: stale_1 must accept offer_a (initial session negotiation)");

    // ── Session 2: fresh gen-G+1 Rtc — sender_2 + fresh_2 ────────────────────
    // Simulates the RESTARTED sender process and the rebuild worker's new Rtc.
    // sender_2 is the restarted sender; fresh_2 is the DrainRole::Primary drain's Rtc.
    let sender_2 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-C: sender_2 (restarted sender) new must succeed");

    let fresh_2 = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .expect("SC-RRD-C: fresh_2 (gen-G+1 Rtc) new must succeed");

    let offer_b = sender_2
        .create_local_offer()
        .expect("SC-RRD-C: sender_2 (restarted sender) create_local_offer must succeed");

    // ── WHEN: NO-COMPETE routing — fresh_2 (Primary) receives offer_b ────────
    //
    // Approach C guarantees: only the rebuild's gen-G+1 Primary drain can connect
    // to the restarted sender (no competing reset re-browse). Therefore offer_b
    // reaches ONLY fresh_2. We simulate this by calling apply_remote_offer directly
    // on fresh_2 (the Primary drain path).
    let result_fresh = fresh_2.apply_remote_offer(offer_b.clone());

    // ── THEN (Assertion 1): fresh Rtc accepts the restarted sender's Offer ────
    assert!(
        result_fresh.is_ok(),
        "SC-RRD-C (Assertion 1): fresh_2 (gen-G+1 Rtc, DrainRole::Primary) MUST accept \
         the restarted sender's Offer (no prior m-line state conflict). \
         Got Err: {:?}",
        result_fresh.err()
    );

    // ── THEN (Assertion 2): stale Rtc REJECTS the same Offer (m-line conflict) ─
    //
    // This is the exact failure mode PR-B's bypass triggered: when the reset drain
    // (bound to stale_1) applied offer_b to the stale Rtc, str0m returned
    // "Changed order for m-line" (#870). The stale Rtc's m-line state (from offer_a)
    // is incompatible with a fresh sender's m-line ordering.
    //
    // Approach C: stale_1 NEVER sees offer_b — but this assertion proves WHY it must
    // not: applying offer_b to stale_1 is FATAL. If routing ever delivers offer_b to
    // stale_1, reconnect fails deterministically.
    let result_stale = stale_1.apply_remote_offer(offer_b);

    assert!(
        result_stale.is_err(),
        "SC-RRD-C (Assertion 2): stale_1 (gen-G Rtc, ResetSignalingOnly) MUST reject \
         the restarted sender's Offer (m-line conflict with prior offer_a session state). \
         Got Ok — which means the Rtc accepted a conflicting offer, indicating str0m \
         m-line state was not established correctly in this test setup."
    );
}

// ─── P1 receiver probe facade contract ──────────────────────────────────────

fn run_p1_receiver_lifecycle(probe: Option<Arc<TransportLedgerProbe>>) {
    let (completion_tx, completion_rx) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .expect("P1 receiver construction must succeed");

        if let Some(probe) = probe {
            receiver.install_transport_ledger_probe_for_test(probe);
        }

        let (packet_tx, _packet_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        let result = receiver
            .start(packet_tx, event_tx)
            .and_then(|()| receiver.stop())
            .map_err(|error| error.to_string());
        let _ = completion_tx.send(result);
    });

    match completion_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => worker.join().expect("P1 receiver worker must not panic"),
        Ok(Err(error)) => {
            worker.join().expect("P1 receiver worker must not panic");
            panic!("P1 receiver lifecycle must succeed: {error}");
        }
        Err(error) => {
            panic!("P1 receiver lifecycle must complete within the test deadline: {error}")
        }
    }
}

#[test]
fn pr5b_p1_receiver_collecting_probe_attaches_before_start_and_stays_inert() {
    let probe = Arc::new(TransportLedgerProbe::collecting());

    run_p1_receiver_lifecycle(Some(Arc::clone(&probe)));

    assert_eq!(probe.positions(), LedgerPositions::default());
    assert_eq!(probe.attempted_delta(), [0; 4]);
}

#[test]
fn pr5b_p1_receiver_rejecting_probe_attaches_before_start_and_stays_inert() {
    let probe = Arc::new(TransportLedgerProbe::rejecting());

    run_p1_receiver_lifecycle(Some(Arc::clone(&probe)));

    assert_eq!(probe.positions(), LedgerPositions::default());
    assert_eq!(probe.attempted_delta(), [0; 4]);
}

#[test]
fn pr5b_p1_receiver_unattached_lifecycle_stays_operational_and_inert() {
    let unattached_probe = Arc::new(TransportLedgerProbe::collecting());

    run_p1_receiver_lifecycle(None);

    assert_eq!(unattached_probe.positions(), LedgerPositions::default());
    assert_eq!(unattached_probe.attempted_delta(), [0; 4]);
}

// ─── PR5B P2A RED: fixed-source opaque relay contracts ──────────────────────

const P2A_RELAY_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Eq, PartialEq)]
enum RelayError {
    Io(std::io::ErrorKind),
    UnexpectedSource(SocketAddr),
    NotRegistered,
    Timeout,
    InvalidEndpoints,
    UnexpectedEvent,
    AlreadyRunning,
    CommandBusy,
    Disconnected,
    ShutdownTimeout,
    CompletionDisconnected,
    WorkerPanicked,
    StaleToken,
    SelectionBusy,
    NotHeld,
}

type RelayResult<T> = Result<T, RelayError>;

struct DatagramObservation {
    source: SocketAddr,
    bytes: Vec<u8>,
}

enum WorkerCommand {
    ResumeUnwind,
    DisconnectCompletion,
}

enum WorkerExit {
    Stopped,
    Panicked,
}

enum WorkerEvent {
    Held { token: u64 },
}

struct SelectedRtp {
    token: u64,
    source_index: usize,
    bytes: [u8; 12],
}

struct HeldRtp {
    token: u64,
    source_index: usize,
    bytes: Vec<u8>,
}

struct HoldState {
    tokens: [u64; 2],
    next_token: u64,
    selected: Option<SelectedRtp>,
    held: Option<HeldRtp>,
}

enum RelayState {
    Idle,
    Running,
    Stopping,
    Stopped,
}

struct BidirectionalUdpRelay {
    socket: UdpSocket,
    endpoints: [SocketAddr; 2],
    registered: [bool; 2],
    worker: Option<std::thread::JoinHandle<()>>,
    state: RelayState,
    stop: Arc<AtomicBool>,
    command_tx: Option<SyncSender<WorkerCommand>>,
    completion_rx: Option<Receiver<WorkerExit>>,
    hold_state: Arc<Mutex<HoldState>>,
    event_rx: Option<Receiver<WorkerEvent>>,
}

impl BidirectionalUdpRelay {
    fn bind(endpoints: [SocketAddr; 2]) -> RelayResult<Self> {
        if endpoints[0] == endpoints[1] || endpoints[0].is_ipv4() != endpoints[1].is_ipv4() {
            return Err(RelayError::InvalidEndpoints);
        }

        let socket = UdpSocket::bind(if endpoints[0].is_ipv4() {
            "127.0.0.1:0"
        } else {
            "[::1]:0"
        })
        .map_err(|error| RelayError::Io(error.kind()))?;

        Ok(Self {
            socket,
            endpoints,
            registered: [false; 2],
            worker: None,
            state: RelayState::Idle,
            stop: Arc::new(AtomicBool::new(false)),
            command_tx: None,
            completion_rx: None,
            hold_state: Arc::new(Mutex::new(HoldState {
                tokens: [1, 2],
                next_token: 3,
                selected: None,
                held: None,
            })),
            event_rx: None,
        })
    }

    fn relay_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .expect("P2A relay exposes its bound address")
    }

    fn register_endpoint(&mut self, addr: SocketAddr) -> RelayResult<()> {
        let Some(index) = self.endpoints.iter().position(|endpoint| *endpoint == addr) else {
            return Err(RelayError::UnexpectedSource(addr));
        };

        let mut hold_state = self
            .hold_state
            .lock()
            .map_err(|_| RelayError::Disconnected)?;
        self.registered[index] = true;
        hold_state.tokens[index] = hold_state.next_token;
        hold_state.next_token = hold_state.next_token.saturating_add(1);
        hold_state.selected = hold_state
            .selected
            .take()
            .filter(|selected| selected.source_index != index);
        hold_state.held = hold_state
            .held
            .take()
            .filter(|held| held.source_index != index);
        Ok(())
    }

    fn start(&mut self) -> RelayResult<()> {
        if self.worker.is_some() {
            return Err(RelayError::AlreadyRunning);
        }
        let (command_tx, command_rx) = sync_channel(1);
        let (event_tx, event_rx) = sync_channel(1);
        let (completion_tx, completion_rx) = sync_channel(1);
        let socket = self
            .socket
            .try_clone()
            .map_err(|error| RelayError::Io(error.kind()))?;
        let endpoints = self.endpoints;
        let registered = self.registered;
        let stop = Arc::clone(&self.stop);
        let hold_state = Arc::clone(&self.hold_state);
        self.stop.store(false, Ordering::Release);
        self.worker = Some(std::thread::spawn(move || {
            let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker_loop(
                    socket, endpoints, registered, stop, command_rx, event_tx, hold_state,
                )
            }));
            if let Some(exit) = worker_exit_after_unwind(exit) {
                let _ = completion_tx.try_send(exit);
            }
        }));
        self.command_tx = Some(command_tx);
        self.completion_rx = Some(completion_rx);
        self.event_rx = Some(event_rx);
        self.state = RelayState::Running;
        Ok(())
    }

    fn recv_at(
        &mut self,
        endpoint: &UdpSocket,
        timeout: Duration,
    ) -> RelayResult<DatagramObservation> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RelayError::Timeout)?;
        if matches!(self.state, RelayState::Idle) {
            self.route_one(deadline)?;
        }
        self.observe_at(endpoint, deadline)
    }

    fn route_one(&self, deadline: Instant) -> RelayResult<()> {
        let mut bytes = [0_u8; 65_535];
        self.socket
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| RelayError::Io(error.kind()))?;
        let (length, source) = self.socket.recv_from(&mut bytes).map_err(receive_error)?;
        let source_index = self
            .endpoints
            .iter()
            .position(|registered| *registered == source)
            .ok_or(RelayError::UnexpectedSource(source))?;
        if !self.registered[source_index] {
            return Err(RelayError::NotRegistered);
        }

        let destination = self.endpoints[1 - source_index];
        self.socket
            .send_to(&bytes[..length], destination)
            .map_err(|error| RelayError::Io(error.kind()))?;
        Ok(())
    }

    fn observe_at(
        &self,
        endpoint: &UdpSocket,
        deadline: Instant,
    ) -> RelayResult<DatagramObservation> {
        let mut observed = [0_u8; 65_535];
        endpoint
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| RelayError::Io(error.kind()))?;
        let (observed_length, observed_source) =
            endpoint.recv_from(&mut observed).map_err(receive_error)?;
        if observed_source != self.relay_addr() {
            return Err(RelayError::UnexpectedSource(observed_source));
        }

        Ok(DatagramObservation {
            source: observed_source,
            bytes: observed[..observed_length].to_vec(),
        })
    }

    fn inject_worker_command(&self, command: WorkerCommand) -> RelayResult<()> {
        let Some(command_tx) = &self.command_tx else {
            return Err(RelayError::Disconnected);
        };
        command_tx.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => RelayError::CommandBusy,
            TrySendError::Disconnected(_) => RelayError::Disconnected,
        })
    }

    fn current_token(&self, endpoint: SocketAddr) -> RelayResult<u64> {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|candidate| *candidate == endpoint)
        else {
            return Err(RelayError::UnexpectedSource(endpoint));
        };
        if !self.registered[index] {
            return Err(RelayError::NotRegistered);
        }
        self.hold_state
            .lock()
            .map_err(|_| RelayError::Disconnected)
            .map(|hold_state| hold_state.tokens[index])
    }

    fn hold_selected_rtp(&self, token: u64, selected_rtp: [u8; 12]) -> RelayResult<()> {
        let mut hold_state = self
            .hold_state
            .lock()
            .map_err(|_| RelayError::Disconnected)?;
        let Some(source_index) = hold_state
            .tokens
            .iter()
            .position(|current_token| *current_token == token)
        else {
            return Err(RelayError::StaleToken);
        };
        if hold_state.selected.is_some() || hold_state.held.is_some() {
            return Err(RelayError::SelectionBusy);
        }
        hold_state.selected = Some(SelectedRtp {
            token,
            source_index,
            bytes: selected_rtp,
        });
        Ok(())
    }

    fn wait_until_selected_rtp_is_held(&mut self, timeout: Duration) -> RelayResult<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RelayError::Timeout)?;
        let expected_token = self
            .hold_state
            .lock()
            .map_err(|_| RelayError::Disconnected)
            .and_then(|hold_state| {
                hold_state
                    .selected
                    .as_ref()
                    .map(|selected| selected.token)
                    .or_else(|| hold_state.held.as_ref().map(|held| held.token))
                    .ok_or(RelayError::UnexpectedEvent)
            })?;
        let event_rx = self.event_rx.as_ref().ok_or(RelayError::Disconnected)?;
        match event_rx.recv_timeout(remaining(deadline)?) {
            Ok(WorkerEvent::Held { token }) if token == expected_token => Ok(()),
            Ok(WorkerEvent::Held { .. }) => Err(RelayError::UnexpectedEvent),
            Err(RecvTimeoutError::Timeout) => Err(RelayError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(RelayError::Disconnected),
        }
    }

    fn release_selected_rtp(&self, token: u64) -> RelayResult<()> {
        let (bytes, destination) = {
            let mut hold_state = self
                .hold_state
                .lock()
                .map_err(|_| RelayError::Disconnected)?;
            if !hold_state.tokens.contains(&token) {
                return Err(RelayError::StaleToken);
            }
            let Some(held) = hold_state.held.as_ref() else {
                return Err(RelayError::NotHeld);
            };
            if held.token != token {
                return Err(RelayError::StaleToken);
            }
            let held = hold_state.held.take().ok_or(RelayError::NotHeld)?;
            (held.bytes, self.endpoints[1 - held.source_index])
        };
        self.socket
            .send_to(&bytes, destination)
            .map_err(|error| RelayError::Io(error.kind()))?;
        Ok(())
    }

    fn shutdown(&mut self, timeout: Duration) -> RelayResult<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RelayError::ShutdownTimeout)?;
        self.state = RelayState::Stopping;
        self.stop.store(true, Ordering::Release);
        let Some(completion_rx) = &self.completion_rx else {
            self.state = RelayState::Stopped;
            return Ok(());
        };
        match completion_rx.recv_timeout(remaining(deadline)?) {
            Ok(WorkerExit::Stopped) => {
                let result = self.cleanup(remaining(deadline)?);
                self.state = RelayState::Stopped;
                result
            }
            Ok(WorkerExit::Panicked) => Err(RelayError::WorkerPanicked),
            Err(RecvTimeoutError::Timeout) => Err(RelayError::ShutdownTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(RelayError::CompletionDisconnected),
        }
    }

    fn cleanup(&mut self, timeout: Duration) -> RelayResult<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RelayError::ShutdownTimeout)?;
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            remaining(deadline).map_err(|_| RelayError::ShutdownTimeout)?;
            std::thread::yield_now();
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| RelayError::WorkerPanicked)?;
        }
        self.command_tx = None;
        self.completion_rx = None;
        self.event_rx = None;
        self.state = RelayState::Stopped;
        Ok(())
    }

    fn recv_error(&mut self, timeout: Duration) -> RelayResult<RelayError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RelayError::Timeout)?;
        let mut bytes = [0_u8; 65_535];
        self.socket
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| RelayError::Io(error.kind()))?;
        let (_, source) = self.socket.recv_from(&mut bytes).map_err(receive_error)?;

        if self.endpoints.contains(&source) {
            Err(RelayError::UnexpectedEvent)
        } else {
            Ok(RelayError::UnexpectedSource(source))
        }
    }
}

impl Drop for BidirectionalUdpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.cleanup(Duration::from_millis(100));
    }
}

fn worker_loop(
    socket: UdpSocket,
    endpoints: [SocketAddr; 2],
    registered: [bool; 2],
    stop: Arc<AtomicBool>,
    command_rx: Receiver<WorkerCommand>,
    event_tx: SyncSender<WorkerEvent>,
    hold_state: Arc<Mutex<HoldState>>,
) -> Option<WorkerExit> {
    let _ = socket.set_read_timeout(Some(Duration::from_millis(10)));
    let mut bytes = [0_u8; 65_535];
    loop {
        match command_rx.try_recv() {
            Ok(WorkerCommand::ResumeUnwind) => {
                std::panic::resume_unwind(Box::new("P2A2 deterministic worker unwind"));
            }
            Ok(WorkerCommand::DisconnectCompletion) => return None,
            Err(TryRecvError::Disconnected) => return Some(WorkerExit::Stopped),
            Err(TryRecvError::Empty) => {}
        }
        if stop.load(Ordering::Acquire) {
            return Some(WorkerExit::Stopped);
        }
        let Ok((length, source)) = socket.recv_from(&mut bytes) else {
            continue;
        };
        let Some(index) = endpoints.iter().position(|endpoint| *endpoint == source) else {
            continue;
        };
        if !registered[index] || stop.load(Ordering::Acquire) {
            continue;
        }
        let held_token = {
            let Ok(mut hold_state) = hold_state.lock() else {
                continue;
            };
            let matches_selected = hold_state.selected.as_ref().is_some_and(|selected| {
                selected.source_index == index && bytes[..length] == selected.bytes
            });
            if !matches_selected {
                None
            } else if let Some(selected) = hold_state.selected.take() {
                let token = selected.token;
                hold_state.held = Some(HeldRtp {
                    token,
                    source_index: index,
                    bytes: bytes[..length].to_vec(),
                });
                Some(token)
            } else {
                None
            }
        };
        if let Some(token) = held_token {
            let _ = event_tx.try_send(WorkerEvent::Held { token });
            continue;
        }
        let _ = socket.send_to(&bytes[..length], endpoints[1 - index]);
    }
}

fn remaining(deadline: Instant) -> RelayResult<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or(RelayError::Timeout)
}

fn receive_error(error: std::io::Error) -> RelayError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => RelayError::Timeout,
        kind => RelayError::Io(kind),
    }
}

fn bind_p2a_endpoint() -> UdpSocket {
    let endpoint = UdpSocket::bind("127.0.0.1:0").expect("P2A endpoint binds");
    endpoint
        .set_read_timeout(Some(P2A_RELAY_TIMEOUT))
        .expect("P2A endpoint uses a bounded receive");
    endpoint
}

#[test]
fn pr5b_p2a_relay_forwards_opaque_datagrams_bidirectionally() {
    let endpoint_a = bind_p2a_endpoint();
    let endpoint_b = bind_p2a_endpoint();
    let mut relay: BidirectionalUdpRelay = BidirectionalUdpRelay::bind([
        endpoint_a.local_addr().expect("endpoint A address"),
        endpoint_b.local_addr().expect("endpoint B address"),
    ])
    .expect("P2A relay binds fixed endpoints");
    let opaque_a_to_b = [0x00, 0xff, 0x10, 0x80, 0x7f];
    let opaque_b_to_a = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x00];

    relay
        .register_endpoint(endpoint_a.local_addr().expect("endpoint A address"))
        .expect("P2A relay registers endpoint A");
    relay
        .register_endpoint(endpoint_b.local_addr().expect("endpoint B address"))
        .expect("P2A relay registers endpoint B");

    endpoint_a
        .send_to(&opaque_a_to_b, relay.relay_addr())
        .expect("endpoint A sends opaque datagram to relay");
    let received_at_b = relay
        .recv_at(&endpoint_b, P2A_RELAY_TIMEOUT)
        .expect("endpoint B receives relay delivery within the test bound");
    assert_eq!(received_at_b.source, relay.relay_addr());
    assert_eq!(received_at_b.bytes, opaque_a_to_b);

    endpoint_b
        .send_to(&opaque_b_to_a, relay.relay_addr())
        .expect("endpoint B sends opaque datagram to relay");
    let received_at_a = relay
        .recv_at(&endpoint_a, P2A_RELAY_TIMEOUT)
        .expect("endpoint A receives relay delivery within the test bound");
    assert_eq!(received_at_a.source, relay.relay_addr());
    assert_eq!(received_at_a.bytes, opaque_b_to_a);
}

#[test]
fn pr5b_p2a_relay_forwards_control_bytes_unchanged() {
    let endpoint_a = bind_p2a_endpoint();
    let endpoint_b = bind_p2a_endpoint();
    let mut relay: BidirectionalUdpRelay = BidirectionalUdpRelay::bind([
        endpoint_a.local_addr().expect("endpoint A address"),
        endpoint_b.local_addr().expect("endpoint B address"),
    ])
    .expect("P2A relay binds fixed endpoints");
    let control = [0x80, 0xc8, 0x00, 0x06, 0x12, 0x34, 0x56, 0x78];

    relay
        .register_endpoint(endpoint_a.local_addr().expect("endpoint A address"))
        .expect("P2A relay registers endpoint A");
    relay
        .register_endpoint(endpoint_b.local_addr().expect("endpoint B address"))
        .expect("P2A relay registers endpoint B");

    endpoint_a
        .send_to(&control, relay.relay_addr())
        .expect("endpoint A sends control bytes to relay");
    let delivered = relay
        .recv_at(&endpoint_b, P2A_RELAY_TIMEOUT)
        .expect("endpoint B receives control bytes within the test bound");

    assert_eq!(delivered.source, relay.relay_addr());
    assert_eq!(delivered.bytes, control);
}

#[test]
fn pr5b_p2a_relay_reports_unexpected_source_for_unregistered_sender() {
    let endpoint_a = bind_p2a_endpoint();
    let endpoint_b = bind_p2a_endpoint();
    let unregistered_sender = bind_p2a_endpoint();
    let mut relay: BidirectionalUdpRelay = BidirectionalUdpRelay::bind([
        endpoint_a.local_addr().expect("endpoint A address"),
        endpoint_b.local_addr().expect("endpoint B address"),
    ])
    .expect("P2A relay binds fixed endpoints");

    relay
        .register_endpoint(endpoint_a.local_addr().expect("endpoint A address"))
        .expect("P2A relay registers endpoint A");
    relay
        .register_endpoint(endpoint_b.local_addr().expect("endpoint B address"))
        .expect("P2A relay registers endpoint B");
    unregistered_sender
        .send_to(&[0x00, 0xff, 0x10], relay.relay_addr())
        .expect("unregistered fixed source sends to relay");

    assert_eq!(
        relay
            .recv_error(P2A_RELAY_TIMEOUT)
            .expect("P2A relay publishes an unexpected-source error"),
        RelayError::UnexpectedSource(
            unregistered_sender
                .local_addr()
                .expect("unregistered sender address")
        )
    );
}

// ─── PR5B P2A2 RED: bounded worker lifecycle contracts ──────────────────────

fn bind_started_p2a2_relay() -> (BidirectionalUdpRelay, UdpSocket, UdpSocket) {
    let endpoint_a = bind_p2a_endpoint();
    let endpoint_b = bind_p2a_endpoint();
    let mut relay = BidirectionalUdpRelay::bind([
        endpoint_a.local_addr().expect("endpoint A address"),
        endpoint_b.local_addr().expect("endpoint B address"),
    ])
    .expect("P2A2 relay binds fixed endpoints");
    assert!(relay.worker.is_none(), "bind must not own a worker");
    relay
        .register_endpoint(endpoint_a.local_addr().expect("endpoint A address"))
        .expect("P2A2 relay registers endpoint A");
    relay
        .register_endpoint(endpoint_b.local_addr().expect("endpoint B address"))
        .expect("P2A2 relay registers endpoint B");
    relay.start().expect("P2A2 relay starts one worker");
    assert!(matches!(relay.worker.as_ref(), Some(worker) if !worker.is_finished()));
    (relay, endpoint_a, endpoint_b)
}

#[test]
fn pr5b_p2a2_relay_recv_at_is_single_attempt_and_bounded() {
    let (mut relay, _endpoint_a, endpoint_b) = bind_started_p2a2_relay();

    assert!(matches!(
        relay.recv_at(&endpoint_b, P2A_RELAY_TIMEOUT),
        Err(RelayError::Timeout)
    ));
}

#[test]
fn pr5b_p2a2_relay_shutdown_joins_and_prevents_post_shutdown_forwarding() {
    let (mut relay, endpoint_a, endpoint_b) = bind_started_p2a2_relay();
    let shutdown = relay.shutdown(P2A_RELAY_TIMEOUT);
    assert!(shutdown.is_ok() && relay.worker.is_none());
    endpoint_a
        .send_to(&[0x81, 0x60, 0x00, 0x01], relay.relay_addr())
        .expect("endpoint A sends only after shutdown");

    assert!(matches!(
        relay.recv_at(&endpoint_b, P2A_RELAY_TIMEOUT),
        Err(RelayError::Timeout)
    ));
}

#[test]
fn pr5b_p2a2_relay_worker_panic_is_reported_and_cleaned_up_boundedly() {
    let (mut relay, _endpoint_a, _endpoint_b) = bind_started_p2a2_relay();
    relay
        .inject_worker_command(WorkerCommand::ResumeUnwind)
        .expect("P2A2 relay accepts the resume-unwind command");

    let shutdown = relay.shutdown(P2A_RELAY_TIMEOUT);
    assert!(shutdown == Err(RelayError::WorkerPanicked) && relay.worker.is_some());
    let cleanup = relay.cleanup(P2A_RELAY_TIMEOUT);
    assert!(cleanup.is_ok() && relay.worker.is_none());
}

fn worker_exit_after_unwind(result: std::thread::Result<Option<WorkerExit>>) -> Option<WorkerExit> {
    match result {
        Ok(exit) => exit,
        Err(_) => Some(WorkerExit::Panicked),
    }
}

#[test]
fn pr5b_p2a2_relay_completion_disconnect_retains_handle_for_bounded_cleanup() {
    let (mut relay, _endpoint_a, _endpoint_b) = bind_started_p2a2_relay();
    relay
        .inject_worker_command(WorkerCommand::DisconnectCompletion)
        .expect("P2A2 relay accepts the completion-disconnect command");

    let shutdown = relay.shutdown(P2A_RELAY_TIMEOUT);
    assert!(shutdown == Err(RelayError::CompletionDisconnected) && relay.worker.is_some());
    let cleanup = relay.cleanup(P2A_RELAY_TIMEOUT);
    assert!(cleanup.is_ok() && relay.worker.is_none());
}

#[test]
fn worker_exit_after_unwind_maps_caught_unwind_to_panicked() {
    let unwind = std::panic::catch_unwind(|| {
        std::panic::resume_unwind(Box::new("deterministic relay unwind"));
    });

    assert!(matches!(
        worker_exit_after_unwind(unwind),
        Some(WorkerExit::Panicked)
    ));
}

#[test]
fn worker_exit_after_unwind_preserves_normal_and_disconnected_exits() {
    assert!(matches!(
        worker_exit_after_unwind(Ok(Some(WorkerExit::Stopped))),
        Some(WorkerExit::Stopped)
    ));
    assert!(worker_exit_after_unwind(Ok(None)).is_none());
}

// ─── PR5B P2B RED: selected RTP hold and generation-token contracts ─────────

fn select_and_hold_rtp(
    relay: &mut BidirectionalUdpRelay,
    endpoint: &UdpSocket,
    selected_rtp: [u8; 12],
) -> u64 {
    let token = relay
        .current_token(endpoint.local_addr().expect("endpoint address"))
        .expect("P2B relay returns the current endpoint token");
    relay
        .hold_selected_rtp(token, selected_rtp)
        .expect("P2B relay selects one exact RTP header");
    endpoint
        .send_to(&selected_rtp, relay.relay_addr())
        .expect("endpoint sends the selected RTP packet");
    relay
        .wait_until_selected_rtp_is_held(P2A_RELAY_TIMEOUT)
        .expect("P2B relay confirms the selected RTP packet is held");
    token
}

#[test]
fn pr5b_p2b_relay_holds_only_exact_selected_rtp() {
    let (mut relay, endpoint_a, endpoint_b) = bind_started_p2a2_relay();
    let selected_rtp = [0x80, 0x60, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 1];
    let _token = select_and_hold_rtp(&mut relay, &endpoint_a, selected_rtp);

    assert!(matches!(
        relay.recv_at(&endpoint_b, P2A_RELAY_TIMEOUT),
        Err(RelayError::Timeout)
    ));
}

#[test]
fn pr5b_p2b_relay_forwards_nonselected_rtp_and_control_while_held() {
    let (mut relay, endpoint_a, endpoint_b) = bind_started_p2a2_relay();
    let selected_rtp = [0x80, 0x60, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 1];
    let nonselected_rtp = [0x80, 0x60, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 1];
    let control = [0x80, 0xc8, 0x00, 0x06, 0x12, 0x34, 0x56, 0x78];
    let _token = select_and_hold_rtp(&mut relay, &endpoint_a, selected_rtp);

    endpoint_a
        .send_to(&nonselected_rtp, relay.relay_addr())
        .expect("endpoint A sends a non-selected RTP packet");
    assert_eq!(
        relay
            .recv_at(&endpoint_b, P2A_RELAY_TIMEOUT)
            .expect("non-selected RTP must still be forwarded")
            .bytes,
        nonselected_rtp
    );

    endpoint_a
        .send_to(&control, relay.relay_addr())
        .expect("endpoint A sends control bytes while RTP is held");
    assert_eq!(
        relay
            .recv_at(&endpoint_b, P2A_RELAY_TIMEOUT)
            .expect("control bytes must still be forwarded")
            .bytes,
        control
    );
}

#[test]
fn pr5b_p2b_relay_releases_selected_rtp_once_for_current_token() {
    let (mut relay, endpoint_a, endpoint_b) = bind_started_p2a2_relay();
    let selected_rtp = [0x80, 0x60, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 1];
    let token = select_and_hold_rtp(&mut relay, &endpoint_a, selected_rtp);

    relay
        .release_selected_rtp(token)
        .expect("current token releases the held RTP packet once");
    assert_eq!(
        relay
            .recv_at(&endpoint_b, P2A_RELAY_TIMEOUT)
            .expect("released RTP packet is delivered")
            .bytes,
        selected_rtp
    );
    assert!(matches!(
        relay.recv_at(&endpoint_b, P2A_RELAY_TIMEOUT),
        Err(RelayError::Timeout)
    ));
}

#[test]
fn pr5b_p2b_relay_rejects_stale_token_without_delivery() {
    let (mut relay, endpoint_a, endpoint_b) = bind_started_p2a2_relay();
    let selected_rtp = [0x80, 0x60, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x01, 0, 0, 0, 1];
    let endpoint_a_addr = endpoint_a.local_addr().expect("endpoint A address");
    let stale_token = select_and_hold_rtp(&mut relay, &endpoint_a, selected_rtp);
    relay
        .register_endpoint(endpoint_a_addr)
        .expect("re-registering an endpoint rotates its token");

    assert_eq!(
        relay.release_selected_rtp(stale_token),
        Err(RelayError::StaleToken)
    );
    assert!(matches!(
        relay.recv_at(&endpoint_b, P2A_RELAY_TIMEOUT),
        Err(RelayError::Timeout)
    ));
}
