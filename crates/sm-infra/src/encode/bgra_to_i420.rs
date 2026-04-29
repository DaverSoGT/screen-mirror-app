// BGRA→I420 stride-aware color converter.
//
// BT.601 limited-range coefficients. No cfg gate — runs on all platforms so that
// Linux/macOS CI can catch stride bugs before any Windows machine sees them.

use sm_domain::CaptureFrame;

/// Planar I420 (YUV420P) buffer.
///
/// Layout: `[Y plane (width×height) | U plane (width/2 × height/2) | V plane (width/2 × height/2)]`
///
/// `buf` is pre-allocated and reused across frames to avoid per-frame heap allocation.
#[cfg_attr(
    not(target_os = "windows"),
    expect(
        dead_code,
        reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
    )
)]
pub(crate) struct I420 {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Contiguous Y, U, V planes — no padding between planes.
    pub(crate) buf: Vec<u8>,
}

impl I420 {
    /// Create a new zero-initialised I420 buffer sized for `width × height`.
    #[cfg_attr(
        not(target_os = "windows"),
        expect(
            dead_code,
            reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
        )
    )]
    pub(crate) fn new(width: u32, height: u32) -> Self {
        let y_size = (width as usize) * (height as usize);
        let uv_size = (width as usize).div_ceil(2) * (height as usize).div_ceil(2);
        let total = y_size + 2 * uv_size;
        Self {
            width,
            height,
            buf: vec![0u8; total],
        }
    }

    /// Byte offset into `buf` where the Y plane starts.
    #[inline]
    #[cfg_attr(
        not(target_os = "windows"),
        expect(
            dead_code,
            reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
        )
    )]
    pub(crate) fn y_offset(&self) -> usize {
        0
    }

    /// Byte offset into `buf` where the U plane starts.
    #[inline]
    #[cfg_attr(
        not(target_os = "windows"),
        expect(
            dead_code,
            reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
        )
    )]
    pub(crate) fn u_offset(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    /// Byte offset into `buf` where the V plane starts.
    #[inline]
    #[cfg_attr(
        not(target_os = "windows"),
        expect(
            dead_code,
            reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
        )
    )]
    pub(crate) fn v_offset(&self) -> usize {
        let y_size = (self.width as usize) * (self.height as usize);
        let uv_size = (self.width as usize).div_ceil(2) * (self.height as usize).div_ceil(2);
        y_size + uv_size
    }
}

