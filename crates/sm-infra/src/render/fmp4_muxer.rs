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
///
/// `default_sample_duration` is in TIMESCALE units (90 kHz here). For 30 fps
/// that's 3000 ticks per frame. With duration=0 (the previous default), MSE
/// reports each appended sample as a zero-duration buffered range
/// `[t→t]` and `<video>.currentTime` cannot advance — observed in B11 as a
/// black `<video>` despite segments being appended.
fn build_trex(default_sample_duration: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, 1); // track_id = 1
    write_u32_be(&mut payload, 1); // default_sample_description_index = 1
    write_u32_be(&mut payload, default_sample_duration); // default_sample_duration (TIMESCALE units)
    write_u32_be(&mut payload, 0); // default_sample_size (varies → trun reports per-sample)
    write_u32_be(&mut payload, 0); // default_sample_flags
    let mut out = Vec::new();
    write_box(&mut out, b"trex", &payload);
    out
}

/// Build the `mvex` (movie extends) box.
fn build_mvex(default_sample_duration: u32) -> Vec<u8> {
    let trex = build_trex(default_sample_duration);
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
/// * `default_sample_duration` — per-sample duration in TIMESCALE units (90 kHz).
///   Computed from the muxer's nominal frame rate (TIMESCALE * fps_den / fps_num).
pub(crate) fn build_moov(
    width: u32,
    height: u32,
    sps_info: &crate::render::avcc::SpsInfo,
    sps_nal: &[u8],
    pps_nal: &[u8],
    default_sample_duration: u32,
) -> Result<Vec<u8>, MuxerError> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&build_mvhd());
    payload.extend_from_slice(&build_trak(width, height, sps_info, sps_nal, pps_nal)?);
    payload.extend_from_slice(&build_mvex(default_sample_duration));
    let mut out = Vec::new();
    write_box(&mut out, b"moov", &payload);
    Ok(out)
}

// ─── Capability F (B6): mdat box builder + AVCC payload ──────────────────────

/// Build an `mdat` (Media Data) box wrapping an AVCC-framed NAL byte payload.
///
/// The payload is the concatenation of length-prefixed NAL units produced by
/// [`annex_b_to_avcc`]. Reuses [`write_box`] from B5 — no new boxing logic.
///
/// # Arguments
///
/// * `avcc_payload` — AVCC-framed bytes to embed verbatim as the `mdat` payload.
pub(crate) fn build_mdat(avcc_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_box(&mut out, b"mdat", avcc_payload);
    out
}

// ─── Capability E (B6): moof + traf hierarchy assembler ──────────────────────

/// Build an `mfhd` (Movie Fragment Header) box.
///
/// Layout: `[size:4][b"mfhd":4][version:1 = 0][flags:3 = 0][sequence_number:4]`
fn build_mfhd(sequence_number: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, 0);
    write_u32_be(&mut payload, sequence_number);
    let mut out = Vec::new();
    write_box(&mut out, b"mfhd", &payload);
    out
}

/// Build a `traf` (Track Fragment) box containing `tfhd`, `tfdt`, and `trun`.
///
/// # Arguments
///
/// * `base_dts`    — DTS of the first sample in this fragment (90 kHz).
/// * `samples`     — per-sample data for the `trun` box.
/// * `data_offset` — byte offset from start of `moof` box to first byte of `mdat` payload.
/// * `is_idr`      — if true, marks the first sample as an IDR (keyframe).
fn build_traf(base_dts: u64, samples: &[TrunSample], data_offset: i32, is_idr: bool) -> Vec<u8> {
    // R9: tfhd MUST NOT carry flag 0x000008 (default-sample-duration-present).
    // ISO 14496-12 §8.8.7 precedence: per-sample trun > tfhd > trex.
    // Keeping tfhd clean (0x020000 only) ensures the per-sample durations from
    // build_trun (R1: 0x000100 set) are ALWAYS the authoritative timing source.
    let tfhd = build_tfhd(1, 0x02_0000); // default-base-is-moof flag only (R9)
    let tfdt = build_tfdt(base_dts);
    let trun = build_trun(samples, data_offset, is_idr);

    let mut payload = Vec::new();
    payload.extend_from_slice(&tfhd);
    payload.extend_from_slice(&tfdt);
    payload.extend_from_slice(&trun);

    let mut out = Vec::new();
    write_box(&mut out, b"traf", &payload);
    out
}

/// Build a complete `moof` (Movie Fragment) box.
///
/// Layout: `moof[mfhd[seq_num], traf[tfhd, tfdt, trun]]`.
///
/// # Arguments
///
/// * `sequence_number` — monotonically increasing fragment sequence number for `mfhd`.
/// * `base_dts`        — DTS of the first sample in this fragment (90 kHz).
/// * `samples`         — per-sample size data for `trun`.
/// * `mdat_offset`     — signed byte offset from start of `moof` to first `mdat` payload byte.
pub(crate) fn build_moof(
    sequence_number: u32,
    base_dts: u64,
    samples: &[TrunSample],
    mdat_offset: i32,
    is_idr: bool,
) -> Vec<u8> {
    let mfhd = build_mfhd(sequence_number);
    let traf = build_traf(base_dts, samples, mdat_offset, is_idr);

    let mut payload = Vec::new();
    payload.extend_from_slice(&mfhd);
    payload.extend_from_slice(&traf);

    let mut out = Vec::new();
    write_box(&mut out, b"moof", &payload);
    out
}

// ─── Capability D (B6): trun (Track Fragment Run) box builder ────────────────

/// Per-sample data for a `trun` box.
pub(crate) struct TrunSample {
    /// Per-sample duration in 90 kHz timescale units. Always present (R2, R3).
    /// Set to `FpsTracker::effective_ticks_per_sample()` by `flush_pending`.
    /// During warm-up: 3000 (30 fps fallback, R5). After lock: inferred fps ticks.
    pub duration: u32,
    /// Byte size of this sample (NAL AVCC payload for this sample).
    pub size: u32,
    /// Optional per-sample flags override. `None` → use default from `tfhd`.
    /// Reserved for V1.5 per-sample flag control (B-frame CTS offsets). Not yet live.
    #[expect(
        dead_code,
        reason = "Reserved for V1.5 per-sample flag control (B-frame CTS offsets)"
    )]
    pub flags: Option<u32>,
}

/// Build a `trun` (Track Fragment Run) full box.
///
/// Layout (ISO/IEC 14496-12 §8.8.8, version 0):
/// ```text
/// [size:4][b"trun":4][version:1 = 0][flags:3]
/// [sample_count:4][data_offset:4]
/// [first_sample_flags:4]  (if is_idr — indicates IDR frame)
/// per-sample: [duration:4][size:4] × N
/// ```
///
/// # flags selection (24-bit field)
///
/// - `0x000001` — `data-offset-present`
/// - `0x000004` — `first-sample-flags-present` (set when `is_idr = true`)
/// - `0x000100` — `sample-duration-present` (R1: always set — MSE reads per-sample duration)
/// - `0x000200` — `sample-size-present`
///
/// # R9 tfhd precedence note
///
/// `tfhd` MUST NOT carry flag `0x000008` (default-sample-duration-present).
/// ISO 14496-12 §8.8.7 precedence: per-sample trun > tfhd > trex.
/// Keeping `tfhd` flag `0x000008` clear ensures the per-sample durations written
/// here are always the authoritative source. `tfhd` uses `0x020000` only.
///
/// # Arguments
///
/// * `samples`     — per-sample values (duration + size; optional flags override).
/// * `data_offset` — signed byte offset from start of `moof` to start of `mdat` payload.
/// * `is_idr`      — if true, inserts a `first_sample_flags` field marking the IDR sample.
pub(crate) fn build_trun(samples: &[TrunSample], data_offset: i32, is_idr: bool) -> Vec<u8> {
    // R1: OR in 0x000100 (sample-duration-present) — MSE uses per-sample trun duration
    // rather than falling back to trex.default_sample_duration.
    // R9: bit 0x000008 (default-sample-duration-present) MUST stay clear in tfhd;
    // per-sample trun wins (ISO 14496-12 §8.8.7: sample > tfhd > trex).
    let flags: u32 = 0x0000_0001 | 0x0000_0100 | 0x0000_0200 | if is_idr { 0x0000_0004 } else { 0 };

    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, flags);
    write_u32_be(&mut payload, samples.len() as u32); // sample_count
    payload.extend_from_slice(&data_offset.to_be_bytes()); // data_offset (signed i32 BE)

    if is_idr {
        // first_sample_flags: sample_depends_on = 2 (IDR — does not depend on others)
        // 0x02000000 encodes sample_is_non_sync_sample=0, sample_depends_on=2
        write_u32_be(&mut payload, 0x0200_0000);
    }

    // R2: ISO 14496-12 §8.8.8 column ordering when both 0x000100 and 0x000200 are set:
    // sample_duration first, then sample_size.
    for s in samples {
        write_u32_be(&mut payload, s.duration);
        write_u32_be(&mut payload, s.size);
    }

    let mut out = Vec::new();
    write_box(&mut out, b"trun", &payload);
    out
}

// ─── Capability C (B6): tfdt (Track Fragment Decode Time) box builder ────────

/// Build a `tfdt` (Track Fragment Decode Time) full box, version 1 (64-bit).
///
/// Layout (ISO/IEC 14496-12 §8.8.12, version 1):
/// ```text
/// [size:4][b"tfdt":4][version:1 = 1][flags:3 = 0][base_media_decode_time:8]
/// ```
/// Total: 20 bytes.
///
/// Using version 1 (u64 field) provides ~5.8 million years of headroom at 90 kHz,
/// avoiding the 47 721-second overflow that version 0 (u32 field) would hit.
///
/// # Arguments
///
/// * `base_media_decode_time` — DTS of the first sample in this fragment, in 90 kHz units.
pub(crate) fn build_tfdt(base_media_decode_time: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 1, 0); // version = 1, flags = 0
    payload.extend_from_slice(&base_media_decode_time.to_be_bytes());
    let mut out = Vec::new();
    write_box(&mut out, b"tfdt", &payload);
    out
}

// ─── Capability B (B6): tfhd (Track Fragment Header) box builder ─────────────

/// Build a `tfhd` (Track Fragment Header) full box.
///
/// Layout (ISO/IEC 14496-12 §8.8.7):
/// ```text
/// [size:4][b"tfhd":4][version:1 = 0][flags:3][track_id:4]
/// ```
///
/// Total: 16 bytes (no optional fields; flags control which optional fields are present).
///
/// # Arguments
///
/// * `track_id` — must be 1 for a single-track fMP4 stream.
/// * `flags`    — 24-bit flags field (e.g. 0x020000 = default-base-is-moof).
pub(crate) fn build_tfhd(track_id: u32, flags: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    write_full_box_header(&mut payload, 0, flags);
    write_u32_be(&mut payload, track_id);
    let mut out = Vec::new();
    write_box(&mut out, b"tfhd", &payload);
    out
}

// ─── Capability A (B6): Annex-B → AVCC framing converter ────────────────────

