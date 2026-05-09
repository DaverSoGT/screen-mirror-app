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

/// Install a tracing subscriber for the duration of one test process.
///
/// nextest runs each test in its own process so a single try_init per test
/// is fine. Use RUST_LOG to control verbosity (e.g. `sm_infra::encode=trace`).
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

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

// ─── T1.1 (Bucket B / Phase 3): Drop ordering — new() then drop without start() ──

/// T1.1 — Constructing a `WindowsMftH264Encoder` and immediately dropping it
/// (without ever calling `start()`) MUST NOT cause an access violation.
///
/// **Before the Drop fix**: process aborts with `0xc0000005` because Rust's
/// automatic field-drop releases COM pointers after `MFShutdown` runs in the
/// old `Drop` body.
/// **After the Drop fix**: `drop(self.mft.take())` + `drop(self.codec_api.take())`
/// execute BEFORE `MFShutdown` so the AV is impossible.
///
/// This is the canonical RED→GREEN evidence for Bucket B (spec R1, T1.1).
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_new_then_drop_does_not_av() {
    let enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");
    // Immediately drop — no start() call.
    // BEFORE fix: ABORT 0xc0000005. AFTER fix: clean return.
    drop(enc);
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
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("frame_tx should be open");

    enc.flush();

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
    init_tracing();
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

    enc.stop().expect("stop should succeed");
    producer.join().expect("producer thread should not panic");

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

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    frame_tx
        .send(make_synthetic_frame(640, 480, 500))
        .expect("frame_tx should be open");

    enc.flush();

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
///
/// CARRY-FORWARD: Slice 4 (`hw-encoder-mft-codec-api-counter-desync`, PR pending) eliminated
/// the codec_api desync panic at windows_mft.rs:1266 and added drain-state handling, but
/// empirically confirmed (Phase 0 P0-B trace #747 + Host A smoke at 70821f1) that Intel QSV
/// does NOT honor `MFSampleExtension_CleanPoint=1` nor `ICodecAPI::SetValue(ForceKeyFrame)`
/// post-ProcessInput for mid-stream IDR. T7.1 reverted to master body (timeout failure mode
/// on Intel QSV — clean Slice-3 stalling, not a panic). Reopened in v2 candidate
/// `hw-encoder-mft-intel-qsv-mid-stream-idr` (#764) with Phase 0 research on alternative
/// IDR mechanisms (GOP-size toggle, Discontinuity attribute, drain+resume cycle).
#[test]
#[ignore = "carry-forward: Intel QSV mid-stream IDR unresolved — see hw-encoder-mft-intel-qsv-mid-stream-idr (#764)"]
fn mft_request_keyframe_marks_next_packet_as_keyframe() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        ..EncoderConfig::default()
    })
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

    // CARRY-FORWARD: this test fails on master daa9522 with timeout (Intel QSV needs ≥3
    // frames before producing output; first recv times out on single-frame submission).
    // Slice 3's flush() does NOT address it because mid-stream codec_api manipulation
    // (request_keyframe via apply_pending_codec_settings) triggers a separate pump_loop
    // counter desync at windows_mft.rs:1266. Carry-forward to a future slice that
    // addresses pump_loop NOTACCEPTING handling around codec_api operations.
    // Reverted to master body (timeout failure mode is cleaner than encoder-thread panic).

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
///
/// CARRY-FORWARD: same root cause as T7.1 — Intel QSV mid-stream IDR mechanism unknown.
/// See T7.1 docstring and v2 candidate `hw-encoder-mft-intel-qsv-mid-stream-idr` (#764).
/// Reverted to master body (timeout failure mode on Intel QSV).
#[test]
#[ignore = "carry-forward: Intel QSV mid-stream IDR unresolved — see hw-encoder-mft-intel-qsv-mid-stream-idr (#764)"]
fn mft_keyframe_flag_cleared_after_idr_emitted() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        ..EncoderConfig::default()
    })
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

    // CARRY-FORWARD: same root cause as T7.1 — pump_loop counter desync when
    // codec_api operations (request_keyframe) interleave with NeedInput credits.
    // Reverted to master body. Will be addressed in a follow-up slice.

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
///
/// Slice 4 GREEN: `set_bitrate()` works mid-stream on both Intel QSV and NVENC under the
/// SWAP-FIRE split (DD1) + drain-state guard (DD14) + post-drain resume (DD17/F2). The
/// encoder thread stays alive and continues emitting packets after the bitrate change.
/// Maps R4, R6, R9, spec S6.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_set_bitrate_updates_encoder_without_restart() {
    init_tracing();
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const PRIMING_COUNT: u64 = 3;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    // Batch-push priming frames and flush.
    for i in 0..PRIMING_COUNT {
        send_frame(i);
    }
    enc.flush();

    // Drain priming output.
    let mut priming_pkts: Vec<_> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(pkt) => {
                priming_pkts.push(pkt);
                if priming_pkts.len() >= PRIMING_COUNT as usize {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("encoder died during priming drain — pump thread exited unexpectedly")
            }
        }
        if priming_pkts.len() >= 20 {
            break;
        }
    }
    assert!(
        !priming_pkts.is_empty(),
        "expected at least one priming packet, got none"
    );
    println!("[T8.2] priming drain: {} packets", priming_pkts.len());

    // Update bitrate — must return Ok(()) without restarting the encoder thread.
    let result = enc.set_bitrate(8_000_000);
    assert!(
        result.is_ok(),
        "set_bitrate(8_000_000) should return Ok(()), got {result:?}"
    );

    // Push 3 more frames and flush. If the encoder is alive, frame_tx.send() succeeds
    // and the drain loop returns packets.  If the thread died from codec_api desync,
    // send() panics (Disconnected) or the drain loop returns Disconnected immediately.
    for i in PRIMING_COUNT..PRIMING_COUNT + 3 {
        send_frame(i);
    }
    enc.flush();

    // Drain post-bitrate-update output.
    let mut post_pkts: Vec<_> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(pkt) => {
                println!(
                    "[T8.2] post-bitrate pkt {}: is_keyframe={} len={}",
                    post_pkts.len(),
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                post_pkts.push(pkt);
                if post_pkts.len() >= 10 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "encoder died after set_bitrate() — codec_api desync (C1 RED expected on Intel QSV)"
                )
            }
        }
    }

    // Spec R4, R6: encoder thread must be alive — verified by receiving packets after
    // the bitrate update (no Disconnected above means thread survived).
    assert!(
        !post_pkts.is_empty(),
        "expected at least one packet after set_bitrate(), got none — encoder may have died"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T6.1 (spec R6): Annex-B detection on no-probe path ─────────────────────

/// T6.1 — After probe removal, the first real output packet must start with the
/// Annex-B 4-byte start code `0x00 0x00 0x00 0x01`.
///
/// This test constructs the encoder at 640×480, sends one frame, receives the
/// first `EncodedPacket`, and asserts its leading 4 bytes match the start code.
/// It exercises the per-packet sniff path implemented in `collect_output`.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_first_real_packet_is_annex_b() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("frame_tx should be open");

    enc.flush();

    let pkt = pkt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first encoded packet should arrive within 5 s");

    assert!(
        pkt.data.len() >= 4,
        "first packet too short: {} bytes",
        pkt.data.len()
    );
    assert_eq!(
        &pkt.data[..4],
        &[0x00, 0x00, 0x00, 0x01],
        "first real packet (post-probe-removal path) must start with Annex-B start code"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T2.1 (spec R2): no frame submitted during init ──────────────────────────

/// T2.1 — After probe removal, the encoder thread must NOT die during `start()`.
/// Verify by sending a frame immediately after `start()` and asserting `Ok(())`.
/// SendError = thread died (was the Bucket A failure mode on master).
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_new_does_not_submit_frames_to_mft_during_init() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Wait 200ms then send the first real frame — if no probe happened, the thread
    // is still alive and the channel is open.
    std::thread::sleep(Duration::from_millis(200));
    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("channel must be open — encoder thread must still be alive (no probe frame was submitted during init)");

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T3.3 / T3.4 (spec R3): dimensions thread through ────────────────────────

/// T3.3 — Encoder configured at 640×480 accepts a 640×480 frame.
/// Verifies that non-zero dimensions are used in setup_mft.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_setup_uses_config_dimensions_when_nonzero() {
    init_tracing();
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Send a 640x480 BGRA frame — if MFT was configured at 640x480 it will accept it.
    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("frame_tx should be open — thread alive after start()");

    enc.flush();

    let _pkt = pkt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("encoded packet must arrive within 5 s — MFT accepted 640x480 frame");

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

/// T3.4 — Encoder configured with sentinel zero (width=0, height=0) uses 1920×1080 fallback.
/// Verifies the sentinel mechanism in effective_dimensions.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_setup_falls_back_when_config_dimensions_zero() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 0,
        height: 0,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Send a 1920×1080 frame — fallback dimensions should match.
    frame_tx
        .send(make_synthetic_frame(1920, 1080, 0))
        .expect("frame_tx should be open — thread alive after start()");

    enc.flush();

    let _pkt = pkt_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("encoded packet must arrive within 5 s — sentinel fallback to 1920x1080");

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T5.1 (DD9 drain smoke): drain after channel close ───────────────────────

/// Drain path: after the input channel is closed, `pump_loop` calls
/// `MFT_MESSAGE_COMMAND_DRAIN` and continues looping until `MEEndOfStream`
/// arrives. This test exercises that path explicitly.
///
/// Without this test the post-disconnect drain path is unsmoked. The `< 2s`
/// assertion documents that the drain path is bounded — if discovery #585's
/// drain-spin pattern ever becomes pathological, this test will catch it.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_drain_after_channel_close_does_not_panic() {
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(8);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(8);
    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Push a few frames so the MFT is mid-encode when we close the channel.
    for i in 0..3u64 {
        frame_tx
            .send(make_synthetic_frame(640, 480, i * 33))
            .expect("frame_tx should be open");
    }
    // Close the input channel. pump_loop's NeedInput arm will hit Disconnected
    // and call ProcessMessage(COMMAND_DRAIN, 0).
    drop(frame_tx);

    // Drain any output packets — don't require a specific count.
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < drain_deadline {
        match pkt_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => continue,
            Err(_) => break, // timeout or disconnected — both fine
        }
    }

    // Most important assertion: stop() does not hang or panic.
    let t0 = Instant::now();
    enc.stop().expect("stop after drain must succeed");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "stop() after drain took {:?} — drain path may be looping",
        t0.elapsed()
    );
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

    // Liveness probe: send one frame and assert the channel is open (thread alive).
    // Before the Bucket A + B fixes this send returns SendError (thread died during probe).
    // After the fixes: channel open = thread alive. Satisfies spec R5/T5.2.
    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("encoder thread died during start() — channel closed");

    drop(frame_tx); // signal encoder thread to drain and exit

    // First stop — must join the thread and return Ok(()).
    enc.stop().expect("first stop() should succeed");

    // Second stop — must return Ok(()) without panic (idempotent).
    enc.stop()
        .expect("second stop() should be idempotent and return Ok(())");
}

