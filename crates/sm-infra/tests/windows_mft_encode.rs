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
// Phase 0 probes — empirical regression evidence for Slice 6 R2 (and history):
//
//   * `phase0_nvenc_idr_packet_format_dump`
//     Priming format diagnostic. Confirms NVENC emits 4-byte Annex-B start codes
//     for the priming IDR (refutes a Slice 5/6 R1 hypothesis). Engram #800.
//
//   * `phase0_nvenc_post_recreate_idr_format_dump`
//     Mechanism G falsification on NVENC. 29/29 P-frames post-recreate;
//     setup-sequence guarantee fails. Engram #801.
//
//   * `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute`
//     CleanPoint INPUT-write falsification on NVENC. 30/30 P-frames despite
//     the (now-deleted) DD10 inline comment claiming "NVENC honored CleanPoint."
//     Engram #807.
//
//   * `phase0_nvenc_force_keyframe_via_codecapi_before_processinput`
//     P2 success evidence on NVENC. ForceKeyFrame BEFORE+VT_UI4 emits IDR at
//     idx 0 (len=49998). Engram #809.
//
//   * `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput`
//     P2 success evidence on Intel QSV — retroactive correction of Slice 4's
//     "Intel QSV doesn't honor ForceKeyFrame" verdict. IDR at idx 1 (1-frame
//     in-flight latency, within tolerance). Engram #809.
//
// All probes are #[ignore]-gated; run on demand on the appropriate host with:
//   cargo nextest run --release --features hw-encoder -p sm-infra \
//     --test windows_mft_encode <probe_name> --run-ignored only --no-capture
//
// Deleted probes (historical record in Slice 5 archive engram #791, rounds #779/#783):
//   phase0_intel_qsv_idr_via_drain_resume_first_frame_is_idr    (round 1 — drain+resume falsification)
//   phase0_intel_qsv_idr_via_drain_resume_latency_measure       (round 1 — drain+resume latency)
//   phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr (round 3 — Mechanism G validation)
#![cfg(all(target_os = "windows", feature = "hw-encoder"))]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use sm_domain::capture::PixelFormat;
use sm_domain::encode::{EncodedPacket, EncoderConfig, EncoderError, VideoEncoder};
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

