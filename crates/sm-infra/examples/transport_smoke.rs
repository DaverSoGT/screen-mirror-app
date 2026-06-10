//! End-to-end transport smoke: synthetic packet pump → `Str0mVideoSender` → loopback
//! WebRTC → `Str0mVideoReceiver` → packet counter.
//!
//! This example wires the full transport pipeline without a real display or encoder:
//!
//! ```text
//! synthetic_pump thread (Annex-B IDR frames every 33 ms)
//!     │  SyncSender<EncodedPacket>
//!     ▼
//! Str0mVideoSender  ──UDP loopback──  Str0mVideoReceiver
//!     │  LoopbackSignaling (in-memory)  │
//!     └──────── offer/answer/ICE ───────┘
//!                                       │  SyncSender<EncodedPacket>
//!                                       ▼
//!                               packet counter + keyframe assertion
//! ```
//!
//! The smoke test:
//! 1. Performs the full SDP offer/answer exchange via `LoopbackSignaling`.
//! 2. Exchanges ICE candidates after `start()` to make loopback connectivity work.
//! 3. Waits for ICE to connect (up to 5 s).
//! 4. Pumps synthetic Annex-B IDR frames for 5 s after ICE connects.
//! 5. Asserts that at least one `EncodedPacket` with `is_keyframe == true` arrives.
//!
//! # Usage
//!
//! ```text
//! cargo run -p sm-infra --example transport_smoke
//! ```
//!
//! The example exits 0 on success, 1 on failure. It is CI-safe (no real network,
//! no display, no audio).
//!
//! # Channel wiring
//!
//! The pump thread holds `pkt_tx: SyncSender<EncodedPacket>`. The sender tick thread
//! holds `pkt_rx: Receiver<EncodedPacket>`. Dropping `pkt_tx` unblocks the sender's
//! `try_recv` loop and lets it see the stop flag.
//!
//! # Shutdown order
//!
//! 1. Drop `pkt_tx` (signals the sender tick loop that the pump is done).
//! 2. Stop the sender.
//! 3. Stop the receiver.
//! 4. Stop both signaling halves.

use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use sm_domain::encode::EncodedPacket;
use sm_domain::signaling::{IceCandidate, Signaling, SignalingEvent, SignalingRole};
use sm_domain::transport::{
    TransportConfig, TransportEvent, TransportRole, VideoReceiver, VideoSender,
};
use sm_infra::signaling::loopback::LoopbackSignaling;
use sm_infra::transport::{Str0mVideoReceiver, Str0mVideoSender};

// ─── Annex-B synthetic frame ─────────────────────────────────────────────────

/// Build a minimal Annex-B IDR frame: SPS (NAL 7) + PPS (NAL 8) + IDR slice (NAL 5).
///
/// The payloads are trivially minimal — enough for str0m's H264 packetizer to
/// transmit and for `contains_idr_nal` to flag `is_keyframe = true` on the receiver.
fn synthetic_idr_frame() -> Arc<[u8]> {
    let mut buf = Vec::new();
    // SPS (NAL type 7 = 0x67), 4-byte start code
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e]);
    // PPS (NAL type 8 = 0x68), 4-byte start code
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]);
    // IDR slice (NAL type 5 = 0x65), 4-byte start code, minimal payload
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff]);
    Arc::from(buf.as_slice())
}

// ─── Synthetic pump thread ───────────────────────────────────────────────────

