//! SPS NAL parser and `AVCDecoderConfigurationRecord` builder.
//!
//! # Responsibilities
//!
//! 1. **SPS parser** (`parse_sps`): reads the H.264 Sequence Parameter Set NAL unit
//!    (ISO/IEC 14496-10 §7.3.2.1.1) using a minimal Exp-Golomb bit reader. Extracts
//!    the 6 fields needed to build the `avcC` box and compute frame dimensions.
//!
//! 2. **Emulation prevention byte stripper** (`unwrap_emulation_prevention`): removes
//!    `0x03` bytes injected by the H.264 encoder to avoid start-code mimicry
//!    (ISO/IEC 14496-10 §7.4.1.1). Must be applied BEFORE bit-reading the RBSP.
//!
//! 3. **`avcC` box builder** (`build_avcc`): produces an `AVCDecoderConfigurationRecord`
//!    byte buffer (ISO/IEC 14496-15 §5.2.4.1.2) from the parsed `SpsInfo` + raw SPS/PPS
//!    NAL bytes. Used by the fMP4 init-segment builder (B5).
//!
//! # Call conventions
//!
//! - All input slices (`nal`, `sps_nal`, `pps_nal`) are raw NAL bytes **including** the
//!   1-byte NAL header (e.g. `0x67` for an SPS NAL). Annex-B start codes MUST be
//!   stripped by the caller before invoking any function here.
//! - The `build_avcc` caller is responsible for passing the same raw NAL bytes (with
//!   header, without start code) to both `parse_sps` and `build_avcc`.

// ─── Error type ──────────────────────────────────────────────────────────────

/// Errors produced by the SPS parser and `avcC` builder.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AvccError {
    /// The input NAL is too short or the bitstream ends unexpectedly mid-field.
    #[error("SPS parse failed: {0}")]
    ParseFailed(String),
    /// Caller passed an invalid (e.g. empty) SPS or PPS buffer.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

// ─── SpsInfo ─────────────────────────────────────────────────────────────────

/// Parsed fields from an H.264 Sequence Parameter Set NAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    /// `profile_idc`: H.264 profile (66 = Baseline, 77 = Main, 100 = High).
    pub profile_idc: u8,
    /// `constraint_set0..5_flag` packed into bits 7..2; bits 1..0 are reserved zeros.
    pub constraint_set_flags: u8,
    /// `level_idc`: H.264 level (e.g. 40 = Level 4.0, 30 = Level 3.0).
    pub level_idc: u8,
    /// Frame width in pixels derived from `(pic_width_in_mbs_minus1 + 1) × 16`.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// `frame_mbs_only_flag`: 1 for progressive, 0 for interlaced.
    pub frame_mbs_only_flag: bool,
}

// ─── Stub implementations (GREEN will fill these in) ─────────────────────────

/// Remove emulation-prevention bytes from a NAL payload.
pub fn unwrap_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let _ = rbsp;
    unimplemented!("unwrap_emulation_prevention — RED stub")
}

/// Parse an H.264 SPS NAL unit and extract the fields needed for `avcC` construction.
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo, AvccError> {
    let _ = nal;
    unimplemented!("parse_sps — RED stub")
}

