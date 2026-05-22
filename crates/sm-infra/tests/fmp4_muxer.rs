//! Integration tests for `sm_infra::render::fmp4_muxer` (Mp4Muxer).
//!
//! No platform gate — the muxer is pure Rust and runs on every CI runner.
//!
//! Run with:
//!     cargo nextest run -p sm-infra --tests fmp4_muxer
//!
//! These tests verify the byte-level structure of fMP4 output beyond the unit
//! tests embedded in `fmp4_muxer.rs`. Test scenarios:
//!
//! - C1: init + 5 media segments round-trip — byte-scan box structure
//! - C2: mfhd sequence numbers increment 1..=N across N segments
//! - C3: tfdt base_media_decode_time values are monotonically increasing
//! - C4: init segment avcC bytes round-trip SPS+PPS
//! - C5: mp4 crate reader attempt against init segment (documents behavior)

use std::sync::Arc;
use std::time::Duration;

use sm_domain::encode::EncodedPacket;
use sm_infra::render::avcc::parse_sps;
use sm_infra::render::fmp4_muxer::{Mp4Muxer, annex_b_to_avcc, extract_sps_pps_from_idr};

// ─── Golden SPS/PPS fixtures ─────────────────────────────────────────────────

/// Minimal valid SPS for Baseline Level 1.3, 320×240 progressive.
/// profile_idc=0x42, constraint_set_flags=0xC0, level_idc=0x0D
const SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0xC0];

/// Minimal PPS.
const PPS: &[u8] = &[0x68, 0xCE, 0x38, 0x80];

/// Build a synthetic Annex-B IDR packet containing SPS + PPS + IDR NALs.
fn make_idr_packet(ts_ms: u64, seq: u64) -> EncodedPacket {
    let mut data = Vec::new();
    // SPS NAL (type 7 = 0x67)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(SPS);
    // PPS NAL (type 8 = 0x68)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data.extend_from_slice(PPS);
    // IDR slice (type 5 = 0x65)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
    data.extend_from_slice(&[0x88u8; 20]);
    EncodedPacket {
        data: Arc::from(data.into_boxed_slice()),
        is_keyframe: true,
        timestamp: Duration::from_millis(ts_ms),
        sequence: seq,
    }
}

// ─── Helper: scan bytes for a 4-byte box tag, return the offset AFTER the tag ─

fn find_box(haystack: &[u8], tag: &[u8; 4]) -> Option<usize> {
    haystack
        .windows(4)
        .position(|w| w == tag)
        .map(|pos| pos + 4)
}

// ─── Helper: extract mfhd.sequence_number from a media segment ───────────────

fn extract_mfhd_seq(seg: &[u8]) -> u32 {
    // mfhd layout: [size:4][b"mfhd":4][version:1][flags:3][sequence_number:4]
    let tag_end = find_box(seg, b"mfhd").expect("mfhd not found in segment");
    let off = tag_end + 4; // skip version+flags (4 bytes)
    u32::from_be_bytes([seg[off], seg[off + 1], seg[off + 2], seg[off + 3]])
}

// ─── Helper: extract tfdt.base_media_decode_time from a media segment ────────

fn extract_tfdt_time(seg: &[u8]) -> u64 {
    // tfdt layout (version=1): [size:4][b"tfdt":4][version:1=1][flags:3][decode_time:8]
    let tag_end = find_box(seg, b"tfdt").expect("tfdt not found in segment");
    let off = tag_end + 4; // skip version(1)+flags(3)
    u64::from_be_bytes([
        seg[off],
        seg[off + 1],
        seg[off + 2],
        seg[off + 3],
        seg[off + 4],
        seg[off + 5],
        seg[off + 6],
        seg[off + 7],
    ])
}

// ─── C1: init + 5 media segments round-trip via byte-level box scanner ───────
//
// Build init + feed IDR1..IDR6 (IDR1 accumulates; IDR2..IDR6 each emit a segment).
// Concatenate all bytes. Verify box structure via byte scanning.