// ─── T-NEW-1 (spec R8/R4/S4.1/S8.3): Stop during idle returns within deadline ─

/// T-NEW-1 — `stop()` returns within `STOP_DEADLINE_MS` when no frames have been sent.
///
/// **Why RED on master**: `pump_loop` blocks in `GetEvent(MF_EVENT_FLAG_NONE)`.
/// No MFT events ever arrive while idle, so the blocking call never returns.
/// `stop()` sets the flag but `join()` deadlocks — circular wait.
///
/// **Why GREEN after fix**: `GetEvent(MF_EVENT_FLAG_NO_WAIT)` returns
/// `MF_E_NO_EVENTS_AVAILABLE` immediately. The top-of-loop stop-flag check
/// sees `state.stop == true` within ≤ 1 ms and the thread exits cleanly.
///
/// Satisfies: R8/S8.1–S8.3, R4/S4.1.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_stop_during_idle_returns_within_deadline() {
    init_tracing();
    const STOP_DEADLINE_MS: u128 = 2000;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (_frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Allow the pump_loop thread to reach its first blocking GetEvent call.
    std::thread::sleep(Duration::from_millis(100));

    // No frames sent — encoder is idle.
    let t0 = Instant::now();
    enc.stop().expect("stop should succeed");
    let elapsed = t0.elapsed();

    assert!(
        elapsed.as_millis() < STOP_DEADLINE_MS,
        "stop() took {:?} — exceeded STOP_DEADLINE_MS={}ms (Bug 2 stop starvation)",
        elapsed,
        STOP_DEADLINE_MS,
    );
}

