//! Integration tests for `WindowsOpenH264Encoder`.
//!
//! All tests are gated `#[cfg(target_os = "windows")]` and marked `#[ignore]`
//! because they run the full encoder stack (BGRA→I420 + OpenH264 SW encode) and
//! are intended to run manually on a Windows host with:
//!
//!     cargo nextest run -p sm-infra --run-ignored only --tests windows_encode
//!
//! NASM in PATH gives OpenH264 a 2–3× SIMD speedup on the encode hot loop.
//! NASM is OPTIONAL — without it, OpenH264 falls back to portable C.
#![cfg(target_os = "windows")]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use sm_domain::capture::PixelFormat;
use sm_domain::encode::{EncoderConfig, VideoEncoder};
use sm_infra::encode::WindowsOpenH264Encoder;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Build a synthetic 1920×1080 BGRA8 `CaptureFrame`.
///
/// The pixel values vary by row to give the codec meaningful content to encode
/// (all-black frames collapse to trivially small IDR packets with no P-frames
/// exercising rate control). A gradient by row index provides enough variety.
fn make_synthetic_frame(width: u32, height: u32, ts_ms: u64) -> sm_domain::encode::FramePayload {
    let stride = width * 4;
    let total = (stride * height) as usize;
    let mut data = vec![0u8; total];

    // Fill with a simple gradient: B=row%256, G=col%256, R=128, A=255.
    for row in 0..height as usize {
        let row_base = row * stride as usize;
        for col in 0..width as usize {
            let pix = row_base + col * 4;
            data[pix] = (row % 256) as u8; // B
            data[pix + 1] = (col % 256) as u8; // G
            data[pix + 2] = 128u8; // R
            data[pix + 3] = 255u8; // A
        }
    }

    sm_domain::encode::FramePayload::Cpu(sm_domain::CaptureFrame {
        data: Arc::from(data.as_slice()),
        width,
        height,
        stride,
        format: PixelFormat::Bgra8,
        timestamp: Duration::from_millis(ts_ms),
    })
}

// ─── I1: synthetic 30-frame smoke — ≥1 IDR + ≥10 P-frames ────────────────────
//
// Spec R14.4 IT1: End-to-end smoke — feed 30 synthetic 1920×1080 BGRA frames,
// observe ≥1 keyframe (is_keyframe == true) and ≥10 non-keyframe packets,
// all within 5 s wall-clock. Also asserts Annex-B start code at offset 0.