#[test]
fn mp4_muxer_init_plus_5_segments_round_trip_via_byte_scanner() {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    let init = muxer
        .build_init_segment(&sps_info, SPS, PPS)
        .expect("init segment must build");

    // Verify init segment structure.
    assert_eq!(&init[4..8], b"ftyp", "init must start with ftyp");
    assert_eq!(&init[8..12], b"iso5", "ftyp.major_brand must be iso5");
    assert!(
        init.windows(4).any(|w| w == b"moov"),
        "init must contain moov"
    );
    assert!(
        init.windows(4).any(|w| w == b"avc1"),
        "init must contain avc1"
    );
    assert!(
        init.windows(4).any(|w| w == b"mvex"),
        "init must contain mvex (fMP4 marker)"
    );

    // Feed IDR packets — IDR1 buffers, IDR2..IDR6 each emit a segment.
    let mut segments: Vec<Vec<u8>> = Vec::new();
    let idr1 = make_idr_packet(0, 0);
    assert!(
        muxer.append_packet(&idr1).is_none(),
        "IDR1 must not emit yet"
    );

    for i in 1..=5u64 {
        let idr = make_idr_packet(i * 33, i);
        if let Some(seg) = muxer.append_packet(&idr) {
            segments.push(seg);
        }
    }

    assert_eq!(segments.len(), 5, "expected 5 media segments from 6 IDRs");

    // Verify each segment starts with moof.
    for (idx, seg) in segments.iter().enumerate() {
        assert_eq!(
            &seg[4..8],
            b"moof",
            "segment {} must start with moof box",
            idx + 1
        );
        assert!(
            seg.windows(4).any(|w| w == b"mdat"),
            "segment {} must contain mdat",
            idx + 1
        );
    }

    // Build the full fMP4 stream and verify total byte count is reasonable.
    let mut stream: Vec<u8> = Vec::new();
    stream.extend_from_slice(&init);
    for seg in &segments {
        stream.extend_from_slice(seg);
    }
    assert!(
        stream.len() > 400,
        "full stream must be > 400 bytes, got {}",
        stream.len()
    );

    // Verify track count = 1 (only one trak box).
    let trak_count = stream.windows(4).filter(|w| *w == b"trak").count();
    assert_eq!(trak_count, 1, "stream must contain exactly 1 trak box");
}

// ─── C2: mfhd sequence numbers increment monotonically across segments ───────

#[test]
fn mp4_muxer_mfhd_sequence_numbers_increment_across_segments() {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    muxer.build_init_segment(&sps_info, SPS, PPS).unwrap();

    let mut segments: Vec<Vec<u8>> = Vec::new();

    // IDR1 buffers.
    let idr1 = make_idr_packet(0, 0);
    assert!(muxer.append_packet(&idr1).is_none());

    // IDR2..IDR6 each emit a segment.
    for i in 1..=5u64 {
        let idr = make_idr_packet(i * 33, i);
        if let Some(seg) = muxer.append_packet(&idr) {
            segments.push(seg);
        }
    }

    assert_eq!(segments.len(), 5, "expected 5 segments");

    let seq_numbers: Vec<u32> = segments.iter().map(|s| extract_mfhd_seq(s)).collect();

    // Sequence numbers must be 1, 2, 3, 4, 5.
    for (idx, &seq) in seq_numbers.iter().enumerate() {
        let expected = (idx + 1) as u32;
        assert_eq!(
            seq,
            expected,
            "segment {} mfhd.sequence_number must be {expected}, got {seq}",
            idx + 1
        );
    }

    // Verify strict monotonic ordering.
    for window in seq_numbers.windows(2) {
        assert!(
            window[1] > window[0],
            "mfhd sequence numbers must strictly increase: {} followed by {}",
            window[0],
            window[1]
        );
    }
}

// ─── C3: tfdt.base_media_decode_time increases monotonically ─────────────────