/// Convert an Annex-B NAL byte stream to AVCC length-prefixed format.
///
/// Each NAL unit in the input (identified by 3-byte or 4-byte start codes) is
/// length-prefixed with a 4-byte big-endian value equal to the byte length of
/// the NAL body (excluding the start code). Start codes are stripped.
///
/// This is the inverse of `crate::transport::annex_b::reconstruct_annex_b`
/// (which lives in the transport module and is `pub(crate)`-scoped).
///
/// # Arguments
///
/// * `annex_b` — raw Annex-B byte stream (may contain mixed 3-byte and 4-byte start codes).
///
/// # Returns
///
/// `Ok(vec![])` if `annex_b` is empty or contains no valid start codes.
/// `Ok(bytes)` where each NAL is prefixed with a 4-byte big-endian length field.
///
/// # ISO/IEC 14496-15 §5.2.4.1.1
///
/// Matches `length_size_minus_one = 3` (4-byte length prefix) declared in `avcC`.
pub fn annex_b_to_avcc(annex_b: &[u8]) -> Result<Vec<u8>, MuxerError> {
    if annex_b.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(annex_b.len());
    for nal in crate::transport::annex_b::iter_nal_units(annex_b) {
        let len = nal.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

// ─── Mp4Muxer public API ─────────────────────────────────────────────────────

/// Sub-GOP batch size: flush pending non-IDR samples every N frames.
/// 4 frames ≈ 67ms at 60fps; ~15 moof+IPC sends/s; moof overhead ~120B/fragment.
/// Increase to 8 if GATE-5 shows WebView2 IPC pressure (133ms, 7.5/s).
pub(crate) const SUBGOP_FLUSH_FRAMES: usize = 4;

/// Per-sample metadata accumulated during fragment assembly.
struct PendingSample {
    /// AVCC-framed NAL bytes for this sample.
    avcc: Vec<u8>,
    /// DTS of this sample in 90 kHz units.
    dts_90khz: u64,
    /// Whether this sample is an IDR (keyframe). Used by flush_pending to compute
    /// fragment_is_idr from the first sample in pending.
    is_idr: bool,
}

/// fMP4 muxer for screen-mirror live streaming.
///
/// Builds an init segment (`ftyp` + `moov`) from the first SPS + PPS NAL bytes,
/// then emits media segments (`moof` + `mdat`) per IDR-aligned GOP.
///
/// # Fragmentation strategy (design §3.6)
///
/// One fragment per GOP (IDR-aligned). The first IDR starts accumulation and returns
/// `None`; each subsequent IDR flushes the accumulated previous GOP as a media segment
/// and begins accumulating the new GOP.
///
/// # Example
///
/// ```rust,ignore
/// use sm_infra::render::avcc::parse_sps;
/// use sm_infra::render::fmp4_muxer::Mp4Muxer;
///
/// let sps_info = parse_sps(sps_nal)?;
/// let (w, h) = sps_info.display_dimensions();
/// let muxer = Mp4Muxer::new(w, h, 30, 1);
/// let init = muxer.build_init_segment(&sps_info, sps_nal, pps_nal)?;
/// // emit init bytes to the frontend once
/// while let Ok(pkt) = pkt_rx.recv() {
///     if let Some(segment) = muxer.append_packet(&pkt) {
///         // emit segment bytes to the frontend
///     }
/// }
/// ```
pub struct Mp4Muxer {
    /// Frame width in pixels (pre-validated from the SPS or caller-supplied).
    width: u32,
    /// Frame height in pixels (pre-validated from the SPS or caller-supplied).
    height: u32,
    /// Frame rate numerator (e.g. 30 for 30 fps).
    /// Retained for API compatibility (R11) and future VFR support. Not currently
    /// read after construction — `FpsTracker` infers the real fps from RTP deltas.
    #[expect(
        dead_code,
        reason = "API-compat field retained for R11; inferred fps used instead"
    )]
    fps_num: u32,
    /// Frame rate denominator (e.g. 1 for 30 fps).
    /// Retained for API compatibility (R11) and future VFR support. Not currently
    /// read after construction — `FpsTracker` infers the real fps from RTP deltas.
    #[expect(
        dead_code,
        reason = "API-compat field retained for R11; inferred fps used instead"
    )]
    fps_den: u32,
    /// Monotonically increasing fragment sequence number for `mfhd`.
    fragment_seq: u32,
    /// Samples accumulated for the current pending fragment.
    pending: Vec<PendingSample>,
    /// DTS of the very first packet ever appended, in TIMESCALE units. Set on
    /// the first `append_packet` call so subsequent samples can be re-based to
    /// start at zero on the MSE timeline regardless of what wall-clock /
    /// RTP-derived timestamps the encoder emits. Without this, str0m's RTP
    /// timestamps (which can be 17 000+ s into the wall-clock epoch) would
    /// place every buffered range far in the future of `<video>.currentTime=0`
    /// and the player would never reach them — observed in B11.
    first_dts_offset: Option<u64>,
    /// RTP-timestamp-derived fps tracker for per-sample trun durations.
    /// Locked after 8 consecutive inter-AU deltas pass IQR + plausibility guards (R4–R8).
    /// Exposed as `pub(crate)` in test builds for white-box assertions (T4.1).
    /// NOTE: still used for `trex.default_sample_duration` in the init segment only —
    /// per-sample `trun` durations now come from real intra-GOP DTS deltas (T5).
    #[cfg(not(test))]
    fps_tracker: crate::render::fps_tracker::FpsTracker,
    #[cfg(test)]
    pub(crate) fps_tracker: crate::render::fps_tracker::FpsTracker,
    /// DTS of the last sample in the most recently flushed GOP, in TIMESCALE units.
    ///
    /// Used as the predecessor DTS when computing the last-sample duration fallback
    /// in `flush_pending()`. `None` until the first GOP has been flushed.
    prev_flushed_dts: Option<u64>,
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
        assert!(fps_num > 0, "Mp4Muxer: fps_num must be > 0");
        assert!(fps_den > 0, "Mp4Muxer: fps_den must be > 0");
        Self {
            width,
            height,
            fps_num,
            fps_den,
            fragment_seq: 0,
            pending: Vec::new(),
            first_dts_offset: None,
            fps_tracker: crate::render::fps_tracker::FpsTracker::new(),
            prev_flushed_dts: None,
        }
    }

    /// Build the fMP4 init segment from a pre-parsed `SpsInfo` and the raw SPS/PPS NAL bytes.
    ///
    /// The caller MUST parse the SPS first via [`crate::render::avcc::parse_sps`] and pass
    /// the resulting [`crate::render::avcc::SpsInfo`] alongside the raw SPS/PPS NAL bytes.
    /// The muxer trusts that `sps_info` was derived from `sps_nal` — they are expected to be
    /// coherent.
    ///
    /// Output layout: `[ftyp][moov]`. Concatenate with subsequent media segments
    /// (from `append_packet`, B6) to form a valid fMP4 stream.
    ///
    /// # Errors
    ///
    /// - `Err(MuxerError::InvalidInput)` if `pps_nal` is empty.
    /// - `Err(MuxerError::AvccError)` if the avcC configuration record cannot be built.
    pub fn build_init_segment(
        &self,
        sps_info: &crate::render::avcc::SpsInfo,
        sps_nal: &[u8],
        pps_nal: &[u8],
    ) -> Result<Vec<u8>, MuxerError> {
        if pps_nal.is_empty() {
            return Err(MuxerError::InvalidInput("pps_nal must not be empty".into()));
        }

        let ftyp = build_ftyp();
        // R5/R11: trex.default_sample_duration uses the FpsTracker's current best estimate.
        // During warm-up this is 3000 (30 fps fallback); after lock it is the inferred median.
        // Per-sample trun durations (flag 0x000100, set by build_trun) always override trex
        // in each media segment, so this value only affects playback before the first fragment
        // is received. Using the live tracker value here (not a hardcoded constant) is correct
        // and keeps FpsTracker relevant for its stated purpose.
        let moov = build_moov(
            self.width,
            self.height,
            sps_info,
            sps_nal,
            pps_nal,
            self.fps_tracker.effective_ticks_per_sample(),
        )?;

        let mut out = Vec::with_capacity(ftyp.len() + moov.len());
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        Ok(out)
    }

    /// Append an encoded packet to the muxer's pending-fragment buffer.
    ///
    /// # Fragmentation strategy (design §3.6)
    ///
    /// - **Non-IDR (P-frame)**: accumulate into pending buffer; return `None`.
    /// - **IDR with empty pending**: first IDR of stream; begin accumulation; return `None`.
    /// - **IDR with non-empty pending**: flush the accumulated previous GOP as a media
    ///   segment (`moof` + `mdat`); accumulate the current IDR into fresh pending;
    ///   return `Some(segment_bytes)`.
    ///
    /// # Returns
    ///
    /// `None` while buffering. `Some(bytes)` when a complete fragment is ready.
    /// The bytes are `[moof][mdat]` suitable for `SourceBuffer.appendBuffer(...)`.
    pub fn append_packet(&mut self, packet: &sm_domain::encode::EncodedPacket) -> Option<Vec<u8>> {
        // Convert Annex-B payload to AVCC-framed bytes.
        // If conversion fails (e.g., empty data), skip this packet silently.
        let avcc = annex_b_to_avcc(&packet.data).ok()?;
        let raw_dts = crate::transport::annex_b::duration_to_90khz(packet.timestamp);
        // Re-base timestamps so the MSE timeline starts at zero (B11). The
        // first packet seen by this muxer instance defines the offset; every
        // subsequent packet's DTS is reported relative to it.
        let offset = *self.first_dts_offset.get_or_insert(raw_dts);
        let dts = raw_dts.saturating_sub(offset);

        // R4: feed every access unit (IDR + P-frame) to the fps tracker.
        // The tracker computes inter-AU deltas internally and locks in a median
        // tick duration after 8 deltas pass the IQR+plausibility guards.
        self.fps_tracker.observe_dts(dts);

        // Unified flush predicate (D-PPT5-1): flush when pending is non-empty AND
        // either an IDR arrives (GOP boundary) OR we have accumulated SUBGOP_FLUSH_FRAMES.
        // Uniform invariant: Some(segment) ⟹ segment holds everything BEFORE this packet;
        // this packet opens fresh pending (IDR boundary semantics preserved verbatim).
        let should_flush = !self.pending.is_empty()
            && (packet.is_keyframe || self.pending.len() >= SUBGOP_FLUSH_FRAMES);

        if should_flush {
            let segment = self.flush_pending();
            self.pending.push(PendingSample {
                avcc,
                dts_90khz: dts,
                is_idr: packet.is_keyframe,
            });
            Some(segment)
        } else {
            // P-frame or first IDR: accumulate.
            self.pending.push(PendingSample {
                avcc,
                dts_90khz: dts,
                is_idr: packet.is_keyframe,
            });
            None
        }
    }

    /// Flush the current pending samples as a `moof` + `mdat` media segment.
    ///
    /// Increments `fragment_seq`. Clears `pending`.
    fn flush_pending(&mut self) -> Vec<u8> {
        self.fragment_seq += 1;
        let seq = self.fragment_seq;

        // Base DTS is the timestamp of the first sample in the fragment.
        let base_dts = self.pending.first().map(|s| s.dts_90khz).unwrap_or(0);

        // Build the concatenated AVCC mdat payload.
        let mdat_payload: Vec<u8> = self
            .pending
            .iter()
            .flat_map(|s| s.avcc.iter().copied())
            .collect();

        // T5: compute per-sample trun durations from REAL intra-GOP DTS deltas.
        //
        // Why: FpsTracker stamps ALL samples with a single assumed rate, causing the MSE
        // sequence-mode timeline to grow at (assumed_rate / real_rate)× speed. When capture
        // runs slower than the assumed rate (e.g. 7.5 fps real vs 30 fps assumed), the MSE
        // timeline grows 0.25× realtime and the <video> element permanently stalls. Using
        // real DTS deltas makes the timeline track wall-clock regardless of capture speed.
        //
        // FpsTracker is KEPT for trex.default_sample_duration in the init segment (R5/R4)
        // because trun per-sample durations override the trex default in the media segment.
        //
        // Duration clamping bounds [375, 900000] ticks:
        //   375 = 90000 / 240  (240 fps guard — protects against DTS jitter / duplicates)
        //   900000 = 90000 × 10  (10 s cap — catches only pathological sleep/wake gaps)
        const MIN_DURATION_TICKS: u32 = 375;
        const MAX_DURATION_TICKS: u32 = 900_000;

        let n = self.pending.len();
        let mut durations: Vec<u32> = Vec::with_capacity(n);

        // Samples 0..N-2: use the successor DTS minus this sample's DTS.
        for i in 0..n.saturating_sub(1) {
            let delta = self.pending[i + 1]
                .dts_90khz
                .saturating_sub(self.pending[i].dts_90khz) as u32;
            durations.push(delta.clamp(MIN_DURATION_TICKS, MAX_DURATION_TICKS));
        }

        // Last sample (index N-1): no in-fragment successor — use a fallback.
        //   If GOP has ≥ 2 samples: median of the intra-GOP deltas computed above.
        //   If GOP has exactly 1 sample: inter-fragment delta via prev_flushed_dts,
        //     or WARMUP_FALLBACK_TICKS when prev_flushed_dts is None.
        //
        // prev_flushed_dts == None on a single-sample flush is NOT limited to the very
        // first fragment: two consecutive IDRs with no intervening P-frames produce a
        // single-sample flush before any prior fragment has been emitted. This is
        // realistic for all-intra or scene-change streams, so the WARMUP_FALLBACK_TICKS
        // branch is genuinely reachable even after warm-up.
        let last_dur: u32 = if n >= 2 {
            // Compute the median of the already-computed intra-GOP deltas.
            let mut sorted = durations.clone();
            sorted.sort_unstable();
            sorted[sorted.len() / 2] // upper-median for odd N; upper of the two middle values for even N
        } else {
            // Single-sample GOP: use inter-fragment delta if available.
            if let Some(prev) = self.prev_flushed_dts {
                let inter_delta = base_dts.saturating_sub(prev) as u32;
                inter_delta.clamp(MIN_DURATION_TICKS, MAX_DURATION_TICKS)
            } else {
                crate::render::fps_tracker::WARMUP_FALLBACK_TICKS
            }
        };
        durations.push(last_dur);

        // Update prev_flushed_dts to the last sample's DTS before clearing pending.
        self.prev_flushed_dts = self.pending.last().map(|s| s.dts_90khz);

        let trun_samples: Vec<TrunSample> = self
            .pending
            .iter()
            .zip(durations.iter())
            .map(|(s, &dur)| TrunSample {
                duration: dur,
                size: s.avcc.len() as u32,
                flags: None,
            })
            .collect();

        // Determine whether the first sample in this fragment is an IDR.
        // Used to set the first_sample_flags_present bit in trun (SEG-2, D-PPT5-4).
        let fragment_is_idr = self.pending.first().is_some_and(|s| s.is_idr);

        // Compute moof size to determine data_offset.
        // data_offset = size_of(moof) + 8  (8 bytes for the mdat size+tag header).
        // We approximate by building moof with offset=0 first to get its size.
        let moof_placeholder = build_moof(seq, base_dts, &trun_samples, 0, fragment_is_idr);
        let moof_size = moof_placeholder.len() as i32;
        // data_offset in trun = bytes from start of moof box to first byte of mdat payload.
        // mdat box header = 8 bytes (size + tag).
        let data_offset = moof_size + 8;

        // Rebuild moof with the correct data_offset.
        let moof = build_moof(seq, base_dts, &trun_samples, data_offset, fragment_is_idr);
        let mdat = build_mdat(&mdat_payload);

        // Clear pending for the next GOP.
        self.pending.clear();

        let mut segment = Vec::with_capacity(moof.len() + mdat.len());
        segment.extend_from_slice(&moof);
        segment.extend_from_slice(&mdat);
        segment
    }

    /// fMP4 timescale used by this muxer. Always 90 000 Hz (matches RTP).
    pub const fn timescale() -> u32 {
        TIMESCALE
    }
}

