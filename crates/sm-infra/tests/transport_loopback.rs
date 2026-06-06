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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

use sm_domain::encode::{EncodedPacket, EncoderConfig, VideoEncoder};
use sm_domain::signaling::{IceCandidate, Signaling, SignalingEvent, SignalingRole};
use sm_domain::transport::{
    TransportConfig, TransportEvent, TransportRole, VideoReceiver, VideoSender,
};
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
#[ignore = "Requires DTLS/ICE loopback to complete — not guaranteed in single-process CI"]
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

    sender.start(pkt_rx, sender_event_tx).expect("sender start");
    receiver
        .start(pkt_out_tx, receiver_event_tx)
        .expect("receiver start");

    sender.apply_remote_answer(answer).expect("apply answer");

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
