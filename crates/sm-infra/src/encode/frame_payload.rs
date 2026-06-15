//! `FramePayload` — capture→encoder channel carrier.
//!
//! This enum is the typed envelope that flows through the
//! `capture_to_enc_{tx,rx}` bounded channel between the WGC capture thread
//! and the encoder thread.
//!
//! # Variants
//!
//! - `Cpu(CaptureFrame)` — today's code path: BGRA8 pixels already read back to
//!   CPU memory as an `Arc<[u8]>`. The encoder dispatches this to the existing
//!   `nv12_convert` + `submit_frame` path VERBATIM. No behaviour change.
//!
//! - `GpuShared { handle, width, height, stride, timestamp }` — future GPU-resident
//!   path (PR-3): the capture thread passes a cross-process shared HANDLE for an
//!   `ID3D11Texture2D` BGRA surface along with its dimensions and timestamp. The
//!   encoder thread acquires the keyed mutex, runs `VideoProcessorBlt` BGRA→NV12
//!   entirely on the GPU, and feeds `MFCreateDXGISurfaceBuffer` to the MFT.
//!
//!   **Not yet produced in PR-2**: the capture side still sends `Cpu` only.
//!   The encoder `GpuShared` match arm is marked `todo!()` with an explicit PR-3
//!   comment; it is safe because no `GpuShared` value can be constructed from
//!   production code in this PR.
//!
//! # PR-2 invariant
//!
//! `GpuShared` is syntactically complete so `select_encode_path` and the enum
//! itself can be unit-tested, but the variant is NEVER CONSTRUCTED in PR-2
//! production code.  The capture side always sends `Cpu`; the encoder pump_loop
//! `GpuShared` arm is `todo!("PR-3: GPU resident path")`.  CI is green because
//! `todo!()` only panics at runtime on a reachable code path.

use sm_domain::CaptureFrame;

/// Cross-thread frame carrier for the capture→encoder channel.
///
/// See module-level documentation for the PR-2 / PR-3 split invariant.
#[derive(Debug)]
pub enum FramePayload {
    /// CPU-staged path: pixel data already in `Arc<[u8]>` (today's code, zero change).
    Cpu(CaptureFrame),

    /// GPU-resident path: shared DXGI texture handle + geometry + timestamp.
    ///
    /// **PR-2**: this variant is defined but NEVER CONSTRUCTED by production code.
    /// The encoder match arm is `todo!("PR-3: GPU resident path")`.
    GpuShared {
        /// Cross-process shared handle for the keyed-mutex `ID3D11Texture2D` BGRA surface.
        ///
        /// # Safety (for future PR-3 implementor)
        ///
        /// The handle is valid only while the encoder thread holds the keyed mutex
        /// (`IDXGIKeyedMutex::AcquireSync`). Release via `ReleaseSync` before the
        /// next `CopyResource` on the capture side. The handle itself is not `Clone`
        /// — ownership transfers through the channel; the capture side must not
        /// re-use the HANDLE after the `FramePayload` is sent.
        handle: isize, // raw HANDLE as isize for Send+Sync without unsafe marker

        /// Surface width in pixels.
        width: u32,
        /// Surface height in pixels.
        height: u32,
        /// Row stride in bytes (BGRA8 source surface).
        stride: u32,
        /// Monotonic capture timestamp (same semantics as `CaptureFrame::timestamp`).
        timestamp: std::time::Duration,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use sm_domain::{CaptureFrame, PixelFormat};

    use super::FramePayload;

    // ── TASK-02 RED→GREEN: Cpu variant round-trip ─────────────────────────────

    /// T-FP-01: `FramePayload::Cpu` round-trips all `CaptureFrame` fields unchanged.
    ///
    /// Asserts that wrapping a `CaptureFrame` in `FramePayload::Cpu` and immediately
    /// destructuring it preserves every field byte-for-byte — this is the behavioural
    /// contract that the existing CPU-staged path is not altered by the new enum.
    #[test]
    fn cpu_variant_round_trips_capture_frame_fields_unchanged() {
        let data: Arc<[u8]> = Arc::from(&[0xDE, 0xAD, 0xBE, 0xEF][..]);
        let frame = CaptureFrame {
            data: Arc::clone(&data),
            width: 1920,
            height: 1080,
            stride: 7680,
            format: PixelFormat::Bgra8,
            timestamp: Duration::from_millis(42),
        };

        let payload = FramePayload::Cpu(frame);

        match payload {
            FramePayload::Cpu(f) => {
                assert!(Arc::ptr_eq(&f.data, &data), "data Arc pointer must be identical (no copy)");
                assert_eq!(f.width, 1920);
                assert_eq!(f.height, 1080);
                assert_eq!(f.stride, 7680);
                assert!(matches!(f.format, PixelFormat::Bgra8));
                assert_eq!(f.timestamp, Duration::from_millis(42));
            }
            FramePayload::GpuShared { .. } => {
                panic!("expected Cpu variant");
            }
        }
    }

    /// T-FP-02: `FramePayload::Cpu` clone is cheap (Arc refcount bump only).
    #[test]
    fn cpu_variant_clone_shares_data_arc() {
        let data: Arc<[u8]> = Arc::from(&[1u8, 2, 3, 4][..]);
        let frame = CaptureFrame {
            data: Arc::clone(&data),
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra8,
            timestamp: Duration::ZERO,
        };

        // Wrap in Cpu and extract to verify the Arc isn't copied.
        let payload = FramePayload::Cpu(frame.clone());
        if let FramePayload::Cpu(inner) = payload {
            assert!(
                Arc::ptr_eq(&inner.data, &data),
                "FramePayload::Cpu must not copy pixel data"
            );
        }
    }
}