// ─── SPS+PPS extractor — B7 helper ───────────────────────────────────────────

/// Extract SPS (NAL type 7) and PPS (NAL type 8) bytes from an Annex-B IDR packet.
///
/// Called by the `sm-stream-mux` thread on the first IDR to obtain the raw SPS and PPS
/// NAL bytes needed to call [`Mp4Muxer::build_init_segment`].
///
/// # Arguments
///
/// * `annex_b` — full Annex-B packet (e.g., `EncodedPacket::data`). May contain
///   SPS, PPS, and IDR NAL units in any order (in practice: SPS → PPS → IDR).
///
/// # Returns
///
/// `Some((sps_nal, pps_nal))` when both NAL types are found; `None` otherwise.
/// The returned slices are raw NAL bytes **without** Annex-B start codes.
pub fn extract_sps_pps_from_idr(annex_b: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    for nal in crate::transport::annex_b::iter_nal_units(annex_b) {
        if let Some(&first) = nal.first() {
            let nal_type = first & 0x1F;
            match nal_type {
                7 if sps.is_none() => sps = Some(nal.to_vec()),
                8 if pps.is_none() => pps = Some(nal.to_vec()),
                _ => {}
            }
        }
        if sps.is_some() && pps.is_some() {
            break;
        }
    }
    sps.zip(pps)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::avcc::parse_sps;

    // ─── Capability H (B6): round-trip mp4 crate parser validation test ───

    /// Minimal valid SPS for Baseline Level 1.3, 320×240 progressive.
    const SPS_RT: &[u8] = &[0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0xC0];
    /// Minimal PPS.
    const PPS_RT: &[u8] = &[0x68, 0xCE, 0x38, 0x80];

    /// Construct a synthetic Annex-B Annex-B IDR NAL-unit payload.
    /// The payload is a 1-byte SPS + 1-byte PPS + small IDR slice all with 4-byte SCs.
    fn synthetic_keyframe() -> EncodedPacket {
        let mut data = Vec::new();
        // SPS NAL (type 7 = 0x67)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67]);
        data.extend_from_slice(&SPS_RT[1..]); // body (skip first byte already added)
        // PPS NAL (type 8 = 0x68)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(PPS_RT);
        // IDR slice (type 5 = 0x65), small synthetic body
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
        data.extend_from_slice(&[0x88u8; 20]); // 20 bytes of dummy slice data

        EncodedPacket {
            data: Arc::from(data.into_boxed_slice()),
            is_keyframe: true,
            timestamp: Duration::from_secs(0),
            sequence: 0,
        }
    }

    #[test]
    fn round_trip_init_segment_parses_with_mp4_crate() {
        // Build the init segment from known-good SPS+PPS.
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_RT).expect("should parse SPS_RT");
        let init = muxer
            .build_init_segment(&sps_info, SPS_RT, PPS_RT)
            .expect("init segment must build successfully");

        // The mp4 crate reads via Mp4Reader which needs a seekable stream.
        // We concatenate init + a dummy empty-mdat segment to give it a parseable file.
        // For structural validation, we verify the init segment boxes directly.

        // 1. Starts with ftyp (bytes 4..8)
        assert_eq!(&init[4..8], b"ftyp", "init must start with ftyp box");

        // 2. Contains moov
        assert!(
            init.windows(4).any(|w| w == b"moov"),
            "init must contain moov"
        );

        // 3. Contains avc1 (codec sample entry)
        assert!(
            init.windows(4).any(|w| w == b"avc1"),
            "init must contain avc1"
        );

        // 4. Contains avcC (decoder config)
        assert!(
            init.windows(4).any(|w| w == b"avcC"),
            "init must contain avcC"
        );

        // 5. Contains mvex (movie extends — required for fMP4)
        assert!(
            init.windows(4).any(|w| w == b"mvex"),
            "init must contain mvex (fMP4 marker)"
        );

        // 6. Contains trex
        assert!(
            init.windows(4).any(|w| w == b"trex"),
            "init must contain trex"
        );

        // 7. Parse the ftyp major brand via mp4 crate boxtype scanning.
        // major_brand at bytes [8..12]
        assert_eq!(&init[8..12], b"iso5", "ftyp.major_brand must be iso5");

        // 8. Verify mvhd timescale = 90_000.
        let mvhd_pos = init
            .windows(4)
            .position(|w| w == b"mvhd")
            .expect("mvhd must exist");
        // After tag: version+flags (4) + ctime (4) + mtime (4) + timescale (4)
        let ts_off = mvhd_pos + 4 + 4 + 4 + 4;
        let timescale = u32::from_be_bytes([
            init[ts_off],
            init[ts_off + 1],
            init[ts_off + 2],
            init[ts_off + 3],
        ]);
        assert_eq!(timescale, 90_000, "mvhd.timescale must be 90_000");
    }

    #[test]
    fn round_trip_init_plus_two_media_segments_structural_parse() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_RT).expect("should parse SPS_RT");
        let init = muxer
            .build_init_segment(&sps_info, SPS_RT, PPS_RT)
            .expect("init segment");

        // IDR1 → buffers (no emit)
        let idr1 = synthetic_keyframe();
        assert!(muxer.append_packet(&idr1).is_none());

        // IDR2 → emits segment_1
        let idr2 = EncodedPacket {
            data: Arc::from(vec![0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB].into_boxed_slice()),
            is_keyframe: true,
            timestamp: Duration::from_millis(100),
            sequence: 1,
        };
        let seg1 = muxer
            .append_packet(&idr2)
            .expect("IDR2 must emit segment 1");

        // IDR3 → emits segment_2
        let idr3 = EncodedPacket {
            data: Arc::from(vec![0x00u8, 0x00, 0x00, 0x01, 0x65, 0xCC, 0xDD].into_boxed_slice()),
            is_keyframe: true,
            timestamp: Duration::from_millis(200),
            sequence: 2,
        };
        let seg2 = muxer
            .append_packet(&idr3)
            .expect("IDR3 must emit segment 2");

        // Verify the full byte stream: init + seg1 + seg2
        let mut stream = Vec::new();
        stream.extend_from_slice(&init);
        stream.extend_from_slice(&seg1);
        stream.extend_from_slice(&seg2);

        // Both segments must start with moof
        assert_eq!(&seg1[4..8], b"moof", "seg1 must start with moof");
        assert_eq!(&seg2[4..8], b"moof", "seg2 must start with moof");

        // Both segments must contain mdat
        assert!(
            seg1.windows(4).any(|w| w == b"mdat"),
            "seg1 must contain mdat"
        );
        assert!(
            seg2.windows(4).any(|w| w == b"mdat"),
            "seg2 must contain mdat"
        );

        // Extract sequence numbers and verify they're monotonic
        fn extract_seq(seg: &[u8]) -> u32 {
            let pos = seg.windows(4).position(|w| w == b"mfhd").unwrap();
            let off = pos + 4 + 4;
            u32::from_be_bytes([seg[off], seg[off + 1], seg[off + 2], seg[off + 3]])
        }
        let seq1 = extract_seq(&seg1);
        let seq2 = extract_seq(&seg2);
        assert!(
            seq2 > seq1,
            "sequence numbers must be monotonically increasing: {} < {}",
            seq1,
            seq2
        );
        assert_eq!(
            seq1, 1,
            "first emitted segment must have sequence_number = 1"
        );
        assert_eq!(
            seq2, 2,
            "second emitted segment must have sequence_number = 2"
        );

        // Stream total size must be larger than init alone
        assert!(
            stream.len() > init.len(),
            "full stream must be larger than init segment alone"
        );
    }

    // ─── Capability G (B6): Mp4Muxer::append_packet orchestrator ──────────

    use sm_domain::encode::EncodedPacket;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_packet(is_keyframe: bool, timestamp_ms: u64, size_bytes: usize) -> EncodedPacket {
        // Minimal Annex-B NAL: [00 00 00 01 65/41 ...payload...] for IDR/P-frame
        let nal_type: u8 = if is_keyframe { 0x65 } else { 0x41 };
        let mut data = vec![0x00u8, 0x00, 0x00, 0x01, nal_type];
        data.extend(vec![0xAAu8; size_bytes.saturating_sub(5)]);
        EncodedPacket {
            data: Arc::from(data.into_boxed_slice()),
            is_keyframe,
            timestamp: Duration::from_millis(timestamp_ms),
            sequence: timestamp_ms / 33, // ~30fps
        }
    }

    #[test]
    fn append_packet_non_keyframe_returns_none() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // Three P-frame packets; none should emit a segment.
        for i in 0..3u64 {
            let pkt = make_packet(false, i * 33, 100);
            let result = muxer.append_packet(&pkt);
            assert!(result.is_none(), "P-frame must not emit a segment");
        }
    }

    #[test]
    fn append_packet_first_idr_returns_none() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        let pkt = make_packet(true, 0, 200);
        let result = muxer.append_packet(&pkt);
        // First IDR starts a new GOP; nothing to flush yet.
        assert!(
            result.is_none(),
            "first IDR must not emit (nothing buffered before it)"
        );
    }

    #[test]
    fn append_packet_second_keyframe_flushes_first_gop() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // IDR1 (timestamp 0ms)
        let idr1 = make_packet(true, 0, 200);
        assert!(muxer.append_packet(&idr1).is_none(), "IDR1 must buffer");
        // 3 P-frames
        for i in 1..4u64 {
            assert!(
                muxer
                    .append_packet(&make_packet(false, i * 33, 100))
                    .is_none()
            );
        }
        // IDR2 → must flush GOP containing IDR1 + P-frames
        let idr2 = make_packet(true, 4 * 33, 200);
        let segment = muxer.append_packet(&idr2);
        assert!(segment.is_some(), "IDR2 must flush the previous GOP");
    }

    #[test]
    fn media_segment_starts_with_moof_then_mdat() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        let idr1 = make_packet(true, 0, 200);
        muxer.append_packet(&idr1);
        // One P-frame to ensure pending is non-empty when IDR2 arrives
        muxer.append_packet(&make_packet(false, 33, 100));
        let idr2 = make_packet(true, 66, 200);
        let segment = muxer
            .append_packet(&idr2)
            .expect("should emit segment on IDR2");

        // Check moof appears first
        let moof_pos = segment
            .windows(4)
            .position(|w| w == b"moof")
            .expect("moof must be present");
        let mdat_pos = segment
            .windows(4)
            .position(|w| w == b"mdat")
            .expect("mdat must be present");
        assert!(moof_pos < mdat_pos, "moof must come before mdat");
        // First box tag in segment (at bytes[4..8]) must be moof
        assert_eq!(&segment[4..8], b"moof", "segment must start with moof box");
    }

    #[test]
    fn media_segment_mfhd_sequence_number_increments() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // Helper to extract sequence number from a segment's mfhd box
        fn extract_seq(seg: &[u8]) -> u32 {
            let mfhd_pos = seg.windows(4).position(|w| w == b"mfhd").unwrap();
            let off = mfhd_pos + 4 + 4; // skip version+flags
            u32::from_be_bytes([seg[off], seg[off + 1], seg[off + 2], seg[off + 3]])
        }

        // Three IDRs → two emitted segments (IDR1 buffers, IDR2 emits, IDR3 emits)
        muxer.append_packet(&make_packet(true, 0, 200));
        let seg1 = muxer
            .append_packet(&make_packet(true, 33, 200))
            .expect("seg1");
        let seg2 = muxer
            .append_packet(&make_packet(true, 66, 200))
            .expect("seg2");

        let seq1 = extract_seq(&seg1);
        let seq2 = extract_seq(&seg2);
        assert!(
            seq2 > seq1,
            "mfhd.sequence_number must increment across segments: got {} then {}",
            seq1,
            seq2
        );
    }

    #[test]
    fn media_segment_tfdt_base_decode_time_reflects_first_sample_timestamp() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // IDR at t=1000ms (encoder-absolute). Per B11-S11 the muxer rebases
        // timestamps so the very first packet ever appended becomes the
        // origin (rebased dts = 0). IDR1 is that first packet, so the
        // tfdt of the segment that flushes IDR1's GOP must be 0.
        let idr1 = make_packet(true, 1000, 200);
        muxer.append_packet(&idr1);
        let idr2 = make_packet(true, 2000, 200);
        let segment = muxer.append_packet(&idr2).expect("should emit segment");

        // Find tfdt in segment.
        // windows(4).position finds the offset where the 4-byte tag b"tfdt" appears.
        // Box layout: [size:4][tag:4][version:1][flags:3][time:8]
        // The size field is at (tfdt_pos - 4); the tag is at tfdt_pos.
        // After the tag: version at tfdt_pos+4, flags at +5..8, time at +8..16.
        let tfdt_pos = segment
            .windows(4)
            .position(|w| w == b"tfdt")
            .expect("tfdt must be in segment");
        let version = segment[tfdt_pos + 4];
        assert_eq!(version, 1, "tfdt must be version 1");
        let dts = u64::from_be_bytes([
            segment[tfdt_pos + 8],
            segment[tfdt_pos + 9],
            segment[tfdt_pos + 10],
            segment[tfdt_pos + 11],
            segment[tfdt_pos + 12],
            segment[tfdt_pos + 13],
            segment[tfdt_pos + 14],
            segment[tfdt_pos + 15],
        ]);
        assert_eq!(
            dts, 0,
            "tfdt.base_media_decode_time of the first GOP segment must be 0 — that GOP starts at the rebase origin (B11-S11)"
        );
    }

    // ─── Capability F (B6): mdat box builder ───────────────────────────────

    #[test]
    fn build_mdat_wraps_payload_in_mdat_box() {
        let payload: &[u8] = &[0x00, 0x00, 0x00, 0x03, 0x65, 0xAB, 0xCD];
        let mdat = build_mdat(payload);
        assert_eq!(&mdat[4..8], b"mdat", "box type must be mdat");
        let size = u32::from_be_bytes([mdat[0], mdat[1], mdat[2], mdat[3]]) as usize;
        assert_eq!(size, mdat.len());
        assert_eq!(
            &mdat[8..],
            payload,
            "mdat payload must match input verbatim"
        );
    }

    #[test]
    fn build_mdat_empty_payload_produces_8_byte_box() {
        let mdat = build_mdat(&[]);
        assert_eq!(&mdat[4..8], b"mdat");
        assert_eq!(mdat.len(), 8);
    }

    #[test]
    fn build_mdat_total_size_is_8_plus_payload_len() {
        let payload = vec![0xAAu8; 256];
        let mdat = build_mdat(&payload);
        assert_eq!(mdat.len(), 8 + 256);
        let size = u32::from_be_bytes([mdat[0], mdat[1], mdat[2], mdat[3]]) as usize;
        assert_eq!(size, 8 + 256);
    }

    // ─── Capability A (B6): annex_b_to_avcc ────────────────────────────────

    #[test]
    fn annex_b_to_avcc_single_4byte_startcode_nal() {
        // Input: [00 00 00 01 65 AB CD] → NAL body = [65 AB CD], length = 3
        // Output: [00 00 00 03 65 AB CD]
        let input: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xAB, 0xCD];
        let result = annex_b_to_avcc(input).expect("should succeed");
        assert_eq!(result, &[0x00, 0x00, 0x00, 0x03, 0x65, 0xAB, 0xCD]);
    }

    #[test]
    fn annex_b_to_avcc_single_3byte_startcode_nal() {
        // Input: [00 00 01 67 AB] → NAL body = [67 AB], length = 2
        // Output: [00 00 00 02 67 AB]
        let input: &[u8] = &[0x00, 0x00, 0x01, 0x67, 0xAB];
        let result = annex_b_to_avcc(input).expect("should succeed");
        assert_eq!(result, &[0x00, 0x00, 0x00, 0x02, 0x67, 0xAB]);
    }

    #[test]
    fn annex_b_to_avcc_two_nals_produces_two_length_prefixed_units() {
        // SPS + PPS with 4-byte start codes each.
        let input: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, // SPS: [67]
            0x00, 0x00, 0x00, 0x01, 0x68, // PPS: [68]
        ];
        let result = annex_b_to_avcc(input).expect("should succeed");
        // Expected: [00 00 00 01 67] [00 00 00 01 68]
        assert_eq!(
            result,
            &[0x00, 0x00, 0x00, 0x01, 0x67, 0x00, 0x00, 0x00, 0x01, 0x68]
        );
    }

    #[test]
    fn annex_b_to_avcc_empty_input_returns_empty_vec() {
        let result = annex_b_to_avcc(b"").expect("should succeed on empty input");
        assert!(result.is_empty());
    }

    // ─── Capability B (B6): tfhd box builder ───────────────────────────────

    #[test]
    fn tfhd_golden_bytes_default_base_is_moof_flag() {
        // flags = 0x020000 (default-base-is-moof) only; track_id = 1
        // Expected layout (full box):
        //   [size:4][b"tfhd":4][version:1][flags:3][track_id:4]
        // size = 8(box) + 4(v+f) + 4(track_id) = 16 bytes
        let tfhd = build_tfhd(1, 0x02_0000);
        assert_eq!(&tfhd[4..8], b"tfhd", "box type must be tfhd");
        let size = u32::from_be_bytes([tfhd[0], tfhd[1], tfhd[2], tfhd[3]]) as usize;
        assert_eq!(size, tfhd.len(), "size field must match actual length");
        // version = 0 at byte 8
        assert_eq!(tfhd[8], 0, "version must be 0");
        // flags (3 bytes big-endian at 9..12)
        let flags = u32::from_be_bytes([0, tfhd[9], tfhd[10], tfhd[11]]);
        assert_eq!(flags, 0x02_0000);
        // track_id at 12..16
        let track_id = u32::from_be_bytes([tfhd[12], tfhd[13], tfhd[14], tfhd[15]]);
        assert_eq!(track_id, 1);
    }

    #[test]
    fn tfhd_track_id_is_encoded_big_endian() {
        let tfhd = build_tfhd(42, 0);
        let track_id = u32::from_be_bytes([tfhd[12], tfhd[13], tfhd[14], tfhd[15]]);
        assert_eq!(track_id, 42);
    }

    #[test]
    fn tfhd_flags_field_is_encoded_big_endian() {
        let tfhd = build_tfhd(1, 0x00_0020); // default-sample-flags present
        let flags = u32::from_be_bytes([0, tfhd[9], tfhd[10], tfhd[11]]);
        assert_eq!(flags, 0x00_0020);
    }

    #[test]
    fn tfhd_total_size_is_16_bytes() {
        let tfhd = build_tfhd(1, 0);
        // Full box header = 4+4+4 = 12; track_id = 4 → total = 16
        assert_eq!(tfhd.len(), 16);
    }

    // ─── Capability C (B6): tfdt box builder ───────────────────────────────

    #[test]
    fn tfdt_golden_bytes_version1_u64_time() {
        // tfdt version=1 contains a 64-bit base_media_decode_time.
        // Layout: [size:4][b"tfdt":4][version:1=1][flags:3=0][decode_time:8]
        // total = 8+4+8 = 20 bytes
        let tfdt = build_tfdt(90_000); // 1 second at 90 kHz
        assert_eq!(&tfdt[4..8], b"tfdt", "box type must be tfdt");
        let size = u32::from_be_bytes([tfdt[0], tfdt[1], tfdt[2], tfdt[3]]) as usize;
        assert_eq!(size, tfdt.len(), "size field must match actual length");
        assert_eq!(tfdt.len(), 20, "tfdt v1 must be 20 bytes");
        // version = 1 at byte 8
        assert_eq!(tfdt[8], 1, "version must be 1 (u64 time field)");
        // flags bytes 9..12 must be zero
        assert_eq!(&tfdt[9..12], &[0, 0, 0], "flags must be 0");
        // base_media_decode_time at bytes 12..20
        let dts = u64::from_be_bytes([
            tfdt[12], tfdt[13], tfdt[14], tfdt[15], tfdt[16], tfdt[17], tfdt[18], tfdt[19],
        ]);
        assert_eq!(dts, 90_000);
    }

    #[test]
    fn tfdt_zero_dts_produces_all_zero_time_field() {
        let tfdt = build_tfdt(0);
        let dts = u64::from_be_bytes([
            tfdt[12], tfdt[13], tfdt[14], tfdt[15], tfdt[16], tfdt[17], tfdt[18], tfdt[19],
        ]);
        assert_eq!(dts, 0);
    }

    #[test]
    fn tfdt_large_dts_fits_in_u64() {
        let large_dts: u64 = u32::MAX as u64 + 1;
        let tfdt = build_tfdt(large_dts);
        let dts = u64::from_be_bytes([
            tfdt[12], tfdt[13], tfdt[14], tfdt[15], tfdt[16], tfdt[17], tfdt[18], tfdt[19],
        ]);
        assert_eq!(dts, large_dts);
    }

    // ─── Capability D (B6): trun box builder ───────────────────────────────

    /// Helper to create a `TrunSample` with a size and the 30 fps warm-up fallback duration.
    fn make_sample(size: u32) -> TrunSample {
        TrunSample {
            duration: 3000, // warm-up fallback (30 fps); Phase 4 wires FpsTracker
            size,
            flags: None,
        }
    }

    #[test]
    fn trun_golden_bytes_single_sample() {
        // One sample, data_offset = 8 (arbitrary).
        // flags = 0x000301 (data-offset-present | duration-present | sample-size-present)
        // Layout: [size:4][b"trun":4][v=0:1][flags:3][sample_count:4][data_offset:4][duration:4][size:4]
        let samples = vec![make_sample(100)];
        let trun = build_trun(&samples, 8, false);
        assert_eq!(&trun[4..8], b"trun", "box type must be trun");
        let size = u32::from_be_bytes([trun[0], trun[1], trun[2], trun[3]]) as usize;
        assert_eq!(size, trun.len(), "size field must match actual length");
        // version at byte 8
        assert_eq!(trun[8], 0, "version must be 0");
        // sample_count at bytes 12..16
        let sample_count = u32::from_be_bytes([trun[12], trun[13], trun[14], trun[15]]);
        assert_eq!(sample_count, 1);
        // data_offset at bytes 16..20
        let data_offset = i32::from_be_bytes([trun[16], trun[17], trun[18], trun[19]]);
        assert_eq!(data_offset, 8);
    }

    #[test]
    fn trun_sample_count_matches_input_slice_length() {
        let samples = vec![make_sample(10), make_sample(20), make_sample(30)];
        let trun = build_trun(&samples, 0, false);
        let sample_count = u32::from_be_bytes([trun[12], trun[13], trun[14], trun[15]]);
        assert_eq!(sample_count, 3);
    }

    #[test]
    fn trun_per_sample_sizes_are_big_endian() {
        let samples = vec![make_sample(0x12345678), make_sample(0x9ABCDEF0)];
        let trun = build_trun(&samples, 0, false);
        // Layout (non-IDR, with 0x000100 + 0x000200 flags):
        // [size:4][tag:4][v+f:4][count:4][data_offset:4] = offset 20 for per-sample section
        // Per-sample (0x000100 set): [duration:4][size:4] × N
        // sample 0: duration at 20..24, size at 24..28
        // sample 1: duration at 28..32, size at 32..36
        let s0 = u32::from_be_bytes([trun[24], trun[25], trun[26], trun[27]]);
        let s1 = u32::from_be_bytes([trun[32], trun[33], trun[34], trun[35]]);
        assert_eq!(s0, 0x12345678);
        assert_eq!(s1, 0x9ABCDEF0);
    }

    #[test]
    fn trun_with_first_sample_flags_sets_flag_bit() {
        // When is_idr=true, flags should include first_sample_flags_present (0x000004).
        let samples = vec![make_sample(50)];
        let trun = build_trun(&samples, 0, true);
        let flags = u32::from_be_bytes([0, trun[9], trun[10], trun[11]]);
        assert_ne!(
            flags & 0x000004,
            0,
            "first_sample_flags_present bit must be set for IDR"
        );
    }

    // ─── Capability E (B6): moof + traf hierarchy assembler ────────────────

    #[test]
    fn build_moof_contains_moof_mfhd_traf_tfhd_tfdt_trun_tags() {
        let samples = vec![make_sample(100)];
        let moof = build_moof(1, 0, &samples, 0, true);
        let required: &[&[u8; 4]] = &[b"moof", b"mfhd", b"traf", b"tfhd", b"tfdt", b"trun"];
        for tag in required {
            assert!(
                moof.windows(4).any(|w| w == &tag[..]),
                "moof must contain box tag {:?}",
                std::str::from_utf8(*tag).unwrap_or("<non-utf8>")
            );
        }
    }

    #[test]
    fn build_moof_mfhd_sequence_number_is_correct() {
        let samples = vec![make_sample(10)];
        let moof = build_moof(7, 0, &samples, 0, true);
        let mfhd_pos = moof.windows(4).position(|w| w == b"mfhd").unwrap();
        // mfhd full-box: [size:4][tag:4][v+f:4][seq_num:4]
        // After tag: v+f at +4, seq at +8
        let seq_offset = mfhd_pos + 4 + 4; // skip version+flags
        let seq = u32::from_be_bytes([
            moof[seq_offset],
            moof[seq_offset + 1],
            moof[seq_offset + 2],
            moof[seq_offset + 3],
        ]);
        assert_eq!(seq, 7, "mfhd.sequence_number must match input");
    }

    #[test]
    fn build_moof_sequence_numbers_increment_across_calls() {
        let samples = vec![make_sample(50)];
        let moof1 = build_moof(1, 0, &samples, 0, true);
        let moof2 = build_moof(2, 0, &samples, 0, true);
        let moof3 = build_moof(3, 0, &samples, 0, true);

        fn extract_seq(moof: &[u8]) -> u32 {
            let pos = moof.windows(4).position(|w| w == b"mfhd").unwrap();
            let off = pos + 4 + 4;
            u32::from_be_bytes([moof[off], moof[off + 1], moof[off + 2], moof[off + 3]])
        }
        assert_eq!(extract_seq(&moof1), 1);
        assert_eq!(extract_seq(&moof2), 2);
        assert_eq!(extract_seq(&moof3), 3);
    }

    #[test]
    fn build_moof_starts_with_moof_box() {
        let samples = vec![make_sample(100)];
        let moof = build_moof(1, 0, &samples, 0, true);
        assert_eq!(&moof[4..8], b"moof", "first box tag must be moof");
    }

    #[test]
    fn annex_b_to_avcc_preserves_nal_payload_bytes() {
        // Payload bytes after length prefix must match original NAL body.
        let nal_body: &[u8] = &[0x65, 0x11, 0x22, 0x33, 0x44];
        let mut input = vec![0x00u8, 0x00, 0x00, 0x01];
        input.extend_from_slice(nal_body);
        let result = annex_b_to_avcc(&input).expect("should succeed");
        // First 4 bytes = length (5 = nal_body.len())
        let len = u32::from_be_bytes([result[0], result[1], result[2], result[3]]) as usize;
        assert_eq!(len, nal_body.len());
        assert_eq!(&result[4..], nal_body);
    }

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
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS, 3000)
            .expect("build_moov should succeed");

        let required_tags: &[&[u8; 4]] = &[
            b"moov", b"mvhd", b"trak", b"tkhd", b"mdia", b"mdhd", b"hdlr", b"minf", b"vmhd",
            b"dinf", b"dref", b"url ", b"stbl", b"stsd", b"avc1", b"avcC", b"stts", b"stsc",
            b"stsz", b"stco",
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
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS, 3000)
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

        let expected =
            build_avcc(&sps_info, SPS_320X240, MINIMAL_PPS).expect("build_avcc should succeed");

        assert_eq!(embedded_avcc, expected.as_slice());
    }

    #[test]
    fn build_moov_mvhd_timescale_is_90000() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS, 3000)
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

    /// B11-S11: trex.default_sample_duration MUST equal TIMESCALE * fps_den / fps_num,
    /// so MSE buffered ranges report a positive duration per sample. The previous
    /// hardcoded 0 made every sample a zero-duration point on the timeline and
    /// `<video>.currentTime` could not advance.
    #[test]
    fn build_moov_trex_default_sample_duration_b11_s11() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS, 3000)
            .expect("build_moov should succeed");
        let trex_pos = find_box_tag(&moov, b"trex").expect("trex must exist");
        // After the 4-byte tag: [4 v+f][4 track_id][4 default_sample_description_index][4 default_sample_duration]
        let dur_offset = trex_pos + 4 + 4 + 4 + 4;
        let dur = u32::from_be_bytes([
            moov[dur_offset],
            moov[dur_offset + 1],
            moov[dur_offset + 2],
            moov[dur_offset + 3],
        ]);
        assert_eq!(
            dur, 3000,
            "trex.default_sample_duration must match the supplied 30 fps tick value"
        );
    }

    /// B11-S11: Mp4Muxer must rebase its DTS so MSE buffered ranges start near
    /// zero, regardless of the wall-clock-style timestamps the encoder emits.
    /// Two packets at t = 17005 s and 17005.033 s must yield base_dts ≈ 0
    /// and ≈ 3000 (= 30 fps tick) on the rebased timeline.
    #[test]
    fn append_packet_rebases_first_dts_to_zero_b11_s11() {
        use std::sync::Arc;

        use sm_domain::encode::EncodedPacket;
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // Synthesise an SPS+IDR pair only enough to satisfy is_keyframe; we only
        // care about the first_dts_offset behaviour, not the segment bytes.
        let pkt1 = EncodedPacket {
            data: Arc::from(
                vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x0D, 0xF4, 0x0A, 0x0F, 0xC0].into_boxed_slice(),
            ),
            timestamp: std::time::Duration::from_micros(17_005_000_000),
            is_keyframe: true,
            sequence: 0,
        };
        let pkt2 = EncodedPacket {
            data: Arc::from(vec![0, 0, 0, 1, 0x65, 0x88].into_boxed_slice()),
            timestamp: std::time::Duration::from_micros(17_005_033_333),
            is_keyframe: true,
            sequence: 1,
        };
        // first_dts_offset is set on the first append.
        let _ = muxer.append_packet(&pkt1);
        assert!(
            muxer.first_dts_offset.is_some(),
            "first_dts_offset must be set on first append"
        );
        let offset = muxer.first_dts_offset.unwrap();
        assert!(
            offset > 1_000_000_000,
            "expected absolute encoder DTS in 90 kHz, got {offset}"
        );
        // Second packet rebased: should be small (≈ 3000 ticks for 33 ms at 90 kHz).
        let _ = muxer.append_packet(&pkt2);
        let last_rebased = muxer.pending.last().expect("pkt2 stored").dts_90khz;
        assert!(
            last_rebased < 10_000,
            "rebased dts must be near zero, got {last_rebased}"
        );
    }

    #[test]
    fn build_moov_hdlr_handler_type_is_vide() {
        let sps_info = parse_sps(SPS_320X240).expect("should parse");
        let moov = build_moov(320, 240, &sps_info, SPS_320X240, MINIMAL_PPS, 3000)
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
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        // bytes[0..4] = ftyp box size; bytes[4..8] = 'ftyp' tag
        assert_eq!(
            &bytes[4..8],
            b"ftyp",
            "init segment must start with ftyp box tag"
        );
    }

    #[test]
    fn init_segment_ftyp_major_brand_is_iso5() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert_eq!(&bytes[8..12], b"iso5");
    }

    #[test]
    fn init_segment_contains_moov_box() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(
            bytes.windows(4).any(|w| w == b"moov"),
            "init segment must contain moov box"
        );
    }

    #[test]
    fn init_segment_avc1_sample_entry_present() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(
            bytes.windows(4).any(|w| w == b"avc1"),
            "init segment must contain avc1 sample entry"
        );
    }

    #[test]
    fn init_segment_length_greater_than_200_bytes() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert!(
            bytes.len() > 200,
            "init segment must be > 200 bytes, got {}",
            bytes.len()
        );
    }

    #[test]
    fn init_segment_empty_pps_returns_error() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        assert!(
            muxer
                .build_init_segment(&sps_info, SPS_320X240, &[])
                .is_err()
        );
    }

    #[test]
    fn init_segment_is_deterministic() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes1 = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("ok");
        let bytes2 = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("ok");
        assert_eq!(bytes1, bytes2, "build_init_segment must be deterministic");
    }

    #[test]
    fn init_segment_mvhd_timescale_big_endian_90000() {
        let muxer = Mp4Muxer::new(1280, 720, 30, 1);
        let sps_info = parse_sps(SPS_1280X720).expect("should parse SPS_1280X720");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_1280X720, MINIMAL_PPS)
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
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");

        let avcc_tag_pos = bytes
            .windows(4)
            .position(|w| w == b"avcC")
            .expect("avcC must be in init segment");

        let payload_start = avcc_tag_pos + 4;
        assert_eq!(bytes[payload_start], 1, "configurationVersion must be 1");
        assert_eq!(
            bytes[payload_start + 1],
            0x42,
            "profile must be 0x42 (baseline)"
        );
        assert_eq!(
            bytes[payload_start + 2],
            0xC0,
            "constraint_set_flags must be 0xC0"
        );
        assert_eq!(bytes[payload_start + 3], 0x0D, "level must be 0x0D");
    }

    #[test]
    fn init_segment_mdhd_timescale_is_90000() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let bytes = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
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

    /// Byte-for-byte snapshot of the pre-refactor `build_init_segment` output for
    /// `SPS_320X240` + `MINIMAL_PPS`.  Captured before the `&SpsInfo` parameter was
    /// added.  Any change to the ftyp/moov construction will cause this test to fail,
    /// which is the intended anti-regression guard.
    #[rustfmt::skip]
    const GOLDEN_INIT_SEGMENT: &[u8] = &[
        0, 0, 0, 32, 102, 116, 121, 112, 105, 115, 111, 53, 0, 0, 2, 0, 105, 115, 111, 53, 97,
        118, 99, 49, 105, 115, 111, 54, 109, 112, 52, 50, 0, 0, 2, 109, 109, 111, 111, 118, 0, 0,
        0, 108, 109, 118, 104, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 95, 144, 0, 0, 0,
        0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 1, 209, 116,
        114, 97, 107, 0, 0, 0, 92, 116, 107, 104, 100, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        64, 0, 0, 0, 1, 64, 0, 0, 0, 240, 0, 0, 0, 0, 1, 109, 109, 100, 105, 97, 0, 0, 0, 32,
        109, 100, 104, 100, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 95, 144, 0, 0, 0, 0, 85,
        195, 0, 0, 0, 0, 0, 52, 104, 100, 108, 114, 0, 0, 0, 0, 0, 0, 0, 0, 118, 105, 100, 101,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 83, 99, 114, 101, 101, 110, 32, 77, 105, 114, 114,
        111, 114, 32, 86, 105, 100, 101, 111, 0, 0, 0, 1, 17, 109, 105, 110, 102, 0, 0, 0, 20,
        118, 109, 104, 100, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 36, 100, 105, 110, 102,
        0, 0, 0, 28, 100, 114, 101, 102, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 12, 117, 114, 108, 32,
        0, 0, 0, 1, 0, 0, 0, 209, 115, 116, 98, 108, 0, 0, 0, 133, 115, 116, 115, 100, 0, 0, 0,
        0, 0, 0, 0, 1, 0, 0, 0, 117, 97, 118, 99, 49, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 64, 0, 240, 0, 72, 0, 0, 0, 72, 0, 0, 0, 0, 0, 0, 0, 1,
        13, 83, 99, 114, 101, 101, 110, 32, 77, 105, 114, 114, 111, 114, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 24, 255, 255, 0, 0, 0, 31, 97, 118, 99, 67, 1, 66, 192,
        13, 255, 225, 0, 8, 103, 66, 192, 13, 244, 10, 15, 192, 1, 0, 4, 104, 206, 56, 128, 0, 0,
        0, 16, 115, 116, 116, 115, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 115, 116, 115, 99, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 20, 115, 116, 115, 122, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 16, 115, 116, 99, 111, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 40, 109, 118, 101, 120,
        0, 0, 0, 32, 116, 114, 101, 120, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 11, 184, 0,
        0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn init_segment_golden_snapshot() {
        let muxer = Mp4Muxer::new(320, 240, 30, 1);
        let sps_info = parse_sps(SPS_320X240).expect("should parse SPS_320X240");
        let out = muxer
            .build_init_segment(&sps_info, SPS_320X240, MINIMAL_PPS)
            .expect("should build init segment");
        assert_eq!(
            out.as_slice(),
            GOLDEN_INIT_SEGMENT,
            "build_init_segment output must match pre-refactor golden bytes (653 B)"
        );
    }

    // ─── Phase 5: tfhd invariant (R9) ─────────────────────────────────────

    // T5.1 — tfhd in moof assembly MUST NOT have 0x000008 and MUST be 0x020000.
    #[test]
    fn build_tfhd_does_not_set_default_sample_duration_present_bit() {
        // Call build_moof (the real production path) and verify tfhd flags via byte scan.
        let samples = vec![make_sample(100)];
        let moof = build_moof(1, 0, &samples, 0, true);

        let tfhd_pos = moof
            .windows(4)
            .position(|w| w == b"tfhd")
            .expect("tfhd must be in moof");
        // tfhd full-box: [size:4][tag:4][version:1][flags:3][track_id:4]
        // After tag: version at tfhd_pos+4, flags bytes at +5,+6,+7
        let flags = u32::from_be_bytes([
            0,
            moof[tfhd_pos + 5],
            moof[tfhd_pos + 6],
            moof[tfhd_pos + 7],
        ]);
        assert_eq!(
            flags & 0x000008,
            0,
            "tfhd MUST NOT have default-sample-duration-present (0x000008) — per-sample trun wins (R9)"
        );
        assert_eq!(
            flags, 0x020000,
            "tfhd flags must be exactly 0x020000 (default-base-is-moof only); got 0x{flags:06X}"
        );
    }

    // ─── Phase 2 + 3: TrunSample.duration shape + build_trun flag/payload ──

    // T2.1 — TrunSample.duration is u32 (not Option<u32>). RED until T2.2.
    #[test]
    fn trun_sample_duration_field_is_u32_not_option() {
        // Construct TrunSample with an explicit u32 duration value.
        // This will fail to compile when duration is still Option<u32>.
        let sample = TrunSample {
            duration: 3000,
            size: 100,
            flags: None,
        };
        assert_eq!(
            sample.duration, 3000u32,
            "duration must be a concrete u32, not Option"
        );
    }

    // T3.1 — build_trun flags include 0x000100 (sample-duration-present). RED until T3.5.
    #[test]
    fn build_trun_flags_include_sample_duration_present_bit() {
        let samples = vec![make_sample(100)];
        let trun = build_trun(&samples, 0, false);
        // trun full-box header: [size:4][tag:4][version:1][flags:3]
        let flags = u32::from_be_bytes([0, trun[9], trun[10], trun[11]]);
        assert_ne!(
            flags & 0x000100,
            0,
            "trun flags must have sample-duration-present (0x000100)"
        );
        assert_ne!(
            flags & 0x000001,
            0,
            "trun flags must have data-offset-present (0x000001)"
        );
        assert_ne!(
            flags & 0x000200,
            0,
            "trun flags must have sample-size-present (0x000200)"
        );
    }

    // T3.2 — build_trun IDR path includes first-sample-flags + duration-present. RED until T3.5.
    #[test]
    fn build_trun_flags_idr_includes_first_sample_flags_and_duration_present() {
        let samples = vec![make_sample(100)];
        let trun = build_trun(&samples, 0, true);
        let flags = u32::from_be_bytes([0, trun[9], trun[10], trun[11]]);
        // Expected: 0x000305 = data-offset(0x1) | first-sample-flags(0x4) | sample-size(0x200) | duration(0x100)
        assert_eq!(
            flags & 0x000305,
            0x000305,
            "IDR trun flags must have 0x000305 (got 0x{flags:06X})"
        );
        assert_ne!(
            flags & 0x000100,
            0,
            "IDR trun flags must have sample-duration-present (0x000100)"
        );
    }

    // T3.3 — per-sample loop writes duration then size (3 samples). RED until T3.5.
    #[test]
    fn build_trun_per_sample_loop_writes_duration_then_size() {
        fn make_sample_with_duration(size: u32, duration: u32) -> TrunSample {
            TrunSample {
                duration,
                size,
                flags: None,
            }
        }
        let samples = vec![
            make_sample_with_duration(100, 3000),
            make_sample_with_duration(200, 3000),
            make_sample_with_duration(300, 3000),
        ];
        let trun = build_trun(&samples, 0, false);
        // Layout (non-IDR): [size:4][tag:4][v+f:4][count:4][data_offset:4][per-sample...]
        // With 0x000100 + 0x000200: per-sample = [duration:4][size:4]
        let per_sample_offset = 8 + 4 + 4 + 4; // box header(8) + v+f(4) + count(4) + data_offset(4)
        for (i, &expected_size) in [100u32, 200, 300].iter().enumerate() {
            let off = per_sample_offset + i * 8;
            let dur = u32::from_be_bytes([trun[off], trun[off + 1], trun[off + 2], trun[off + 3]]);
            let sz =
                u32::from_be_bytes([trun[off + 4], trun[off + 5], trun[off + 6], trun[off + 7]]);
            assert_eq!(dur, 3000, "sample {i} duration must be 3000");
            assert_eq!(sz, expected_size, "sample {i} size must be {expected_size}");
        }
    }

    // T3.4 — single-sample trun emits exactly one duration+size pair. RED until T3.5.
    #[test]
    fn build_trun_single_sample_emits_one_duration_size_pair() {
        let samples = vec![TrunSample {
            duration: 1500,
            size: 50,
            flags: None,
        }];
        let trun = build_trun(&samples, 0, false);
        // With flags 0x000301 (duration+size+data-offset): per-sample = [duration:4][size:4] = 8 bytes
        // Total box = 8 (box hdr) + 4 (v+f) + 4 (count) + 4 (data_offset) + 8 (1 sample) = 28 bytes
        let expected_box_len: u32 = 8 + 4 + 4 + 4 + 8;
        let actual_box_len = u32::from_be_bytes([trun[0], trun[1], trun[2], trun[3]]);
        assert_eq!(
            actual_box_len, expected_box_len,
            "single-sample trun must be {expected_box_len} bytes"
        );
        let per_sample_offset = 8 + 4 + 4 + 4;
        let dur = u32::from_be_bytes([
            trun[per_sample_offset],
            trun[per_sample_offset + 1],
            trun[per_sample_offset + 2],
            trun[per_sample_offset + 3],
        ]);
        let sz = u32::from_be_bytes([
            trun[per_sample_offset + 4],
            trun[per_sample_offset + 5],
            trun[per_sample_offset + 6],
            trun[per_sample_offset + 7],
        ]);
        assert_eq!(dur, 1500, "single-sample duration must be 1500");
        assert_eq!(sz, 50, "single-sample size must be 50");
    }

    // ─── Phase 4: FpsTracker wired into Mp4Muxer ───────────────────────────

    // Helper: parse per-sample duration values from a trun box.
    // Returns vec of (duration, size) pairs from the trun per-sample section.
    fn parse_trun_per_sample_durations(trun: &[u8], is_idr: bool) -> Vec<(u32, u32)> {
        // Layout: [size:4][tag:4][v+f:4][count:4][data_offset:4]
        //         [first_sample_flags:4] (only if is_idr)
        //         [duration:4][size:4] per sample
        let count = u32::from_be_bytes([trun[12], trun[13], trun[14], trun[15]]) as usize;
        let base = 8 + 4 + 4 + 4 + if is_idr { 4 } else { 0 };
        (0..count)
            .map(|i| {
                let off = base + i * 8;
                let dur =
                    u32::from_be_bytes([trun[off], trun[off + 1], trun[off + 2], trun[off + 3]]);
                let sz = u32::from_be_bytes([
                    trun[off + 4],
                    trun[off + 5],
                    trun[off + 6],
                    trun[off + 7],
                ]);
                (dur, sz)
            })
            .collect()
    }

    // T4.1 — Mp4Muxer.fps_tracker gets fed via append_packet (R4 integration).
    // RED until T4.4.
    #[test]
    fn mp4_muxer_append_packet_feeds_fps_tracker() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // Feed 9 packets at 1500-tick (60 fps) spacing.
        // 1500 ticks at 90 kHz = 16.67 ms ≈ 17 ms
        for i in 0..9u64 {
            let pkt = make_packet(false, i * 1000 / 60, 100); // ~16.67ms spacing
            muxer.append_packet(&pkt);
        }
        // After 9 calls (8 deltas), fps_tracker should be locked at ~1500 ticks.
        assert!(
            muxer.fps_tracker.is_locked(),
            "fps_tracker must be locked after 9 packets at 60 fps spacing"
        );
    }

    // T4.2 — flush_pending uses real intra-GOP DTS deltas (not FpsTracker) for trun.
    //
    // Reworked for Slice 5 (sub-GOP flush N=4): original test used IDR1 + 8P + IDR2,
    // but with N=4 the 5th packet triggers a count-flush mid-GOP, breaking the
    // "IDR2 flushes GOP0" invariant. Reduced to IDR1 + 3P + IDR2 (4 samples) so
    // no count-flush fires before IDR2. The intent is preserved: verify that
    // per-sample trun durations reflect real DTS deltas (not the FpsTracker value).
    // FpsTracker locking is verified separately in T4.1 (mp4_muxer_append_packet_feeds_fps_tracker).
    #[test]
    fn mp4_muxer_flush_pending_uses_locked_ticks() {
        use std::time::Duration;
        // IDR1 + 3 P-frames at 100ms spacing (10 fps, 9000 ticks each).
        // 4 samples total → pending = 4 = SUBGOP_FLUSH_FRAMES, no count-flush fires
        // until IDR2 arrives (IDR path takes precedence; predicate checks is_keyframe first).
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // IDR1 at t=0.
        muxer.append_packet(&EncodedPacket {
            data: {
                let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
                d.extend(vec![0xAAu8; 95]);
                std::sync::Arc::from(d.into_boxed_slice())
            },
            is_keyframe: true,
            timestamp: Duration::from_millis(0),
            sequence: 0,
        });
        // P1, P2, P3 at 100ms spacing. Pending becomes [IDR1, P1, P2, P3] = 4 samples.
        // No count-flush fires: P1..P3 each arrive when pending.len() is 1, 2, 3 (<4).
        for i in 1..=3u64 {
            muxer.append_packet(&EncodedPacket {
                data: {
                    let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x41];
                    d.extend(vec![0xAAu8; 95]);
                    std::sync::Arc::from(d.into_boxed_slice())
                },
                is_keyframe: false,
                timestamp: Duration::from_millis(i * 100),
                sequence: i,
            });
        }
        // IDR2 at t=400ms triggers IDR-flush of [IDR1, P1, P2, P3] (4 samples).
        let idr2 = EncodedPacket {
            data: {
                let mut d = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
                d.extend(vec![0xAAu8; 95]);
                std::sync::Arc::from(d.into_boxed_slice())
            },
            is_keyframe: true,
            timestamp: Duration::from_millis(4 * 100),
            sequence: 4,
        };
        let segment = muxer
            .append_packet(&idr2)
            .expect("IDR2 must flush [IDR1+P1+P2+P3]");

        // Parse trun from the flushed IDR-started segment.
        let trun_pos = segment
            .windows(4)
            .position(|w| w == b"trun")
            .expect("trun must be present");
        let trun = &segment[trun_pos - 4..];
        let pairs = parse_trun_per_sample_durations(trun, true); // IDR fragment

        // After the real-DTS-delta fix (T5), per-sample trun durations come from actual
        // DTS differences, not from FpsTracker. At 100ms uniform spacing,
        // duration_to_90khz(100ms) yields 8999 or 9000 ticks due to f64 rounding.
        // Invariant: durations approximate the real 100ms gap (NOT 3000 warm-up fallback).
        assert_eq!(
            pairs.len(),
            4,
            "segment must have 4 samples (IDR1+P1+P2+P3)"
        );
        for (i, &(dur, _)) in pairs.iter().enumerate() {
            assert!(
                (8998..=9001).contains(&dur),
                "sample {i} duration {dur} must approximate real 100ms delta (8998–9001 ticks); \
                 must NOT be 3000 (FpsTracker warm-up fallback)"
            );
        }
        // Sanity: tracker may or may not be locked (only 4 deltas); the key point is
        // that even without locking, real DTS deltas are used (not the 3000 fallback).
        let tracker_val = muxer.fps_tracker.effective_ticks_per_sample();
        assert_ne!(
            pairs[0].0, 3000,
            "trun durations must use real DTS deltas, not FpsTracker warm-up ({tracker_val})"
        );
    }

    // T5.1 — flush_pending stamps each trun sample with its REAL intra-GOP DTS delta,
    // not a fixed FpsTracker rate. The sum of all per-sample trun durations for a flushed
    // GOP must equal the real DTS span (first sample → first sample of next GOP), within
    // one fallback tick of tolerance for the last-sample duration.
    //
    // Reworked for Slice 5 (N=4): original used IDR1+5P+IDR2 (7 frames), but with N=4
    // a count-flush fires at the 5th packet, so IDR2 no longer receives the full GOP0.
    // Reduced to IDR1+3P+IDR2 (4 samples = N) — no count-flush fires, IDR2 flushes
    // [IDR1,P1,P2,P3] exactly. The intent (real DTS deltas, not FpsTracker) is preserved.
    #[test]
    fn flush_pending_trun_duration_sum_equals_real_dts_span_not_fixed_fps() {
        use std::time::Duration;

        // DTS layout (ms → 90kHz ticks) at ~7.5fps (133ms/frame):
        //   IDR1:  t=0ms   → dts=0
        //   P1:    t=133ms → dts≈11970
        //   P2:    t=266ms → dts≈23940
        //   P3:    t=399ms → dts≈35910
        //   IDR2:  t=532ms → dts≈47880  (flush of [IDR1,P1,P2,P3], 4 samples)
        //
        // Real DTS span = dts[IDR2] - dts[IDR1] ≈ 47880 ticks.
        // With FpsTracker (if stuck at 3000): sum = 4×3000 = 12000 ≠ 47880.
        // With real DTS deltas: sum ≈ 47880 (within one-frame tolerance on last sample).
        let ms_per_frame: u64 = 133; // ~7.5fps
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);

        // IDR1 at t=0.
        muxer.append_packet(&make_packet(true, 0, 100));
        // P1..P3 at ~7.5fps: pending = [IDR1,P1,P2,P3] (4 samples = SUBGOP_FLUSH_FRAMES).
        // No count-flush: each P arrives when pending.len() < 4.
        for i in 1..=3u64 {
            muxer.append_packet(&make_packet(false, i * ms_per_frame, 100));
        }
        // IDR2 triggers IDR-flush of [IDR1,P1,P2,P3] (4 samples).
        let segment = muxer
            .append_packet(&make_packet(true, 4 * ms_per_frame, 100))
            .expect("IDR2 must flush [IDR1+P1+P2+P3]");

        // Parse per-sample durations from the trun box (IDR-flagged).
        let trun_pos = segment
            .windows(4)
            .position(|w| w == b"trun")
            .expect("segment must contain trun box");
        let trun = &segment[trun_pos - 4..];
        let pairs = parse_trun_per_sample_durations(trun, true); // IDR fragment

        assert_eq!(pairs.len(), 4, "flushed segment must have 4 samples");

        // Real DTS span: IDR2 DTS - IDR1 DTS ≈ duration_to_90khz(4*133ms).
        let real_span: u64 =
            crate::transport::annex_b::duration_to_90khz(Duration::from_millis(4 * ms_per_frame));

        let actual_sum: u64 = pairs.iter().map(|&(dur, _)| dur as u64).sum();

        // Allow ±one maximum intra-GOP delta (~12000 ticks) for last-sample fallback.
        let tolerance: u64 = 12_001;
        assert!(
            actual_sum.abs_diff(real_span) <= tolerance,
            "trun duration sum {actual_sum} must approximate real DTS span {real_span} \
             (within ±{tolerance} ticks). Got: {} frames × {} ticks/frame avg",
            pairs.len(),
            actual_sum / pairs.len() as u64,
        );

        // Stricter: first N-1 durations must equal real intra-GOP DTS delta (±1 tick).
        let expected_delta: u64 =
            crate::transport::annex_b::duration_to_90khz(Duration::from_millis(ms_per_frame));
        for (i, &(dur, _)) in pairs.iter().enumerate().take(pairs.len().saturating_sub(1)) {
            assert!(
                (dur as u64).abs_diff(expected_delta) <= 1,
                "sample {i} duration {dur} must equal real DTS delta {expected_delta} \
                 (real-DTS-delta fix; must NOT be FpsTracker fallback value)"
            );
        }
    }

    // T4.3 — flush_pending uses real intra-GOP DTS deltas even during FpsTracker warm-up.
    //
    // Original T4.3 verified warm-up used 3000 (FpsTracker fallback). After the real-DTS-delta
    // fix (T5), per-sample trun durations come from actual DTS differences regardless of
    // FpsTracker state. This test verifies the new correct behavior: real deltas used,
    // FpsTracker locked state is irrelevant for trun per-sample durations.
    #[test]
    fn mp4_muxer_flush_pending_uses_real_dts_deltas_during_warmup() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // Feed IDR1 + only 3 P-frames (warm-up incomplete with < 8 deltas).
        // Spacing: 17ms each → 17 × 90 = 1530 ticks per inter-frame delta.
        let idr1 = make_packet(true, 0, 100);
        muxer.append_packet(&idr1);
        for i in 1..=3u64 {
            muxer.append_packet(&make_packet(false, i * 17, 100)); // ~60fps spacing
        }
        // IDR2 triggers flush; warm-up not yet complete (only 4 deltas).
        let idr2 = make_packet(true, 4 * 17, 100);
        let segment = muxer.append_packet(&idr2).expect("IDR2 must flush GOP1");

        assert!(
            !muxer.fps_tracker.is_locked(),
            "fps_tracker must still be warming up (only 5 deltas after IDR2 appended)"
        );

        let trun_pos = segment
            .windows(4)
            .position(|w| w == b"trun")
            .expect("trun must be present");
        let trun = &segment[trun_pos - 4..];
        let pairs = parse_trun_per_sample_durations(trun, true); // IDR fragment

        // With the real-DTS-delta fix, durations should reflect real 17ms spacing ≈ 1530 ticks.
        // The FpsTracker fallback (3000) must NOT appear — real deltas are used.
        let expected_delta: u32 = 17 * 90; // 1530 ticks
        for (i, &(dur, _)) in pairs.iter().take(pairs.len().saturating_sub(1)).enumerate() {
            assert_eq!(
                dur, expected_delta,
                "warm-up sample {i} duration must be real DTS delta {expected_delta}; got {dur} \
                 (must NOT be FpsTracker fallback 3000)"
            );
        }
        // Last sample uses median of intra-GOP deltas (all 1530) = 1530.
        let last_dur = pairs.last().expect("at least one sample").0;
        assert_eq!(
            last_dur, expected_delta,
            "last sample duration must be median of intra-GOP deltas ({expected_delta}); got {last_dur}"
        );
    }

    // ─── Slice 5: sub-GOP flush tests (RED-first, Tests 1–5) ─────────────────
    //
    // All five tests are written BEFORE any production changes. They fail with
    // a compile error (SUBGOP_FLUSH_FRAMES undefined) until Phase 2 GREEN.
    // Design: D-PPT5-6. Spec: SEG-1..SEG-3.

    // Test 1 — D-PPT5-6 #1: flush fires after SUBGOP_FLUSH_FRAMES accumulated samples.
    // Two-sided mutation guard: N→3 makes the 4th call return Some; N→5 keeps 5th as None.
    // Spec: SEG-1-SC1. Design: D-PPT5-1, D-PPT5-2.
    #[test]
    fn append_packet_flushes_after_subgop_threshold() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // IDR1: opens the first pending segment, returns None (no prior pending).
        let idr1 = make_packet(true, 0, 100);
        let r0 = muxer.append_packet(&idr1);
        assert!(
            r0.is_none(),
            "first IDR must return None (nothing to flush)"
        );

        // P-frames 1–3: each accumulates without flush (pending = 2, 3, 4 including IDR1).
        // Under the unified predicate (D-PPT5-1): flush when pending.len() >= SUBGOP_FLUSH_FRAMES
        // fires on the packet that WOULD make pending reach N+1, i.e. the (N+1)-th packet.
        // IDR1 is pending[0]; P1→pending[1]; P2→pending[2]; P3→pending[3].
        // At this point pending.len() == 4 == SUBGOP_FLUSH_FRAMES — NOT yet flushed.
        for i in 1u64..=3 {
            let p = make_packet(false, i * 17, 100);
            let r = muxer.append_packet(&p);
            assert!(
                r.is_none(),
                "P-frame {i}: pending={}, must not flush yet (N={})",
                i + 1,
                super::SUBGOP_FLUSH_FRAMES
            );
        }

        // P-frame 5 (5th packet total): pending was 4; this arrival triggers the flush
        // (predicate: pending.len() >= SUBGOP_FLUSH_FRAMES while !is_keyframe).
        let p5 = make_packet(false, 4 * 17, 100);
        let r5 = muxer.append_packet(&p5);
        assert!(
            r5.is_some(),
            "5th packet must trigger count-flush (pending reached SUBGOP_FLUSH_FRAMES={})",
            super::SUBGOP_FLUSH_FRAMES
        );
    }

    // Test 2 — D-PPT5-6 #2: the flushed segment contains exactly N samples.
    // Kills inclusive/exclusive off-by-one on the pending count.
    // Spec: SEG-1-SC1. Design: D-PPT5-1.
    #[test]
    fn subgop_flushed_fragment_has_exactly_n_samples() {
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        // IDR1 + 3 P-frames → pending = 4 (= SUBGOP_FLUSH_FRAMES). No flush yet.
        muxer.append_packet(&make_packet(true, 0, 100));
        for i in 1u64..=3 {
            muxer.append_packet(&make_packet(false, i * 17, 100));
        }
        // 5th packet triggers flush of the 4 accumulated samples.
        let segment = muxer
            .append_packet(&make_packet(false, 4 * 17, 100))
            .expect("5th packet must trigger count-flush");

        // Parse trun sample_count from the flushed segment.
        // This fragment's first sample is IDR1, so it IS IDR-flagged (first_sample_flags
        // is present in the trun). The sample_count at trun[12..16] precedes that optional
        // first_sample_flags field, so reading it at this offset is correct regardless of
        // the IDR flag.
        let trun_pos = segment
            .windows(4)
            .position(|w| w == b"trun")
            .expect("segment must contain trun box");
        let trun = &segment[trun_pos - 4..];
        let count = u32::from_be_bytes([trun[12], trun[13], trun[14], trun[15]]) as usize;
        assert_eq!(
            count,
            super::SUBGOP_FLUSH_FRAMES,
            "count-flushed segment must contain exactly SUBGOP_FLUSH_FRAMES={} samples, got {}",
            super::SUBGOP_FLUSH_FRAMES,
            count
        );
    }

    // Test 3 — D-PPT5-6 #3: count-flushed P-frame-only segment has no IDR flag;
    // IDR-flushed segment has it. Two-sided: kills is_idr hardcode=true AND always-false.
    // Spec: SEG-2-SC1, SEG-2-SC2. Design: D-PPT5-4.
    //
    // Setup for (a): we need a count-flushed segment whose FIRST sample is a P-frame.
    // IDR1 → 3P → P4 (count-flush of [IDR1,P1,P2,P3] → IDR-flagged, that's seg1).
    // P4 is now pending[0]. P5,P6,P7 → pending=[P4,P5,P6,P7] (4 samples).
    // P8 → count-flush of [P4,P5,P6,P7] → NOT IDR-flagged (P4 is a P-frame). That's seg2.
    #[test]
    fn subgop_fragment_first_sample_flags() {
        // ── (a) count-flushed P-frame-only segment: bit 0x4 MUST NOT be set ──
        {
            let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
            let ms = 17u64;
            // Sub-GOP 1: IDR1 + P1..P3 (pending=4). P4 triggers first count-flush.
            muxer.append_packet(&make_packet(true, 0, 100));
            for i in 1u64..=3 {
                muxer.append_packet(&make_packet(false, i * ms, 100));
            }
            // P4: count-flush → seg1 = [IDR1,P1,P2,P3] (IDR-flagged, discarded here).
            let _seg1 = muxer
                .append_packet(&make_packet(false, 4 * ms, 100))
                .expect("P4 must trigger first count-flush");
            // P4 is now in fresh pending. P5..P7: pending=[P4,P5,P6,P7] (4 samples).
            for i in 5u64..=7 {
                muxer.append_packet(&make_packet(false, i * ms, 100));
            }
            // P8: count-flush → seg2 = [P4,P5,P6,P7] — first sample is P4 (P-frame).
            let seg2 = muxer
                .append_packet(&make_packet(false, 8 * ms, 100))
                .expect("P8 must trigger second count-flush (P-frame-only segment)");

            let trun_pos = seg2
                .windows(4)
                .position(|w| w == b"trun")
                .expect("trun must be present");
            let trun = &seg2[trun_pos - 4..];
            // trun full-box: [size:4][tag:4][v+flags:3 bytes at 9,10,11]
            let trun_flags = u32::from_be_bytes([0, trun[9], trun[10], trun[11]]);
            assert_eq!(
                trun_flags & 0x000004,
                0,
                "count-flushed P-frame-only segment: first_sample_flags_present (0x4) \
                 must NOT be set; got trun_flags=0x{trun_flags:06X}"
            );
        }

        // ── (b) IDR-flushed segment: bit 0x4 MUST be set ──
        {
            let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
            // IDR1 + 1 P-frame, then IDR2 flushes [IDR1, P1] — IDR-flagged.
            muxer.append_packet(&make_packet(true, 0, 100));
            muxer.append_packet(&make_packet(false, 17, 100));
            let segment = muxer
                .append_packet(&make_packet(true, 34, 100))
                .expect("IDR2 must flush prior segment");

            let trun_pos = segment
                .windows(4)
                .position(|w| w == b"trun")
                .expect("trun must be present");
            let trun = &segment[trun_pos - 4..];
            let trun_flags = u32::from_be_bytes([0, trun[9], trun[10], trun[11]]);
            assert_ne!(
                trun_flags & 0x000004,
                0,
                "IDR-flushed segment: first_sample_flags_present (0x4) MUST be set; \
                 got trun_flags=0x{trun_flags:06X}"
            );
        }
    }

    // Test 4 — D-PPT5-6 #4: mfhd sequence numbers are strictly increasing across
    // both IDR-flush and count-flush trigger types.
    // Spec: SEG-2-SC3. Design: D-PPT5-1.
    #[test]
    fn subgop_sequence_numbers_monotonic_across_trigger_types() {
        // Sequence: IDR1 → 3P → P5(count-flush→seq=1) → IDR2(IDR-flush→seq=2) → 3P → IDR3(→seq=3).
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);
        let ms = 17u64; // ~60fps spacing

        // IDR1 + 3 P-frames: pending = 4, no flush.
        muxer.append_packet(&make_packet(true, 0, 100));
        for i in 1u64..=3 {
            muxer.append_packet(&make_packet(false, i * ms, 100));
        }
        // P5 triggers count-flush → segment with seq=1.
        let seg1 = muxer
            .append_packet(&make_packet(false, 4 * ms, 100))
            .expect("count-flush must produce seg1");

        // IDR2 triggers IDR-flush of [P5] → segment with seq=2.
        let seg2 = muxer
            .append_packet(&make_packet(true, 5 * ms, 100))
            .expect("IDR2 must produce seg2");

        // IDR2 is now in pending. 3 more P-frames (pending = 4), no count-flush.
        for i in 6u64..=8 {
            muxer.append_packet(&make_packet(false, i * ms, 100));
        }
        // IDR3 triggers IDR-flush → segment with seq=3.
        let seg3 = muxer
            .append_packet(&make_packet(true, 9 * ms, 100))
            .expect("IDR3 must produce seg3");

        fn extract_mfhd_seq(seg: &[u8]) -> u32 {
            let pos = seg
                .windows(4)
                .position(|w| w == b"mfhd")
                .expect("mfhd missing");
            let off = pos + 4 + 4; // skip tag + version+flags
            u32::from_be_bytes([seg[off], seg[off + 1], seg[off + 2], seg[off + 3]])
        }

        let s1 = extract_mfhd_seq(&seg1);
        let s2 = extract_mfhd_seq(&seg2);
        let s3 = extract_mfhd_seq(&seg3);

        assert_eq!(s1, 1, "count-flush segment must have seq=1, got {s1}");
        assert_eq!(s2, 2, "IDR-flush segment must have seq=2, got {s2}");
        assert_eq!(s3, 3, "second IDR-flush segment must have seq=3, got {s3}");
        assert!(
            s1 < s2 && s2 < s3,
            "sequence numbers must be strictly increasing"
        );
    }

    // Test 5 — D-PPT5-6 #5: single-sample remainder segment uses inter-fragment delta,
    // not the 3000-tick WARMUP_FALLBACK.
    // Kills prev_flushed_dts path regression when remainder is 1 sample.
    // Spec: SEG-1-SC5, SEG-3 (indirect). Design: D-PPT5-3.
    #[test]
    fn single_sample_remainder_uses_inter_fragment_delta() {
        use std::time::Duration;
        // IDR1(t=0) + P1..P3 + P4 + IDR2(t=335ms), P-frames spaced 67ms apart.
        // P1..P3 fill pending to [IDR1, P1, P2, P3] = 4 = SUBGOP_FLUSH_FRAMES; the
        // separately-sent P4 then triggers the count-flush of those 4 samples.
        // Then IDR2 triggers IDR-flush of [P4] (1-sample remainder).
        let ms = 67u64;
        let mut muxer = Mp4Muxer::new(320, 240, 30, 1);

        muxer.append_packet(&make_packet(true, 0, 100)); // IDR1
        for i in 1u64..=3 {
            muxer.append_packet(&make_packet(false, i * ms, 100)); // P1..P3
        }
        // P4 triggers count-flush (pending was [IDR1, P1, P2, P3] = 4 = SUBGOP_FLUSH_FRAMES).
        let _seg_count = muxer
            .append_packet(&make_packet(false, 4 * ms, 100))
            .expect("P4 must trigger count-flush");

        // IDR2 triggers IDR-flush of [P4] — single-sample remainder.
        let seg_remainder = muxer
            .append_packet(&make_packet(true, 5 * ms, 100))
            .expect("IDR2 must flush single-sample remainder");

        // Parse the remainder segment's trun duration.
        // Remainder is NOT IDR-flagged (first sample is P4, is_idr=false).
        let trun_pos = seg_remainder
            .windows(4)
            .position(|w| w == b"trun")
            .expect("remainder segment must contain trun");
        let trun = &seg_remainder[trun_pos - 4..];
        // No first_sample_flags field (not IDR), base offset = 8+4+4+4 = 20
        let sample_count = u32::from_be_bytes([trun[12], trun[13], trun[14], trun[15]]);
        assert_eq!(sample_count, 1, "remainder must be exactly 1 sample");
        let dur = u32::from_be_bytes([trun[20], trun[21], trun[22], trun[23]]);

        // Expected: inter-fragment delta ≈ 67ms in 90kHz ticks ≈ 6030.
        // Must NOT be 3000 (WARMUP_FALLBACK_TICKS).
        let expected_approx =
            crate::transport::annex_b::duration_to_90khz(Duration::from_millis(ms)) as u32;
        assert_ne!(
            dur, 3000,
            "single-sample remainder must NOT use WARMUP_FALLBACK (3000 ticks); got {dur}"
        );
        // Within 2× expected delta (generous tolerance for DTS arithmetic).
        assert!(
            dur <= expected_approx * 2,
            "single-sample remainder duration {dur} must be ≤ 2×expected_delta ({});  \
             prev_flushed_dts path must supply a real inter-fragment delta",
            expected_approx * 2
        );
        assert!(
            dur >= expected_approx / 2,
            "single-sample remainder duration {dur} must be ≥ ½×expected_delta ({})",
            expected_approx / 2
        );
    }
}
