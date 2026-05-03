// BGRA→NV12 stride-aware color converter.
//
// BT.601 limited-range coefficients. No cfg gate — runs on all platforms so that
// Linux/macOS CI can catch stride bugs before any Windows machine sees them.

use sm_domain::CaptureFrame;

/// Planar NV12 (YUV420SP) buffer.
///
/// Layout: `[Y plane (width×height) | UV interleaved plane (ceil(width/2)×ceil(height/2)×2)]`
///
/// The UV plane stores chroma as interleaved pairs: U₀ V₀ U₁ V₁ …
///
/// `buf` is pre-allocated and reused across frames to avoid per-frame heap allocation.
pub(crate) struct Nv12 {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Contiguous Y plane then interleaved UV plane — no padding between planes.
    pub(crate) buf: Vec<u8>,
}

impl Nv12 {
    /// Create a new zero-initialised NV12 buffer sized for `width × height`.
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let y_size = (width as usize) * (height as usize);
        // UV plane: ceil(width/2) * ceil(height/2) chroma samples, each 2 bytes (U=Cb, V=Cr).
        // Using ceil for odd dimensions (e.g. 1×1 → 1 chroma sample = 2 bytes).
        let chroma_w = (width as usize).div_ceil(2);
        let chroma_h = (height as usize).div_ceil(2);
        let uv_size = chroma_w * chroma_h * 2;
        Self {
            width,
            height,
            buf: vec![0u8; y_size + uv_size],
        }
    }

    /// Byte offset into `buf` where the Y plane starts (always 0).
    #[inline]
    pub(crate) fn y_offset(&self) -> usize {
        0
    }

    /// Byte offset into `buf` where the UV interleaved plane starts.
    #[inline]
    pub(crate) fn uv_offset(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }
}

