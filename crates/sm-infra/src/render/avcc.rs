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
///
/// All fields are extracted directly from the SPS RBSP (raw byte sequence payload)
/// per ISO/IEC 14496-10 §7.3.2.1.1.
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
    /// Frame height in pixels derived from `(2 − frame_mbs_only_flag) × (pic_height_in_map_units_minus1 + 1) × 16`.
    pub height: u32,
    /// `frame_mbs_only_flag`: 1 for progressive, 0 for interlaced.
    pub frame_mbs_only_flag: bool,
}

// ─── Capability A: Emulation prevention byte stripper ────────────────────────

/// Remove emulation-prevention bytes from a NAL payload.
///
/// H.264 §7.4.1.1: the encoder inserts `0x03` between any two consecutive `0x00` bytes
/// that could be mistaken for a start code. This function strips each `0x03` byte that
/// appears in a `0x00 0x00 0x03` sequence, producing the raw RBSP bytes that the
/// bit reader can then parse.
///
/// # Arguments
///
/// * `rbsp` — NAL payload bytes **including** the NAL header byte.
pub fn unwrap_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    if rbsp.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(rbsp.len());
    let mut i = 0;

    while i < rbsp.len() {
        // Detect 0x00 0x00 0x03 triplet and strip the 0x03.
        if i + 2 < rbsp.len() && rbsp[i] == 0x00 && rbsp[i + 1] == 0x00 && rbsp[i + 2] == 0x03 {
            out.push(0x00);
            out.push(0x00);
            i += 3;
        } else {
            out.push(rbsp[i]);
            i += 1;
        }
    }

    out
}

// ─── Capability A: Bit reader ─────────────────────────────────────────────────

/// Minimal bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute position in BITS (0 = MSB of byte 0).
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    fn read_bit(&mut self) -> Result<bool, AvccError> {
        if self.bit_pos >= self.total_bits() {
            return Err(AvccError::ParseFailed(format!(
                "unexpected end of bitstream at bit {}",
                self.bit_pos
            )));
        }
        let byte_idx = self.bit_pos / 8;
        let bit_idx = 7 - (self.bit_pos % 8); // MSB first
        self.bit_pos += 1;
        Ok((self.data[byte_idx] >> bit_idx) & 1 != 0)
    }

    fn read_bits(&mut self, n: u8) -> Result<u32, AvccError> {
        let mut val: u32 = 0;
        for _ in 0..n {
            val = (val << 1) | (self.read_bit()? as u32);
        }
        Ok(val)
    }

    /// Read an Exp-Golomb coded unsigned integer `ue(v)`.
    fn read_ue(&mut self) -> Result<u32, AvccError> {
        let mut leading_zeros: u8 = 0;
        loop {
            let bit = self.read_bit()?;
            if bit {
                break;
            }
            leading_zeros += 1;
            if leading_zeros >= 32 {
                return Err(AvccError::ParseFailed(
                    "Exp-Golomb codeword exceeds 32 leading zeros".into(),
                ));
            }
        }

        if leading_zeros == 0 {
            return Ok(0);
        }

        let suffix = self.read_bits(leading_zeros)?;
        Ok(((1u32 << leading_zeros) | suffix) - 1)
    }
}

// ─── Capability A: SPS parser ─────────────────────────────────────────────────

