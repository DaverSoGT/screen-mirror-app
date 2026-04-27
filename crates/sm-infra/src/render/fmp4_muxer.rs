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

// ─── Errors ──────────────────────────────────────────────────────────────────

/// Errors produced by the fMP4 muxer.
#[derive(Debug, thiserror::Error)]
pub enum MuxerError {
    /// The provided SPS or PPS bytes are empty or invalid.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An error occurred while building the avcC configuration record.
    #[error("avcc error: {0}")]
    AvccError(#[from] crate::render::avcc::AvccError),
}

// ─── ISO/IEC 14496-12 box framing primitive ──────────────────────────────────

/// Write a single ISO base-media file format box into `out`.
///
/// Layout: `[u32_BE total_size][4 ASCII type bytes][payload bytes]`.
/// The `total_size` field is `payload.len() + 8` (4 bytes for size + 4 bytes for type).
///
/// # Arguments
///
/// * `out`      — output buffer to extend.
/// * `box_type` — exactly 4 ASCII bytes identifying the box type (e.g. `b"ftyp"`).
/// * `payload`  — box payload bytes (may be empty).
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

// ─── moov hierarchy builder ──────────────────────────────────────────────────

/// fMP4 timescale: 90 kHz matches RTP and `EncodedPacket::timestamp` mapping.
const TIMESCALE: u32 = 90_000;

/// Write a `u16` big-endian value into `out`.
fn write_u16_be(out: &mut Vec<u8>, val: u16) {
    out.extend_from_slice(&val.to_be_bytes());
}

/// Write a `u32` big-endian value into `out`.
fn write_u32_be(out: &mut Vec<u8>, val: u32) {
    out.extend_from_slice(&val.to_be_bytes());
}

/// Write a full-box header: 1 byte version + 3 bytes flags (big-endian u24).
fn write_full_box_header(out: &mut Vec<u8>, version: u8, flags: u32) {
    out.push(version);
    out.push(((flags >> 16) & 0xFF) as u8);
    out.push(((flags >> 8) & 0xFF) as u8);
    out.push((flags & 0xFF) as u8);
}

/// Build the `mvhd` (movie header) box.
fn build_mvhd() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // creation_time
    write_u32_be(&mut payload, 0); // modification_time
    write_u32_be(&mut payload, TIMESCALE); // timescale = 90_000
    write_u32_be(&mut payload, 0); // duration = 0 (live/unknown)
    write_u32_be(&mut payload, 0x0001_0000); // rate = 1.0 (16.16 fixed)
    write_u16_be(&mut payload, 0x0100); // volume = 1.0 (8.8 fixed)
    payload.extend_from_slice(&[0u8; 10]); // reserved (2 + 8)
    // Unity matrix (3×3 as 9 × i32 in 2.30 fixed-point).
    #[rustfmt::skip]
    payload.extend_from_slice(&[
        0x00, 0x01, 0x00, 0x00,  // a = 1.0
        0x00, 0x00, 0x00, 0x00,  // b = 0
        0x00, 0x00, 0x00, 0x00,  // u = 0
        0x00, 0x00, 0x00, 0x00,  // c = 0
        0x00, 0x01, 0x00, 0x00,  // d = 1.0
        0x00, 0x00, 0x00, 0x00,  // v = 0
        0x00, 0x00, 0x00, 0x00,  // tx = 0
        0x00, 0x00, 0x00, 0x00,  // ty = 0
        0x40, 0x00, 0x00, 0x00,  // w = 1.0 (2.30 fixed, 1 << 30)
    ]);
    payload.extend_from_slice(&[0u8; 24]); // pre_defined[6]
    write_u32_be(&mut payload, 2); // next_track_id = 2

    let mut out = Vec::new();
    write_box(&mut out, b"mvhd", &payload);
    out
}