/// Build an `AVCDecoderConfigurationRecord` (`avcC`) byte buffer.
pub fn build_avcc(
    sps_info: &SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, AvccError> {
    let _ = (sps_info, sps_nal, pps_nal);
    unimplemented!("build_avcc — RED stub")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Corrected 320x240 SPS fixture ──────────────────────────────────────
    //
    // Bit stream for 320x240 Baseline Level 1.3 SPS:
    //
    // NAL header:  0x67 (forbidden=0, nal_ref_idc=3, type=7)
    // profile_idc: 0x42 (66, Baseline)
    // flags:       0xC0 (constraint_set0=1, constraint_set1=1)
    // level_idc:   0x0D (13 = Level 1.3)
    //
    // RBSP body bits (MSB first):
    //   pos  0: 1   seq_parameter_set_id=0 → ue(0)="1"
    //   pos  1: 1   log2_max_frame_num_minus4=0 → ue(0)="1"
    //   pos  2: 1   pic_order_cnt_type=0 → ue(0)="1"
    //   pos  3: 1   log2_max_pic_order_cnt_lsb_minus4=0 → ue(0)="1"
    //   pos  4: 0   max_num_ref_frames=1 → ue(1): leading zero
    //   pos  5: 1   max_num_ref_frames=1: terminator (n=1)
    //   pos  6: 0   max_num_ref_frames=1: suffix[0]=0 → value=2^1+0-1=1 ✓
    //   pos  7: 0   gaps_in_frame_num_value_allowed_flag=0
    //   --- byte 0: 1111 0100 = 0xF4 ---
    //   pos  8: 0   pic_width_in_mbs_minus1=19: ue(19) leading zero 1/4
    //   pos  9: 0   leading zero 2/4
    //   pos 10: 0   leading zero 3/4
    //   pos 11: 0   leading zero 4/4
    //   pos 12: 1   terminator (n=4)
    //   pos 13: 0   suffix[3] of 0100 (=4=20-16)
    //   pos 14: 1   suffix[2]
    //   pos 15: 0   suffix[1]
    //   --- byte 1: 0000 1010 = 0x0A ---
    //   pos 16: 0   suffix[0] of 0100 → value=2^4+4-1=19 ✓
    //   pos 17: 0   pic_height_in_map_units_minus1=14: ue(14) leading zero 1/3
    //   pos 18: 0   leading zero 2/3
    //   pos 19: 0   leading zero 3/3
    //   pos 20: 1   terminator (n=3)
    //   pos 21: 1   suffix[2] of 111 (=7=15-8)
    //   pos 22: 1   suffix[1]
    //   pos 23: 1   suffix[0] → value=2^3+7-1=14 ✓
    //   --- byte 2: 0000 1111 = 0x0F ---
    //   pos 24: 1   frame_mbs_only_flag=1
    //   pos 25: 1   RBSP trailing stop bit
    //   pos 26-31: 0 (padding)
    //   --- byte 3: 1100 0000 = 0xC0 ---
    //
    // Expected parse result: width=320, height=240, profile=66, level=13, progressive=true

    const SPS_320X240: &[u8] = &[
        0x67, 0x42, 0xC0, 0x0D, // NAL hdr + profile + flags + level
        0xF4, 0x0A, 0x0F, 0xC0, // RBSP body
    ];

    /// Interlaced variant: frame_mbs_only_flag=0, height becomes 480.
    ///
    /// Bit stream identical to SPS_320X240 except pos 24 = 0 (interlaced).
    /// Byte 3: 0 1 00 0000 = 0x40 (frame_mbs_only=0, stop=1, padding)
    const SPS_320X240_INTERLACED: &[u8] = &[
        0x67, 0x42, 0xC0, 0x0D,
        0xF4, 0x0A, 0x0F, 0x40,
    ];

    /// Golden 1920x1080 SPS from openh264 (contains emulation-prevention bytes).
    ///
    /// The 0x03 bytes at positions 13 and 18 are emulation-prevention bytes inserted
    /// in the `0x00 0x00 0x03` patterns. This SPS is used to verify EPB stripping.
    const GOLDEN_SPS_1920X1080: &[u8] = &[
        0x67, 0x42, 0xC0, 0x28,
        0xD9, 0x00, 0xA0, 0x47,
        0xFE, 0xC0, 0x44, 0x00,
        0x00, 0x03, 0x00, 0x04,
        0x00, 0x00, 0x03, 0x00,
        0xCA, 0x3C, 0x48, 0x96,
        0x58,
    ];

    const MINIMAL_SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x1F, 0xAB, 0xCD];
    const MINIMAL_PPS: &[u8] = &[0x68, 0xCE, 0x38, 0x80];

    fn minimal_sps_info() -> SpsInfo {
        SpsInfo {
            profile_idc: 66,
            constraint_set_flags: 0xC0,
            level_idc: 31,
            width: 1280,
            height: 720,
            frame_mbs_only_flag: true,
        }
    }

    // ─── Capability A: SPS Exp-Golomb parser tests ─────────────────────────

    #[test]
    #[should_panic]
    fn parse_sps_empty_returns_err() {
        // RED: parse_sps is unimplemented → panics with unimplemented!()
        // After GREEN: parse_sps(&[]).unwrap_err() with AvccError::ParseFailed
        let _ = parse_sps(&[]);
    }

    #[test]
    #[should_panic]
    fn parse_sps_too_short_returns_err() {
        let _ = parse_sps(&[0x67, 0x42, 0xC0]);
    }

    #[test]
    #[should_panic]
    fn parse_sps_wrong_nal_type_returns_err() {
        let _ = parse_sps(&[0x65, 0x42, 0xC0, 0x28, 0xFF]);
    }

    #[test]
    #[should_panic]
    fn parse_sps_320x240_correct_profile_level() {
        let _ = parse_sps(SPS_320X240);
    }

    #[test]
    #[should_panic]
    fn parse_sps_320x240_correct_dimensions() {
        let _ = parse_sps(SPS_320X240);
    }

    #[test]
    #[should_panic]
    fn parse_sps_320x240_frame_mbs_only_progressive() {
        let _ = parse_sps(SPS_320X240);
    }

    #[test]
    #[should_panic]
    fn parse_sps_interlaced_height_uses_two_factor() {
        let _ = parse_sps(SPS_320X240_INTERLACED);
    }

    #[test]
    #[should_panic]
    fn parse_sps_1280x720_correct_dimensions() {
        let sps: &[u8] = &[
            0x67, 0x42, 0xC0, 0x1F,
            0xF4, 0x02, 0x80, 0x2D, 0xC0,
        ];
        let _ = parse_sps(sps);
    }

    #[test]
    #[should_panic]
    fn parse_sps_with_emulation_prevention_bytes_no_panic() {
        let _ = parse_sps(GOLDEN_SPS_1920X1080);
    }

    // ─── Emulation prevention byte tests ──────────────────────────────────

    #[test]
    #[should_panic]
    fn unwrap_emulation_prevention_empty() {
        let _ = unwrap_emulation_prevention(&[]);
    }

    #[test]
    #[should_panic]
    fn unwrap_emulation_prevention_no_epb() {
        let input = vec![0x67u8, 0x42, 0xC0, 0x28, 0xD9, 0xAB];
        let _ = unwrap_emulation_prevention(&input);
    }

    #[test]
    #[should_panic]
    fn unwrap_emulation_prevention_strips_epb() {
        let input = vec![0x67u8, 0x00, 0x00, 0x03, 0x01];
        let _ = unwrap_emulation_prevention(&input);
    }

    #[test]
    #[should_panic]
    fn unwrap_emulation_prevention_multiple_epbs() {
        let input = vec![0x67u8, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0xFF];
        let _ = unwrap_emulation_prevention(&input);
    }

    // ─── Capability B: avcC box builder tests ─────────────────────────────

    #[test]
    #[should_panic]
    fn build_avcc_configuration_version_is_one() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_profile_compatibility_level_correct() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_length_size_minus_one_is_three() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_num_sps_field_is_0xe1() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_sps_length_is_big_endian() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_sps_bytes_verbatim() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_num_pps_is_one() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_pps_length_is_big_endian() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_pps_bytes_verbatim() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_total_size_correct() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_empty_sps_returns_err() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, &[], MINIMAL_PPS);
    }

    #[test]
    #[should_panic]
    fn build_avcc_empty_pps_returns_err() {
        let info = minimal_sps_info();
        let _ = build_avcc(&info, MINIMAL_SPS, &[]);
    }

    #[test]
    #[should_panic]
    fn build_avcc_golden_round_trip() {
        let info = parse_sps(SPS_320X240).expect("parse");
        let pps = &[0x68u8, 0xCE, 0x38, 0x80];
        let _ = build_avcc(&info, SPS_320X240, pps);
    }
}
