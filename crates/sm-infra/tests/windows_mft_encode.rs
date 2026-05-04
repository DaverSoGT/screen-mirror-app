//! Hardware H.264 MFT encoder integration tests.
//!
//! All tests are gated `#[cfg(all(target_os = "windows", feature = "hw-encoder"))]`
//! and marked `#[ignore]` because they require a Windows host with a dedicated GPU
//! (Intel Quick Sync, NVIDIA NVENC, AMD AMF, or any Windows-registered hardware H.264 MFT).
//!
//! # Run on a hardware-capable machine
//!
//!     cargo nextest run -p sm-infra --features hw-encoder --run-ignored only --tests windows_mft_encode
//!
//! # Naming convention
//!
//! All tests in this file follow the DD6 naming pattern:
//!     `mft_<scenario>_<expectation>`
//!
//! # CI behaviour
//!
//!     cargo nextest run --workspace
//!
//! skips every test in this file (all are `#[ignore]`). This satisfies R11/T11.1.
#![cfg(all(target_os = "windows", feature = "hw-encoder"))]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use sm_domain::capture::PixelFormat;
use sm_domain::encode::{EncoderConfig, EncoderError, VideoEncoder};
use sm_infra::encode::windows_mft::WindowsMftH264Encoder;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Build a synthetic BGRA8 `CaptureFrame` with a gradient pattern.
///
/// The gradient (B=row%256, G=col%256, R=128, A=255) gives the codec meaningful
/// content to encode. All-black frames produce trivially small IDR packets that
/// exercise almost no rate-control or P-frame logic.
fn make_synthetic_frame(width: u32, height: u32, ts_ms: u64) -> sm_domain::CaptureFrame {
    let stride = width * 4;
    let total = (stride * height) as usize;
    let mut data = vec![0u8; total];

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

    sm_domain::CaptureFrame {
        data: Arc::from(data.as_slice()),
        width,
        height,
        stride,
        format: PixelFormat::Bgra8,
        timestamp: Duration::from_millis(ts_ms),
    }
}

// ─── T3.1 / T3.2: MFT hardware enumeration ───────────────────────────────────

/// T3.2 — `new()` succeeds when a hardware H.264 MFT is registered.
///
/// Also covers T3.1 implicitly: if no HW encoder is available, `new()` returns
/// `Err(InitFailed)` and this test panics with a clear message (expected on CI).
/// On a machine WITH hardware: asserts `Ok(_)` and that `drop` does not panic.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_new_on_hw_capable_machine_returns_ok() {
    let enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed on a HW-capable machine");
    // drop() must not panic (tests MFShutdown + CoUninitialize path).
    drop(enc);
}

/// T3.1 — `new()` returns `Err(InitFailed)` when no hardware MFT is available.
///
/// Run on a machine WITHOUT a compatible GPU, or inject a MFTEnumEx stub that
/// returns an empty list. On a HW machine this test passes vacuously (Ok variant).
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a machine without a GPU to test InitFailed path"]
fn mft_new_returns_init_failed_when_no_hardware_mft() {
    match WindowsMftH264Encoder::new(EncoderConfig::default()) {
        Err(EncoderError::InitFailed(_)) => {
            // Expected: no hardware MFT present.
        }
        Ok(_) => {
            // Hardware MFT found — test passes vacuously (wrong machine).
        }
        Err(other) => {
            panic!("unexpected error variant from new() on no-GPU machine: {other:?}");
        }
    }
}

// ─── T4.1 / T4.2: Annex-B output ─────────────────────────────────────────────