/// Build the `tkhd` (track header) box.
fn build_tkhd(width: u32, height: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    // flags = 3: track_enabled (0x01) | track_in_movie (0x02)
    write_full_box_header(&mut payload, 0, 3);
    write_u32_be(&mut payload, 0); // creation_time
    write_u32_be(&mut payload, 0); // modification_time
    write_u32_be(&mut payload, 1); // track_id = 1
    write_u32_be(&mut payload, 0); // reserved
    write_u32_be(&mut payload, 0); // duration = 0 (live)
    payload.extend_from_slice(&[0u8; 8]); // reserved (2 × u32)
    write_u16_be(&mut payload, 0); // layer = 0
    write_u16_be(&mut payload, 0); // alternate_group = 0
    write_u16_be(&mut payload, 0); // volume = 0 (video track: muted)
    write_u16_be(&mut payload, 0); // reserved
    // Unity matrix
    #[rustfmt::skip]
    payload.extend_from_slice(&[
        0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x40, 0x00, 0x00, 0x00,
    ]);
    // width and height as 16.16 fixed-point
    write_u32_be(&mut payload, width << 16);
    write_u32_be(&mut payload, height << 16);

    let mut out = Vec::new();
    write_box(&mut out, b"tkhd", &payload);
    out
}

/// Build the `mdhd` (media header) box.
fn build_mdhd() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // creation_time
    write_u32_be(&mut payload, 0); // modification_time
    write_u32_be(&mut payload, TIMESCALE); // timescale = 90_000
    write_u32_be(&mut payload, 0); // duration = 0 (live)
    // language: 'und' packed into ISO 639-2/T 15-bit field.
    // Each char: subtract 0x60. u=21, n=14, d=4.
    // Packed: ((21-1) << 10) | ((14-1) << 5) | (4-1) = 0x55C3
    write_u16_be(&mut payload, 0x55C3);
    write_u16_be(&mut payload, 0); // pre_defined

    let mut out = Vec::new();
    write_box(&mut out, b"mdhd", &payload);
    out
}

/// Build the `hdlr` (handler reference) box for a video track.
fn build_hdlr() -> Vec<u8> {
    let name = b"Screen Mirror Video\0";
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // pre_defined
    payload.extend_from_slice(b"vide"); // handler_type
    payload.extend_from_slice(&[0u8; 12]); // reserved (3 × u32)
    payload.extend_from_slice(name); // null-terminated name

    let mut out = Vec::new();
    write_box(&mut out, b"hdlr", &payload);
    out
}

/// Build the `vmhd` (video media header) box.
fn build_vmhd() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 1); // flags = 0x000001 per ISO spec
    write_u16_be(&mut payload, 0); // graphicsMode = 0
    payload.extend_from_slice(&[0u8; 6]); // opcolor (3 × u16)

    let mut out = Vec::new();
    write_box(&mut out, b"vmhd", &payload);
    out
}

/// Build the `url ` (data entry URL) box — self-contained (flags = 1).
fn build_url() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 1); // flags = 0x000001 (self-contained)

    let mut out = Vec::new();
    write_box(&mut out, b"url ", &payload);
    out
}

/// Build the `dref` (data reference) box containing one self-referential `url ` entry.
fn build_dref() -> Vec<u8> {
    let url_box = build_url();
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 1); // entry_count = 1
    payload.extend_from_slice(&url_box);

    let mut out = Vec::new();
    write_box(&mut out, b"dref", &payload);
    out
}

/// Build the `dinf` (data information) box.
fn build_dinf() -> Vec<u8> {
    let dref = build_dref();
    let mut out = Vec::new();
    write_box(&mut out, b"dinf", &dref);
    out
}

/// Build the `avc1` sample entry with an embedded `avcC` box.
fn build_avc1(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let avcc_bytes = crate::render::avcc::build_avcc(sps_info, sps_nal, pps_nal)?;

    let mut avcc_box = Vec::new();
    write_box(&mut avcc_box, b"avcC", &avcc_bytes);

    // 32-byte compressor name: 1 length byte + up to 31 chars.
    let compressor_name: [u8; 32] = {
        let mut arr = [0u8; 32];
        let label = b"\x0DScreen Mirror"; // 0x0D = 13 = length of "Screen Mirror"
        arr[..label.len()].copy_from_slice(label);
        arr
    };

    let mut payload = Vec::new();
    payload.extend_from_slice(&[0u8; 6]); // reserved
    write_u16_be(&mut payload, 1); // data_reference_index = 1
    payload.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    write_u16_be(&mut payload, width as u16); // width
    write_u16_be(&mut payload, height as u16); // height
    write_u32_be(&mut payload, 0x0048_0000); // horizresolution = 72 dpi
    write_u32_be(&mut payload, 0x0048_0000); // vertresolution = 72 dpi
    write_u32_be(&mut payload, 0); // reserved
    write_u16_be(&mut payload, 1); // frame_count = 1
    payload.extend_from_slice(&compressor_name); // 32 bytes
    write_u16_be(&mut payload, 0x0018); // depth = 24-bit
    payload.extend_from_slice(&[0xFF, 0xFF]); // pre_defined = -1

    payload.extend_from_slice(&avcc_box);

    let mut out = Vec::new();
    write_box(&mut out, b"avc1", &payload);
    Ok(out)
}

