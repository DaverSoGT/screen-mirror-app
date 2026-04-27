// I420 → BGRA8 color converter (BT.601 limited-range).
//
// This is the inverse of `crates/sm-infra/src/encode/bgra_to_i420.rs`.
// No cfg gate — pure Rust, cross-platform: Linux/macOS CI catches regressions
// before any Windows machine sees them.

/// Convert planar I420 (YUV 4:2:0) to packed BGRA8 in-place.
///
/// # Parameters
///
/// - `y` — Y (luma) plane; must be exactly `width × height` bytes.
/// - `u` — U (Cb) plane; must be exactly `(width/2) × (height/2)` bytes
///   (integer division — no padding).
/// - `v` — V (Cr) plane; same size as `u`.
/// - `width` — frame width in pixels.
/// - `height` — frame height in pixels.
/// - `dst` — output buffer; MUST be exactly `width * height * 4` bytes.
///   Output byte order per pixel: `[B, G, R, A]` (A = 255 opaque).
///
/// # Panics
///
/// Panics if `dst.len() != width * height * 4`.
///
/// # Color space
///
/// BT.601 limited-range (studio swing, matching [`crate::encode::bgra_to_i420`]):
///
/// ```text
/// R = clamp((298*(Y-16) + 409*(V-128)                        + 128) >> 8, 0, 255)
/// G = clamp((298*(Y-16) - 100*(U-128) - 208*(V-128)          + 128) >> 8, 0, 255)
/// B = clamp((298*(Y-16) + 516*(U-128)                        + 128) >> 8, 0, 255)
/// ```
///
/// The integer scaling factor of 256 (× 256 base, `>> 8` at the end) mirrors the
/// forward transform in `bgra_to_i420`, enabling a lossless cross-module
/// coefficient audit.
///
/// # Round-trip fidelity
///
/// For any BGRA pixel, the round-trip `BGRA → I420 → BGRA` is within ±2 per
/// channel due to BT.601 quantization and chroma subsampling (R4.5).
pub fn convert(y: &[u8], u: &[u8], v: &[u8], width: u32, height: u32, dst: &mut [u8]) {
    let w = width as usize;
    let h = height as usize;
    let expected_dst = w * h * 4;
    assert_eq!(
        dst.len(),
        expected_dst,
        "dst.len()={} but expected width*height*4={}",
        dst.len(),
        expected_dst
    );

    // I420 chroma planes use integer-division stride (no padding).
    let chroma_w = w / 2;

    for row in 0..h {
        for col in 0..w {
            let y_val = y[row * w + col] as i32;
            let u_val = u[(row / 2) * chroma_w + (col / 2)] as i32;
            let v_val = v[(row / 2) * chroma_w + (col / 2)] as i32;

            // BT.601 limited-range inverse (integer fixed-point, scale × 256, shift >> 8).
            // Coefficients (all × 256):
            //   298 ≈ 1.164 × 256  (Y contribution)
            //   409 ≈ 1.596 × 256  (V → R)
            //   100 ≈ 0.391 × 256  (U → G, negative)
            //   208 ≈ 0.813 × 256  (V → G, negative)
            //   516 ≈ 2.018 × 256  (U → B)
            let c = 298 * (y_val - 16);
            let r = ((c + 409 * (v_val - 128) + 128) >> 8).clamp(0, 255) as u8;
            let g =
                ((c - 100 * (u_val - 128) - 208 * (v_val - 128) + 128) >> 8).clamp(0, 255) as u8;
            let b = ((c + 516 * (u_val - 128) + 128) >> 8).clamp(0, 255) as u8;

            let px = (row * w + col) * 4;
            dst[px] = b;
            dst[px + 1] = g;
            dst[px + 2] = r;
            dst[px + 3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── compile-time Send+Sync check ────────────────────────────────────────
    // `convert` is a free function (no state), so there is nothing to assert
    // Send+Sync on directly. This test confirms the function is callable from
    // any thread context by spawning a thread and calling convert inside it.
    #[test]
    fn convert_is_callable_from_multiple_threads() {
        let y = vec![81u8; 4];
        let u = vec![90u8];
        let v = vec![240u8];
        let mut dst = vec![0u8; 16];
        std::thread::spawn(move || {
            convert(&y, &u, &v, 2, 2, &mut dst);
        })
        .join()
        .expect("thread panicked");
    }

    // ─── S4.1 — pure-red pixel round-trip ────────────────────────────────────
    // BT.601 limited-range encoding of R=255,G=0,B=0 via bgra_to_i420:
    //   Y  = (66*255 + 128)>>8 + 16 = 81
    //   U  = (-38*255 + 128)>>8 + 128 = 90
    //   V  = (112*255 + 128)>>8 + 128 = 240
    // Inverse:
    //   c = 298*(81-16) = 19370
    //   R = (19370 + 409*(240-128) + 128)>>8 = 65306>>8 = 255
    //   G = (19370 - 100*(90-128) - 208*(240-128) + 128)>>8 = 2>>8 = 0
    //   B = (19370 + 516*(90-128) + 128)>>8 = -110>>8 → clamp → 0
    #[test]
    fn s4_1_pure_red_pixel() {
        // 2×2 frame: Y=81 for all luma, U=90, V=240 for the single chroma sample
        let y = vec![81u8; 4];
        let u = vec![90u8]; // 1×1 chroma block
        let v = vec![240u8];
        let mut dst = vec![0u8; 16]; // 2×2×4
        convert(&y, &u, &v, 2, 2, &mut dst);

        for i in 0..4 {
            let b = dst[i * 4];
            let g = dst[i * 4 + 1];
            let r = dst[i * 4 + 2];
            let a = dst[i * 4 + 3];
            assert!((0..=5).contains(&b), "pixel {i}: B={b} expected ~0 for red");
            assert!((0..=5).contains(&g), "pixel {i}: G={g} expected ~0 for red");
            assert!(
                (250..=255).contains(&r),
                "pixel {i}: R={r} expected ~255 for red"
            );
            assert_eq!(a, 0xFF, "pixel {i}: A must be 255");
        }
    }

    // ─── S4.2 — black frame (Y=16, U=128, V=128) ─────────────────────────────
    // c = 298*(16-16)=0; all channels = clamp(128>>8) = clamp(0) = 0
    #[test]
    fn s4_2_black_frame() {
        let size = 4 * 4;
        let y = vec![16u8; size];
        let u = vec![128u8; size / 4]; // 2×2 chroma
        let v = vec![128u8; size / 4];
        let mut dst = vec![0u8; size * 4];
        convert(&y, &u, &v, 4, 4, &mut dst);

        for i in 0..size {
            let b = dst[i * 4];
            let g = dst[i * 4 + 1];
            let r = dst[i * 4 + 2];
            let a = dst[i * 4 + 3];
            assert_eq!(b, 0, "pixel {i}: B={b} expected 0 for black");
            assert_eq!(g, 0, "pixel {i}: G={g} expected 0 for black");
            assert_eq!(r, 0, "pixel {i}: R={r} expected 0 for black");
            assert_eq!(a, 0xFF);
        }
    }

    // ─── S4.3 — white frame (Y=235, U=128, V=128) ────────────────────────────
    // c = 298*(235-16) = 298*219 = 65262
    // R = G = B = (65262 + 0 + 128)>>8 = 65390>>8 = 255
    #[test]
    fn s4_3_white_frame() {
        let size = 4 * 4;
        let y = vec![235u8; size];
        let u = vec![128u8; size / 4];
        let v = vec![128u8; size / 4];
        let mut dst = vec![0u8; size * 4];
        convert(&y, &u, &v, 4, 4, &mut dst);

        for i in 0..size {
            let b = dst[i * 4];
            let g = dst[i * 4 + 1];
            let r = dst[i * 4 + 2];
            let a = dst[i * 4 + 3];
            assert!(
                (252..=255).contains(&b),
                "pixel {i}: B={b} expected ~255 for white"
            );
            assert!(
                (252..=255).contains(&g),
                "pixel {i}: G={g} expected ~255 for white"
            );
            assert!(
                (252..=255).contains(&r),
                "pixel {i}: R={r} expected ~255 for white"
            );
            assert_eq!(a, 0xFF);
        }
    }

    // ─── S4.4 — stride-awareness: 2×2 frame, verify chroma index formula ─────
    // I420 U/V planes use stride = width/2 (integer division, no padding).
    // With width=2: chroma_w = 1, so all 4 luma pixels share the same chroma sample.
    #[test]
    fn s4_4_stride_awareness_2x2() {
        let y = vec![235u8; 4]; // 2×2 luma: all white luma
        let u = vec![128u8; 1]; // 1×1 chroma
        let v = vec![128u8; 1];
        let mut dst = vec![0u8; 16];
        convert(&y, &u, &v, 2, 2, &mut dst);

        for i in 0..4 {
            let b = dst[i * 4];
            let g = dst[i * 4 + 1];
            let r = dst[i * 4 + 2];
            assert!((252..=255).contains(&b), "pixel {i}: B={b}");
            assert!((252..=255).contains(&g), "pixel {i}: G={g}");
            assert!((252..=255).contains(&r), "pixel {i}: R={r}");
        }
    }

    // ─── S4.4 (boundary) — dst too small must panic ───────────────────────────
    #[test]
    #[should_panic(expected = "dst.len()=3 but expected width*height*4=4")]
    fn s4_4_dst_too_small_panics() {
        let y = vec![16u8; 1];
        let u = vec![128u8; 1];
        let v = vec![128u8; 1];
        let mut dst = vec![0u8; 3]; // must be 4
        convert(&y, &u, &v, 1, 1, &mut dst);
    }

    // ─── R4.5 — round-trip BGRA → I420 → BGRA within ±2 per channel ─────────
    // Uses the forward BT.601 integer coefficients (from bgra_to_i420) to build
    // a synthetic I420 frame, then converts back and checks ±2 tolerance.
    #[test]
    fn round_trip_bgra_to_i420_to_bgra_within_tolerance() {
        let b_in: u8 = 100;
        let g_in: u8 = 150;
        let r_in: u8 = 200;

        // Forward (bgra_to_i420 integer formula):
        let r = r_in as i32;
        let g = g_in as i32;
        let b = b_in as i32;
        let y_val = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(16, 235) as u8;
        let u_val = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
        let v_val = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(16, 240) as u8;

        let y_plane = vec![y_val; 4];
        let u_plane = vec![u_val; 1];
        let v_plane = vec![v_val; 1];

        let mut dst = vec![0u8; 16];
        convert(&y_plane, &u_plane, &v_plane, 2, 2, &mut dst);

        for i in 0..4 {
            let b_out = dst[i * 4] as i32;
            let g_out = dst[i * 4 + 1] as i32;
            let r_out = dst[i * 4 + 2] as i32;
            assert!(
                (b_out - b).abs() <= 2,
                "pixel {i}: B round-trip: in={b} out={b_out}"
            );
            assert!(
                (g_out - g).abs() <= 2,
                "pixel {i}: G round-trip: in={g} out={g_out}"
            );
            assert!(
                (r_out - r).abs() <= 2,
                "pixel {i}: R round-trip: in={r} out={r_out}"
            );
        }
    }

    // ─── even dimensions and correct chroma-plane sizing ─────────────────────
    // I420 requires even dimensions. This test verifies that a 4×4 frame
    // produces the correct total output size (64 bytes) without index OOB.
    #[test]
    fn even_dimensions_correct_chroma_indexing() {
        let y = vec![235u8; 16]; // 4×4 luma
        let u = vec![128u8; 4]; // 2×2 chroma
        let v = vec![128u8; 4];
        let mut dst = vec![0u8; 64]; // 4×4×4
        convert(&y, &u, &v, 4, 4, &mut dst);

        for i in 0..16 {
            assert_eq!(dst[i * 4 + 3], 0xFF, "pixel {i}: A must be 255");
        }
    }
}