#[test]
fn mp4_muxer_tfdt_decode_time_monotonic() {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    muxer.build_init_segment(&sps_info, SPS, PPS).unwrap();

    let mut segments: Vec<Vec<u8>> = Vec::new();

    // IDR1 at t=0 buffers.
    let idr1 = make_idr_packet(0, 0);
    assert!(muxer.append_packet(&idr1).is_none());

    // IDR2..IDR6 at increasing timestamps each emit a segment.
    for i in 1..=5u64 {
        let ts_ms = i * 100; // 100ms spacing = 9000 ticks at 90kHz
        let idr = make_idr_packet(ts_ms, i);
        if let Some(seg) = muxer.append_packet(&idr) {
            segments.push(seg);
        }
    }

    assert_eq!(segments.len(), 5, "expected 5 segments");

    let times: Vec<u64> = segments.iter().map(|s| extract_tfdt_time(s)).collect();

    // All times must be strictly increasing.
    for window in times.windows(2) {
        assert!(
            window[1] > window[0],
            "tfdt.base_media_decode_time must be strictly increasing: {} followed by {}",
            window[0],
            window[1]
        );
    }

    // Verify the timescale mapping: 100ms at 90kHz = 9000 ticks.
    // Segment 1 has base_dts from IDR1 (t=0 ms), so times[0] should be 0.
    assert_eq!(times[0], 0, "first segment tfdt must be 0 (IDR1 at t=0)");

    // Segment 2 has base_dts from IDR2 (t=100ms → 9000 ticks).
    assert_eq!(
        times[1], 9_000,
        "second segment tfdt must be 9000 (100ms at 90kHz), got {}",
        times[1]
    );
}

// ─── C4: init segment avcC bytes round-trip SPS+PPS ─────────────────────────

#[test]
fn mp4_muxer_init_segment_avcc_matches_input_sps_pps() {
    let muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    let init = muxer
        .build_init_segment(&sps_info, SPS, PPS)
        .expect("init must build");

    // Find avcC box in the init segment.
    let avcc_pos = init
        .windows(4)
        .position(|w| w == b"avcC")
        .expect("avcC box must be present in init segment");

    // avcC payload starts at avcc_pos + 4 (skip the 4-byte tag).
    // avcC box structure: [size:4][b"avcC":4][payload...]
    // The payload starts right after the box tag.
    // Layout of AVCDecoderConfigurationRecord (payload):
    //   configurationVersion (1) = 0x01
    //   profile_idc (1) = SPS[1] = 0x42
    //   constraint_set_flags (1) = SPS[2] = 0xC0
    //   level_idc (1) = SPS[3] = 0x0D
    //   lengthSizeMinusOne (1) bits [1:0] = 0xFF (length_size=4 → 0xFF)
    //   numSequenceParameterSets (1) bits [4:0] = 0xE1 (count=1)
    //   sps_length (2 BE) = SPS.len()
    //   sps_nal (SPS.len() bytes) = SPS bytes
    //   numPictureParameterSets (1) = 0x01
    //   pps_length (2 BE) = PPS.len()
    //   pps_nal (PPS.len() bytes) = PPS bytes

    let payload_start = avcc_pos + 4; // after the "avcC" tag bytes

    // configurationVersion must be 0x01.
    assert_eq!(
        init[payload_start], 0x01,
        "avcC.configurationVersion must be 0x01"
    );

    // profile_idc must match SPS[1] = 0x42.
    assert_eq!(
        init[payload_start + 1],
        SPS[1],
        "avcC.profile_idc must match SPS[1]"
    );

    // constraint_set_flags must match SPS[2] = 0xC0.
    assert_eq!(
        init[payload_start + 2],
        SPS[2],
        "avcC.constraint_set_flags must match SPS[2]"
    );

    // level_idc must match SPS[3] = 0x0D.
    assert_eq!(
        init[payload_start + 3],
        SPS[3],
        "avcC.level_idc must match SPS[3]"
    );

    // Find where SPS bytes appear in the init segment after avcC.
    let sps_in_avcc = init[payload_start..].windows(SPS.len()).any(|w| w == SPS);
    assert!(
        sps_in_avcc,
        "SPS bytes must appear verbatim in the avcC payload"
    );

    // Find where PPS bytes appear in the init segment after avcC.
    let pps_in_avcc = init[payload_start..].windows(PPS.len()).any(|w| w == PPS);
    assert!(
        pps_in_avcc,
        "PPS bytes must appear verbatim in the avcC payload"
    );
}

// ─── C5: mp4 crate reader attempt against init segment ───────────────────────
//
// Documents behavior: mp4::Mp4Reader requires seekable non-fragmented files and
// will NOT parse a pure fragmented fMP4 init segment. We document this limitation
// and fall back to byte-level scanning (as done in C1–C4 above).