/// T4.1 — Every encoded packet starts with the Annex-B 4-byte start code.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_encoded_packet_starts_with_annex_b_start_code() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("frame_tx should be open");

    let pkt = pkt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("encoded packet should arrive within 5 s");

    assert!(
        pkt.data.len() >= 4,
        "packet too short: {} bytes",
        pkt.data.len()
    );
    assert_eq!(
        &pkt.data[..4],
        &[0x00, 0x00, 0x00, 0x01],
        "first encoded packet must start with Annex-B start code 00 00 00 01"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

/// T4.2 — 30-frame smoke: at least 1 keyframe and at least 10 non-keyframes within 10 s.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_thirty_frame_smoke_emits_at_least_one_keyframe() {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const N_FRAMES: u64 = 30;
    const DEADLINE: Duration = Duration::from_secs(10);

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let producer = std::thread::spawn(move || {
        for i in 0..N_FRAMES {
            let frame = make_synthetic_frame(WIDTH, HEIGHT, i * 33);
            if frame_tx.send(frame).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(33)); // ~30 fps
        }
        // frame_tx dropped here.
    });

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
                    assert_eq!(
                        &pkt.data[..4],
                        &[0x00, 0x00, 0x00, 0x01],
                        "keyframe must start with Annex-B start code"
                    );
                } else {
                    p_frame_count += 1;
                }
                println!(
                    "[T4.2] pkt {} is_keyframe={} len={} elapsed={:.1}ms",
                    received,
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
        "[T4.2] done: {received} packets ({keyframe_count} IDR + {p_frame_count} P) in {elapsed:?}"
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

// ─── T5.1: Timestamp passthrough ─────────────────────────────────────────────

/// T5.1 — `EncodedPacket.timestamp` must equal the source `CaptureFrame.timestamp`.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_encoded_packet_timestamp_matches_capture_frame() {
    const EXPECTED_TS: Duration = Duration::from_millis(500);

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    frame_tx
        .send(make_synthetic_frame(640, 480, 500))
        .expect("frame_tx should be open");

    let pkt = pkt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("encoded packet should arrive within 5 s");

    assert_eq!(
        pkt.timestamp, EXPECTED_TS,
        "EncodedPacket.timestamp ({:?}) must equal CaptureFrame.timestamp ({:?})",
        pkt.timestamp, EXPECTED_TS
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T7.1 / T7.2: request_keyframe ───────────────────────────────────────────

/// T7.1 — `request_keyframe()` causes the next encoded packet to be a keyframe.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_request_keyframe_marks_next_packet_as_keyframe() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = || {
        pkt_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("packet should arrive within 3 s")
    };

    // Drain the initial IDR.
    send_frame(0);
    let first = recv_pkt();
    assert!(first.is_keyframe, "first packet must be an IDR");

    // Encode a few P-frames.
    for i in 1..=3u64 {
        send_frame(i);
        let pkt = recv_pkt();
        println!("[T7.1] P-frame {i}: is_keyframe={}", pkt.is_keyframe);
    }

    // Request a forced keyframe.
    enc.request_keyframe();
    send_frame(4);

    let forced_idr = recv_pkt();
    println!(
        "[T7.1] forced IDR: is_keyframe={} seq={}",
        forced_idr.is_keyframe, forced_idr.sequence
    );

    // Spec R7: the next packet after request_keyframe() MUST be a keyframe
    // and its data MUST begin with NAL type 7 (SPS) = 0x00 0x00 0x00 0x01 0x67.
    assert!(
        forced_idr.is_keyframe,
        "packet after request_keyframe() must have is_keyframe == true"
    );
    assert!(
        forced_idr.data.len() >= 5,
        "forced IDR packet too short: {} bytes",
        forced_idr.data.len()
    );
    assert_eq!(
        &forced_idr.data[..4],
        &[0x00, 0x00, 0x00, 0x01],
        "forced IDR must start with Annex-B start code"
    );
    // NAL type 7 = SPS (byte & 0x1F == 0x07).
    assert_eq!(
        forced_idr.data[4] & 0x1F,
        0x07,
        "first NAL in forced IDR must be SPS (type 7), got type {}",
        forced_idr.data[4] & 0x1F
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

/// T7.2 — After a forced IDR is emitted, the `request_keyframe` flag is cleared
/// so the following packet is a P-frame.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_keyframe_flag_cleared_after_idr_emitted() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = || {
        pkt_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("packet should arrive within 3 s")
    };

    // Drain initial IDR.
    send_frame(0);
    let _ = recv_pkt(); // initial IDR

    // Encode P-frames then force an IDR.
    for i in 1..=2u64 {
        send_frame(i);
        let _ = recv_pkt();
    }

    enc.request_keyframe();
    send_frame(3);
    let forced = recv_pkt();
    assert!(forced.is_keyframe, "forced IDR must be a keyframe");

    // The NEXT packet (after the IDR) must NOT be a keyframe — flag was cleared.
    send_frame(4);
    let after_idr = recv_pkt();
    assert!(
        !after_idr.is_keyframe,
        "packet after forced IDR must have is_keyframe == false, got true"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T8.2: set_bitrate runtime update ────────────────────────────────────────

/// T8.2 — `set_bitrate()` updates the encoder at runtime without restarting the thread.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_set_bitrate_updates_encoder_without_restart() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Feed a few frames at 4 Mbps.
    for i in 0..3u64 {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx open");
        let _ = pkt_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("packet should arrive");
    }

    // Update bitrate to 8 Mbps — must return Ok(()) without restarting.
    let result = enc.set_bitrate(8_000_000);
    assert!(
        result.is_ok(),
        "set_bitrate(8_000_000) should return Ok(()), got {result:?}"
    );

    // Continue encoding — encoder thread must still be alive (channel not disconnected).
    for i in 3..6u64 {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx must still be open after set_bitrate");
        let pkt = pkt_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("packet should arrive after bitrate update");
        println!(
            "[T8.2] post-bitrate-update pkt {}: is_keyframe={} len={}",
            i,
            pkt.is_keyframe,
            pkt.data.len()
        );
    }

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T13.1 / T13.2: Lifecycle ────────────────────────────────────────────────

/// T13.1 — `stop()` is idempotent: calling it twice returns `Ok(())` both times.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_stop_is_idempotent() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(4);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(4);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    drop(frame_tx); // signal encoder thread to drain and exit

    // First stop — must join the thread and return Ok(()).
    enc.stop().expect("first stop() should succeed");

    // Second stop — must return Ok(()) without panic (idempotent).
    enc.stop()
        .expect("second stop() should be idempotent and return Ok(())");
}

/// T13.2 — Dropping without calling `stop()` must not leak the encoder thread.
///
/// This test is inherently best-effort: we cannot directly inspect the OS thread
/// count from safe Rust. The assertion is that `drop` completes within a
/// reasonable deadline and does not panic — verifying the JoinHandle is joined.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_drop_without_stop_does_not_leak_thread() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig::default())
        .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(4);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(4);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Drop frame_tx first so the encoder thread can drain and exit.
    drop(frame_tx);

    // Drop the encoder without calling stop() — Drop impl must join the thread.
    // If the thread leaked, the process would hang here (test timeout would fire).
    let t0 = Instant::now();
    drop(enc);
    let elapsed = t0.elapsed();

    // Drop must complete promptly (encoder thread had no frames to process).
    assert!(
        elapsed < Duration::from_secs(5),
        "drop() took {elapsed:?} — expected to complete within 5 s (thread may have leaked)"
    );
}