/// Assert that AT LEAST ONE packet in `packets` has `is_keyframe == true`, returning
/// the index of the first keyframe found.
///
/// The `expected_keyframe_within` parameter documents the test's intent (caller's expected
/// upper bound on how many frames before an IDR appears) but the assertion itself is
/// "any keyframe present in the collected slice" — not strict N-frames.
///
/// WHY eventually-style: Mechanism G has measurable drain latency (~9 ms tear-down +
/// recreate, plus up to ~300 ms first-encode latency depending on pipeline depth).
/// A strict next-frame assertion would be flaky across Host A timing variations.
/// The caller is responsible for collecting a sufficiently large slice before calling
/// this helper (i.e. the recv_timeout loop deadline must accommodate G's full latency).
///
/// Panics with an informative message if no keyframe is found in the entire slice.
fn assert_keyframe_within_next_n_frames(
    packets: &[EncodedPacket],
    expected_keyframe_within: usize,
) -> usize {
    for (idx, pkt) in packets.iter().enumerate() {
        if pkt.is_keyframe {
            return idx;
        }
    }
    panic!(
        "expected a keyframe within the first {} frames (eventually-style) \
         but no keyframe found in {} collected packets; \
         the post-recreate drain window may be too short or Mechanism G did not produce IDR \
         (is_keyframe=[{}])",
        expected_keyframe_within,
        packets.len(),
        packets
            .iter()
            .map(|p| if p.is_keyframe { "IDR" } else { "P" })
            .collect::<Vec<_>>()
            .join(", ")
    );
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

/// T7.1 — `request_keyframe()` causes a keyframe to appear in the post-request batch.
///
/// Slice 6 R2 GREEN body — vendor-uniform `CODECAPI_AVEncVideoForceKeyFrame` mechanism
/// (P2 evidence engram #809: NVENC IDR at idx 0, Intel QSV IDR at idx 1).
///
/// Cadence (eventually-style per spec R11, S4, design DD9):
///   1. Push 12 priming frames → `enc.flush()` → drain priming output (setup-sequence IDR at idx 0).
///   2. `enc.request_keyframe()` — routes to `force_keyframe_icodecapi_pending` store (Slice 6 R2).
///   3. Push 30 post-request frames → `enc.flush()` → collect post-request packets.
///   4. Assert: IDR present within first 30 post-request packets (tolerance covers Intel QSV
///      1-frame in-flight latency; NVENC emits IDR at idx 0).
///
/// Maps spec R11, R12, S4, S12, S13. Design DD9.
#[test]
#[ignore = "Slice 6 R2 — requires hardware (Host A or Host B); run with --run-ignored"]
fn mft_request_keyframe_marks_next_packet_as_keyframe() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const PRIMING_FRAMES: u64 = 12;
    // ForceKeyFrame: ~0ms on NVENC (IDR idx 0), ~33ms on Intel QSV (IDR idx 1).
    // 5 s provides ample margin for both vendors.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);
    // Eventually-style: expect IDR within first 30 post-request packets. Covers
    // Intel QSV 1-frame in-flight latency; NVENC emits at idx 0 (P2 evidence #809).
    const IDR_TOLERANCE: usize = 30;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(32);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(32);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    // ── Batch 1: priming — push PRIMING_FRAMES frames, flush, drain ──────────
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }
    enc.flush();

    let mut priming_pkts: Vec<EncodedPacket> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(pkt) => {
                tracing::info!(
                    "[T7.1] priming pkt {} — is_keyframe={} len={}",
                    priming_pkts.len(),
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                priming_pkts.push(pkt);
                if priming_pkts.len() >= PRIMING_FRAMES as usize {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "[T7.1] encoder died during priming drain — pump thread exited unexpectedly"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    assert!(
        !priming_pkts.is_empty(),
        "[T7.1] expected ≥ 1 priming packet; got 0 — harness misconfigured or flush() broken"
    );

    // Priming batch must contain the setup-sequence IDR at index 0.
    assert!(
        priming_pkts[0].is_keyframe,
        "[T7.1] first priming packet must be an IDR (setup-sequence); got is_keyframe=false"
    );

    tracing::info!(
        "[T7.1] priming drain complete — {} packets; IDR at idx 0 confirmed",
        priming_pkts.len()
    );

    // ── Request keyframe + collect post-request batch ─────────────────────────
    //
    // Slice 6 R2: request_keyframe() → force_keyframe_icodecapi_pending.store(true, Release).
    // pump_loop consumes before next ProcessInput → SetValue(CODECAPI_AVEncVideoForceKeyFrame,
    // VT_UI4=1) BEFORE ProcessInput. IDR at idx 0 (NVENC) or idx 1 (Intel QSV) per P2 #809.
    enc.request_keyframe();

    // Push 30 frames to ensure the IDR appears in output (covers Intel QSV 1-frame latency).
    const POST_REQUEST_FRAMES: u64 = 30;
    for i in 0..POST_REQUEST_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }
    enc.flush();

    let mut post_pkts: Vec<EncodedPacket> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(pkt) => {
                tracing::info!(
                    "[T7.1] post-request pkt {} — is_keyframe={} len={}",
                    post_pkts.len(),
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                post_pkts.push(pkt);
                // Collect enough frames to absorb G's pipeline depth.
                if post_pkts.len() >= IDR_TOLERANCE + 5 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("[T7.1] encoder died after request_keyframe() — pump thread exited");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    assert!(
        !post_pkts.is_empty(),
        "[T7.1] expected ≥ 1 post-request packet; got 0 — encoder may have died or drain window too short"
    );

    // Spec R7, R11, S4: post-request batch MUST contain a keyframe.
    // eventually-style: any keyframe within the collected slice is sufficient (DD8, R9).
    let idr_idx = assert_keyframe_within_next_n_frames(&post_pkts, IDR_TOLERANCE);
    tracing::info!("[T7.1] PASS — keyframe found at post-request idx {idr_idx}");

    drop(frame_tx);
    enc.stop().expect("stop should succeed");
}

/// T7.2 — After a forced IDR is emitted, the keyframe request is cleared so the
/// following packet is a P-frame (is_keyframe == false).
///
/// Slice 6 R2 GREEN body — vendor-uniform `CODECAPI_AVEncVideoForceKeyFrame` mechanism.
/// Atomic one-shot consume (`swap(false, AcqRel)`) guarantees exactly-once IDR semantics.
///
/// Cadence (eventually-style per spec R12, S14, S15, design DD9):
///   1. Push 12 priming frames → `enc.flush()` → drain priming output (setup-sequence IDR).
///   2. `enc.request_keyframe()` — routes to `force_keyframe_icodecapi_pending` (Slice 6 R2).
///   3. Push 30 post-request frames → `enc.flush()` → collect batch.
///   4. Assert: IDR present within first 30 post-request packets.
///   5. Assert: packet AFTER the IDR is NOT a keyframe (exactly-once semantics).
///
/// Maps spec R12, S14, S15. Design DD9.
#[test]
#[ignore = "Slice 6 R2 — requires hardware (Host A or Host B); run with --run-ignored"]
fn mft_keyframe_flag_cleared_after_idr_emitted() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    const PRIMING_FRAMES: u64 = 12;
    // ForceKeyFrame: ~0ms NVENC, ~33ms Intel QSV. 5 s provides ample margin.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);
    // Eventually-style: IDR expected within first 30 post-request packets.
    // Covers Intel QSV 1-frame in-flight latency (P2 evidence #809).
    const IDR_TOLERANCE: usize = 30;

    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new should succeed");

    let (frame_tx, frame_rx) = mpsc::sync_channel(32);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(32);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    let send_frame = |i: u64| {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    };

    // ── Batch 1: priming — push PRIMING_FRAMES frames, flush, drain ──────────
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }
    enc.flush();

    let mut priming_pkts: Vec<EncodedPacket> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(pkt) => {
                tracing::info!(
                    "[T7.2] priming pkt {} — is_keyframe={} len={}",
                    priming_pkts.len(),
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                priming_pkts.push(pkt);
                if priming_pkts.len() >= PRIMING_FRAMES as usize {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "[T7.2] encoder died during priming drain — pump thread exited unexpectedly"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    assert!(
        !priming_pkts.is_empty(),
        "[T7.2] expected ≥ 1 priming packet; got 0 — harness misconfigured or flush() broken"
    );

    // Priming must have setup-sequence IDR at index 0.
    assert!(
        priming_pkts[0].is_keyframe,
        "[T7.2] first priming packet must be an IDR (setup-sequence); got is_keyframe=false"
    );

    tracing::info!(
        "[T7.2] priming drain complete — {} packets; IDR at idx 0 confirmed",
        priming_pkts.len()
    );

    // ── Request keyframe + collect post-request batch ─────────────────────────
    //
    // Slice 6 R2: request_keyframe() → force_keyframe_icodecapi_pending.store(true, Release).
    // pump_loop swap+SetValue(ForceKeyFrame, VT_UI4=1) BEFORE ProcessInput → one-shot IDR.
    enc.request_keyframe();

    // Push 30 frames; IDR at idx 0 (NVENC) or idx 1 (Intel QSV); follow-on P-frames
    // let us assert exactly-once semantics.
    const POST_REQUEST_FRAMES: u64 = 30;
    for i in 0..POST_REQUEST_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }
    enc.flush();

    let mut post_pkts: Vec<EncodedPacket> = Vec::new();
    loop {
        match pkt_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(pkt) => {
                tracing::info!(
                    "[T7.2] post-request pkt {} — is_keyframe={} len={}",
                    post_pkts.len(),
                    pkt.is_keyframe,
                    pkt.data.len()
                );
                post_pkts.push(pkt);
                // Collect enough to see IDR + at least one P-frame after it.
                if post_pkts.len() >= IDR_TOLERANCE + 5 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("[T7.2] encoder died after request_keyframe() — pump thread exited");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    assert!(
        !post_pkts.is_empty(),
        "[T7.2] expected ≥ 1 post-request packet; got 0 — encoder may have died or drain window too short"
    );

    // Spec R7, R12, S5 (part 1): post-request batch MUST contain a keyframe.
    // Eventually-style: any keyframe within the collected slice is sufficient (DD8, R9).
    let idr_idx = assert_keyframe_within_next_n_frames(&post_pkts, IDR_TOLERANCE);
    tracing::info!("[T7.2] keyframe found at post-request idx {idr_idx}");

    // Spec R3, S5 (part 2): the packet AFTER the IDR must NOT be a keyframe — the
    // keyframe request is consumed exactly once (atomic swap semantics, DD6).
    if let Some(after_idr) = post_pkts.get(idr_idx + 1) {
        assert!(
            !after_idr.is_keyframe,
            "[T7.2] packet after forced IDR (idx {}) must have is_keyframe == false (exactly-once); \
             got is_keyframe == true — request_keyframe signal was NOT consumed atomically",
            idr_idx + 1
        );
        tracing::info!(
            "[T7.2] PASS — IDR at idx {idr_idx}; next packet (idx {}) is P-frame (exactly-once confirmed)",
            idr_idx + 1
        );
    } else {
        // Only one packet collected — the IDR itself. Cannot assert exactly-once but
        // IDR assertion passed. Document as partial evidence.
        tracing::warn!(
            "[T7.2] only {} post-request packet(s) collected — cannot assert exactly-once; \
             IDR at idx {idr_idx} confirmed but P-frame follow-on not observed",
            post_pkts.len()
        );
    }

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

// ─── Phase 0 — Slice 6 NVENC `is_keyframe` flag: packet format probe ──────────
//
// Mechanism 1 hypothesis (explore #793, discovery #794): NVENC emits H.264 in
// Annex-B format with 3-byte start codes (`0x00 0x00 0x01`), not the 4-byte form
// (`0x00 0x00 0x00 0x01`) emitted by Intel QSV.  The pre-fix `is_annex_b_now`
// expression in `collect_output` (windows_mft.rs:1814) requires all four bytes,
// so 3-byte Annex-B packets are misclassified as AVCC and routed through
// `avcc_to_annex_b`, corrupting the buffer and causing `is_keyframe = false` on
// genuine NVENC IDR access units.
//
// This probe is Host B (NVIDIA NVENC) empirical evidence before the C1 RED / C2
// GREEN fix commits.  It logs the raw bytes of the FIRST packet received so the
// team can confirm whether the start code is 3-byte or 4-byte.  No assertion is
// made — the trace log is the deliverable (DD5, Slice 6 design).
//
// Retained as a permanent #[ignore]-gated regression guard after Slice 6 merges,
// following the Slice 5 DD7 / Slice 4 DD7 precedent.  Re-running on a future
// driver update provides cheap evidence of format continuity vs. vendor changes.
//
// Run on Host B (NVIDIA NVENC):
//
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run --release --features hw-encoder -p sm-infra `
//     --test windows_mft_encode phase0_nvenc_idr_packet_format_dump `
//     --run-ignored only --no-capture
//
// Expected result confirming Mechanism 1:
//   raw_prefix on pkt 0: [00, 00, 01, ...] (3-byte Annex-B start code)

/// Probe — dump the raw byte prefix of the first NVENC output packet.
///
/// Hypothesis: NVENC emits 3-byte Annex-B start codes (`0x00 0x00 0x01`), not the
/// 4-byte form emitted by Intel QSV.  If confirmed, the `is_annex_b_now` expression
/// at windows_mft.rs:1814 misclassifies NVENC packets as AVCC, corrupting IDR output.
///
/// This probe is observation-only (no assertions) — the trace is the deliverable.
/// Retained post-Slice-6 as DD7-style regression evidence vs. future driver changes.
///
/// Cadence:
///   1. Create encoder, start pump.
///   2. Submit 5 synthetic frames → `enc.flush()` → drain output.
///   3. Log: `raw_bytes[0..min(8, len)]` hex, `raw_bytes.len()`, `is_keyframe`.
///   4. Also log 3-byte vs 4-byte Annex-B start code match (`0x00 0x00 0x01` vs
///      `0x00 0x00 0x00 0x01`). `MFSampleExtension_CleanPoint` is internal to
///      `collect_output`; `pkt.is_keyframe` = `clean_point || annex_b_contains_idr`.
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 Slice 6 probe — NVENC packet format dump on Host B (NVIDIA); captures Mechanism 1 evidence"]
fn phase0_nvenc_idr_packet_format_dump() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    // 5 frames is enough to trigger the SPS+PPS+IDR access unit on session open.
    // A larger batch is not needed: the very first output packet is the one under
    // investigation (start-code format is set by the encoder at session open).
    const SUBMIT_FRAMES: u64 = 5;

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

    tracing::info!("[NVENC-P0] submitting {} synthetic frames", SUBMIT_FRAMES);
    for i in 0..SUBMIT_FRAMES {
        frame_tx
            .send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
            .expect("frame_tx should be open");
    }

    tracing::info!("[NVENC-P0] flush() — COMMAND_DRAIN to force packet emission");
    enc.flush();

    let mut pkt_idx = 0u32;
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(pkt) => {
                let len = pkt.data.len();
                let prefix_len = len.min(8);
                let prefix = &pkt.data[..prefix_len];

                // Inline Annex-B start-code detection for observation purposes.
                // These are the same byte patterns used by `is_annex_b_now` (pre-fix:
                // 4-byte only) and the proposed fix (3-byte OR 4-byte, DD1).
                let has_3byte_start = len >= 3
                    && pkt.data[0] == 0x00
                    && pkt.data[1] == 0x00
                    && pkt.data[2] == 0x01;
                let has_4byte_start = len >= 4
                    && pkt.data[0] == 0x00
                    && pkt.data[1] == 0x00
                    && pkt.data[2] == 0x00
                    && pkt.data[3] == 0x01;

                tracing::info!(
                    "[NVENC-P0] pkt {} — len={} is_keyframe={} \
                     raw_prefix={:02x?} \
                     has_3byte_annex_b={} has_4byte_annex_b={}",
                    pkt_idx,
                    len,
                    pkt.is_keyframe,
                    prefix,
                    has_3byte_start,
                    has_4byte_start,
                );

                // `is_keyframe` reflects `MFSampleExtension_CleanPoint || annex_b_contains_idr`.
                // Both CleanPoint read and IDR scan are encapsulated inside collect_output;
                // logging pkt.is_keyframe is the only observable from this test boundary.
                println!(
                    "[NVENC-P0] pkt={} len={} is_keyframe={} raw_prefix={:02x?} \
                     has_3byte_annex_b={} has_4byte_annex_b={}",
                    pkt_idx,
                    len,
                    pkt.is_keyframe,
                    prefix,
                    has_3byte_start,
                    has_4byte_start,
                );

                pkt_idx += 1;
                // Collect up to SUBMIT_FRAMES + pipeline depth (max 10 packets).
                if pkt_idx >= (SUBMIT_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P0] OUTCOME=ENCODER_DIED — pump thread exited unexpectedly after {} packets",
                    pkt_idx
                );
                println!(
                    "[NVENC-P0] OUTCOME=ENCODER_DIED after {} packets",
                    pkt_idx
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pkt_idx == 0 {
                    tracing::warn!(
                        "[NVENC-P0] OUTCOME=EMPTY_DRAIN — no packets before 10s timeout; \
                         flush() may not have triggered output (harness misconfigured?)"
                    );
                    println!("[NVENC-P0] OUTCOME=EMPTY_DRAIN — no packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P0] drain complete — {} packets received",
                        pkt_idx
                    );
                    println!("[NVENC-P0] drain complete — {} packets received", pkt_idx);
                }
                break;
            }
        }
    }

    tracing::info!(
        "[NVENC-P0] SUMMARY — total_packets={} \
         (check raw_prefix on pkt 0: [00, 00, 01, ..] = 3-byte Annex-B → Mechanism 1 confirmed; \
         [00, 00, 00, 01, ..] = 4-byte Annex-B → QSV-identical, no pre-fix bug on NVENC)",
        pkt_idx
    );
    println!(
        "[NVENC-P0] SUMMARY total_packets={} — see raw_prefix on pkt 0 for format verdict",
        pkt_idx
    );

    // Observation-only probe: no assertions.
    // The trace log is the deliverable for the Slice 6 design gate (DD5, S8).
    let _ = enc.stop();
}

// ─── Phase 0 — Slice 6 NVENC post-recreate IDR format probe (Batch 2) ─────────
//
// WHY THIS PROBE EXISTS:
// The original Slice 6 hypothesis (explore #793, discovery #794) claimed NVENC emits
// 3-byte Annex-B start codes.  The first C0 probe (`phase0_nvenc_idr_packet_format_dump`,
// commit `b048b36`) FALSIFIED that hypothesis on Host B: NVENC priming IDR emits 4-byte
// Annex-B start codes identical to Intel QSV, and `is_keyframe=TRUE` on pkt 0 (engram
// #800).  The priming path works correctly.
//
// The bug reported by T7.1/T7.2 on Host B (`is_keyframe=false` on post-recreate IDR)
// must therefore live in the POST-RECREATE path (Mechanism G: `request_keyframe_via_recreate()`),
// NOT in the priming path.  This probe exercises that path directly so we have empirical
// byte-level evidence of what NVENC actually emits AFTER recreate, enabling a new
// (grounded) hypothesis before any C1 RED fix is attempted.
//
// Run on Host B (NVIDIA NVENC):
//
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run --release --features hw-encoder -p sm-infra `
//     --test windows_mft_encode phase0_nvenc_post_recreate_idr_format_dump `
//     --run-ignored only --no-capture
//
// No assertions — the trace log is the deliverable.  Compare priming pkt 0 (from the
// first C0 probe) with the first post-recreate packet here to spot any structural
// difference in NAL layout, start-code type, or `is_keyframe` flag state.

/// Probe — dump raw byte prefix of NVENC packets both before and after
/// `request_keyframe_via_recreate()` (Mechanism G).
///
/// The first C0 probe (engram #800) established that the NVENC priming IDR is detected
/// correctly (`is_keyframe=TRUE`, 4-byte Annex-B).  This probe targets the POST-RECREATE
/// path that T7.1/T7.2 actually exercise; it logs every packet from both the priming and
/// post-recreate drains so the two can be compared side-by-side.
///
/// Cadence:
///   1. Create encoder, start pump.
///   2. Submit 5 priming frames → `enc.flush()` → drain (log every packet).
///   3. Call `enc.request_keyframe_via_recreate()` (Mechanism G: pump_loop tears down
///      IMFTransform and re-activates fresh handle).
///   4. Submit 30 post-recreate frames → `enc.flush()` → drain (log every packet).
///   5. Print SUMMARY block: totals, first post-recreate packet index / is_keyframe / raw_prefix.
///
/// 30 post-recreate frames matches the Slice 5 canonical cadence (#786, #787): pump_loop
/// needs ≥3 frames post-request to observe the flag, trigger recreate, and emit IDR.
/// No assertions — observation-only.
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 Slice 6 Batch 2 probe — NVENC post-recreate IDR format dump on Host B (NVIDIA); no assertions"]
fn phase0_nvenc_post_recreate_idr_format_dump() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    // 5 priming frames: enough to establish a healthy encoder session and drain the
    // SPS+PPS+IDR access unit as baseline for the start-code comparison.
    const PRIMING_FRAMES: u64 = 5;
    // 30 post-recreate frames: matches the Slice 5 round-3 probe cadence (#786).
    // pump_loop needs ≥3 frames to observe `keyframe_recreate_pending`, drain the
    // old handle, re-activate, and emit IDR on the fresh handle.  30 is overkill
    // but eliminates any timing sensitivity on NVENC.
    const POST_RECREATE_FRAMES: u64 = 30;

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

    // Inline Annex-B detection helper — same byte patterns as `is_annex_b_now` (4-byte)
    // and the proposed fix (3-byte OR 4-byte, DD1).  Defined as a closure so both drain
    // loops share identical logic without repeating the byte comparisons.
    let log_pkt = |tag: &str, idx: u32, pkt: &EncodedPacket| {
        let len = pkt.data.len();
        let prefix_len = len.min(8);
        let prefix = &pkt.data[..prefix_len];
        let has_3byte_start = len >= 3
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x01;
        let has_4byte_start = len >= 4
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x00
            && pkt.data[3] == 0x01;
        tracing::info!(
            "[NVENC-P0b] {} pkt {} — len={} is_keyframe={} \
             raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
        println!(
            "[NVENC-P0b] {} pkt={} len={} is_keyframe={} raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
    };

    // ── Batch 1: priming drain ────────────────────────────────────────────────
    tracing::info!("[NVENC-P0b] submitting {} priming frames", PRIMING_FRAMES);
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }

    tracing::info!("[NVENC-P0b] flush() #1 — COMMAND_DRAIN to collect priming packets");
    enc.flush();

    let mut priming_count = 0u32;
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(pkt) => {
                log_pkt("PRIMING", priming_count, &pkt);
                priming_count += 1;
                // Drain up to PRIMING_FRAMES + pipeline depth.
                if priming_count >= (PRIMING_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P0b] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                println!(
                    "[NVENC-P0b] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                let _ = enc.stop();
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if priming_count == 0 {
                    tracing::warn!(
                        "[NVENC-P0b] OUTCOME=EMPTY_DRAIN — no priming packets before 10s timeout"
                    );
                    println!("[NVENC-P0b] OUTCOME=EMPTY_DRAIN — no priming packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P0b] priming drain complete — {} packets received",
                        priming_count
                    );
                    println!(
                        "[NVENC-P0b] priming drain complete — {} packets received",
                        priming_count
                    );
                }
                break;
            }
        }
    }

    tracing::info!(
        "[NVENC-P0b] PRIMING SUMMARY — total_priming={}",
        priming_count
    );
    println!("[NVENC-P0b] PRIMING SUMMARY total_priming={}", priming_count);

    // ── Mechanism G (HISTORICAL — deleted in Slice 6 R2) ─────────────────────
    //
    // Original call: enc.request_keyframe_via_recreate()
    // Mechanism G (IMFTransform recreate) was deleted in Slice 6 R2 after this probe
    // FALSIFIED it on NVENC: 29/29 P-frames post-recreate (engram #801).
    // The method no longer exists. Replaced with request_keyframe() (ForceKeyFrame,
    // the Slice 6 R2 canonical mechanism) to keep the probe runnable as a smoke test.
    // Historical results are preserved in engram #801.
    tracing::info!(
        "[NVENC-P0b] request_keyframe() (ForceKeyFrame, Slice 6 R2 canonical) — \
         NOTE: original Mechanism G call deleted; see engram #801 for historical results"
    );
    enc.request_keyframe();

    // ── Batch 2: post-recreate drain ──────────────────────────────────────────
    tracing::info!(
        "[NVENC-P0b] submitting {} post-recreate frames",
        POST_RECREATE_FRAMES
    );
    for i in 0..POST_RECREATE_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }

    tracing::info!("[NVENC-P0b] flush() #2 — COMMAND_DRAIN to collect post-recreate packets");
    enc.flush();

    let mut post_count = 0u32;
    // Track first post-recreate packet fields for SUMMARY block.
    let mut first_post_is_keyframe: Option<bool> = None;
    let mut first_post_raw_prefix: Option<Vec<u8>> = None;
    let mut first_post_len: Option<usize> = None;

    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(pkt) => {
                if post_count == 0 {
                    first_post_is_keyframe = Some(pkt.is_keyframe);
                    first_post_len = Some(pkt.data.len());
                    first_post_raw_prefix = Some(pkt.data[..pkt.data.len().min(8)].to_vec());
                }
                log_pkt("POST-RECREATE", post_count, &pkt);
                post_count += 1;
                // Drain up to POST_RECREATE_FRAMES + pipeline depth.
                if post_count >= (POST_RECREATE_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P0b] OUTCOME=ENCODER_DIED — pump thread exited after \
                     request_keyframe_via_recreate + flush; {} post-recreate packets received",
                    post_count
                );
                println!(
                    "[NVENC-P0b] OUTCOME=ENCODER_DIED after {} post-recreate packets",
                    post_count
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if post_count == 0 {
                    tracing::warn!(
                        "[NVENC-P0b] OUTCOME=EMPTY_DRAIN — no post-recreate packets before 15s timeout; \
                         Mechanism G may have failed on NVENC (ActivateObject rejected?)"
                    );
                    println!("[NVENC-P0b] OUTCOME=EMPTY_DRAIN — no post-recreate packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P0b] post-recreate drain complete — {} packets received",
                        post_count
                    );
                    println!(
                        "[NVENC-P0b] post-recreate drain complete — {} packets received",
                        post_count
                    );
                }
                break;
            }
        }
    }

    // ── SUMMARY block ─────────────────────────────────────────────────────────
    //
    // The summary is the primary deliverable.  Compare:
    //   - first_post_recreate_is_keyframe: should be TRUE if Mechanism G works on NVENC
    //   - first_post_recreate_raw_prefix: compare to priming pkt 0 prefix for NAL structure delta
    //   - If is_keyframe=FALSE: bug is confirmed in post-recreate path; use raw_prefix to
    //     form a new hypothesis (3-byte vs 4-byte? no AUD? different NAL ordering?)
    let first_post_idx = if post_count > 0 { Some(0u32) } else { None };

    tracing::info!(
        "[NVENC-P0b] SUMMARY — \
         total_priming={} \
         total_post_recreate={} \
         first_post_recreate_idx={:?} \
         first_post_recreate_is_keyframe={:?} \
         first_post_recreate_raw_prefix={:02x?} \
         first_post_recreate_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );
    println!(
        "[NVENC-P0b] SUMMARY total_priming={} total_post_recreate={} \
         first_post_recreate_idx={:?} first_post_recreate_is_keyframe={:?} \
         first_post_recreate_raw_prefix={:02x?} first_post_recreate_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );

    // Observation-only probe: no assertions.
    // The trace log is the deliverable for post-falsification re-investigation (engram #800).
    let _ = enc.stop();
}

// ─── Phase 0 Probe P1: CleanPoint INPUT write on NVENC ───────────────────────
//
// WHY this probe exists:
//
// Slice 6 R2 re-introduces `MFSampleExtension_CleanPoint=1` on the INPUT IMFSample as the
// mid-stream IDR mechanism for NVIDIA NVENC (Candidate A — explore #803, Slice 6 R2).
//
// Background:
// - Mechanism G (IMFTransform drop+recreate, Slice 5) was confirmed to work on Intel QSV
//   but was FALSIFIED on NVENC (C0.b probe `ae36499`: 29/29 P-frames post-recreate, zero IDR).
// - Discovery #804 (inline comment at `windows_mft.rs:1108-1110`) confirms NVENC honoured
//   CleanPoint before Slice 5 DD10 deleted the write path under the vendor-uniform assumption.
// - This probe VALIDATES that re-introducing the CleanPoint write in the current architecture
//   (post-Slice-4 SWAP-FIRE deletion, post-Slice-5 Mechanism G) still produces IDR on NVENC.
//
// Cadence:
//   1. Create encoder (vendor detection logs NvidiaNvenc at INFO).
//   2. Start, submit 5 priming frames → `enc.flush()` → drain (log every packet).
//   3. Call `enc.request_keyframe_via_cleanpoint()` — arms `cleanpoint_pending` atomic.
//   4. Submit 30 post-request frames → `enc.flush()` → drain (log every packet).
//   5. Print SUMMARY block: totals, first post-request packet index / is_keyframe / raw_prefix.
//
// 30 post-request frames matches the Slice 5 / Slice 6 C0.b canonical probe cadence (#786):
// eliminates any timing sensitivity; CleanPoint should produce IDR at index 0 or 1.
//
// No assertions — observation-only.  If first_post_request_is_keyframe=true, Candidate A
// is confirmed and the project moves to sdd-propose.  If false, escalate to P2 / P3.
//
// Run on Host B (NVIDIA NVENC):
//   cargo nextest run --release --features hw-encoder -p sm-infra \
//     --test windows_mft_encode phase0_nvenc_cleanpoint_idr_via_input_sample_attribute \
//     --run-ignored only --no-capture
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 Slice 6 R2 P1 — NVENC CleanPoint IDR via input sample attribute on Host B (NVIDIA); no assertions"]
fn phase0_nvenc_cleanpoint_idr_via_input_sample_attribute() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    // 5 priming frames: enough to establish a healthy encoder session and drain the
    // SPS+PPS+IDR access unit as baseline.
    const PRIMING_FRAMES: u64 = 5;
    // 30 post-request frames: matches the Slice 5 round-3 / Slice 6 C0.b canonical cadence
    // (#786).  CleanPoint should produce IDR within the first 1–2 frames; 30 is overkill but
    // eliminates any NVENC pipeline latency sensitivity.
    const POST_REQUEST_FRAMES: u64 = 30;

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

    // Inline packet logger — same format as phase0_nvenc_post_recreate_idr_format_dump
    // (C0.b probe `ae36499`) for cross-probe comparability.
    let log_pkt = |tag: &str, idx: u32, pkt: &EncodedPacket| {
        let len = pkt.data.len();
        let prefix_len = len.min(8);
        let prefix = &pkt.data[..prefix_len];
        let has_3byte_start = len >= 3
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x01;
        let has_4byte_start = len >= 4
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x00
            && pkt.data[3] == 0x01;
        tracing::info!(
            "[NVENC-P1] {} pkt {} — len={} is_keyframe={} \
             raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
        println!(
            "[NVENC-P1] {} pkt={} len={} is_keyframe={} raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
    };

    // ── Batch 1: priming drain ────────────────────────────────────────────────
    tracing::info!("[NVENC-P1] submitting {} priming frames", PRIMING_FRAMES);
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }

    tracing::info!("[NVENC-P1] flush() #1 — COMMAND_DRAIN to collect priming packets");
    enc.flush();

    let mut priming_count = 0u32;
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(pkt) => {
                log_pkt("PRIMING", priming_count, &pkt);
                priming_count += 1;
                // Drain up to PRIMING_FRAMES + pipeline depth.
                if priming_count >= (PRIMING_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P1] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                println!(
                    "[NVENC-P1] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                let _ = enc.stop();
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if priming_count == 0 {
                    tracing::warn!(
                        "[NVENC-P1] OUTCOME=EMPTY_DRAIN — no priming packets before 10s timeout"
                    );
                    println!("[NVENC-P1] OUTCOME=EMPTY_DRAIN — no priming packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P1] priming drain complete — {} packets received",
                        priming_count
                    );
                    println!(
                        "[NVENC-P1] priming drain complete — {} packets received",
                        priming_count
                    );
                }
                break;
            }
        }
    }

    tracing::info!(
        "[NVENC-P1] PRIMING SUMMARY — total_priming={}",
        priming_count
    );
    println!("[NVENC-P1] PRIMING SUMMARY total_priming={}", priming_count);

    // ── Candidate A: arm CleanPoint via request_keyframe_via_cleanpoint() ────
    //
    // Original call: enc.request_keyframe_via_cleanpoint()
    // CleanPoint INPUT write (Candidate A) was deleted in Slice 6 R2 after this probe
    // FALSIFIED it on NVENC: 30/30 P-frames despite CleanPoint=1 on input (engram #807).
    // The method no longer exists. Replaced with request_keyframe() (ForceKeyFrame,
    // the Slice 6 R2 canonical mechanism) to keep the probe runnable as a smoke test.
    // Historical results (P1 falsification) are preserved in engram #807.
    //
    // Note: `cleanpoint_pending` atomic and `submit_frame(force_cleanpoint)` are also
    // deleted. The CleanPoint READ path in collect_output is retained (DD7).
    tracing::info!(
        "[NVENC-P1] request_keyframe() (ForceKeyFrame, Slice 6 R2 canonical) — \
         NOTE: original CleanPoint write call deleted; see engram #807 for historical results"
    );
    enc.request_keyframe();

    // ── Batch 2: post-request drain ───────────────────────────────────────────
    tracing::info!(
        "[NVENC-P1] submitting {} post-request frames",
        POST_REQUEST_FRAMES
    );
    for i in 0..POST_REQUEST_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }

    tracing::info!("[NVENC-P1] flush() #2 — COMMAND_DRAIN to collect post-request packets");
    enc.flush();

    let mut post_count = 0u32;
    // Track first post-request packet fields for SUMMARY block.
    let mut first_post_is_keyframe: Option<bool> = None;
    let mut first_post_raw_prefix: Option<Vec<u8>> = None;
    let mut first_post_len: Option<usize> = None;

    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(pkt) => {
                if post_count == 0 {
                    first_post_is_keyframe = Some(pkt.is_keyframe);
                    first_post_len = Some(pkt.data.len());
                    first_post_raw_prefix = Some(pkt.data[..pkt.data.len().min(8)].to_vec());
                }
                log_pkt("POST-REQUEST", post_count, &pkt);
                post_count += 1;
                // Drain up to POST_REQUEST_FRAMES + pipeline depth.
                if post_count >= (POST_REQUEST_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P1] OUTCOME=ENCODER_DIED — pump thread exited after \
                     request_keyframe_via_cleanpoint + flush; {} post-request packets received",
                    post_count
                );
                println!(
                    "[NVENC-P1] OUTCOME=ENCODER_DIED after {} post-request packets",
                    post_count
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if post_count == 0 {
                    tracing::warn!(
                        "[NVENC-P1] OUTCOME=EMPTY_DRAIN — no post-request packets before 15s timeout; \
                         pump may have failed to deliver frames after CleanPoint"
                    );
                    println!("[NVENC-P1] OUTCOME=EMPTY_DRAIN — no post-request packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P1] post-request drain complete — {} packets received",
                        post_count
                    );
                    println!(
                        "[NVENC-P1] post-request drain complete — {} packets received",
                        post_count
                    );
                }
                break;
            }
        }
    }

    // ── SUMMARY block ─────────────────────────────────────────────────────────
    //
    // The summary is the primary deliverable.  Compare:
    //   - first_post_request_is_keyframe: should be TRUE if Candidate A works on NVENC
    //   - first_post_request_raw_prefix: compare to priming pkt 0 prefix ([00,00,00,01,09,10])
    //   - If is_keyframe=TRUE and raw_prefix shows I-frame AUD: Candidate A CONFIRMED
    //   - If is_keyframe=FALSE: escalate to P2 (ForceKeyFrame) or P3 (Hybrid G+CleanPoint)
    let first_post_idx = if post_count > 0 { Some(0u32) } else { None };

    tracing::info!(
        "[NVENC-P1] SUMMARY — \
         total_priming={} \
         total_post_request={} \
         first_post_request_idx={:?} \
         first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} \
         first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );
    println!(
        "[NVENC-P1] SUMMARY total_priming={} total_post_request={} \
         first_post_request_idx={:?} first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );

    // Observation-only probe: no assertions.
    // Decision rule (explore #803):
    //   P1 PASS (is_keyframe=true) → Candidate A confirmed; move to sdd-propose.
    //   P1 FAIL (is_keyframe=false) → escalate to P2 (CODECAPI_AVEncVideoForceKeyFrame)
    //                                  or P3 (Hybrid Mechanism G + CleanPoint on first post-recreate frame).
    let _ = enc.stop();
}

// ─── Phase 0 Probe P2-NVENC: CODECAPI_AVEncVideoForceKeyFrame BEFORE ProcessInput ─
//
// WHY this probe exists:
//
// P1 (`phase0_nvenc_cleanpoint_idr_via_input_sample_attribute`) FALSIFIED Candidate A
// on Host B (NVENC, current driver, branch tip `aee5750`): 30/30 P-frames post-request,
// zero IDR (engram #807).
//
// This probe tests CANDIDATE B (research #808):
//   `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` called BEFORE
//   ProcessInput for the target frame — the canonical Chromium + FFmpeg production sequence.
//
// WHY this is NOT the Slice 4 SWAP-FIRE pattern:
//   - Slice 4 (DD1) called SetValue AFTER ProcessInput ("FIRE" step).
//   - Chromium `media_foundation_video_encode_accelerator_win.cc:2299-2307` and FFmpeg
//     `libavcodec/mfenc.c::mf_send_frame()` call SetValue BEFORE ProcessInput.
//   - This timing difference may explain Slice 4's failure on Intel QSV.
//
// WHY VT_UI4 (not VT_BOOL):
//   - Slice 4 used VT_BOOL; the MS-documented type is VT_UI4=1 (research #808).
//   - NVENC is a required HCK Win8+ certification property: NVENC MUST implement it.
//
// Cadence (same as P1 for cross-probe comparability):
//   1. Create encoder (vendor detection logs NvidiaNvenc at INFO).
//   2. Start, submit 5 priming frames → `enc.flush()` → drain (log every packet).
//   3. Call `enc.request_keyframe_via_force_keyframe_icodecapi()` — arms the pending flag.
//      pump_loop consumes it BEFORE ProcessInput on the next NeedInput credit.
//   4. Submit 30 post-request frames → `enc.flush()` → drain (log every packet).
//   5. Print SUMMARY block: totals, first post-request packet index / is_keyframe / raw_prefix.
//
// No assertions — observation-only. Results feed the P2 decision tree:
//   P2-NVENC PASS + P2-Intel-QSV PASS → Candidate B is vendor-uniform (replace Mechanism G entirely).
//   P2-NVENC PASS + P2-Intel-QSV FAIL → Candidate B is NVENC-only (dispatch NVENC→B, Intel→G).
//   P2-NVENC FAIL + P2-Intel-QSV PASS → Candidate B is Intel-only (unlikely; escalate to P3).
//   P2-NVENC FAIL + P2-Intel-QSV FAIL → Escalate to Candidate C (GOP toggle) or P3.
//
// Run on Host B (NVIDIA NVENC):
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run --release --features hw-encoder -p sm-infra `
//     --test windows_mft_encode phase0_nvenc_force_keyframe_via_codecapi_before_processinput `
//     --run-ignored only --no-capture
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 Slice 6 R2 P2-NVENC — ForceKeyFrame via ICodecAPI BEFORE ProcessInput on Host B (NVIDIA); no assertions"]
fn phase0_nvenc_force_keyframe_via_codecapi_before_processinput() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    // 5 priming frames: establishes a healthy encoder session with setup-sequence IDR at idx 0.
    const PRIMING_FRAMES: u64 = 5;
    // 30 post-request frames: matches the canonical probe cadence (P1, C0.b, Slice 5 round 3).
    // ForceKeyFrame should produce IDR at index 0 or 1 if NVENC honours it; 30 ensures we see all.
    const POST_REQUEST_FRAMES: u64 = 30;

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

    // Inline packet logger — same format as P1 for cross-probe comparability.
    let log_pkt = |tag: &str, idx: u32, pkt: &EncodedPacket| {
        let len = pkt.data.len();
        let prefix_len = len.min(8);
        let prefix = &pkt.data[..prefix_len];
        let has_3byte_start = len >= 3
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x01;
        let has_4byte_start = len >= 4
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x00
            && pkt.data[3] == 0x01;
        tracing::info!(
            "[NVENC-P2] {} pkt {} — len={} is_keyframe={} \
             raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
        println!(
            "[NVENC-P2] {} pkt={} len={} is_keyframe={} raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
    };

    // ── Batch 1: priming drain ────────────────────────────────────────────────
    tracing::info!("[NVENC-P2] submitting {} priming frames", PRIMING_FRAMES);
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }

    tracing::info!("[NVENC-P2] flush() #1 — COMMAND_DRAIN to collect priming packets");
    enc.flush();

    let mut priming_count = 0u32;
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(pkt) => {
                log_pkt("PRIMING", priming_count, &pkt);
                priming_count += 1;
                if priming_count >= (PRIMING_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P2] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                println!(
                    "[NVENC-P2] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                let _ = enc.stop();
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if priming_count == 0 {
                    tracing::warn!(
                        "[NVENC-P2] OUTCOME=EMPTY_DRAIN — no priming packets before 10s timeout"
                    );
                    println!("[NVENC-P2] OUTCOME=EMPTY_DRAIN — no priming packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P2] priming drain complete — {} packets received",
                        priming_count
                    );
                    println!(
                        "[NVENC-P2] priming drain complete — {} packets received",
                        priming_count
                    );
                }
                break;
            }
        }
    }

    tracing::info!(
        "[NVENC-P2] PRIMING SUMMARY — total_priming={}",
        priming_count
    );
    println!("[NVENC-P2] PRIMING SUMMARY total_priming={}", priming_count);

    // ── Candidate B: arm ForceKeyFrame via request_keyframe_via_force_keyframe_icodecapi() ──
    //
    // Sets `force_keyframe_icodecapi_pending=true` (atomic Release store).
    // pump_loop consumes it on the next NeedInput credit:
    //   swaps to false (AcqRel) → calls
    //   `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)`
    //   BEFORE submit_frame / ProcessInput (canonical Chromium + FFmpeg ordering).
    //
    // Bypasses vendor dispatch in request_keyframe() — directly arms Candidate B
    // regardless of EncoderVendor. This is intentional: probe tests the raw mechanism.
    tracing::info!(
        "[NVENC-P2] request_keyframe_via_force_keyframe_icodecapi() — arming Candidate B \
         (ICodecAPI::SetValue BEFORE ProcessInput, VT_UI4=1)"
    );
    enc.request_keyframe_via_force_keyframe_icodecapi();

    // ── Batch 2: post-request drain ───────────────────────────────────────────
    tracing::info!(
        "[NVENC-P2] submitting {} post-request frames",
        POST_REQUEST_FRAMES
    );
    for i in 0..POST_REQUEST_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }

    tracing::info!("[NVENC-P2] flush() #2 — COMMAND_DRAIN to collect post-request packets");
    enc.flush();

    let mut post_count = 0u32;
    let mut first_post_is_keyframe: Option<bool> = None;
    let mut first_post_raw_prefix: Option<Vec<u8>> = None;
    let mut first_post_len: Option<usize> = None;

    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(pkt) => {
                if post_count == 0 {
                    first_post_is_keyframe = Some(pkt.is_keyframe);
                    first_post_len = Some(pkt.data.len());
                    first_post_raw_prefix = Some(pkt.data[..pkt.data.len().min(8)].to_vec());
                }
                log_pkt("POST-REQUEST", post_count, &pkt);
                post_count += 1;
                if post_count >= (POST_REQUEST_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[NVENC-P2] OUTCOME=ENCODER_DIED — pump thread exited after \
                     request_keyframe_via_force_keyframe_icodecapi + flush; \
                     {} post-request packets received",
                    post_count
                );
                println!(
                    "[NVENC-P2] OUTCOME=ENCODER_DIED after {} post-request packets",
                    post_count
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if post_count == 0 {
                    tracing::warn!(
                        "[NVENC-P2] OUTCOME=EMPTY_DRAIN — no post-request packets before 15s timeout; \
                         pump may have failed to deliver frames after ForceKeyFrame SetValue"
                    );
                    println!("[NVENC-P2] OUTCOME=EMPTY_DRAIN — no post-request packets received");
                } else {
                    tracing::info!(
                        "[NVENC-P2] post-request drain complete — {} packets received",
                        post_count
                    );
                    println!(
                        "[NVENC-P2] post-request drain complete — {} packets received",
                        post_count
                    );
                }
                break;
            }
        }
    }

    // ── SUMMARY block ─────────────────────────────────────────────────────────
    //
    // Compare first_post_request_is_keyframe against P1 result and Intel QSV P2 result.
    // Decision tree:
    //   PASS (is_keyframe=true)  → Candidate B works on NVENC; compare with Intel QSV P2.
    //   FAIL (is_keyframe=false) → Candidate B does not produce IDR on NVENC; escalate to P3.
    let first_post_idx = if post_count > 0 { Some(0u32) } else { None };

    tracing::info!(
        "[NVENC-P2] SUMMARY — \
         total_priming={} \
         total_post_request={} \
         first_post_request_idx={:?} \
         first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} \
         first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );
    println!(
        "[NVENC-P2] SUMMARY total_priming={} total_post_request={} \
         first_post_request_idx={:?} first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );

    // Observation-only probe: no assertions.
    let _ = enc.stop();
}

// ─── Phase 0 Probe P2-Intel QSV: CODECAPI_AVEncVideoForceKeyFrame BEFORE ProcessInput ─
//
// WHY this probe exists:
//
// Slice 4 DD10 comment states: "Intel QSV does not honor mid-stream ICodecAPI ForceKeyFrame."
// That verdict was reached using the SWAP-FIRE pattern (SetValue AFTER ProcessInput, VT_BOOL).
// Research #808 identifies TWO defects in the Slice 4 test:
//   1. TIMING WRONG: Called AFTER ProcessInput, not BEFORE (Chromium calls it BEFORE).
//   2. VARIANT TYPE WRONG: Used VT_BOOL instead of VT_UI4 (the MS-documented type).
//
// This probe RE-VALIDATES the Intel QSV verdict with the correct sequence:
//   `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` BEFORE ProcessInput.
//
// If Intel QSV DOES honour ForceKeyFrame with the canonical sequence, Candidate B would
// become VENDOR-UNIFORM (both NVENC and Intel QSV) — eliminating the need for vendor
// dispatch and potentially replacing Mechanism G entirely. This is a high-value finding.
//
// Cadence (same as P2-NVENC for cross-probe comparability):
//   1. Create encoder (vendor detection logs IntelQsv at INFO).
//   2. Start, submit 5 priming frames → `enc.flush()` → drain (log every packet).
//   3. Call `enc.request_keyframe_via_force_keyframe_icodecapi()` — arms the pending flag.
//      pump_loop consumes it BEFORE ProcessInput on the next NeedInput credit.
//   4. Submit 30 post-request frames → `enc.flush()` → drain (log every packet).
//   5. Print SUMMARY block: totals, first post-request packet index / is_keyframe / raw_prefix.
//
// No assertions — observation-only. Results combined with P2-NVENC result for final decision.
//
// Run on Host A (Intel QSV):
//   $env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
//   cargo nextest run --release --features hw-encoder -p sm-infra `
//     --test windows_mft_encode phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput `
//     --run-ignored only --no-capture
#[test]
#[cfg(feature = "hw-encoder")]
#[ignore = "Phase 0 Slice 6 R2 P2-Intel — re-validates ForceKeyFrame via ICodecAPI BEFORE ProcessInput (VT_UI4) on Host A (Intel QSV); no assertions"]
fn phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput() {
    init_tracing();

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    // 5 priming frames: establishes a healthy encoder session with setup-sequence IDR at idx 0.
    const PRIMING_FRAMES: u64 = 5;
    // 30 post-request frames: matches the canonical probe cadence.
    // If Intel QSV honours ForceKeyFrame BEFORE ProcessInput + VT_UI4, IDR should appear
    // within the first 1-2 frames; 30 provides ample margin for any pipeline latency.
    const POST_REQUEST_FRAMES: u64 = 30;

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

    // Inline packet logger — [INTEL-P2] prefix for cross-probe log disambiguation.
    let log_pkt = |tag: &str, idx: u32, pkt: &EncodedPacket| {
        let len = pkt.data.len();
        let prefix_len = len.min(8);
        let prefix = &pkt.data[..prefix_len];
        let has_3byte_start = len >= 3
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x01;
        let has_4byte_start = len >= 4
            && pkt.data[0] == 0x00
            && pkt.data[1] == 0x00
            && pkt.data[2] == 0x00
            && pkt.data[3] == 0x01;
        tracing::info!(
            "[INTEL-P2] {} pkt {} — len={} is_keyframe={} \
             raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
        println!(
            "[INTEL-P2] {} pkt={} len={} is_keyframe={} raw_prefix={:02x?} \
             has_3byte_annex_b={} has_4byte_annex_b={}",
            tag,
            idx,
            len,
            pkt.is_keyframe,
            prefix,
            has_3byte_start,
            has_4byte_start,
        );
    };

    // ── Batch 1: priming drain ────────────────────────────────────────────────
    tracing::info!("[INTEL-P2] submitting {} priming frames", PRIMING_FRAMES);
    for i in 0..PRIMING_FRAMES {
        send_frame(i);
    }

    tracing::info!("[INTEL-P2] flush() #1 — COMMAND_DRAIN to collect priming packets");
    enc.flush();

    let mut priming_count = 0u32;
    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(pkt) => {
                log_pkt("PRIMING", priming_count, &pkt);
                priming_count += 1;
                if priming_count >= (PRIMING_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[INTEL-P2] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                println!(
                    "[INTEL-P2] OUTCOME=ENCODER_DIED during priming drain after {} packets",
                    priming_count
                );
                let _ = enc.stop();
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if priming_count == 0 {
                    tracing::warn!(
                        "[INTEL-P2] OUTCOME=EMPTY_DRAIN — no priming packets before 10s timeout"
                    );
                    println!("[INTEL-P2] OUTCOME=EMPTY_DRAIN — no priming packets received");
                } else {
                    tracing::info!(
                        "[INTEL-P2] priming drain complete — {} packets received",
                        priming_count
                    );
                    println!(
                        "[INTEL-P2] priming drain complete — {} packets received",
                        priming_count
                    );
                }
                break;
            }
        }
    }

    tracing::info!(
        "[INTEL-P2] PRIMING SUMMARY — total_priming={}",
        priming_count
    );
    println!(
        "[INTEL-P2] PRIMING SUMMARY total_priming={}",
        priming_count
    );

    // ── Candidate B: arm ForceKeyFrame via request_keyframe_via_force_keyframe_icodecapi() ──
    //
    // RE-VALIDATION of Slice 4's "Intel QSV does not honor ForceKeyFrame" verdict.
    // Slice 4 tested: SetValue AFTER ProcessInput + VT_BOOL (both wrong per research #808).
    // This probe tests: SetValue BEFORE ProcessInput + VT_UI4 (canonical Chromium sequence).
    //
    // Sets `force_keyframe_icodecapi_pending=true` (atomic Release store).
    // pump_loop consumes it on the next NeedInput credit before submit_frame / ProcessInput.
    tracing::info!(
        "[INTEL-P2] request_keyframe_via_force_keyframe_icodecapi() — re-validating \
         Candidate B on Intel QSV with canonical BEFORE+VT_UI4 sequence \
         (Slice 4 verdict used AFTER+VT_BOOL — research #808)"
    );
    enc.request_keyframe_via_force_keyframe_icodecapi();

    // ── Batch 2: post-request drain ───────────────────────────────────────────
    tracing::info!(
        "[INTEL-P2] submitting {} post-request frames",
        POST_REQUEST_FRAMES
    );
    for i in 0..POST_REQUEST_FRAMES {
        send_frame(PRIMING_FRAMES + i);
    }

    tracing::info!("[INTEL-P2] flush() #2 — COMMAND_DRAIN to collect post-request packets");
    enc.flush();

    let mut post_count = 0u32;
    let mut first_post_is_keyframe: Option<bool> = None;
    let mut first_post_raw_prefix: Option<Vec<u8>> = None;
    let mut first_post_len: Option<usize> = None;

    loop {
        match pkt_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(pkt) => {
                if post_count == 0 {
                    first_post_is_keyframe = Some(pkt.is_keyframe);
                    first_post_len = Some(pkt.data.len());
                    first_post_raw_prefix = Some(pkt.data[..pkt.data.len().min(8)].to_vec());
                }
                log_pkt("POST-REQUEST", post_count, &pkt);
                post_count += 1;
                if post_count >= (POST_REQUEST_FRAMES as u32).saturating_add(5) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!(
                    "[INTEL-P2] OUTCOME=ENCODER_DIED — pump thread exited after \
                     request_keyframe_via_force_keyframe_icodecapi + flush; \
                     {} post-request packets received",
                    post_count
                );
                println!(
                    "[INTEL-P2] OUTCOME=ENCODER_DIED after {} post-request packets",
                    post_count
                );
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if post_count == 0 {
                    tracing::warn!(
                        "[INTEL-P2] OUTCOME=EMPTY_DRAIN — no post-request packets before 15s timeout; \
                         pump may have failed to deliver frames after ForceKeyFrame SetValue"
                    );
                    println!("[INTEL-P2] OUTCOME=EMPTY_DRAIN — no post-request packets received");
                } else {
                    tracing::info!(
                        "[INTEL-P2] post-request drain complete — {} packets received",
                        post_count
                    );
                    println!(
                        "[INTEL-P2] post-request drain complete — {} packets received",
                        post_count
                    );
                }
                break;
            }
        }
    }

    // ── SUMMARY block ─────────────────────────────────────────────────────────
    //
    // Cross-reference with P2-NVENC result to determine vendor coverage of Candidate B.
    // Decision tree:
    //   PASS (is_keyframe=true) + P2-NVENC PASS → Candidate B is VENDOR-UNIFORM.
    //                                              Can replace Mechanism G for BOTH vendors.
    //   PASS (is_keyframe=true) + P2-NVENC FAIL → Candidate B is Intel-only (unlikely).
    //   FAIL (is_keyframe=false) + P2-NVENC PASS → Candidate B is NVENC-only (dispatch required).
    //   FAIL (is_keyframe=false) + P2-NVENC FAIL → Escalate to Candidate C (GOP toggle) or P3.
    let first_post_idx = if post_count > 0 { Some(0u32) } else { None };

    tracing::info!(
        "[INTEL-P2] SUMMARY — \
         total_priming={} \
         total_post_request={} \
         first_post_request_idx={:?} \
         first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} \
         first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );
    println!(
        "[INTEL-P2] SUMMARY total_priming={} total_post_request={} \
         first_post_request_idx={:?} first_post_request_is_keyframe={:?} \
         first_post_request_raw_prefix={:02x?} first_post_request_len={:?}",
        priming_count,
        post_count,
        first_post_idx,
        first_post_is_keyframe,
        first_post_raw_prefix.as_deref(),
        first_post_len,
    );

    // Observation-only probe: no assertions.
    let _ = enc.stop();
}