/// Build the `stsd` (sample description) box.
fn build_stsd(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let avc1 = build_avc1(width, height, sps_info, sps_nal, pps_nal)?;
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 1); // entry_count = 1
    payload.extend_from_slice(&avc1);
    let mut out = Vec::new();
    write_box(&mut out, b"stsd", &payload);
    Ok(out)
}

/// Build an empty `stts` (time-to-sample) box.
fn build_stts() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // entry_count = 0
    let mut out = Vec::new();
    write_box(&mut out, b"stts", &payload);
    out
}

/// Build an empty `stsc` (sample-to-chunk) box.
fn build_stsc() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // entry_count = 0
    let mut out = Vec::new();
    write_box(&mut out, b"stsc", &payload);
    out
}

/// Build an empty `stsz` (sample size) box.
fn build_stsz() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // sample_size (uniform) = 0
    write_u32_be(&mut payload, 0); // sample_count = 0
    let mut out = Vec::new();
    write_box(&mut out, b"stsz", &payload);
    out
}

/// Build an empty `stco` (chunk offset) box.
fn build_stco() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 0); // entry_count = 0
    let mut out = Vec::new();
    write_box(&mut out, b"stco", &payload);
    out
}

/// Build the `stbl` (sample table) box.
fn build_stbl(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let stsd = build_stsd(width, height, sps_info, sps_nal, pps_nal)?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&stsd);
    payload.extend_from_slice(&build_stts());
    payload.extend_from_slice(&build_stsc());
    payload.extend_from_slice(&build_stsz());
    payload.extend_from_slice(&build_stco());
    let mut out = Vec::new();
    write_box(&mut out, b"stbl", &payload);
    Ok(out)
}

/// Build the `minf` (media information) box.
fn build_minf(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&build_vmhd());
    payload.extend_from_slice(&build_dinf());
    payload.extend_from_slice(&build_stbl(width, height, sps_info, sps_nal, pps_nal)?);
    let mut out = Vec::new();
    write_box(&mut out, b"minf", &payload);
    Ok(out)
}

/// Build the `mdia` (media) box.
fn build_mdia(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&build_mdhd());
    payload.extend_from_slice(&build_hdlr());
    payload.extend_from_slice(&build_minf(width, height, sps_info, sps_nal, pps_nal)?);
    let mut out = Vec::new();
    write_box(&mut out, b"mdia", &payload);
    Ok(out)
}

/// Build the `trak` (track) box.
fn build_trak(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&build_tkhd(width, height));
    payload.extend_from_slice(&build_mdia(width, height, sps_info, sps_nal, pps_nal)?);
    let mut out = Vec::new();
    write_box(&mut out, b"trak", &payload);
    Ok(out)
}

/// Build the `trex` (track extends defaults) box.
fn build_trex() -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 1); // track_id = 1
    write_u32_be(&mut payload, 1); // default_sample_description_index = 1
    write_u32_be(&mut payload, 0); // default_sample_duration
    write_u32_be(&mut payload, 0); // default_sample_size
    write_u32_be(&mut payload, 0); // default_sample_flags
    let mut out = Vec::new();
    write_box(&mut out, b"trex", &payload);
    out
}

/// Build the `mvex` (movie extends) box.
fn build_mvex() -> Vec<u8> {
    let trex = build_trex();
    let mut out = Vec::new();
    write_box(&mut out, b"mvex", &trex);
    out
}

/// Build the complete `moov` (movie) box.
///
/// Contains `mvhd`, one `trak` (with full `mdia`/`minf`/`stbl`/`avc1`/`avcC` hierarchy),
/// and `mvex`/`trex`.
///
/// # Arguments
///
/// * `width`, `height` — track dimensions in pixels.
/// * `sps_info` — parsed SPS fields (profile, level, dimensions).
/// * `sps_nal`, `pps_nal` — raw NAL bytes (without Annex-B start codes).
pub(crate) fn build_moov(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
) -> Result<Vec<u8>, MuxerError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&build_mvhd());
    payload.extend_from_slice(&build_trak(width, height, sps_info, sps_nal, pps_nal)?);
    payload.extend_from_slice(&build_mvex());
    let mut out = Vec::new();
    write_box(&mut out, b"moov", &payload);
    Ok(out)
}

