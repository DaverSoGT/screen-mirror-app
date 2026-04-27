//! fMP4 muxer for live screen-mirror streaming.
//!
//! Builds an init segment (`ftyp` + `moov`) once per stream from the first SPS + PPS,
//! then emits one media segment (`moof` + `mdat`) per IDR-aligned fragment.
//! All output is byte-for-byte ISO/IEC 14496-12 (base file format) +
//! ISO/IEC 14496-15 (NAL file format) compliant.
//!
//! # OQ-mux-1 resolution
//!
//! Option (c) — manual mini-muxer (~300 LOC). We own the byte layout, no external
//! runtime dep, best fit for live MSE streaming. The `mp4` crate is NOT a runtime
//! dependency.

// ─── ISO/IEC 14496-12 box framing primitive ──────────────────────────────────

/// Write a single ISO base-media file format box into `out`.
///
/// Layout: `[u32_BE total_size][4 ASCII type bytes][payload bytes]`.
/// The `total_size` field is `payload.len() + 8` (4 bytes for size + 4 bytes for type).
pub(crate) fn write_box(out: &mut Vec<u8>, box_type: &[u8; 4], payload: &[u8]) {
    let size = (payload.len() + 8) as u32;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(box_type);
    out.extend_from_slice(payload);
}

// ─── ftyp box builder ────────────────────────────────────────────────────────