#[test]
#[ignore]
fn synthetic_bgra_30_frames_yields_idr_and_p_frames() {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const N_FRAMES: u64 = 30;
    const DEADLINE: Duration = Duration::from_secs(5);

    let mut enc = WindowsOpenH264Encoder::new(EncoderConfig::default())
        .expect("encoder construction should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Feed 30 frames at ~33 ms intervals (30 fps) on a producer thread.
    let producer = std::thread::spawn(move || {
        for i in 0..N_FRAMES {
            let frame = make_synthetic_frame(WIDTH, HEIGHT, i * 33);
            if frame_tx.send(frame).is_err() {
                break; // encoder stopped early
            }
            // Pace the producer to give the encoder time to process.
            std::thread::sleep(Duration::from_millis(33));
        }
        // frame_tx dropped here — signals encoder thread to exit.
    });

    // Collect packets from the output channel.
    let t0 = Instant::now();
    let mut keyframe_count = 0usize;
    let mut p_frame_count = 0usize;
    let mut received = 0usize;

    while received < N_FRAMES as usize && t0.elapsed() < DEADLINE {
        match pkt_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(pkt) => {
                received += 1;
                if pkt.is_keyframe {
                    keyframe_count += 1;
                    // Verify Annex-B start code at offset 0.
                    assert!(
                        pkt.data.len() >= 4,
                        "keyframe packet too short: {} bytes",
                        pkt.data.len()
                    );
                    assert_eq!(
                        &pkt.data[..4],
                        &[0x00, 0x00, 0x00, 0x01],
                        "keyframe packet must start with Annex-B start code"
                    );
                } else {
                    p_frame_count += 1;
                }
                println!(
                    "[I1] pkt {} seq={} is_keyframe={} len={} elapsed={:.1}ms",
                    received,
                    pkt.sequence,
                    pkt.is_keyframe,
                    pkt.data.len(),
                    t0.elapsed().as_millis()
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    producer.join().expect("producer thread should not panic");
    enc.stop().expect("stop should succeed");

    let elapsed = t0.elapsed();
    println!(
        "[I1] done: {received} packets ({keyframe_count} IDR + {p_frame_count} P) in {elapsed:?}"
    );

    assert!(
        keyframe_count >= 1,
        "expected ≥1 IDR keyframe, got {keyframe_count}"
    );
    assert!(
        p_frame_count >= 10,
        "expected ≥10 P-frames, got {p_frame_count}"
    );
    assert!(
        elapsed < DEADLINE,
        "encoder produced {received} frames in {elapsed:?} — expected < {DEADLINE:?}"
    );
}

// ─── I2: request_keyframe mid-stream forces IDR on next packet ────────────────
//
// Spec R14.4 IT2: stream 10 P-frames (skip the initial IDR), call request_keyframe(),
// read the next packet and assert is_keyframe == true.

#[test]
#[ignore]
fn request_keyframe_midstream_produces_idr_on_next_packet() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsOpenH264Encoder::new(EncoderConfig::default())
        .expect("encoder construction should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Feed frames sequentially from the same thread via a helper.
    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = || {
        pkt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("packet should arrive within 2 s")
    };

    // ── Step 1: drain the initial IDR ────────────────────────────────────────
    // OpenH264 always emits an IDR as the first packet. Skip it.
    send_frame(0);
    let first = recv_pkt();
    assert!(
        first.is_keyframe,
        "first packet after start must be an IDR, got is_keyframe={}",
        first.is_keyframe
    );

    // ── Step 2: encode a few P-frames ────────────────────────────────────────
    for i in 1..=5u64 {
        send_frame(i);
        let pkt = recv_pkt();
        println!(
            "[I2] P-frame {i}: is_keyframe={} seq={}",
            pkt.is_keyframe, pkt.sequence
        );
    }

    // ── Step 3: request a keyframe, send one more frame ──────────────────────
    enc.request_keyframe();
    send_frame(6);

    // The next packet MUST be an IDR.
    let forced_idr = recv_pkt();
    println!(
        "[I2] forced IDR: is_keyframe={} seq={}",
        forced_idr.is_keyframe, forced_idr.sequence
    );
    assert!(
        forced_idr.is_keyframe,
        "packet after request_keyframe() must have is_keyframe == true"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── I3: slow consumer — dropped_frames counter increments ────────────────────
//
// Spec R14.4 IT3: use output sync_channel(2). Sleep consumer for 500 ms while
// feeding frames at ~60 fps. After 30 encode iterations, assert dropped_frames() > 0.

#[test]
#[ignore]
fn slow_consumer_increments_dropped_frames() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const N_FRAMES: u64 = 30;

    let mut enc = WindowsOpenH264Encoder::new(EncoderConfig {
        framerate: 60,
        ..EncoderConfig::default()
    })
    .expect("encoder construction should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(32);
    // Deliberate small capacity — fills up quickly with a sleeping consumer.
    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<sm_domain::encode::EncodedPacket>(2);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Producer: flood the encoder at 60 fps pace.
    let producer = std::thread::spawn(move || {
        for i in 0..N_FRAMES {
            let frame = make_synthetic_frame(WIDTH, HEIGHT, i * 16);
            if frame_tx.send(frame).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(16)); // ~60 fps
        }
        // frame_tx dropped here.
    });

    // Consumer: sleep 500 ms without reading — causes channel to fill.
    std::thread::sleep(Duration::from_millis(500));

    // Drain whatever is left in the channel (don't care about contents).
    while pkt_rx.try_recv().is_ok() {}

    producer.join().expect("producer should not panic");
    // frame_tx was moved into the producer closure and dropped when the thread finished.

    enc.stop().expect("stop should succeed");

    let dropped = enc.dropped_frames();
    println!("[I3] dropped_frames = {dropped}");
    assert!(
        dropped > 0,
        "expected dropped_frames > 0 with slow consumer, got {dropped}"
    );
}