// ─── T-NEW-2 (spec R9/R4/S4.2/S9.3): Stop during active encode returns within deadline ─

/// T-NEW-2 — `stop()` returns within `STOP_DEADLINE_MS` when 5 frames have been
/// sent and `frame_tx` is still open at the time of the call.
///
/// Keeping `frame_tx` open is the "active encode" case: the encoder thread may be
/// mid-stream waiting for the next MFT event. On master the same `GetEvent` blocking
/// bug causes indefinite deadlock even when the MFT has processed some frames.
///
/// After the pump_loop fix the top-of-loop stop check exits in ≤ 1 ms regardless
/// of MFT event cadence. `frame_tx` is dropped AFTER `stop()` returns.
///
/// Satisfies: R9/S9.1–S9.3, R4/S4.2.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_stop_during_active_encode_returns_within_deadline() {
    init_tracing();
    const STOP_DEADLINE_MS: u128 = 2000;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Send 5 frames spaced 20 ms apart — pump_loop is actively processing.
    for i in 0..5u64 {
        frame_tx
            .send(make_synthetic_frame(640, 480, i * 33))
            .expect("frame_tx should be open during active encode");
        std::thread::sleep(Duration::from_millis(20));
    }

    // frame_tx is NOT dropped here — this is the "active encode" case (DD7 / S9.2).
    let t0 = Instant::now();
    enc.stop().expect("stop should succeed");
    let elapsed = t0.elapsed();

    // Drop frame_tx after stop() has returned (channel is already disconnected
    // on the encoder side; this is a clean-up only).
    drop(frame_tx);

    assert!(
        elapsed.as_millis() < STOP_DEADLINE_MS,
        "stop() took {:?} — exceeded STOP_DEADLINE_MS={}ms (Bug 2 stop starvation)",
        elapsed,
        STOP_DEADLINE_MS,
    );
}

// ─── T13.2: Drop without stop ─────────────────────────────────────────────────

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

    // Liveness probe: send one frame and assert the channel is open (thread alive).
    // Before the Bucket A + B fixes this send returns SendError (thread died during probe).
    // After the fixes: channel open = thread alive. Satisfies spec R5/T5.1.
    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("encoder thread died during start() — channel closed");

    // Drop frame_tx so the encoder thread can drain and exit.
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

// ─── Phase 0 trace probes (Slice 3 — single-frame-flush) ─────────────────────
//
// Empirical OQ-1 from sdd/hw-encoder-mft-single-frame-flush/explore (#701):
//   Does Intel QSV honor MFT_MESSAGE_COMMAND_DRAIN with 1 frame submitted?
//
// These tests are TRACE PROBES, not correctness assertions. They submit N frames,
// drop frame_tx (triggering pump_loop's existing Disconnected → COMMAND_DRAIN
// path at windows_mft.rs:1285-1294), wait for output, and print the outcome.
//
// Run on Host A (Intel QSV) with trace logging:
//
//   $env:RUST_LOG="sm_infra::encode=trace"
//   cargo nextest run -p sm-infra --features hw-encoder `
//     --test windows_mft_encode `
//     -E 'test(/phase_0/)' `
//     --run-ignored=all --test-threads=1 --no-fail-fast --nocapture `
//     *> phase-0-trace.log
//
// Capture phase-0-trace.log and save to engram topic
//   `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`
// to unblock sdd-design.
//
// Decision tree (per #707 D6):
//   1F GOOD (packet)              → Approach C clean for all 8 tests
//   1F EMPTY  + 2F GOOD (packet)  → Approach C; tests 1-5 must submit ≥2 frames
//   1F EMPTY  + 2F EMPTY          → re-explore (vendor requires ≥3 frames; alt mechanism)