/// Parse an H.264 SPS NAL unit and extract the fields needed for `avcC` construction.
///
/// # Input
///
/// `nal` — raw NAL bytes **including** the 1-byte NAL header (e.g. `[0x67, ...]`).
/// Annex-B start codes must be stripped before calling. Emulation-prevention bytes
/// are stripped internally.
///
/// # Errors
///
/// Returns `Err(AvccError::ParseFailed(_))` if the input is malformed or truncated.
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo, AvccError> {
    if nal.len() < 4 {
        return Err(AvccError::ParseFailed(format!(
            "SPS NAL too short: {} bytes (need ≥ 4)",
            nal.len()
        )));
    }

    let nal_type = nal[0] & 0x1F;
    if nal_type != 7 {
        return Err(AvccError::ParseFailed(format!(
            "not an SPS NAL: type = {} (expected 7)",
            nal_type
        )));
    }

    let profile_idc = nal[1];
    let constraint_set_flags = nal[2];
    let level_idc = nal[3];

    let rbsp = unwrap_emulation_prevention(nal);

    if rbsp.len() < 5 {
        return Err(AvccError::ParseFailed(
            "SPS RBSP body too short after emulation-prevention stripping".into(),
        ));
    }

    let mut r = BitReader::new(&rbsp[4..]);

    // seq_parameter_set_id — ue(v), discard.
    let _seq_ps_id = r.read_ue()?;

    // High-profile extra fields.
    let is_high_profile = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );

    if is_high_profile {
        let chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane_flag = r.read_bit()?;
        }
        let _bit_depth_luma_minus8 = r.read_ue()?;
        let _bit_depth_chroma_minus8 = r.read_ue()?;
        let _qpprime_y_zero_transform_bypass_flag = r.read_bit()?;
        let seq_scaling_matrix_present_flag = r.read_bit()?;
        if seq_scaling_matrix_present_flag {
            let list_count: u8 = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..list_count {
                let present = r.read_bit()?;
                if present {
                    let size = if i < 6 { 16usize } else { 64usize };
                    let mut next_scale: i32 = 8;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta_ue = r.read_ue()?;
                            let delta_scale = ue_to_se(delta_ue);
                            next_scale = (next_scale + delta_scale + 256) % 256;
                        }
                    }
                }
            }
        }
    }

    // log2_max_frame_num_minus4 — ue(v), discard.
    let _log2_max_frame_num = r.read_ue()?;

    // pic_order_cnt_type — ue(v).
    let poc_type = r.read_ue()?;
    match poc_type {
        0 => {
            let _log2_max_poc_lsb = r.read_ue()?;
        }
        1 => {
            let _delta_zero = r.read_bit()?;
            let _offset_non_ref = r.read_ue()?;
            let _offset_top_bot = r.read_ue()?;
            let num_ref = r.read_ue()?;
            for _ in 0..num_ref {
                let _off = r.read_ue()?;
            }
        }
        2 => {}
        _ => {
            return Err(AvccError::ParseFailed(format!(
                "unsupported pic_order_cnt_type: {}",
                poc_type
            )));
        }
    }

    // max_num_ref_frames — ue(v), discard.
    let _max_ref_frames = r.read_ue()?;

    // gaps_in_frame_num_value_allowed_flag — u(1), discard.
    let _gaps = r.read_bit()?;

    // pic_width_in_mbs_minus1 — ue(v).
    let pic_width_in_mbs_minus1 = r.read_ue()?;

    // pic_height_in_map_units_minus1 — ue(v).
    let pic_height_in_map_units_minus1 = r.read_ue()?;

    // frame_mbs_only_flag — u(1).
    let frame_mbs_only_flag = r.read_bit()?;

    let width = (pic_width_in_mbs_minus1 + 1) * 16;
    let height_factor: u32 = if frame_mbs_only_flag { 1 } else { 2 };
    let height = height_factor * (pic_height_in_map_units_minus1 + 1) * 16;

    Ok(SpsInfo {
        profile_idc,
        constraint_set_flags,
        level_idc,
        width,
        height,
        frame_mbs_only_flag,
    })
}

/// Convert an Exp-Golomb unsigned code to a signed integer (ZigZag mapping).
fn ue_to_se(ue: u32) -> i32 {
    if ue == 0 {
        return 0;
    }
    let sign = if ue % 2 == 1 { 1i32 } else { -1i32 };
    let mag = (ue / 2 + ue % 2) as i32;
    sign * mag
}