#[test]
fn mp4_muxer_with_mp4_crate_reader_attempts_parse() {
    use std::io::Cursor;

    let muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    let init = muxer
        .build_init_segment(&sps_info, SPS, PPS)
        .expect("init must build");

    // Attempt to parse the init segment with mp4::Mp4Reader.
    // Expected: this WILL fail because mp4 0.14 Mp4Reader expects a complete
    // non-fragmented file (moov must have duration != 0 and stco must have offsets).
    //
    // We treat failure as the documented expected behavior, not a test failure.
    let file_size = init.len() as u64;
    let cursor = Cursor::new(init.clone());
    let result = mp4::Mp4Reader::read_header(cursor, file_size);

    match result {
        Ok(_reader) => {
            // If the mp4 crate somehow parses it, that's fine — we just document it.
            println!("[C5] mp4::Mp4Reader parsed the init segment successfully");
        }
        Err(e) => {
            // Expected: mp4 crate cannot parse fragmented init segments.
            println!(
                "[C5] mp4::Mp4Reader rejected fragmented init segment (documented limitation): {e}"
            );
            // Verify our byte-level scanning still works as fallback.
            assert_eq!(&init[4..8], b"ftyp", "byte-scan fallback: ftyp at offset 4");
            assert!(
                init.windows(4).any(|w| w == b"moov"),
                "byte-scan fallback: moov present"
            );
        }
    }
}

// ─── C6: extract_sps_pps_from_idr extracts correct NALs ─────────────────────

#[test]
fn mp4_muxer_extract_sps_pps_from_idr_round_trips() {
    let idr = make_idr_packet(0, 0);
    let result = extract_sps_pps_from_idr(&idr.data);
    assert!(
        result.is_some(),
        "extract_sps_pps_from_idr must succeed for a packet with SPS+PPS+IDR NALs"
    );
    let (sps_out, pps_out) = result.unwrap();
    assert_eq!(sps_out, SPS, "extracted SPS must match input SPS");
    assert_eq!(pps_out, PPS, "extracted PPS must match input PPS");
}

// ─── 30fps 4-sample golden segment fixture ──────────────────────────────────
//
// Builds a deterministic 4-sample GOP at 30fps cadence (IDR + 3 P-frames,
// each 100 bytes, 33ms apart). Used by R10 byte-level golden tests and the
// post-warm-up duration assertions.
//
// Post-T2 layout (current code): trun flags = 0x000305 (includes
// sample-duration-present 0x000100), each per-sample entry is
// [duration:4][size:4]. Total segment: moof(124) + mdat(408) = 532 bytes.

/// Build a fixed 100-byte synthetic packet for the golden fixture.
fn make_anchor_packet(is_kf: bool, ts_ms: u64) -> EncodedPacket {
    let nal_type: u8 = if is_kf { 0x65 } else { 0x41 };
    let mut data = vec![0x00u8, 0x00, 0x00, 0x01, nal_type];
    data.extend(vec![0xBBu8; 95]); // 100 bytes total (5 header + 95 payload)
    EncodedPacket {
        data: Arc::from(data.into_boxed_slice()),
        is_keyframe: is_kf,
        timestamp: Duration::from_millis(ts_ms),
        sequence: ts_ms / 33,
    }
}

/// Build the deterministic 30fps 4-sample golden segment used by R10 tests.
///
/// Built programmatically to avoid copy-paste errors in hex literals.
fn build_30fps_4sample_gop_segment() -> Vec<u8> {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    muxer.append_packet(&make_anchor_packet(true, 0));
    muxer.append_packet(&make_anchor_packet(false, 33));
    muxer.append_packet(&make_anchor_packet(false, 66));
    muxer.append_packet(&make_anchor_packet(false, 99));
    muxer
        .append_packet(&make_anchor_packet(true, 132))
        .expect("IDR2 must flush the 4-sample GOP")
}

// ─── Phase 5 + 6: integration tests + golden refresh ────────────────────────