// ─── Mp4Muxer public API ─────────────────────────────────────────────────────

/// fMP4 muxer for screen-mirror live streaming.
///
/// Builds an init segment (`ftyp` + `moov`) from the first SPS + PPS NAL bytes,
/// then emits media segments (`moof` + `mdat`) per IDR-aligned GOP.
///
/// # Example
///
/// ```rust,ignore
/// let muxer = Mp4Muxer::new(1920, 1080, 30, 1);
/// let init = muxer.build_init_segment(sps_nal, pps_nal)?;
/// // emit init bytes to the frontend once
/// ```
pub struct Mp4Muxer {
    /// Frame width in pixels (pre-validated from the SPS or caller-supplied).
    width: u32,
    /// Frame height in pixels (pre-validated from the SPS or caller-supplied).
    height: u32,
    /// Frame rate numerator (e.g. 30 for 30 fps). Reserved for B6 media-segment timing.
    #[allow(dead_code)]
    fps_num: u32,
    /// Frame rate denominator (e.g. 1 for 30 fps). Reserved for B6 media-segment timing.
    #[allow(dead_code)]
    fps_den: u32,
}

impl Mp4Muxer {
    /// Construct a new muxer with the target track parameters.
    ///
    /// # Arguments
    ///
    /// * `width`   — frame width in pixels (must be > 0).
    /// * `height`  — frame height in pixels (must be > 0).
    /// * `fps_num` — nominal frame rate numerator.
    /// * `fps_den` — nominal frame rate denominator.
    ///
    /// # Panics
    ///
    /// Panics if `width == 0` or `height == 0`.
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Self {
        assert!(width > 0, "Mp4Muxer: width must be > 0");
        assert!(height > 0, "Mp4Muxer: height must be > 0");
        Self { width, height, fps_num, fps_den }
    }

    /// Build the fMP4 init segment from the first SPS and PPS NAL bytes.
    ///
    /// Output layout: `[ftyp][moov]`. Concatenate with subsequent media segments
    /// (from `append_packet`, B6) to form a valid fMP4 stream.
    ///
    /// # Errors
    ///
    /// - `Err(MuxerError::InvalidInput)` if `sps_nal` or `pps_nal` is empty.
    /// - `Err(MuxerError::AvccError)` if the SPS bytes are malformed or unparseable.
    pub fn build_init_segment(&self, sps_nal: &[u8], pps_nal: &[u8]) -> Result<Vec<u8>, MuxerError> {
        if sps_nal.is_empty() {
            return Err(MuxerError::InvalidInput("sps_nal must not be empty".into()));
        }
        if pps_nal.is_empty() {
            return Err(MuxerError::InvalidInput("pps_nal must not be empty".into()));
        }

        let sps_info = crate::render::avcc::parse_sps(sps_nal)?;

        let ftyp = build_ftyp();
        let moov = build_moov(self.width, self.height, &sps_info, sps_nal, pps_nal)?;

        let mut out = Vec::with_capacity(ftyp.len() + moov.len());
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        Ok(out)
    }