/// Convert a BGRA8 `CaptureFrame` into NV12, reusing the destination buffer.
///
/// # Stride-aware
///
/// `frame.stride` MAY exceed `frame.width * 4` due to WGC GPU-aligned row padding.
/// This function uses `frame.stride` as the row pitch when reading source bytes —
/// NOT `frame.width * 4`. Using the wrong pitch causes the "diagonal screen tear"
/// corruption bug at resolutions where stride > width * 4 (e.g., 1366×768).
///
/// # Buffer reuse
///
/// If `out` already has the correct dimensions the `buf` allocation is reused
/// (capacity never shrinks). Only if width or height changes is the buffer replaced.
///
/// # NV12 layout
///
/// Y plane (`width × height` bytes) immediately followed by the interleaved UV plane.
/// The UV plane layout is: U₀ V₀ U₁ V₁ … where each (U,V) pair corresponds to a
/// 2×2 block of pixels (4:2:0 chroma subsampling).
///
/// # Color space
///
/// BT.601 limited-range (studio swing):
/// - Y  = clip( 0.257·R + 0.504·G + 0.098·B + 16  )
/// - Cb = clip(-0.148·R - 0.291·G + 0.439·B + 128 )
/// - Cr = clip( 0.439·R - 0.368·G - 0.071·B + 128 )
pub(crate) fn convert(frame: &CaptureFrame, out: &mut Nv12) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride as usize; // row pitch in bytes (may include WGC GPU alignment padding)

    // Resize buffer only when dimensions change.
    if out.width != frame.width || out.height != frame.height {
        *out = Nv12::new(frame.width, frame.height);
    }

    let y_off = out.y_offset();
    let uv_off = out.uv_offset();
    let chroma_w = w.div_ceil(2);
    let src = frame.data.as_ref();

    for y in 0..h {
        let src_row = y * stride;
        for x in 0..w {
            let p = src_row + x * 4;
            // BGRA byte order: B=0, G=1, R=2, A=3
            let b = src[p] as i32;
            let g = src[p + 1] as i32;
            let r = src[p + 2] as i32;

            // BT.601 limited-range luma (integer fixed-point × 256)
            // Y = (66*R + 129*G + 25*B + 128) >> 8 + 16
            let luma = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            out.buf[y_off + y * w + x] = luma.clamp(16, 235) as u8;

            // Chroma subsampling: one (Cb, Cr) sample per 2×2 block (top-left pixel only).
            if y % 2 == 0 && x % 2 == 0 {
                // Cb = (-38*R - 74*G + 112*B + 128) >> 8 + 128
                let cb = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                // Cr = (112*R - 94*G - 18*B + 128) >> 8 + 128
                let cr = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let block_x = x / 2;
                let block_y = y / 2;
                // NV12: 2 bytes per chroma sample (U=Cb then V=Cr), interleaved per row of width chroma_w
                let uv_idx = uv_off + block_y * (chroma_w * 2) + block_x * 2;
                out.buf[uv_idx] = cb.clamp(16, 240) as u8; // U (Cb)
                out.buf[uv_idx + 1] = cr.clamp(16, 240) as u8; // V (Cr)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_domain::capture::PixelFormat;
    use std::sync::Arc;
    use std::time::Duration;

    /// Build a synthetic `CaptureFrame` from raw BGRA bytes with a given stride.
    fn make_frame(width: u32, height: u32, stride: u32, bgra_data: Vec<u8>) -> CaptureFrame {
        CaptureFrame {
            data: Arc::from(bgra_data.into_boxed_slice()),
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            timestamp: Duration::ZERO,
        }
    }

    /// Build a frame where every pixel has the same BGRA value.
    fn solid_frame(width: u32, height: u32, b: u8, g: u8, r: u8) -> CaptureFrame {
        let stride = width * 4;
        let data = (0..height)
            .flat_map(|_| (0..width).flat_map(|_| [b, g, r, 0xFF]))
            .collect::<Vec<u8>>();
        make_frame(width, height, stride, data)
    }

    // ─── T6.1: nv12_single_red_pixel_produces_correct_yuv_values ────────────────
    // BT.601 limited-range red (R=255, G=0, B=0):
    //   Y  ≈ 81,  Cb ≈ 90,  Cr ≈ 240
    #[test]
    fn nv12_single_red_pixel_produces_correct_yuv_values() {
        // BGRA for pure red: B=0, G=0, R=255, A=255
        let frame = solid_frame(1, 1, 0, 0, 255);
        let mut out = Nv12::new(1, 1);
        convert(&frame, &mut out);
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.uv_offset()];
        let v = out.buf[out.uv_offset() + 1];
        assert!((75..=87).contains(&y), "Y={y} expected ~81 for red");
        assert!((85..=96).contains(&u), "U={u} expected ~90 for red");
        assert!((234..=245).contains(&v), "V={v} expected ~240 for red");
    }

    // ─── nv12_single_white_pixel_yuv_values ─────────────────────────────────────
    // BT.601 limited-range white (R=255, G=255, B=255): Y=235, U=128, V=128
    #[test]
    fn nv12_single_white_pixel_yuv_values() {
        let frame = solid_frame(1, 1, 255, 255, 255);
        let mut out = Nv12::new(1, 1);
        convert(&frame, &mut out);
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.uv_offset()];
        let v = out.buf[out.uv_offset() + 1];
        assert!((233..=237).contains(&y), "Y={y} expected 235 for white");
        assert!((126..=130).contains(&u), "U={u} expected 128 for white");
        assert!((126..=130).contains(&v), "V={v} expected 128 for white");
    }

    // ─── nv12_single_black_pixel_yuv_values ─────────────────────────────────────
    // BT.601 limited-range black (R=0, G=0, B=0): Y=16, U=128, V=128
    #[test]
    fn nv12_single_black_pixel_yuv_values() {
        let frame = solid_frame(1, 1, 0, 0, 0);
        let mut out = Nv12::new(1, 1);
        convert(&frame, &mut out);
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.uv_offset()];
        let v = out.buf[out.uv_offset() + 1];
        assert!((14..=18).contains(&y), "Y={y} expected 16 for black");
        assert!((126..=130).contains(&u), "U={u} expected 128 for black");
        assert!((126..=130).contains(&v), "V={v} expected 128 for black");
    }

    // ─── T6.2: nv12_stride_padded_frame_no_garbage ──────────────────────────────
    //
    // 4×2 frame. Active pixels = 4 wide, so row bytes = 4*4 = 16.
    // stride = 20: each row has 4 bytes of garbage padding at the end.
    #[test]
    fn nv12_stride_padded_frame_no_garbage() {
        let width: u32 = 4;
        let height: u32 = 2;
        let stride: u32 = 20; // 4*4 + 4 padding bytes per row
        let mut data = Vec::new();
        for _ in 0..height {
            for _ in 0..width {
                data.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]); // red pixel
            }
            data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // garbage padding
        }
        assert_eq!(data.len(), height as usize * stride as usize);

        let frame = make_frame(width, height, stride, data);
        let mut out = Nv12::new(width, height);
        convert(&frame, &mut out);

        // All 8 Y values should be ~81 (red), not garbage values
        for i in 0..(width as usize * height as usize) {
            let y = out.buf[out.y_offset() + i];
            assert!(
                (75..=87).contains(&y),
                "Y[{i}]={y} — expected ~81 (red), possible stride bug"
            );
        }
    }

    // ─── T6.3: nv12_uv_plane_is_interleaved_u_v_u_v ────────────────────────────
    // 4×2 solid white frame.
    // NV12 chroma: chroma_w=2, chroma_h=1 → 2 samples × 2 bytes = 4 UV bytes.
    // Layout: U0 V0 U1 V1. For white: U≈128, V≈128.
    #[test]
    fn nv12_uv_plane_is_interleaved_u_v_u_v() {
        let frame = solid_frame(4, 2, 255, 255, 255);
        let mut out = Nv12::new(4, 2);
        convert(&frame, &mut out);

        let uv_off = out.uv_offset();
        let uv_len = out.buf.len() - uv_off;
        // 4×2 NV12: 2 chroma samples × 2 bytes = 4 bytes
        assert_eq!(uv_len, 4, "UV plane for 4×2 frame should be 4 bytes");

        for i in (0..uv_len).step_by(2) {
            let u = out.buf[uv_off + i];
            let v = out.buf[uv_off + i + 1];
            assert!(
                (126..=130).contains(&u),
                "UV[{i}]={u} — U expected ~128 for white"
            );
            assert!(
                (126..=130).contains(&v),
                "UV[{i}+1]={v} — V expected ~128 for white"
            );
        }
    }

    // ─── T6.6: nv12_output_size_matches_nv12_formula_for_1080p ─────────────────
    #[test]
    fn nv12_output_size_matches_nv12_formula_for_1080p() {
        let w: u32 = 1920;
        let h: u32 = 1080;
        let stride = w * 4;
        let data = vec![0u8; (h * stride) as usize];
        let frame = make_frame(w, h, stride, data);
        let mut out = Nv12::new(w, h);
        convert(&frame, &mut out);
        assert_eq!(
            out.buf.len(),
            3_110_400,
            "NV12 1920×1080 should be 3_110_400 bytes"
        );
    }

    // ─── T6.4: nv12_buffer_reused_on_same_dimensions ────────────────────────────
    #[test]
    fn nv12_buffer_reused_on_same_dimensions() {
        let frame = solid_frame(64, 64, 128, 128, 128);
        let mut out = Nv12::new(64, 64);
        convert(&frame, &mut out);
        let cap_after_first = out.buf.capacity();
        convert(&frame, &mut out);
        assert_eq!(
            out.buf.capacity(),
            cap_after_first,
            "buffer capacity grew on second call — per-frame allocation detected"
        );
    }

    // ─── T6.5: nv12_buffer_replaced_on_dimension_change ─────────────────────────
    #[test]
    fn nv12_buffer_replaced_on_dimension_change() {
        let frame_hd = solid_frame(1920, 1080, 0, 0, 0);
        let frame_small = solid_frame(1366, 768, 0, 0, 0);
        let mut out = Nv12::new(1920, 1080);
        convert(&frame_hd, &mut out);
        convert(&frame_small, &mut out);
        // 1366×768 NV12: Y=1366*768, UV=ceil(1366/2)*ceil(768/2)*2=683*384*2
        let expected = 1366 * 768 + 683 * 384 * 2;
        assert_eq!(
            out.buf.len(),
            expected,
            "after resolution change, buf.len() should be {} for 1366×768 NV12",
            expected
        );
    }

    // ─── nv12_odd_resolutions_handled ───────────────────────────────────────────
    // 1×1 frame: Y=1 byte, UV=ceil(1/2)*ceil(1/2)*2=1*1*2=2 bytes. Total=3.
    #[test]
    fn nv12_odd_resolutions_handled() {
        let frame = solid_frame(1, 1, 64, 128, 192);
        let mut out = Nv12::new(1, 1);
        convert(&frame, &mut out);
        assert_eq!(
            out.buf.len(),
            3,
            "1×1 NV12 should be 3 bytes total (1Y + 2UV)"
        );
        let y = out.buf[out.y_offset()];
        assert!((16..=235).contains(&y), "Y={y} out of limited-range");
    }
}