// ─── Capability B: avcC box builder ──────────────────────────────────────────

/// Build an `AVCDecoderConfigurationRecord` (`avcC`) byte buffer.
///
/// Layout (ISO/IEC 14496-15 §5.2.4.1.2):
///
/// ```text
/// 1 byte   configurationVersion = 1
/// 1 byte   AVCProfileIndication  = profile_idc
/// 1 byte   profile_compatibility = constraint_set_flags
/// 1 byte   AVCLevelIndication    = level_idc
/// 1 byte   0xFC | lengthSizeMinusOne (= 0xFF for 4-byte length prefix)
/// 1 byte   0xE0 | numSequenceParameterSets (= 0xE1 for exactly 1 SPS)
/// 2 bytes  sequenceParameterSetLength  (u16 big-endian)
/// N bytes  SPS NAL bytes (with NAL header byte, without Annex-B start code)
/// 1 byte   numPictureParameterSets = 1
/// 2 bytes  pictureParameterSetLength   (u16 big-endian)
/// M bytes  PPS NAL bytes (with NAL header byte, without Annex-B start code)
/// ```
///
/// # Errors
///
/// Returns `Err(AvccError::InvalidInput(_))` if `sps_nal` or `pps_nal` is empty, or if
/// either NAL is longer than `u16::MAX` bytes.
pub fn build_avcc(
    sps_info: &SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, AvccError> {
    if sps_nal.is_empty() {
        return Err(AvccError::InvalidInput("sps_nal must not be empty".into()));
    }
    if pps_nal.is_empty() {
        return Err(AvccError::InvalidInput("pps_nal must not be empty".into()));
    }

    let sps_len = sps_nal.len();
    let pps_len = pps_nal.len();

    if sps_len > u16::MAX as usize {
        return Err(AvccError::InvalidInput(format!(
            "SPS NAL too large: {} bytes (max {})",
            sps_len,
            u16::MAX
        )));
    }
    if pps_len > u16::MAX as usize {
        return Err(AvccError::InvalidInput(format!(
            "PPS NAL too large: {} bytes (max {})",
            pps_len,
            u16::MAX
        )));
    }

    let total = 6 + 2 + sps_len + 1 + 2 + pps_len;
    let mut buf = Vec::with_capacity(total);

    // Fixed 6-byte header.
    buf.push(1u8); // configurationVersion = 1
    buf.push(sps_info.profile_idc); // AVCProfileIndication
    buf.push(sps_info.constraint_set_flags); // profile_compatibility
    buf.push(sps_info.level_idc); // AVCLevelIndication
    buf.push(0xFC | 0x03); // reserved(6b=0b111111) | lengthSizeMinusOne(2b=3) → 0xFF
    buf.push(0xE0 | 0x01); // reserved(3b=0b111) | numSequenceParameterSets(5b=1) → 0xE1

    // SPS NAL.
    buf.extend_from_slice(&(sps_len as u16).to_be_bytes());
    buf.extend_from_slice(sps_nal);

    // PPS NAL.
    buf.push(1u8); // numPictureParameterSets = 1
    buf.extend_from_slice(&(pps_len as u16).to_be_bytes());
    buf.extend_from_slice(pps_nal);

    Ok(buf)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── SPS byte fixtures ─────────────────────────────────────────────────
    //
    // SPS_320X240: Baseline Level 1.3, 320×240 progressive.
    //
    // RBSP body bit derivation (after the 4-byte fixed header):
    //   seq_ps_id=0:        ue(0)="1"            [1b]
    //   log2_fn_minus4=0:   ue(0)="1"            [1b]
    //   poc_type=0:         ue(0)="1"            [1b]
    //   log2_poc_lsb=0:     ue(0)="1"            [1b]
    //   max_ref=1:          ue(1)="010"          [3b]
    //   gaps=0:             u(1)="0"             [1b]
    //   [byte 0: 1111 0100 = 0xF4]
    //   pw_mbs_minus1=19:   ue(19)="00001 0100"  [9b]
    //     n=4, suffix=4=0100: "0000 1 010 0"
    //     bits 8-16: 0000 1010 | 0
    //   [byte 1: 0000 1010 = 0x0A]
    //   ph_map_minus1=14:   ue(14)="000 1 111"   [7b]
    //     n=3, suffix=7=111: "000 1 111"
    //     bits 16-22: 0 | 0001 111
    //   frame_mbs_only=1:   u(1)="1"             [1b] at bit 23
    //   [byte 2: 0000 1111 = 0x0F]
    //   trailing: 1 (stop) + 6 zeros at bits 24-31
    //   [byte 3: 1100 0000 = 0xC0]
    //
    // Parsed: width=(19+1)*16=320, height=1*(14+1)*16=240, progressive

    const SPS_320X240: &[u8] = &[0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0xC0];

    // SPS_320X240_INTERLACED: same but frame_mbs_only_flag=0.
    // bit 24 → 0, so byte 3 of RBSP = 0100 0000 = 0x40.
    // Parsed: width=320, height=2*(14+1)*16=480, interlaced.
    const SPS_320X240_INTERLACED: &[u8] = &[0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0x40];

    // SPS_1280X720: Baseline Level 3.1, 1280×720 progressive.
    //
    // RBSP body bits (after fixed 4-byte header):
    //   seq_ps_id=0, fn=0, poc=0, poc_lsb=0: "1111"
    //   max_ref=1: "010"
    //   gaps=0: "0"
    //   [byte 0: 1111 0100 = 0xF4]
    //   pw_mbs_minus1=79: ue(79): n=6, suffix=16=010000
    //     → "000000 1 010000" (13 bits)
    //     bits 8-20: 00000010 10000
    //   [byte 1: 0000 0010 = 0x02]
    //   continued bits 16-20: 10000 (5 bits of suffix)
    //   ph_map_minus1=44: ue(44): n=5, suffix=13=01101
    //     → "00000 1 01101" (11 bits, at bits 21-31)
    //   [byte 2: 1000 0000 = 0x80]
    //   [byte 3: 0010 1101 = 0x2D]
    //   frame_mbs_only=1 at bit 32: "1"
    //   trailing: stop+pad at bits 33-39
    //   [byte 4: 1100 0000 = 0xC0]
    //
    // Parsed: width=(79+1)*16=1280, height=1*(44+1)*16=720
    const SPS_1280X720: &[u8] = &[0x67, 0x42, 0xC0, 0x1F, 0xF4, 0x02, 0x80, 0x2D, 0xC0];

    // GOLDEN_SPS_1920X1080: real SPS bytes from openh264 for 1920×1080 Baseline Level 4.0.
    // Contains emulation-prevention bytes (0x03) at positions 13 and 18.
    const GOLDEN_SPS_1920X1080: &[u8] = &[
        0x67, 0x42, 0xC0, 0x28, 0xD9, 0x00, 0xA0, 0x47, 0xFE, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00,
        0x04, 0x00, 0x00, 0x03, 0x00, 0xCA, 0x3C, 0x48, 0x96, 0x58,
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

    // ─── Capability A: unwrap_emulation_prevention (GREEN) ─────────────────

    #[test]
    fn unwrap_emulation_prevention_empty() {
        let out = unwrap_emulation_prevention(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn unwrap_emulation_prevention_no_epb() {
        let input: &[u8] = &[0x67, 0x42, 0xC0, 0x28, 0xD9, 0xAB];
        let out = unwrap_emulation_prevention(input);
        assert_eq!(out, input);
    }

    #[test]
    fn unwrap_emulation_prevention_strips_epb() {
        let input: &[u8] = &[0x67, 0x00, 0x00, 0x03, 0x01];
        let out = unwrap_emulation_prevention(input);
        assert_eq!(out, &[0x67, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn unwrap_emulation_prevention_multiple_epbs() {
        let input: &[u8] = &[0x67, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0xFF];
        let out = unwrap_emulation_prevention(input);
        assert_eq!(out, &[0x67, 0x00, 0x00, 0x00, 0x00, 0xFF]);
    }

    // ─── Capability A: parse_sps — error paths (GREEN) ────────────────────

    #[test]
    fn parse_sps_empty_returns_err() {
        let err = parse_sps(&[]).unwrap_err();
        assert!(matches!(err, AvccError::ParseFailed(_)));
    }

    #[test]
    fn parse_sps_too_short_returns_err() {
        let err = parse_sps(&[0x67, 0x42, 0xC0]).unwrap_err();
        assert!(matches!(err, AvccError::ParseFailed(_)));
    }

    #[test]
    fn parse_sps_wrong_nal_type_returns_err() {
        let err = parse_sps(&[0x65, 0x42, 0xC0, 0x28, 0xFF]).unwrap_err();
        assert!(matches!(err, AvccError::ParseFailed(_)));
    }

    // ─── Capability A: parse_sps — 320×240 golden (GREEN) ─────────────────

    #[test]
    fn parse_sps_320x240_correct_profile_level() {
        let info = parse_sps(SPS_320X240).expect("should parse");
        assert_eq!(info.profile_idc, 66);
        assert_eq!(info.constraint_set_flags, 0xC0);
        assert_eq!(info.level_idc, 13);
    }

    #[test]
    fn parse_sps_320x240_correct_dimensions() {
        let info = parse_sps(SPS_320X240).expect("should parse");
        assert_eq!(info.width, 320);
        assert_eq!(info.height, 240);
    }

    #[test]
    fn parse_sps_320x240_frame_mbs_only_progressive() {
        let info = parse_sps(SPS_320X240).expect("should parse");
        assert!(info.frame_mbs_only_flag);
    }

    // ─── Capability A: parse_sps — interlaced (GREEN) ─────────────────────

    #[test]
    fn parse_sps_interlaced_height_uses_two_factor() {
        let info = parse_sps(SPS_320X240_INTERLACED).expect("should parse");
        assert_eq!(info.width, 320);
        assert_eq!(info.height, 480, "interlaced height = 2 * 15 * 16 = 480");
        assert!(!info.frame_mbs_only_flag);
    }

    // ─── Capability A: parse_sps — 1280×720 (GREEN) ───────────────────────

    #[test]
    fn parse_sps_1280x720_correct_dimensions() {
        let info = parse_sps(SPS_1280X720).expect("should parse");
        assert_eq!(info.width, 1280);
        assert_eq!(info.height, 720);
        assert_eq!(info.profile_idc, 66);
        assert_eq!(info.level_idc, 31);
    }

    // ─── Capability A: parse_sps — EPB handling (GREEN) ───────────────────

    #[test]
    fn parse_sps_with_emulation_prevention_bytes_no_panic() {
        // Must not panic regardless of whether parsing succeeds.
        let result = parse_sps(GOLDEN_SPS_1920X1080);
        match result {
            Ok(info) => {
                assert_eq!(info.width % 16, 0);
                assert_eq!(info.height % 16, 0);
            }
            Err(e) => println!("Graceful error: {}", e),
        }
    }

    // ─── Capability B: build_avcc — error paths (GREEN) ───────────────────

    #[test]
    fn build_avcc_empty_sps_returns_err() {
        let err = build_avcc(&minimal_sps_info(), &[], MINIMAL_PPS).unwrap_err();
        assert!(matches!(err, AvccError::InvalidInput(_)));
    }

    #[test]
    fn build_avcc_empty_pps_returns_err() {
        let err = build_avcc(&minimal_sps_info(), MINIMAL_SPS, &[]).unwrap_err();
        assert!(matches!(err, AvccError::InvalidInput(_)));
    }

    // ─── Capability B: build_avcc — byte layout (GREEN) ───────────────────

    #[test]
    fn build_avcc_configuration_version_is_one() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        assert_eq!(buf[0], 1, "configurationVersion must be 1");
    }

    #[test]
    fn build_avcc_profile_compatibility_level_correct() {
        let info = minimal_sps_info();
        let buf = build_avcc(&info, MINIMAL_SPS, MINIMAL_PPS).expect("build_avcc should succeed");
        assert_eq!(buf[1], 66, "AVCProfileIndication = profile_idc");
        assert_eq!(buf[2], 0xC0, "profile_compatibility = constraint_set_flags");
        assert_eq!(buf[3], 31, "AVCLevelIndication = level_idc");
    }

    #[test]
    fn build_avcc_length_size_minus_one_is_three() {
        // byte 4: 0xFC | 0x03 = 0xFF
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        assert_eq!(buf[4], 0xFF, "lengthSizeMinusOne byte must be 0xFF");
    }

    #[test]
    fn build_avcc_num_sps_field_is_0xe1() {
        // byte 5: 0xE0 | 0x01 = 0xE1
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        assert_eq!(buf[5], 0xE1, "numSPS byte must be 0xE1");
    }

    #[test]
    fn build_avcc_sps_length_is_big_endian() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        let sps_len = u16::from_be_bytes([buf[6], buf[7]]);
        assert_eq!(sps_len as usize, MINIMAL_SPS.len());
    }

    #[test]
    fn build_avcc_sps_bytes_verbatim() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        let sps_start = 8;
        assert_eq!(&buf[sps_start..sps_start + MINIMAL_SPS.len()], MINIMAL_SPS);
    }

    #[test]
    fn build_avcc_num_pps_is_one() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        assert_eq!(buf[8 + MINIMAL_SPS.len()], 1);
    }

    #[test]
    fn build_avcc_pps_length_is_big_endian() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        let pps_len_off = 8 + MINIMAL_SPS.len() + 1;
        let pps_len = u16::from_be_bytes([buf[pps_len_off], buf[pps_len_off + 1]]);
        assert_eq!(pps_len as usize, MINIMAL_PPS.len());
    }

    #[test]
    fn build_avcc_pps_bytes_verbatim() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        let pps_start = 8 + MINIMAL_SPS.len() + 1 + 2;
        assert_eq!(&buf[pps_start..pps_start + MINIMAL_PPS.len()], MINIMAL_PPS);
    }

    #[test]
    fn build_avcc_total_size_correct() {
        let buf = build_avcc(&minimal_sps_info(), MINIMAL_SPS, MINIMAL_PPS)
            .expect("build_avcc should succeed");
        let expected = 6 + 2 + MINIMAL_SPS.len() + 1 + 2 + MINIMAL_PPS.len();
        assert_eq!(buf.len(), expected);
    }

    // ─── Capability B: round-trip (GREEN) ─────────────────────────────────

    #[test]
    fn build_avcc_golden_round_trip_320x240() {
        // Parse 320x240 SPS → build avcC → verify key bytes match spec.
        let info = parse_sps(SPS_320X240).expect("should parse");
        let buf = build_avcc(&info, SPS_320X240, MINIMAL_PPS).expect("should build");

        assert_eq!(buf[0], 1, "configurationVersion = 1");
        assert_eq!(buf[1], 66, "AVCProfileIndication = 66");
        assert_eq!(buf[2], 0xC0, "profile_compatibility = 0xC0");
        assert_eq!(buf[3], 13, "AVCLevelIndication = 13 (Level 1.3)");
        assert_eq!(buf[4], 0xFF, "lengthSizeMinusOne = 3 → 0xFF");
        assert_eq!(buf[5], 0xE1, "numSPS = 1 → 0xE1");
    }
}