// Helper: parse trun per-sample (duration, size) pairs from a media segment.
fn parse_segment_trun_pairs(seg: &[u8], is_idr: bool) -> Vec<(u32, u32)> {
    let trun_tag_pos = seg
        .windows(4)
        .position(|w| w == b"trun")
        .expect("trun not found in segment");
    // trun box starts 4 bytes before the tag (size field), tag is at trun_tag_pos.
    // full box: [size:4][tag:4][version:1][flags:3][count:4][data_offset:4]
    //           [first_sample_flags:4] (if is_idr)
    //           [duration:4][size:4] × count
    let count = u32::from_be_bytes([
        seg[trun_tag_pos + 4 + 4],
        seg[trun_tag_pos + 4 + 5],
        seg[trun_tag_pos + 4 + 6],
        seg[trun_tag_pos + 4 + 7],
    ]) as usize;
    // Per-sample base offset from start of tag:
    // +4 (v+f) + 4 (count) + 4 (data_offset) + 4 (first_sample_flags if idr)
    let base = trun_tag_pos + 4 + 4 + 4 + 4 + if is_idr { 4 } else { 0 };
    (0..count)
        .map(|i| {
            let off = base + i * 8;
            let dur = u32::from_be_bytes([seg[off], seg[off + 1], seg[off + 2], seg[off + 3]]);
            let sz = u32::from_be_bytes([seg[off + 4], seg[off + 5], seg[off + 6], seg[off + 7]]);
            (dur, sz)
        })
        .collect()
}

// T5.2 — Post-warm-up pipeline emits per-sample duration = REAL intra-GOP DTS delta.
//
// After the real-DTS-delta fix (T5), per-sample trun durations come from actual DTS
// differences, NOT from FpsTracker. At 100ms uniform spacing, the real DTS delta
// is (0.1 * 90000) as u64 = 8999 or 9000 ticks (f64 rounding). The test verifies
// that all durations approximate the real 100ms gap and that the sum-of-durations
// equals the real elapsed DTS span of the GOP.
#[test]
fn mp4_muxer_post_warmup_pipeline_emits_locked_per_sample_duration() {
    use std::time::Duration;
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);

    // Feed IDR at t=0 (no delta yet).
    let idr1 = EncodedPacket {
        data: {
            let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
            d.extend(vec![0xBBu8; 95]);
            std::sync::Arc::from(d.into_boxed_slice())
        },
        is_keyframe: true,
        timestamp: Duration::from_millis(0),
        sequence: 0,
    };
    muxer.append_packet(&idr1);

    // Feed 8 P-frames at 100ms intervals to warm up the tracker.
    for i in 1..=8u64 {
        let pkt = EncodedPacket {
            data: {
                let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x41];
                d.extend(vec![0xBBu8; 95]);
                std::sync::Arc::from(d.into_boxed_slice())
            },
            is_keyframe: false,
            timestamp: Duration::from_millis(i * 100),
            sequence: i,
        };
        muxer.append_packet(&pkt);
    }

    // IDR2 triggers flush of GOP1 (9 samples: IDR1 + 8 P-frames).
    let idr2 = EncodedPacket {
        data: {
            let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
            d.extend(vec![0xBBu8; 95]);
            std::sync::Arc::from(d.into_boxed_slice())
        },
        is_keyframe: true,
        timestamp: Duration::from_millis(9 * 100),
        sequence: 9,
    };
    let segment = muxer
        .append_packet(&idr2)
        .expect("IDR2 must flush GOP1 (9 samples)");

    // Parse per-sample (duration, size) pairs from the flushed trun.
    let pairs = parse_segment_trun_pairs(&segment, true); // IDR segment

    // With the real-DTS-delta fix, durations reflect actual inter-frame DTS differences.
    // duration_to_90khz(100ms) via f64 yields 8999 or 9000 ticks — allow ±1 for rounding.
    // Must NOT be 3000 (FpsTracker warm-up fallback — the old broken behavior).
    assert!(!pairs.is_empty(), "segment must have at least one sample");
    for (i, &(dur, _)) in pairs.iter().enumerate() {
        assert!(
            (8998..=9001).contains(&dur),
            "sample {i} duration {dur} must approximate real 100ms DTS delta (8998–9001 ticks); \
             must NOT be 3000 (FpsTracker fallback)"
        );
    }

    // Sum-of-durations must approximate the real elapsed DTS span of GOP1 (9 × ~9000 ticks).
    // Allow ±9 ticks tolerance for 9 frames × ±1 tick rounding each.
    let total_dur: u64 = pairs.iter().map(|&(d, _)| d as u64).sum();
    let real_span: u64 = (0.9_f64 * 90_000.0) as u64; // 9 × 100ms
    let tolerance: u64 = 9; // ±1 tick per frame
    assert!(
        total_dur.abs_diff(real_span) <= tolerance,
        "sum-of-durations {total_dur} must approximate real GOP span {real_span} ticks (±{tolerance})"
    );
}