#[test]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV) only"]
fn mft_one_frame_drain_probe_phase_0() {
    init_tracing();

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(4);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(8);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    eprintln!("[PHASE_0_PROBE_1F] sending 1 frame (640x480 BGRA)");
    frame_tx
        .send(make_synthetic_frame(640, 480, 0))
        .expect("frame_tx should be open");

    eprintln!("[PHASE_0_PROBE_1F] dropping frame_tx → pump_loop fires COMMAND_DRAIN");
    drop(frame_tx);

    let started = Instant::now();
    let outcome = pkt_rx.recv_timeout(Duration::from_secs(10));
    let elapsed = started.elapsed();

    match &outcome {
        Ok(pkt) => eprintln!(
            "[PHASE_0_PROBE_1F] OUTCOME=GOOD — packet received in {:?} (len={}, is_keyframe={})",
            elapsed,
            pkt.data.len(),
            pkt.is_keyframe
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => eprintln!(
            "[PHASE_0_PROBE_1F] OUTCOME=EMPTY_DRAIN — no packet within 10s (Intel QSV did NOT honor 1-frame DRAIN)"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => eprintln!(
            "[PHASE_0_PROBE_1F] OUTCOME=ENCODER_DIED — pkt_tx disconnected (encoder thread exited)"
        ),
    }

    let _ = enc.stop();
}

#[test]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV) only"]
fn mft_two_frame_drain_probe_phase_0() {
    init_tracing();

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(4);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(8);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    eprintln!("[PHASE_0_PROBE_2F] sending 2 frames (640x480 BGRA)");
    for i in 0..2u64 {
        frame_tx
            .send(make_synthetic_frame(640, 480, i * 33))
            .expect("frame_tx should be open");
    }

    eprintln!("[PHASE_0_PROBE_2F] dropping frame_tx → pump_loop fires COMMAND_DRAIN");
    drop(frame_tx);

    let started = Instant::now();
    let outcome = pkt_rx.recv_timeout(Duration::from_secs(10));
    let elapsed = started.elapsed();

    match &outcome {
        Ok(pkt) => eprintln!(
            "[PHASE_0_PROBE_2F] OUTCOME=GOOD — packet received in {:?} (len={}, is_keyframe={})",
            elapsed,
            pkt.data.len(),
            pkt.is_keyframe
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => eprintln!(
            "[PHASE_0_PROBE_2F] OUTCOME=EMPTY_DRAIN — no packet within 10s (vendor needs ≥3 frames)"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("[PHASE_0_PROBE_2F] OUTCOME=ENCODER_DIED — pkt_tx disconnected")
        }
    }

    let _ = enc.stop();
}

// ─── Phase 0 trace probes (Slice 4 — codec_api counter desync) ───────────────
//
// Empirical gate for sdd/hw-encoder-mft-codec-api-counter-desync:
//
//   OQ-1: Does ICodecAPI::SetValue BEFORE ProcessInput cause MF_E_NOTACCEPTING
//         on Intel QSV when a NeedInput credit is outstanding?  → P0-A
//   OQ-2: Does Approach B (reorder codec_api to AFTER ProcessInput) eliminate
//         the desync?  → P0-B
//   OQ-3: Does the forced IDR land on frame 4 (T7.1 cadence) under Approach B?
//         → P0-B (observe which frame carries is_keyframe=true)
//
// Run on Host A (Intel QSV) with trace logging:
//
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run -p sm-infra --features hw-encoder `
//     --test windows_mft_encode `
//     -E 'test(/^phase0_codec/)' `
//     --run-ignored=ignored-only --test-threads=1 --no-fail-fast --no-capture `
//     *> phase0-codec-api-trace.log
//
// Capture phase0-codec-api-trace.log and save to engram topic
//   `sdd/hw-encoder-mft-codec-api-counter-desync/phase-0-trace`
// to unblock sdd-design (design is BLOCKED on this evidence).
//
// Decision tree (per proposal D-PHASE0 / spec R1):
//   P0-A triggers NOTACCEPTING → root cause confirmed; proceed with Approach B
//   P0-A passes on NVENC       → expected (NVENC tolerates current ordering)
//   P0-B IDR on frame 3        → CleanPoint is authoritative; ICodecAPI ForceKeyFrame may be dropped
//   P0-B IDR on frame 4+       → ICodecAPI ForceKeyFrame must be retained post-ProcessInput
//
// Both probes are retained as permanent #[ignore]-gated regression guards after
// the fix lands (spec R8, proposal D-PROBES-RETENTION).

/// P0-A — reproduce the Intel QSV codec_api counter desync on master code.
///
/// Hypothesis: calling `ICodecAPI::SetValue` (via `apply_pending_codec_settings`)
/// BEFORE `ProcessInput`, while a NeedInput credit is outstanding, causes Intel QSV
/// to transiently enter non-accepting state. The subsequent `ProcessInput` returns
/// `MF_E_NOTACCEPTING` (0xC00D36B5), which fires the `debug_assert!(false)` at
/// `windows_mft.rs:1266` and exits the pump thread.
///
/// Cadence (batch-then-flush): push 5 priming frames into the channel (no recv) →
/// `set_bitrate(8_000_000)` → push 5 more frames → `enc.flush()` → drain output.
/// Intel QSV does NOT emit packets for short frame counts without a DRAIN trigger;
/// using `enc.flush()` (sets `drain_pending` atomically, pump sends `COMMAND_DRAIN`
/// on next NeedInput iteration) is required to obtain output — see Slice 3 #710.
///
/// If the pump dies mid-stream from MF_E_NOTACCEPTING, `pkt_rx` returns
/// `Disconnected` — this is the ENCODER_DIED outcome (bug reproduced).
/// If the pump tolerates the ordering (e.g. NVENC), we get packets back — SURVIVED.
///
/// On NVENC (Host B): expected to PASS or produce no NOTACCEPTING event — NVENC
/// tolerates the current ICodecAPI ordering. The probe prints the outcome either way.
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV); captures R1 evidence for sdd-design gate"]
fn phase0_codec_api_before_processinput_triggers_notaccepting() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = |deadline: Duration| pkt_rx.recv_timeout(deadline);

    // Push 5 priming frames into the channel without calling recv — pump_loop pulls
    // them asynchronously. Intel QSV does NOT emit packets for short streams without
    // a DRAIN trigger, so we must NOT call recv here (that's the C0 stall bug).
    tracing::info!("P0-A: batch-pushing 5 priming frames (no recv — pump pulls async)");
    for i in 0..5u64 {
        send_frame(i);
    }

    // Arm pending_bitrate — apply_pending_codec_settings() will fire
    // ICodecAPI::SetValue(MeanBitRate) at the TOP of the NEXT NeedInput servicing
    // iteration in pump_loop, BEFORE ProcessInput. This is the desync trigger on
    // Intel QSV when ni_count > 0.
    tracing::info!("P0-A: calling set_bitrate(8_000_000) — arms pending_bitrate for ICodecAPI");
    let _ = enc.set_bitrate(8_000_000);

    // Push 5 more frames so pump_loop has work to do AFTER the codec_api fires.
    tracing::info!("P0-A: batch-pushing frames 5–9 so codec_api races live NeedInput credits");
    for i in 5..10u64 {
        send_frame(i);
    }

    // Force emission via flush() (Slice 3 inherent method). Sets drain_pending
    // atomically; pump sends COMMAND_DRAIN on next NeedInput servicing iteration.
    // ~250 ms latency on Intel QSV (Phase 0 trace #710).
    tracing::info!("P0-A: calling enc.flush() — arms drain_pending for COMMAND_DRAIN");
    enc.flush();

    // Drain output. If pump died from MF_E_NOTACCEPTING, recv returns Disconnected.
    // Otherwise we collect packets until Timeout (no more output) or 20-packet cap.
    let mut received = 0u32;
    let mut died = false;
    loop {
        match recv_pkt(Duration::from_secs(5)) {
            Ok(pkt) => {
                tracing::info!(
                    "P0-A: pkt {} — is_keyframe={} len={}",
                    received,
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                received += 1;
                if received >= 20 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "P0-A: OUTCOME=ENCODER_DIED — pkt_tx disconnected; pump thread exited after MF_E_NOTACCEPTING (counter desync confirmed on this vendor)"
                );
                died = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if received == 0 {
                    tracing::warn!(
                        "P0-A: OUTCOME=EMPTY_DRAIN — no packets before timeout (harness misconfig; flush() should have triggered output)"
                    );
                } else {
                    tracing::warn!("P0-A: OUTCOME=NO_MORE_PACKETS — drained");
                }
                break;
            }
        }
    }

    tracing::info!(
        "P0-A: SUMMARY — received={} died={} (SURVIVED={})",
        received,
        died,
        !died
    );
    if !died {
        tracing::info!(
            "P0-A: OUTCOME=SURVIVED — received {} packets; ICodecAPI desync did NOT kill the pump (vendor likely tolerates current ordering, e.g. NVENC)",
            received
        );
    }

    // Observation-only probe: no assertion — outcome is vendor-dependent.
    // The trace log is the deliverable for the phase-0-trace engram topic.
    let _ = enc.stop();
}

/// P0-B — confirm that the planned Approach B reorder eliminates the desync
/// AND that the forced IDR lands on the expected frame index (OQ-3).
///
/// This probe runs against MASTER production code (C0 commit). On Intel QSV it
/// will likely show the same NOTACCEPTING failure as P0-A — because the production
/// fix has not landed yet. That is expected and intentional: the trace captures the
/// CURRENT behaviour of a `request_keyframe()` scenario, which becomes the baseline
/// for verifying the C2 (GREEN) fix when P0-B is re-run after the production reorder.
///
/// Cadence (batch-then-flush): push 3 priming frames (no recv) →
/// `request_keyframe()` → push 3 more frames (including the IDR target at index 3)
/// → `enc.flush()` → drain output, recording which received-packet-index has
/// `is_keyframe == true`. Intel QSV does NOT emit packets for short frame counts
/// without a DRAIN trigger; see Slice 3 #710 and the flush() contract.
///
/// Per spec R3 / OQ-3 with this batch cadence, the IDR target is the 4th submitted
/// frame (frame index 3, 0-indexed). If all 6 submitted frames produce output, we
/// expect `is_keyframe == true` at received-packet-index 3.
///
/// Specifically, this probe traces WHICH received-packet-index carries
/// `is_keyframe == true` after `request_keyframe()`, resolving OQ-3 (spec §9).
/// If frame index 3 carries the IDR (CleanPoint authoritative), the
/// `ICodecAPI::SetValue(ForceKeyFrame)` path may be dropped post-fix.
/// If the IDR slides to index 4+, we retain both paths.
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV); captures R2/R3 evidence for sdd-design gate"]
fn phase0_codec_api_after_processinput_no_notaccepting_and_idr_on_frame_4() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = |deadline: Duration| pkt_rx.recv_timeout(deadline);

    // Push 3 priming frames into the channel without calling recv. Intel QSV does
    // NOT emit packets without a DRAIN trigger, so we must NOT call recv here
    // (that's the C0 stall bug: one-frame-at-a-time recv stalls indefinitely).
    tracing::info!("P0-B: batch-pushing 3 priming frames (no recv — pump pulls async)");
    for i in 0..3u64 {
        send_frame(i);
    }

    // Arm keyframe_pending — submit_frame() will set MFSampleExtension_CleanPoint=1
    // on the next sample (frame index 3, the 4th submitted). Under Approach B,
    // ICodecAPI::SetValue(ForceKeyFrame) fires AFTER ProcessInput; under current
    // master code it fires BEFORE (desync trigger on Intel QSV).
    tracing::info!(
        "P0-B: calling request_keyframe() — arms keyframe_pending; frame index 3 is IDR target (OQ-3)"
    );
    enc.request_keyframe();

    // Push 3 more frames so pump_loop has work after the codec_api fires.
    // Frame index 3 is the IDR target under T7.1/batch cadence semantics.
    tracing::info!("P0-B: batch-pushing frames 3–5 (frame 3 is IDR target)");
    for i in 3..6u64 {
        send_frame(i);
    }

    // Force emission via flush() (Slice 3 inherent method). Sets drain_pending
    // atomically; pump sends COMMAND_DRAIN on next NeedInput servicing iteration.
    // ~250 ms latency on Intel QSV (Phase 0 trace #710).
    tracing::info!("P0-B: calling enc.flush() — arms drain_pending for COMMAND_DRAIN");
    enc.flush();

    // Drain output. Track which received-packet-index carries is_keyframe==true.
    // If pump died from MF_E_NOTACCEPTING, recv returns Disconnected (ENCODER_DIED).
    let mut received = 0u32;
    let mut died = false;
    let mut keyframe_indices: Vec<u32> = Vec::new();
    loop {
        match recv_pkt(Duration::from_secs(5)) {
            Ok(pkt) => {
                tracing::info!(
                    "P0-B: pkt {} — is_keyframe={} len={}",
                    received,
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                if pkt.is_keyframe {
                    keyframe_indices.push(received);
                }
                received += 1;
                if received >= 20 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "P0-B: OUTCOME=ENCODER_DIED — pump thread exited; ICodecAPI ForceKeyFrame BEFORE ProcessInput killed the encoder (master RED state confirmed)"
                );
                died = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if received == 0 {
                    tracing::warn!(
                        "P0-B: OUTCOME=EMPTY_DRAIN — no packets before timeout (harness misconfig; flush() should have triggered output)"
                    );
                } else {
                    tracing::warn!("P0-B: OUTCOME=NO_MORE_PACKETS — drained");
                }
                break;
            }
        }
    }

    tracing::info!(
        "P0-B: SUMMARY — received={} died={} keyframe_indices={:?} (OQ-3: expected IDR at index 3 under batch cadence)",
        received,
        died,
        keyframe_indices
    );

    // Observation-only probe: no assertion — the trace is the deliverable.
    // After C2 (GREEN) lands, re-run this probe to confirm:
    //   - no ENCODER_DIED outcome
    //   - keyframe_indices contains 3 (IDR on the 4th submitted frame)
    //   - no other keyframe indices (no spurious IDRs)
    let _ = enc.stop();
}

