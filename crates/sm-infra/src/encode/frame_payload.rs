//! `FramePayload` — capture→encoder channel carrier.
//!
//! The type itself now lives in `sm-domain` ([`sm_domain::FramePayload`]) so the
//! frozen `CaptureSource::start` / `VideoEncoder::start` channel signatures can
//! carry it without `sm-domain` depending on `windows` (PR-4 / TASK-08, design
//! D7). This module re-exports it under the original `sm_infra::encode::frame_payload`
//! path so existing `use crate::encode::frame_payload::FramePayload` imports keep
//! working, and retains the PR-2 round-trip unit tests as the regression net.
//!
//! # Variants (see [`sm_domain::FramePayload`])
//!
//! - `Cpu(CaptureFrame)` — today's code path: BGRA8 pixels already read back to
//!   CPU memory as an `Arc<[u8]>`. The encoder dispatches this to the existing
//!   `nv12_convert` + `submit_frame` path VERBATIM. No behaviour change.
//! - `GpuShared { handle, width, height, stride, timestamp }` — GPU-resident
//!   path: the Windows capture thread copies the WGC texture into a keyed-mutex
//!   shared `ID3D11Texture2D` and passes its share `HANDLE` (as `isize`). The
//!   encoder thread acquires the keyed mutex, runs `VideoProcessorBlt` BGRA→NV12
//!   on the GPU, and feeds `MFCreateDXGISurfaceBuffer` to the MFT.

pub use sm_domain::FramePayload;

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
                assert!(
                    Arc::ptr_eq(&f.data, &data),
                    "data Arc pointer must be identical (no copy)"
                );
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
        let FramePayload::Cpu(inner) = payload else {
            panic!("expected Cpu");
        };
        assert!(
            Arc::ptr_eq(&inner.data, &data),
            "FramePayload::Cpu must not copy pixel data"
        );
    }

    /// T-FP-03: `GpuShared::timestamp()` accessor returns the variant's timestamp.
    #[test]
    fn gpu_shared_timestamp_accessor_returns_timestamp() {
        let payload = FramePayload::GpuShared {
            handle: 0x1234,
            width: 2560,
            height: 1440,
            stride: 2560 * 4,
            timestamp: Duration::from_millis(99),
        };
        assert_eq!(payload.timestamp(), Duration::from_millis(99));
    }
}
