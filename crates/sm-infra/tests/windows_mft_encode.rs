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
/// Cadence: batch-push 3 priming frames → `enc.flush()` → drain (assert initial IDR
/// received and ≥1 P-frame) → `request_keyframe()` → push 1 IDR-target frame →
/// `enc.flush()` → drain → assert at least one received packet after the request has
/// `is_keyframe == true` and begins with NAL type 7 (SPS).
///
/// On Intel QSV (Host A) at C1 RED this test FAILS — `apply_pending_codec_settings`
/// fires ICodecAPI ForceKeyFrame BEFORE ProcessInput, Intel QSV returns
/// MF_E_NOTACCEPTING, `debug_assert!(false)` at `windows_mft.rs:1266` kills the pump
/// thread, and the drain loop returns `Disconnected`.  C2 GREEN (SWAP-FIRE split) will
/// make it PASS.  Maps R3, R5, R9, spec S4.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_request_keyframe_marks_next_packet_as_keyframe() {
    init_tracing();
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const PRIMING_COUNT: u64 = 3;

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

    // Batch-push priming frames and flush — Intel QSV requires a DRAIN trigger
    // before emitting any output (Slice 3 discovery #710).
    for i in 0..PRIMING_COUNT {
        send_frame(i);
    }
    enc.flush();

    // Drain priming output.  Expect at least an IDR + 1 P-frame; cap at 20 packets.
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
    assert!(
        priming_pkts[0].is_keyframe,
        "first priming packet must be an IDR (initial keyframe)"
    );
    println!(
        "[T7.1] priming drain: {} packets, first is_keyframe={}",
        priming_pkts.len(),
        priming_pkts[0].is_keyframe
    );

    // Request a forced keyframe, then push one IDR-target frame and flush.
    enc.request_keyframe();
    send_frame(PRIMING_COUNT); // frame index = PRIMING_COUNT (the IDR target)
    enc.flush();

    // Drain post-request output.  At least one packet must have is_keyframe == true.
    let mut post_pkts: Vec<_> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(pkt) => {
                println!(
                    "[T7.1] post-request pkt {}: is_keyframe={} len={}",
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
                    "encoder died after request_keyframe() — codec_api desync (C1 RED expected on Intel QSV)"
                )
            }
        }
    }

    // Spec R3, R5: at least one packet after request_keyframe() must be a keyframe,
    // and it must begin with NAL type 7 (SPS) — Annex-B IDR marker.
    let forced_idr = post_pkts
        .iter()
        .find(|p| p.is_keyframe)
        .expect("at least one packet after request_keyframe() must have is_keyframe == true");
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
/// Cadence: batch-push 3 priming frames → `enc.flush()` → drain → `request_keyframe()`
/// → push IDR-target frame → push one more P-frame → `enc.flush()` → drain → assert
/// the forced IDR has `is_keyframe == true` and the packet after it has
/// `is_keyframe == false` (flag cleared exactly once per R5).
///
/// On Intel QSV (Host A) at C1 RED this test FAILS — the pump thread dies from the
/// codec_api desync panic at `windows_mft.rs:1266`.  C2 GREEN will make it PASS.
/// Maps R5, R9, spec S5.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_keyframe_flag_cleared_after_idr_emitted() {
    init_tracing();
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const PRIMING_COUNT: u64 = 3;

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
    println!("[T7.2] priming drain: {} packets", priming_pkts.len());

    // Request a forced keyframe, push the IDR-target frame plus one more P-frame,
    // then flush to drain both.
    enc.request_keyframe();
    send_frame(PRIMING_COUNT); // IDR target
    send_frame(PRIMING_COUNT + 1); // P-frame after IDR
    enc.flush();

    // Drain post-request output.
    let mut post_pkts: Vec<_> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(pkt) => {
                println!(
                    "[T7.2] post-request pkt {}: is_keyframe={} len={}",
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
                    "encoder died after request_keyframe() — codec_api desync (C1 RED expected on Intel QSV)"
                )
            }
        }
    }

    // Spec R5: forced IDR must be present and the packet immediately after it must
    // have is_keyframe == false (flag cleared exactly once per request).
    let idr_pos = post_pkts
        .iter()
        .position(|p| p.is_keyframe)
        .expect("at least one packet after request_keyframe() must have is_keyframe == true");
    assert!(
        post_pkts[idr_pos].is_keyframe,
        "forced IDR packet must have is_keyframe == true"
    );
    // Verify the flag was cleared: the packet after the IDR must be a P-frame.
    assert!(
        idr_pos + 1 < post_pkts.len(),
        "expected a P-frame packet after the forced IDR, but drain produced only {} post-request packet(s)",
        post_pkts.len()
    );
    assert!(
        !post_pkts[idr_pos + 1].is_keyframe,
        "packet after forced IDR must have is_keyframe == false (flag cleared after first IDR)"
    );

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

// ─── T8.2: set_bitrate runtime update ────────────────────────────────────────

/// T8.2 — `set_bitrate()` updates the encoder at runtime without restarting the thread.
///
/// Cadence: batch-push 3 priming frames → `enc.flush()` → drain → `set_bitrate(8_000_000)`
/// returns `Ok(())` → push 3 more frames → `enc.flush()` → drain → all packets arrive
/// without `Disconnected` (encoder thread alive throughout).
///
/// The PASS criterion is encoder thread survival: `frame_tx.send()` after `set_bitrate()`
/// must succeed and packets must continue to arrive.  Maps R4, R6, R9, spec S6.
///
/// On Intel QSV (Host A) at C1 RED this test FAILS — the pump thread dies from the
/// codec_api desync panic at `windows_mft.rs:1266`.  C2 GREEN will make it PASS.
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