/// Convert a BGRA8 `CaptureFrame` into I420, reusing the destination buffer.
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
/// # Odd dimensions
///
/// For odd `width` or `height`, the chroma planes are sized as
/// `⌈width/2⌉ × ⌈height/2⌉`. The final chroma row (for odd height) uses the
/// luminance values from the last row only. This matches OpenH264's expectation.
///
/// # Color space
///
/// BT.601 limited-range (studio swing):
/// - Y  = clip( 0.257·R + 0.504·G + 0.098·B + 16  )
/// - Cb = clip(-0.148·R - 0.291·G + 0.439·B + 128 )
/// - Cr = clip( 0.439·R - 0.368·G - 0.071·B + 128 )
#[cfg_attr(
    not(target_os = "windows"),
    expect(
        dead_code,
        reason = "Production caller (encode/windows.rs) is cfg-gated to Windows; items are exercised via cross-platform tests."
    )
)]
pub(crate) fn convert(frame: &CaptureFrame, out: &mut I420) {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let stride = frame.stride as usize; // row pitch in bytes (may include padding)

    // Resize buffer only when dimensions change.
    if out.width != frame.width || out.height != frame.height {
        *out = I420::new(frame.width, frame.height);
    }

    let chroma_w = w.div_ceil(2);
    let y_off = out.y_offset();
    let u_off = out.u_offset();
    let v_off = out.v_offset();
    let src = frame.data.as_ref();

    for y in 0..h {
        let src_row_start = y * stride;
        for x in 0..w {
            let src_px = src_row_start + x * 4;
            // BGRA: byte order is B=0, G=1, R=2, A=3
            let b = src[src_px] as i32;
            let g = src[src_px + 1] as i32;
            let r = src[src_px + 2] as i32;

            // BT.601 limited-range Y (using integer fixed-point × 1024)
            // Y = (66*R + 129*G + 25*B + 128) >> 8 + 16
            let luma = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            out.buf[y_off + y * w + x] = luma.clamp(16, 235) as u8;

            // Chroma subsampling: one U/V sample per 2×2 block (top-left pixel)
            if y % 2 == 0 && x % 2 == 0 {
                // Cb = (-38*R - 74*G + 112*B + 128) >> 8 + 128
                let cb = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                // Cr = (112*R - 94*G - 18*B + 128) >> 8 + 128
                let cr = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let uv_idx = (y / 2) * chroma_w + (x / 2);
                out.buf[u_off + uv_idx] = cb.clamp(16, 240) as u8;
                out.buf[v_off + uv_idx] = cr.clamp(16, 240) as u8;
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

    // ─── U1: single_red_pixel_yuv_values ──────────────────────────────────────
    // BT.601 limited-range red (R=255, G=0, B=0):
    //   Y  = (66*255 + 129*0 + 25*0 + 128)>>8 + 16  = (16830+128)>>8+16 = 65+16 = 81 → ~82
    //   Cb = (-38*255 -74*0 +112*0 +128)>>8+128 = (-9690+128)>>8+128 = -37+128 = 91 → ~90
    //   Cr = (112*255 -94*0 -18*0 +128)>>8+128 = (28560+128)>>8+128 = 111+128 = 239 → ~240
    #[test]
    fn single_red_pixel_yuv_values() {
        // BGRA for pure red: B=0, G=0, R=255, A=255
        let frame = solid_frame(1, 1, 0, 0, 255);
        let mut out = I420::new(1, 1);
        convert(&frame, &mut out);
        // BT.601 limited-range: Y≈81, U≈90, V≈240 (± 2 for rounding)
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.u_offset()];
        let v = out.buf[out.v_offset()];
        assert!((75..=87).contains(&y), "Y={y} expected ~81 for red");
        assert!((85..=96).contains(&u), "U={u} expected ~90 for red");
        assert!((234..=245).contains(&v), "V={v} expected ~240 for red");
    }

    // ─── U2: single_white_pixel_yuv_values ────────────────────────────────────
    // BT.601 limited-range white (R=255, G=255, B=255): Y=235, U=128, V=128
    #[test]
    fn single_white_pixel_yuv_values() {
        let frame = solid_frame(1, 1, 255, 255, 255);
        let mut out = I420::new(1, 1);
        convert(&frame, &mut out);
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.u_offset()];
        let v = out.buf[out.v_offset()];
        // Limited-range white: Y=235, U=128, V=128 (± 2 rounding tolerance)
        assert!((233..=237).contains(&y), "Y={y} expected 235 for white");
        assert!((126..=130).contains(&u), "U={u} expected 128 for white");
        assert!((126..=130).contains(&v), "V={v} expected 128 for white");
    }

    // ─── U3: single_black_pixel_yuv_values ────────────────────────────────────
    // BT.601 limited-range black (R=0, G=0, B=0): Y=16, U=128, V=128
    #[test]
    fn single_black_pixel_yuv_values() {
        let frame = solid_frame(1, 1, 0, 0, 0);
        let mut out = I420::new(1, 1);
        convert(&frame, &mut out);
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.u_offset()];
        let v = out.buf[out.v_offset()];
        assert!((14..=18).contains(&y), "Y={y} expected 16 for black");
        assert!((126..=130).contains(&u), "U={u} expected 128 for black");
        assert!((126..=130).contains(&v), "V={v} expected 128 for black");
    }

    // ─── U4: stride_padded_frame_no_garbage — THE STRIDE TEST ─────────────────
    //
    // 4×2 frame. Active pixels = 4 wide, so row bytes = 4*4 = 16.
    // stride = 20: each row has 4 bytes of garbage padding at the end.
    // The first 2 rows are red pixels, the last 4 bytes of each row are 0xFF garbage.
    // Expected: Y plane reflects only the active pixels (red), not the garbage.
    #[test]
    fn stride_padded_frame_no_garbage() {
        let width: u32 = 4;
        let height: u32 = 2;
        let stride: u32 = 20; // 4*4 + 4 padding bytes per row
        // Build rows: [B=0,G=0,R=255,A=255] × 4 pixels + [0xFF,0xFF,0xFF,0xFF] padding
        let mut data = Vec::new();
        for _ in 0..height {
            for _ in 0..width {
                data.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]); // red pixel
            }
            data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // garbage padding
        }
        assert_eq!(data.len(), height as usize * stride as usize);

        let frame = make_frame(width, height, stride, data);
        let mut out = I420::new(width, height);
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

    // ─── U5: even_height_chroma_plane_sizes ───────────────────────────────────
    #[test]
    fn even_height_chroma_plane_sizes() {
        let w: usize = 1920;
        let h: usize = 1080;
        let expected_total = w * h * 3 / 2; // 3_110_400
        assert_eq!(expected_total, 3_110_400);

        // Build a minimal frame (zero-filled is fine for size verification)
        let stride = (w * 4) as u32;
        let data = vec![0u8; h * w * 4];
        let frame = make_frame(w as u32, h as u32, stride, data);
        let mut out = I420::new(w as u32, h as u32);
        convert(&frame, &mut out);

        assert_eq!(
            out.buf.len(),
            expected_total,
            "I420 buf len mismatch for 1920×1080"
        );
    }

    // ─── U6: convert_reuses_buffer ─────────────────────────────────────────────
    #[test]
    fn convert_reuses_buffer() {
        let frame = solid_frame(64, 64, 128, 128, 128);
        let mut out = I420::new(64, 64);
        convert(&frame, &mut out);
        let cap_after_first = out.buf.capacity();
        // Second call with same frame — must not grow capacity
        convert(&frame, &mut out);
        assert_eq!(
            out.buf.capacity(),
            cap_after_first,
            "buffer capacity grew on second call — per-frame allocation detected"
        );
    }

    // ─── U7: convert_handles_resolution_change ────────────────────────────────
    #[test]
    fn convert_handles_resolution_change() {
        let frame_hd = solid_frame(1920, 1080, 0, 0, 0);
        let frame_small = solid_frame(1366, 768, 0, 0, 0);
        let mut out = I420::new(1920, 1080);
        convert(&frame_hd, &mut out);
        assert_eq!(out.width, 1920);
        assert_eq!(out.height, 1080);
        // Now resize to a different resolution
        convert(&frame_small, &mut out);
        assert_eq!(out.width, 1366);
        assert_eq!(out.height, 768);
        let expected = 1366 * 768 + 2 * (683 * 384); // 683 = ceil(1366/2), 384 = ceil(768/2)
        assert_eq!(out.buf.len(), expected);
    }

    // ─── U8: odd_resolutions_handled ──────────────────────────────────────────
    // 1×1 frame: chroma planes are 1 byte each (⌈1/2⌉ = 1).
    #[test]
    fn odd_resolutions_handled() {
        let frame = solid_frame(1, 1, 64, 128, 192); // some color
        let mut out = I420::new(1, 1);
        convert(&frame, &mut out);
        // Y plane: 1 byte, U plane: 1 byte, V plane: 1 byte
        assert_eq!(out.buf.len(), 3, "1×1 I420 should be 3 bytes total");
        // Values should be valid limited-range values
        let y = out.buf[out.y_offset()];
        let u = out.buf[out.u_offset()];
        let v = out.buf[out.v_offset()];
        assert!((16..=235).contains(&y), "Y={y} out of limited-range");
        assert!((16..=240).contains(&u), "U={u} out of chroma range");
        assert!((16..=240).contains(&v), "V={v} out of chroma range");
    }
}