// ─── Phase 0 trace probes (Slice 5 — Intel QSV mid-stream IDR via drain+resume) ─
//
// Empirical gate for sdd/hw-encoder-mft-intel-qsv-mid-stream-idr:
//
//   OQ-1: Does the first frame after DrainComplete+BEGIN_STREAMING+START_OF_STREAM
//         on Intel QSV always emit as IDR (NAL type 5)?  → C1 (PRIMARY DESIGN GATE)
//   OQ-2: What is the actual drain+resume round-trip latency for a keyframe request?
//         → C2 (informational; R2 sets ≤ 2s SHOULD)
//
// Run on Host A (Intel QSV) with trace logging:
//
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run -p sm-infra --features hw-encoder `
//     --test windows_mft_encode `
//     -E 'test(/^phase0_intel_qsv_idr/)' `
//     --run-ignored=ignored-only --test-threads=1 --no-fail-fast --no-capture `
//     *> phase0-intel-qsv-idr-trace.log
//
// Capture phase0-intel-qsv-idr-trace.log and save to engram topic
//   `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/phase-0-trace`
// to unblock sdd-design (design is BLOCKED on C1 evidence — spec §7).
//
// Decision tree (per proposal D-PHASE0 / spec R7):
//   C1 PASS (is_keyframe=true post-drain) → Mechanism C validated; proceed with sdd-design
//   C1 FAIL (is_keyframe=false post-drain) → BLOCK design; escalate to Mechanism F / VPL
//   C2 latency ≤ 1s                       → R2 (≤ 2s SHOULD) comfortably satisfied
//   C2 latency 1–2s                        → flag for design review; informational
//   C2 latency > 2s                        → hard-assert fires; reopen R2 threshold
//
// Both probes are retained as permanent #[ignore]-gated regression guards after
// the fix lands (spec R7, proposal D-PROBES-RETENTION).