    /// fMP4 timescale used by this muxer. Always 90 000 Hz (matches RTP).
    pub const fn timescale() -> u32 {
        TIMESCALE
    }
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
        // After the 4-byte tag: [4 v+f][4 ctime][4 mtime][4 timescale]
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
        // After the 4-byte tag: [4 v+f][4 pre_defined][4 handler_type]
        let handler_type_offset = hdlr_tag_pos + 4 + 4 + 4;
        assert_eq!(
            &moov[handler_type_offset..handler_type_offset + 4],
            b"vide",
            "hdlr.handler_type must be 'vide'"
        );
    }

    // ─── Capability D: Mp4Muxer::build_init_segment ─────────────────────────

    /// SPS: Baseline Level 3.1, 1280×720 progressive.
    const SPS_1280X720: &[u8] = &[0x67, 0x42, 0xC0, 0x1F, 0xF4, 0x02, 0x80, 0x2D, 0xC0];

    #[test]
    #[should_panic(expected = "width must be > 0")]
    fn mp4_muxer_new_zero_width_panics() {
        Mp4Muxer::new(0, 1080, 30, 1);
    }

    #[test]
    #[should_panic(expected = "height must be > 0")]
    fn mp4_muxer_new_zero_height_panics() {
        Mp4Muxer::new(1920, 0, 30, 1);
    }

    #[test]
    fn init_segment_first_8_bytes_are_ftyp_box() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        // bytes[0..4] = ftyp box size; bytes[4..8] = 'ftyp' tag
        assert_eq!(&bytes[4..8], b"ftyp", "init segment must start with ftyp box tag");
    }

    #[test]
    fn init_segment_ftyp_major_brand_is_iso5() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert_eq!(&bytes[8..12], b"iso5");
    }

    #[test]
    fn init_segment_contains_moov_box() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(bytes.windows(4).any(|w| w == b"moov"), "init segment must contain moov box");
    }

    #[test]
    fn init_segment_avc1_sample_entry_present() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(bytes.windows(4).any(|w| w == b"avc1"), "init segment must contain avc1 sample entry");
    }

    #[test]
    fn init_segment_length_greater_than_200_bytes() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(bytes.len() > 200, "init segment must be > 200 bytes, got {}", bytes.len());
    }

    #[test]
    fn init_segment_empty_sps_returns_error() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        assert!(muxer.build_init_segment(&[], MINIMAL_PPS).is_err());
    }

    #[test]
    fn init_segment_empty_pps_returns_error() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        assert!(muxer.build_init_segment(SPS_320X240, &[]).is_err());
    }

    #[test]
    fn init_segment_is_deterministic() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes1 = muxer.build_init_segment(SPS_320X240, MINIMAL_PPS).expect("ok");
        let bytes2 = muxer.build_init_segment(SPS_320X240, MINIMAL_PPS).expect("ok");
        assert_eq!(bytes1, bytes2, "build_init_segment must be deterministic");
    }

    #[test]
    fn init_segment_mvhd_timescale_big_endian_90000() {
        let muxer = Mp4Muxer::new(1280, 720, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_1280X720, MINIMAL_PPS)
            .expect("should build init segment");

        let mvhd_tag_pos = bytes
            .windows(4)
            .position(|w| w == b"mvhd")
            .expect("mvhd must be in init segment");

        // After tag: [4 v+f][4 ctime][4 mtime][4 timescale]
        let ts_offset = mvhd_tag_pos + 4 + 4 + 4 + 4;
        let timescale = u32::from_be_bytes([
            bytes[ts_offset],
            bytes[ts_offset + 1],
            bytes[ts_offset + 2],
            bytes[ts_offset + 3],
        ]);
        assert_eq!(timescale, 90_000);
    }

    #[test]
    fn init_segment_avc_codec_bytes_match_baseline_13() {
        // SPS_320X240: profile_idc=66=0x42, constraint_set_flags=0xC0, level_idc=13=0x0D
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");

        let avcc_tag_pos = bytes
            .windows(4)
            .position(|w| w == b"avcC")
            .expect("avcC must be in init segment");

        let payload_start = avcc_tag_pos + 4;
        assert_eq!(bytes[payload_start], 1, "configurationVersion must be 1");
        assert_eq!(bytes[payload_start + 1], 0x42, "profile must be 0x42 (baseline)");
        assert_eq!(bytes[payload_start + 2], 0xC0, "constraint_set_flags must be 0xC0");
        assert_eq!(bytes[payload_start + 3], 0x0D, "level must be 0x0D");
    }

    #[test]
    fn init_segment_mdhd_timescale_is_90000() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let bytes = muxer
            .build_init_segment(SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");

        let mdhd_tag_pos = bytes
            .windows(4)
            .position(|w| w == b"mdhd")
            .expect("mdhd must be in init segment");

        let ts_offset = mdhd_tag_pos + 4 + 4 + 4 + 4;
        let timescale = u32::from_be_bytes([
            bytes[ts_offset],
            bytes[ts_offset + 1],
            bytes[ts_offset + 2],
            bytes[ts_offset + 3],
        ]);
        assert_eq!(timescale, 90_000, "mdhd.timescale must be 90_000");
    }
}