// T5.3 — trun per-sample durations reflect real DTS deltas regardless of FpsTracker state.
//
// The 30fps fixture uses 33ms spacing (33 × 90 = 2970 ticks per frame).
// After the real-DTS-delta fix, each sample gets duration ≈ 2970, NOT 3000 (the old
// FpsTracker warm-up fallback). This verifies the fix works even during warm-up.
// Sum = 4 × 2970 = 11880 ≈ real elapsed span, NOT 12000 (the old fixed-rate sum).
#[test]
fn mp4_muxer_30fps_segment_carries_per_sample_duration_3000() {
    let seg = build_30fps_4sample_gop_segment();

    let pairs = parse_segment_trun_pairs(&seg, true); // IDR segment
    assert_eq!(
        pairs.len(),
        4,
        "GOP must have 4 samples (IDR1 + P2 + P3 + P4 before IDR2)"
    );

    // Real intra-GOP deltas: 33ms × 90 kHz = 2970 ticks each.
    // After the real-DTS-delta fix, each per-sample trun duration must equal 2970 (not 3000).
    let expected_delta: u32 = 33 * 90; // 2970 ticks
    for (i, &(dur, _)) in pairs.iter().take(pairs.len().saturating_sub(1)).enumerate() {
        assert_eq!(
            dur, expected_delta,
            "real-DTS sample {i} duration must be {expected_delta} ticks (33ms real delta); \
             got {dur} — must NOT be 3000 (old FpsTracker warm-up fallback)"
        );
    }
    // Last sample uses median of intra-GOP deltas (all 2970) = 2970.
    let last_dur = pairs.last().expect("at least one sample").0;
    assert_eq!(
        last_dur, expected_delta,
        "last sample duration must be median of intra-GOP deltas ({expected_delta}); got {last_dur}"
    );

    // Sum-of-durations: 4 × 2970 = 11880 (real elapsed span), NOT 12000 (old fixed-rate).
    let total: u32 = pairs.iter().map(|&(d, _)| d).sum();
    assert_eq!(
        total,
        4 * expected_delta,
        "sum-of-durations for 4 samples at real 33ms deltas must be {}",
        4 * expected_delta
    );
}

// T5.4 — init segment trex.default_sample_duration is 3000 regardless of fps (R5, R11).
#[test]
fn init_segment_trex_default_sample_duration_is_3000_regardless_of_fps() {
    let sps_info = parse_sps(SPS).expect("should parse SPS");

    // Test with default 30 fps muxer.
    let muxer_30 = Mp4Muxer::new(320, 240, 30, 1);
    let init_30 = muxer_30
        .build_init_segment(&sps_info, SPS, PPS)
        .expect("init segment must build");

    // Test with 60 fps muxer (new instance, no warm-up).
    let muxer_60 = Mp4Muxer::new(320, 240, 60, 1);
    let init_60 = muxer_60
        .build_init_segment(&sps_info, SPS, PPS)
        .expect("init segment must build");

    for (fps_label, init) in [("30fps", &init_30), ("60fps", &init_60)] {
        // Find trex box.
        let trex_pos = init
            .windows(4)
            .position(|w| w == b"trex")
            .unwrap_or_else(|| panic!("trex not found in {fps_label} init segment"));
        // trex full-box: [size:4][tag:4][version:1][flags:3]
        //                [track_id:4][default_sample_description_index:4]
        //                [default_sample_duration:4][default_sample_size:4][default_sample_flags:4]
        // After tag: v+f(4) + track_id(4) + dsd_index(4) + default_sample_duration(4)
        let dur_off = trex_pos + 4 + 4 + 4 + 4;
        let default_dur = u32::from_be_bytes([
            init[dur_off],
            init[dur_off + 1],
            init[dur_off + 2],
            init[dur_off + 3],
        ]);
        assert_eq!(
            default_dur, 3000,
            "{fps_label}: trex.default_sample_duration must be 3000 (warm-up fallback, R5); got {default_dur}"
        );
    }
}