/// C1 — confirm that Mechanism C (drain+resume) produces an IDR as first post-resume frame.
///
/// Hypothesis: after flushing (triggering `MFT_MESSAGE_COMMAND_DRAIN`) and the Slice 4
/// DD17/F2 handler sends `BEGIN_STREAMING + START_OF_STREAM`, the first frame of the new
/// stream session is an IDR by H.264 spec. This is vendor-agnostic — the stream restart
/// guarantee comes from the codec, not from an `ICodecAPI` signal Intel QSV may ignore.
///
/// This probe validates Mechanism C WITHOUT calling `request_keyframe()` (the production
/// trigger is not yet implemented). We manually execute the drain+resume cycle by calling
/// `enc.flush()` twice — once to drain the priming batch and once to drain the IDR-target
/// frame — and observe whether the packet produced in the second batch carries
/// `is_keyframe == true`.
///
/// Cadence (two-batch flush):
///   1. Push 3 priming frames (no recv) → `enc.flush()` → drain priming output.
///   2. Submit 1 IDR-target frame → `enc.flush()` → recv IDR-target packet.
///
/// The second flush triggers a fresh DRAIN after F2 resumes the encoder. The first frame
/// of the second session MUST be IDR — that is the C1 assertion.
///
/// Expected outcome (Mechanism C valid):
///   - First batch: at least one packet received (≥ 1 priming frame emitted).
///   - Second batch first packet: `is_keyframe == true`.
///   - If FAIL: Mechanism C cannot guarantee IDR via drain+resume on this vendor → BLOCK.
///
/// On NVENC (Host B): expected to also PASS (R6 — vendor-uniform mechanism).
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV); confirms Mechanism C drain+resume IDR mechanism for sdd-design gate"]
fn phase0_intel_qsv_idr_via_drain_resume_first_frame_is_idr() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = |deadline: Duration| pkt_rx.recv_timeout(deadline);

    // ── Batch 1: push 3 priming frames, flush, drain output ──────────────────
    //
    // Intel QSV does NOT emit packets without a DRAIN trigger. We batch-push first
    // and drain with flush() — the same pattern as P0-A/P0-B and T8.2.
    tracing::info!("[C1] batch-pushing 3 priming frames (no recv — pump pulls async)");
    for i in 0..3u64 {
        send_frame(i);
    }

    // First flush: triggers MFT_MESSAGE_COMMAND_DRAIN on priming frames. After
    // METransformDrainComplete, the Slice 4 F2 handler sends BEGIN_STREAMING +
    // START_OF_STREAM and the encoder auto-resumes — this is the load-bearing
    // IDR mechanism we are validating.
    tracing::info!("[C1] flush() #1 — drain priming batch (triggers COMMAND_DRAIN)");
    enc.flush();

    // Drain priming output. We expect ≥ 1 packet (at least the initial IDR).
    // If Disconnected: encoder died — probe cannot continue, record and abort.
    let mut priming_received = 0u32;
    let mut priming_keyframe_indices: Vec<u32> = Vec::new();
    loop {
        match recv_pkt(Duration::from_secs(5)) {
            Ok(pkt) => {
                tracing::info!(
                    "[C1] priming pkt {} — is_keyframe={} len={}",
                    priming_received,
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                if pkt.is_keyframe {
                    priming_keyframe_indices.push(priming_received);
                }
                priming_received += 1;
                // Cap at 10 — we only submitted 3 frames.
                if priming_received >= 10 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[C1] OUTCOME=ENCODER_DIED during priming drain — pump thread exited unexpectedly"
                );
                // Cannot continue — hard-assert so the probe fails visibly.
                panic!("[C1] encoder died during priming drain; cannot validate Mechanism C");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if priming_received == 0 {
                    tracing::warn!(
                        "[C1] OUTCOME=EMPTY_DRAIN — no priming packets before timeout; flush() may not have triggered DRAIN"
                    );
                } else {
                    tracing::info!(
                        "[C1] priming drain complete — {} packets received",
                        priming_received
                    );
                }
                break;
            }
        }
    }

    tracing::info!(
        "[C1] priming batch summary — received={} keyframe_indices={:?} (expect [0] — initial IDR only)",
        priming_received,
        priming_keyframe_indices
    );

    // Priming must have produced at least 1 packet. If not, the encoder/harness is broken.
    assert!(
        priming_received >= 1,
        "[C1] expected ≥ 1 priming packet; got 0 — harness misconfigured or flush() broken"
    );

    // ── Batch 2: submit 1 IDR-target frame, flush, recv and assert IDR ────────
    //
    // After F2 resumes the encoder with BEGIN_STREAMING+START_OF_STREAM, frame index 3
    // (the first frame of the new stream session) MUST be IDR by H.264 spec. We do NOT
    // call request_keyframe() here — the drain+resume itself is the IDR trigger.
    tracing::info!(
        "[C1] submitting IDR-target frame (frame index 3 — first of new session after resume)"
    );
    send_frame(3);

    // Second flush: drain the IDR-target frame out of the encoder. ~250 ms latency.
    tracing::info!("[C1] flush() #2 — drain IDR-target frame");
    enc.flush();

    // Receive the IDR-target packet. 5s timeout provides ample margin for the drain
    // roundtrip (~250 ms empirical from Slice 3 #710 and Slice 4 #747).
    let idr_result = recv_pkt(Duration::from_secs(5));

    match &idr_result {
        Ok(pkt) => {
            tracing::info!(
                "[C1] IDR-target pkt — is_keyframe={} len={}",
                pkt.is_keyframe,
                pkt.data.len()
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::error!("[C1] OUTCOME=ENCODER_DIED — pump thread exited after second flush");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::error!(
                "[C1] OUTCOME=TIMEOUT — no IDR-target packet within 5s (DRAIN latency exceeded or harness broken)"
            );
        }
    }

    // PRIMARY GATE: the IDR-target packet MUST be is_keyframe == true.
    // FAIL here → Mechanism C cannot guarantee IDR via drain+resume → block sdd-design.
    let idr_pkt = idr_result.expect("[C1] expected IDR-target packet; got Disconnected or Timeout");
    assert!(
        idr_pkt.is_keyframe,
        "[C1] MECHANISM C INVALID — first post-drain+resume packet is NOT an IDR (is_keyframe=false); escalate to Mechanism F"
    );

    tracing::info!(
        "[C1] OUTCOME=PASS — Mechanism C validated: drain+resume produces IDR as first post-resume packet"
    );

    let _ = enc.stop();
}