/// Spawn a background thread that pumps synthetic IDR frames into `pkt_tx` at ~30 fps.
///
/// Returns an `Arc<std::thread::JoinHandle<()>>` — drop the returned pkt_tx to stop
/// the pump (the channel becoming disconnected causes the thread to exit).
fn spawn_pump(pkt_tx: SyncSender<EncodedPacket>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("synthetic-pump".into())
        .spawn(move || {
            let frame_interval = Duration::from_millis(33); // ~30 fps
            let mut seq = 0u64;
            loop {
                let pkt = EncodedPacket {
                    data: synthetic_idr_frame(),
                    is_keyframe: true,
                    timestamp: Duration::from_millis(seq * 33),
                    sequence: seq,
                };
                seq += 1;
                // try_send: if the channel is full or disconnected, stop pumping.
                if pkt_tx.try_send(pkt).is_err() {
                    break;
                }
                std::thread::sleep(frame_interval);
            }
        })
        .expect("spawn synthetic pump thread")
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    println!("transport_smoke — full WebRTC loopback pipeline smoke test");
    println!("  synthetic pump → Str0mVideoSender → UDP loopback → Str0mVideoReceiver");
    println!();

    match run_smoke() {
        Ok(()) => {
            println!();
            println!("PASS: transport_smoke completed successfully");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!();
            eprintln!("FAIL: transport_smoke failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_smoke() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Build LoopbackSignaling pair ───────────────────────────────────────
    let (mut sender_sig, mut receiver_sig) =
        LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

    let (sender_sig_event_tx, sender_sig_event_rx) = sync_channel::<SignalingEvent>(8);
    let (receiver_sig_event_tx, receiver_sig_event_rx) = sync_channel::<SignalingEvent>(8);

    sender_sig.start(sender_sig_event_tx)?;
    receiver_sig.start(receiver_sig_event_tx)?;

    // ── 2. Build transport adapters ───────────────────────────────────────────
    let mut sender = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })?;

    let mut receiver = Str0mVideoReceiver::new(TransportConfig {
        udp_port: 0,
        role: TransportRole::Receiver,
        ..TransportConfig::default()
    })?;

    // ── 3. Pre-start SDP offer/answer exchange ────────────────────────────────
    let offer = sender.create_local_offer()?;
    println!(
        "  [1/6] local SDP offer generated ({} bytes)",
        offer.0.len()
    );

    // Publish offer via signaling and let receiver pick it up.
    // Cold-start attempt is 1 (matching supervisor.rs:268 seed and receiver expected_attempt seed).
    sender_sig.publish_local_offer(offer.clone(), 1)?;
    let offer_received = match receiver_sig_event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SignalingEvent::OfferReceived(o, _attempt)) => o,
        Ok(other) => return Err(format!("expected OfferReceived, got {other:?}").into()),
        Err(e) => return Err(format!("OfferReceived timeout: {e}").into()),
    };

    let answer = receiver.apply_remote_offer(offer_received)?;
    println!(
        "  [2/6] remote offer applied; answer generated ({} bytes)",
        answer.0.len()
    );

    receiver_sig.publish_local_answer(answer)?;
    let answer_received = match sender_sig_event_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(SignalingEvent::AnswerReceived(a)) => a,
        Ok(other) => return Err(format!("expected AnswerReceived, got {other:?}").into()),
        Err(e) => return Err(format!("AnswerReceived timeout: {e}").into()),
    };

    // ── 4. Create channels and start transport tick threads ───────────────────
    // The packet channel: pump thread → sender tick loop.
    // Use a generous capacity so the pump can get ahead.
    let (pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(32);
    let (sender_event_tx, sender_event_rx) = sync_channel::<TransportEvent>(16);
    let (pkt_out_tx, pkt_out_rx) = sync_channel::<EncodedPacket>(64);
    let (receiver_event_tx, _receiver_event_rx) = sync_channel::<TransportEvent>(16);

    sender.start(pkt_rx, sender_event_tx)?;
    receiver.start(pkt_out_tx, receiver_event_tx)?;

    println!("  [3/6] transport tick threads started");

    // ── 5. Apply answer + exchange ICE candidates ─────────────────────────────
    sender.apply_remote_answer(answer_received)?;

    let sender_addr = sender
        .local_addr()
        .ok_or("sender local_addr not available after start()")?;
    let receiver_addr = receiver
        .local_addr()
        .ok_or("receiver local_addr not available after start()")?;

    println!("  [4/6] exchanging ICE candidates (sender={sender_addr}, receiver={receiver_addr})");

    let sender_cand = str0m::Candidate::host(sender_addr, "udp")?;
    let receiver_cand = str0m::Candidate::host(receiver_addr, "udp")?;

    let sender_cand_json = serde_json::to_string(&sender_cand)?;
    let receiver_cand_json = serde_json::to_string(&receiver_cand)?;

    receiver.add_remote_candidate(IceCandidate(sender_cand_json))?;
    sender.add_remote_candidate(IceCandidate(receiver_cand_json))?;

    // ── 6. Wait for ICE to connect ────────────────────────────────────────────
    let ice_deadline = Instant::now() + Duration::from_secs(5);
    let mut ice_connected = false;
    while Instant::now() < ice_deadline {
        match sender_event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(TransportEvent::IceConnected) => {
                ice_connected = true;
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if !ice_connected {
        return Err("ICE did not connect within 5 s".into());
    }
    println!("  [5/6] ICE connected");

    // ── 7. Start the synthetic pump and collect packets ───────────────────────
    // Start pumping AFTER ICE connects so the sender has a valid SRTP path.
    // The pump sends IDR frames continuously at ~30 fps.
    let pump_handle = spawn_pump(pkt_tx.clone());

    let run_deadline = Instant::now() + Duration::from_secs(5);
    let mut total_packets = 0u32;
    let mut keyframe_packets = 0u32;
    let mut bytes_received = 0u64;

    println!("  [6/6] collecting packets for 5 s...");

    while Instant::now() < run_deadline {
        match pkt_out_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(pkt) => {
                total_packets += 1;
                if pkt.is_keyframe {
                    keyframe_packets += 1;
                }
                bytes_received += pkt.data.len() as u64;
                // Print a dot for each batch of 10 packets so CI can see progress.
                if total_packets % 10 == 0 {
                    print!(".");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    println!(); // newline after the dots

    // ── 8. Shutdown ───────────────────────────────────────────────────────────
    // Stop the sender tick thread first — this drops pkt_rx, which causes the
    // pump's try_send to return Err(Full/Disconnected) → pump thread exits.
    // We must NOT join the pump first because the pump loops forever until the
    // receiver end (pkt_rx) is dropped.
    drop(pkt_tx); // drop our clone so the only remaining sender is the pump's clone
    sender.stop()?; // joins sender tick thread; this drops pkt_rx inside the thread
    // Now pump's next try_send fails → pump exits.
    let _ = pump_handle.join();

    receiver.stop()?;
    sender_sig.stop()?;
    receiver_sig.stop()?;

    // ── 9. Report ─────────────────────────────────────────────────────────────
    println!();
    println!("Results:");
    println!("  total packets received:    {total_packets}");
    println!("  keyframe packets received: {keyframe_packets}");
    println!("  bytes received:            {bytes_received}");
    println!("  sender dropped_frames:     {}", sender.dropped_frames());
    println!("  receiver dropped_frames:   {}", receiver.dropped_frames());

    // ── 10. Assert at least one keyframe arrived ──────────────────────────────
    if keyframe_packets == 0 {
        return Err(format!(
            "assertion failed: expected ≥1 keyframe packet, got 0 \
             (total_packets={total_packets})"
        )
        .into());
    }

    println!();
    println!("Assertion PASSED: ≥1 keyframe received (got {keyframe_packets})");

    Ok(())
}