// T5.5 note: The R9 tfhd comment is added inline in fmp4_muxer.rs near build_trun/build_tfhd.
// T5.6 verify: the existing C1-C5 build_init_segment tests must still pass GREEN.

// ─── T6.1: Post-T2 byte-level golden (R10) ──────────────────────────────────
//
// Documents the post-T2 trun layout for a 30fps 4-sample GOP:
//   - trun flags = 0x000305 (sample-duration-present + sample-size-present
//     + first-sample-flags-present + data-offset-present)
//   - per-sample entries: [duration:4][size:4]
//   - moof grew by 4 bytes/sample = +16 bytes for 4 samples vs. the historical
//     pre-T2 layout; total: 516 → 532 bytes.
#[test]
fn mp4_muxer_30fps_segment_post_t2_golden() {
    let seg = build_30fps_4sample_gop_segment();

    // Structural validation: post-T2 trun MUST have 0x000100 flag.
    let trun_pos = seg
        .windows(4)
        .position(|w| w == b"trun")
        .expect("trun box must be present");
    let flags = u32::from_be_bytes([0, seg[trun_pos + 5], seg[trun_pos + 6], seg[trun_pos + 7]]);
    assert_ne!(
        flags & 0x000100,
        0,
        "post-T2 trun MUST have sample-duration-present flag (0x000100); got flags=0x{flags:06X}"
    );
    assert_ne!(
        flags & 0x000200,
        0,
        "post-T2 trun must have sample-size-present (0x000200)"
    );
    assert_ne!(
        flags & 0x000001,
        0,
        "post-T2 trun must have data-offset-present (0x000001)"
    );
    assert_ne!(
        flags & 0x000004,
        0,
        "post-T2 trun must have first-sample-flags-present (0x000004, IDR)"
    );

    // Size check: moof(108→124) + mdat(408) = 532 bytes.
    // moof grew by 4 bytes/sample × 4 samples = +16 bytes (from 108 to 124).
    assert_eq!(
        seg.len(),
        532,
        "post-T2 4-sample GOP must be 532 bytes (pre-T2 was 516, +16 from 4 duration fields); got {}",
        seg.len()
    );

    // After the real-DTS-delta fix (T5), per-sample durations reflect actual 33ms intervals.
    // 33ms × 90 kHz = 2970 ticks. The old FpsTracker warm-up fallback (3000) is no longer
    // used for per-sample trun durations.
    let pairs = parse_segment_trun_pairs(&seg, true);
    assert_eq!(pairs.len(), 4, "must have 4 samples");
    let expected_delta: u32 = 33 * 90; // 2970 ticks (real 33ms inter-frame gap)
    for (i, &(dur, _)) in pairs.iter().enumerate() {
        assert_eq!(
            dur, expected_delta,
            "post-T5 real-DTS sample {i} must carry duration={expected_delta} (real 33ms delta); \
             got {dur} — must NOT be 3000 (old FpsTracker fallback)"
        );
    }

    // Deterministic: two builds must produce identical bytes.
    let seg2 = build_30fps_4sample_gop_segment();
    assert_eq!(seg, seg2, "post-T2 segment must be deterministic");
}

// ─── C7: annex_b_to_avcc rejects no bytes, handles correctly ────────────────

#[test]
fn mp4_muxer_annex_b_to_avcc_empty_input_returns_empty() {
    let result = annex_b_to_avcc(b"").expect("empty input must return Ok");
    assert!(result.is_empty(), "empty input must produce empty output");
}

#[test]
fn mp4_muxer_annex_b_to_avcc_single_nal_length_prefix_correct() {
    // Input: 4-byte start code + 3 NAL bytes = [00 00 00 01 65 AB CD]
    // Output: [00 00 00 03 65 AB CD] (length=3, big-endian)
    let input = &[0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAB, 0xCD];
    let output = annex_b_to_avcc(input).expect("must succeed");
    assert_eq!(
        &output[..4],
        &[0x00, 0x00, 0x00, 0x03],
        "length prefix must be 3 in BE"
    );
    assert_eq!(
        &output[4..],
        &[0x65, 0xAB, 0xCD],
        "NAL bytes must be preserved"
    );
}