/// C2 — measure the drain+resume latency for a keyframe request (informational).
///
/// Hypothesis: the round-trip from `enc.flush()` (COMMAND_DRAIN) to IDR-target packet
/// arrival is ~250 ms on Intel QSV (Slice 3 Phase 0 trace #710, Slice 4 trace #747).
/// R2 sets a ≤ 2s SHOULD bound. This probe captures the empirical latency to inform
/// sdd-design of actual T7.1/T7.2 recv_timeout budgets.
///
/// Same cadence as C1 (batch 1 priming → flush → drain → batch 2 IDR-target → flush
/// → recv) with `Instant::now()` timing placed around the second flush→recv step.
///
/// The assert threshold is 2000 ms (R2). If latency exceeds 2000 ms, sdd-design must
/// revisit the recv_timeout values in T7.1/T7.2 and reconsider the feasibility of
/// Mechanism C for real-time WebRTC use cases.
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV); measures drain+resume IDR latency for R2 (≤ 2s SHOULD) and sdd-design recv_timeout budgets"]
fn phase0_intel_qsv_idr_via_drain_resume_latency_measure() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(16);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(16);
    enc.start(frame_rx, pkt_tx).expect("start must succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    let recv_pkt = |deadline: Duration| pkt_rx.recv_timeout(deadline);

    // ── Batch 1: priming drain (same as C1) ──────────────────────────────────
    tracing::info!("[C2] batch-pushing 3 priming frames (no recv — pump pulls async)");
    for i in 0..3u64 {
        send_frame(i);
    }

    tracing::info!("[C2] flush() #1 — drain priming batch");
    enc.flush();

    let mut priming_received = 0u32;
    loop {
        match recv_pkt(Duration::from_secs(5)) {
            Ok(_pkt) => {
                priming_received += 1;
                if priming_received >= 10 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("[C2] encoder died during priming drain");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break;
            }
        }
    }

    assert!(
        priming_received >= 1,
        "[C2] expected ≥ 1 priming packet; got 0 — harness misconfigured"
    );

    tracing::info!(
        "[C2] priming drain complete — {} packets received",
        priming_received
    );

    // ── Batch 2: IDR-target with latency measurement ──────────────────────────
    //
    // The clock starts immediately before flush() #2 — this is when the DRAIN
    // request is armed. The clock stops when the IDR-target packet is received.
    // This captures the full drain roundtrip: flush() → COMMAND_DRAIN → pump processes
    // → METransformDrainComplete → F2 BEGIN_STREAMING+START_OF_STREAM → resume →
    // ProcessInput(IDR-target) → METransformHaveOutput → collect_output → pkt_rx.
    tracing::info!("[C2] submitting IDR-target frame (frame index 3 — first of new session)");
    send_frame(3);

    tracing::info!("[C2] flush() #2 — drain IDR-target; starting latency clock");
    let drain_start = Instant::now();
    enc.flush();

    let idr_result = recv_pkt(Duration::from_secs(5));
    let elapsed = drain_start.elapsed();

    match &idr_result {
        Ok(pkt) => {
            tracing::info!(
                "[C2] IDR-target pkt received — is_keyframe={} len={} elapsed={:?}",
                pkt.is_keyframe,
                pkt.data.len(),
                elapsed
            );
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::error!(
                "[C2] OUTCOME=ENCODER_DIED after flush() #2 — elapsed={:?}",
                elapsed
            );
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::error!(
                "[C2] OUTCOME=TIMEOUT — no IDR-target packet within 5s — elapsed={:?}",
                elapsed
            );
        }
    }

    tracing::info!(
        "[C2] LATENCY MEASUREMENT — drain+resume→IDR elapsed={:?} (R2 threshold: ≤ 2000ms SHOULD)",
        elapsed
    );

    // R2 hard assert: latency MUST be < 2000ms. If this fires, sdd-design must
    // revisit recv_timeout budgets in T7.1/T7.2 and re-examine Mechanism C viability.
    assert!(
        elapsed.as_millis() < 2000,
        "[C2] R2 VIOLATED — drain+resume latency {}ms exceeds 2000ms threshold; sdd-design must revisit recv_timeout",
        elapsed.as_millis()
    );

    // Also assert the IDR packet arrived and was correctly flagged (belt-and-suspenders
    // with C1 — C2 is informational but still validates the mechanism).
    let idr_pkt = idr_result.expect("[C2] IDR-target packet not received");
    assert!(
        idr_pkt.is_keyframe,
        "[C2] first post-drain+resume packet is NOT IDR (is_keyframe=false)"
    );

    tracing::info!(
        "[C2] OUTCOME=PASS — latency={}ms is_keyframe=true (R2 satisfied)",
        elapsed.as_millis()
    );

    let _ = enc.stop();
}