/// Build the `ftyp` box for a live fMP4 stream.
///
/// - major_brand: `iso5`, minor_version: `0x00000200`
/// - compatible_brands: `[iso5, avc1, iso6, mp42]`
pub(crate) fn build_ftyp() -> Vec<u8> {
    let mut payload = Vec::with_capacity(24);
    payload.extend_from_slice(b"iso5");
    payload.extend_from_slice(&0x0000_0200u32.to_be_bytes());
    payload.extend_from_slice(b"iso5");
    payload.extend_from_slice(b"avc1");
    payload.extend_from_slice(b"iso6");
    payload.extend_from_slice(b"mp42");

    let mut out = Vec::with_capacity(32);
    write_box(&mut out, b"ftyp", &payload);
    out
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::avcc::parse_sps;

    /// SPS: Baseline Level 1.3, 320×240 progressive.
    const SPS_320X240: &[u8] = &[0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0xC0];
    /// Minimal PPS for unit tests.
    const MINIMAL_PPS: &[u8] = &[0x68, 0xCE, 0x38, 0x80];

    // ─── Capability A ───────────────────────────────────────────────────────

    #[test]
    fn write_box_golden_bytes_with_payload() {
        let mut out = Vec::new();
        write_box(&mut out, b"test", &[0x01, 0x02, 0x03]);
        assert_eq!(
            out,
            &[0, 0, 0, 11, b't', b'e', b's', b't', 0x01, 0x02, 0x03]
        );
    }

    #[test]
    fn write_box_empty_payload_produces_8_bytes() {
        let mut out = Vec::new();
        write_box(&mut out, b"empt", &[]);
        assert_eq!(out, &[0, 0, 0, 8, b'e', b'm', b'p', b't']);
    }

    #[test]
    fn write_box_large_payload_correct_size_field() {
        let payload = vec![0xABu8; 70_000];
        let mut out = Vec::new();
        write_box(&mut out, b"lrge", &payload);
        let total = 70_000usize + 8;
        let size_field = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(size_field, total);
        assert_eq!(&out[4..8], b"lrge");
        assert_eq!(out.len(), total);
    }

    #[test]
    fn write_box_type_bytes_are_exact_4_ascii() {
        let mut out = Vec::new();
        write_box(&mut out, b"moov", &[0x01]);
        assert_eq!(&out[4..8], b"moov");
    }

    // ─── Capability B ───────────────────────────────────────────────────────

    #[test]
    fn build_ftyp_starts_with_size_then_ftyp_tag() {
        let ftyp = build_ftyp();
        let size = u32::from_be_bytes([ftyp[0], ftyp[1], ftyp[2], ftyp[3]]) as usize;
        assert_eq!(size, ftyp.len());
        assert_eq!(&ftyp[4..8], b"ftyp");
    }

    #[test]
    fn build_ftyp_major_brand_is_iso5() {
        let ftyp = build_ftyp();
        assert_eq!(&ftyp[8..12], b"iso5");
    }

    #[test]
    fn build_ftyp_minor_version_is_0x00000200() {
        let ftyp = build_ftyp();
        let minor = u32::from_be_bytes([ftyp[12], ftyp[13], ftyp[14], ftyp[15]]);
        assert_eq!(minor, 0x0000_0200);
    }

    #[test]
    fn build_ftyp_compatible_brands_include_avc1_and_mp42() {
        let ftyp = build_ftyp();
        let brands = &ftyp[16..];
        let brand_strings: Vec<&[u8]> = brands.chunks(4).collect();
        assert!(brand_strings.contains(&&b"avc1"[..]));
        assert!(brand_strings.contains(&&b"mp42"[..]));
    }

    #[test]
    fn build_ftyp_is_deterministic() {
        assert_eq!(build_ftyp(), build_ftyp());
    }

    // ─── Capability C: moov hierarchy builder ──────────────────────────────

    /// Scan `bytes` for a 4-byte ASCII tag. Returns first offset where tag appears.
    fn find_box_tag(bytes: &[u8], tag: &[u8; 4]) -> Option<usize> {
        bytes.windows(4).position(|w| w == &tag[..])
    }

    #[test]
    fn build_moov_contains_all_required_box_tags() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse 320x240 SPS");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("build_moov should succeed");

        let required_tags: &[&[u8; 4]] = &[
            b"moov", b"mvhd", b"trak", b"tkhd", b"mdia", b"mdhd", b"hdlr",
            b"minf", b"vmhd", b"dinf", b"dref", b"url ", b"stbl", b"stsd",
            b"avc1", b"avcC", b"stts", b"stsc", b"stsz", b"stco",
        ];

        for tag in required_tags {
            assert!(
                find_box_tag(&moov, tag).is_some(),
                "moov must contain box tag {:?}",
                std::str::from_utf8(*tag).unwrap_or("<non-utf8>")
            );
        }
    }

    #[test]
    fn build_moov_avcc_bytes_match_build_avcc_output() {
        use crate::render::avcc::build_avcc;
        let sps_info = parse_sps(SPS_320X240).expect("should parse 320x240 SPS");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("build_moov should succeed");

        let avcc_tag_pos = find_box_tag(&moov, b"avcC").expect("avcC must be in moov");
        // The box size field is 4 bytes before the tag.
        let avcc_box_start = avcc_tag_pos - 4;
        let avcc_size = u32::from_be_bytes([
            moov[avcc_box_start],
            moov[avcc_box_start + 1],
            moov[avcc_box_start + 2],
            moov[avcc_box_start + 3],
        ]) as usize;
        let payload_start = avcc_tag_pos + 4;
        let payload_end = avcc_box_start + avcc_size;
        let embedded_avcc = &moov[payload_start..payload_end];

        let expected = build_avcc(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("build_avcc should succeed");

        assert_eq!(embedded_avcc, expected.as_slice());
    }

    #[test]
    fn build_moov_mvhd_timescale_is_90000() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("build_moov should succeed");

        let mvhd_tag_pos = find_box_tag(&moov, b"mvhd").expect("mvhd must exist");
        // mvhd box: [4 size][4 tag][4 v+f][4 ctime][4 mtime][4 timescale]
        // tag is at mvhd_tag_pos; after tag (4 bytes) comes v+f (4), ctime (4), mtime (4), then ts.
        let ts_offset = mvhd_tag_pos + 4 + 4 + 4 + 4;
        let timescale = u32::from_be_bytes([
            moov[ts_offset],
            moov[ts_offset + 1],
            moov[ts_offset + 2],
            moov[ts_offset + 3],
        ]);
        assert_eq!(timescale, 90_000, "mvhd.timescale must be 90_000");
    }

    #[test]
    fn build_moov_hdlr_handler_type_is_vide() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("build_moov should succeed");

        let hdlr_tag_pos = find_box_tag(&moov, b"hdlr").expect("hdlr must exist");
        // hdlr payload: [4 v+f][4 pre_defined][4 handler_type]
        let handler_type_offset = hdlr_tag_pos + 4 + 4 + 4;
        assert_eq!(
            &moov[handler_type_offset..handler_type_offset + 4],
            b"vide",
            "hdlr.handler_type must be 'vide'"
        );
    }
}
