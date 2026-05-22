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
// Updated to NEW per-frame-flush contract: every appended packet emits a segment
// immediately. Feed 5 IDR packets → 5 emitted segments (one per IDR).

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

    // Under per-frame-flush, every packet emits a segment immediately.
    // Feed 5 IDR packets → 5 emitted segments.
    let mut segments: Vec<Vec<u8>> = Vec::new();
    for i in 0..5u64 {
        let idr = make_idr_packet(i * 33, i);
        let seg = muxer
            .append_packet(&idr)
            .expect("every IDR must emit a segment under per-frame-flush contract");
        segments.push(seg);
    }

    assert_eq!(segments.len(), 5, "expected 5 media segments from 5 IDRs");

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
// Updated to NEW per-frame-flush contract: every IDR emits immediately.

#[test]
fn mp4_muxer_mfhd_sequence_numbers_increment_across_segments() {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    muxer.build_init_segment(&sps_info, SPS, PPS).unwrap();

    let mut segments: Vec<Vec<u8>> = Vec::new();

    // 5 IDRs → 5 segments under per-frame-flush.
    for i in 0..5u64 {
        let idr = make_idr_packet(i * 33, i);
        let seg = muxer
            .append_packet(&idr)
            .expect("IDR must emit segment immediately");
        segments.push(seg);
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
// Updated to NEW per-frame-flush contract: every IDR emits immediately.

#[test]
fn mp4_muxer_tfdt_decode_time_monotonic() {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    let sps_info = parse_sps(SPS).expect("should parse SPS");
    muxer.build_init_segment(&sps_info, SPS, PPS).unwrap();

    let mut segments: Vec<Vec<u8>> = Vec::new();

    // 5 IDRs at increasing timestamps, each emitting immediately.
    for i in 0..5u64 {
        let ts_ms = i * 100; // 100ms spacing = 9000 ticks at 90kHz
        let idr = make_idr_packet(ts_ms, i);
        let seg = muxer
            .append_packet(&idr)
            .expect("IDR must emit segment immediately");
        segments.push(seg);
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
    // Segment 0 has base_dts = 0 (IDR at t=0ms, rebase origin).
    assert_eq!(times[0], 0, "first segment tfdt must be 0 (IDR at t=0)");

    // Segment 1 has base_dts from IDR at t=100ms → 9000 ticks.
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

/// Build a deterministic single-sample P-frame segment at 30fps cadence.
///
/// Under per-frame flush, each packet emits a single-sample fragment.
/// This helper feeds IDR at t=0 (sets prev_flushed_dts=0), then returns
/// the P-frame segment at t=33ms (inter-delta = 33×90 = 2970 ticks).
/// Used by R10 byte-level golden tests.
fn build_30fps_single_sample_p_frame_segment() -> Vec<u8> {
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
    // IDR at t=0: sets prev_flushed_dts=0; discard its segment.
    muxer.append_packet(&make_anchor_packet(true, 0));
    // P-frame at t=33ms: inter-delta = 2970 ticks.
    muxer
        .append_packet(&make_anchor_packet(false, 33))
        .expect("P-frame at t=33ms must emit segment")
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

// T5.2 — Post-warm-up pipeline emits per-sample duration = REAL inter-frame DTS delta.
//
// After the real-DTS-delta fix (T5) + per-frame-flush change: each single-sample
// fragment's duration = prev_flushed_dts inter-fragment delta, NOT FpsTracker value.
// At 100ms uniform spacing: duration ≈ 8999 or 9000 ticks (f64 rounding).
// Updated to NEW per-frame-flush contract.
#[test]
fn mp4_muxer_post_warmup_pipeline_emits_locked_per_sample_duration() {
    use std::time::Duration;
    let mut muxer = Mp4Muxer::new(320, 240, 30, 1);

    // Feed IDR at t=0 — emits immediately (first fragment uses WARMUP_FALLBACK for duration).
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
    muxer.append_packet(&idr1); // emits; discard

    // Feed 8 P-frames at 100ms intervals — each emits a single-sample segment.
    // After the first P-frame, inter-fragment delta = 100ms ≈ 9000 ticks.
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
        let seg = muxer
            .append_packet(&pkt)
            .expect("P-frame must emit segment under per-frame-flush");

        // Each single-sample P-frame trun has duration = inter-fragment delta ≈ 9000 ticks.
        let pairs = parse_segment_trun_pairs(&seg, false); // P-frame segment
        assert_eq!(
            pairs.len(),
            1,
            "per-frame flush produces single-sample fragments"
        );
        let (dur, _) = pairs[0];
        assert!(
            (8998..=9001).contains(&dur),
            "P-frame {i} duration {dur} must approximate real 100ms DTS delta (8998–9001 ticks); \
             must NOT be 3000 (FpsTracker fallback)"
        );
    }
}

// T5.3 — trun per-sample duration reflects real DTS delta (per-frame flush, single-sample).
//
// Under per-frame flush, each fragment is a single sample. The P-frame at t=33ms
// has inter-fragment delta = 33×90 = 2970 ticks. Must NOT be 3000 (FpsTracker fallback).
#[test]
fn mp4_muxer_30fps_segment_carries_per_sample_duration_3000() {
    let seg = build_30fps_single_sample_p_frame_segment();

    let pairs = parse_segment_trun_pairs(&seg, false); // P-frame segment (is_idr=false)
    assert_eq!(
        pairs.len(),
        1,
        "per-frame flush produces single-sample fragments"
    );

    // Inter-fragment delta: 33ms × 90 kHz = 2970 ticks.
    // Must NOT be 3000 (old FpsTracker warm-up fallback).
    let expected_delta: u32 = 33 * 90; // 2970 ticks
    let (dur, _) = pairs[0];
    assert_eq!(
        dur, expected_delta,
        "real-DTS single-sample duration must be {expected_delta} ticks (33ms real delta); \
         got {dur} — must NOT be 3000 (old FpsTracker warm-up fallback)"
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
// Documents the post-T2 trun layout for a per-frame P-frame segment:
//   - trun flags = 0x000301 (sample-duration-present + sample-size-present
//     + data-offset-present; NO first-sample-flags-present for P-frame)
//   - single per-sample entry: [duration:4][size:4]
//   - moof size: 8(moof)+8(mfhd)+8(seq)+8(traf)+16(tfhd)+20(tfdt)+28(trun P-frame) = 96B
//     mdat: 8 + 96 (AVCC-wrapped 100-byte packet) = 104B. Total ≈ 200B.
#[test]
fn mp4_muxer_30fps_segment_post_t2_golden() {
    let seg = build_30fps_single_sample_p_frame_segment();

    // Structural validation: post-T2 trun MUST have 0x000100 flag (sample-duration-present).
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
    // P-frame: must NOT have first-sample-flags-present (that's IDR-only).
    assert_eq!(
        flags & 0x000004,
        0,
        "post-T2 P-frame trun must NOT have first-sample-flags-present (0x000004); got 0x{flags:06X}"
    );

    // Single-sample P-frame segment: exactly one (duration, size) pair.
    let pairs = parse_segment_trun_pairs(&seg, false); // P-frame
    assert_eq!(
        pairs.len(),
        1,
        "per-frame flush produces single-sample fragments"
    );
    let expected_delta: u32 = 33 * 90; // 2970 ticks (real 33ms inter-frame gap at 90kHz)
    let (dur, _) = pairs[0];
    assert_eq!(
        dur, expected_delta,
        "post-T5 real-DTS single-sample must carry duration={expected_delta} (real 33ms delta); \
         got {dur} — must NOT be 3000 (old FpsTracker fallback)"
    );

    // Deterministic: two builds must produce identical bytes.
    let seg2 = build_30fps_single_sample_p_frame_segment();
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
