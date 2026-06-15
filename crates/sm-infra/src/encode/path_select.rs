//! Path-selection gate for the capture→encode pipeline.
//!
//! # Overview
//!
//! [`select_encode_path`] is a **pure function** — no COM, no hardware, no OS calls.
//! It maps `(capture_luid, encode_luid, vendor)` to an [`EncodePath`] variant:
//!
//! - [`EncodePath::GpuResident`] — selected when the capture adapter and the
//!   encode adapter are on **the same physical GPU** (LUID equality) AND the
//!   encoder vendor is [`EncoderVendor::IntelQsv`].  Both conditions must hold.
//!
//! - [`EncodePath::CpuStagedFallback`] — selected in every other case.
//!
//! This logic is evaluated ONCE at pipeline initialisation time (not per-frame).
//!
//! # PR-2 note
//!
//! The function is wired and log-gated at init time in this PR.  BOTH arms
//! route to the existing CPU path for now — the `GpuResident` execution path
//! is implemented in PR-3.  The function exists so the truth-table tests and
//! the NVENC pinning regression tests can all pass on CI without any hardware.
//!
//! # Design reference
//!
//! Design Decision D6 (design artifact #1206): "Gpu IFF `capture_luid==encode_luid
//! && vendor==IntelQsv`.  Else CpuStaged."  Vendor floor is belt-and-suspenders;
//! the LUID check alone is not sufficient because cross-adapter LUID equality is
//! theoretically possible on exotic virtual-GPU setups.

use crate::encode::windows_mft::EncoderVendor;

// ── Path enum ─────────────────────────────────────────────────────────────────

/// Encode path selected by [`select_encode_path`] at pipeline init time.
///
/// In PR-2 BOTH variants route to the CPU-staged fallback (the `GpuResident`
/// execution branch is implemented in PR-3). The enum itself is the seam that
/// makes the truth-table unit tests and the NVENC regression test pass on CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodePath {
    /// GPU-resident path: capture texture stays on GPU from WGC through MFT
    /// `ProcessInput` (zero CPU round-trips).
    ///
    /// **PR-2**: not yet executed — wired but not reachable from production code.
    /// The full implementation lands in PR-3.
    GpuResident,

    /// CPU-staged fallback: today's exact code path (`frame.buffer()` → rayon
    /// `bgra_to_nv12` → `MFCreateMemoryBuffer` → `ProcessInput`).
    CpuStagedFallback,
}

// ── Gate function ─────────────────────────────────────────────────────────────

/// Select the encode path for this pipeline instance.
///
/// Evaluates ONCE at capture→encoder pipeline initialisation.  Result should be
/// cached for the session lifetime; do NOT call per-frame.
///
/// # Arguments
///
/// * `capture_luid` — LUID of the D3D11 adapter used by the WGC capture source.
///   Obtain via the capture device's `IDXGIAdapter::GetDesc().AdapterLuid`.
/// * `encode_luid` — LUID of the D3D11 adapter used by the MFT encoder.
///   Obtain via the same path on the adapter backing the hardware MFT.
/// * `vendor` — encoder vendor detected at MFT probe time via
///   [`EncoderVendor::detect`].
///
/// # Selection logic
///
/// Returns [`EncodePath::GpuResident`] if and only if:
/// - `capture_luid == encode_luid` (same physical adapter), AND
/// - `vendor == EncoderVendor::IntelQsv` (vendor floor per design D6).
///
/// Returns [`EncodePath::CpuStagedFallback`] in every other case, including:
/// - `vendor == EncoderVendor::NvidiaNvenc` (regardless of LUID)
/// - `capture_luid != encode_luid` (cross-adapter topology)
/// - `vendor == EncoderVendor::Amd` or `EncoderVendor::Unknown`
pub(crate) fn select_encode_path(
    capture_luid: i64,
    encode_luid: i64,
    vendor: EncoderVendor,
) -> EncodePath {
    if capture_luid == encode_luid && vendor == EncoderVendor::IntelQsv {
        EncodePath::GpuResident
    } else {
        EncodePath::CpuStagedFallback
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{EncodePath, select_encode_path};
    use crate::encode::windows_mft::EncoderVendor;

    // Synthetic LUID constants.  LUIDs are i64 here (signed) because LUID.HighPart
    // is LONG (signed) and LUID.LowPart is DWORD; combined as (high << 32 | low) in i64.
    const LUID_CAPTURE: i64 = 0x0000_8086_0000_0001_u64 as i64; // synthetic Intel iGPU
    const LUID_NVIDIA: i64 = 0x0000_10DE_0000_0001_u64 as i64; // synthetic NVIDIA dGPU

    // ── T-PS-01: same LUID + IntelQsv → GpuResident ─────────────────────────

    /// Scenario S-07 row 1 (spec #1205): same adapter + QSV → GPU path selected.
    #[test]
    fn same_luid_intel_qsv_selects_gpu_resident() {
        let path = select_encode_path(LUID_CAPTURE, LUID_CAPTURE, EncoderVendor::IntelQsv);
        assert_eq!(
            path,
            EncodePath::GpuResident,
            "same LUID + IntelQsv must select GpuResident"
        );
    }

    // ── T-PS-02: same LUID + NvidiaNvenc → CpuStagedFallback ────────────────

    /// Scenario S-07 row 2 (spec #1205): same adapter + NVENC → CPU fallback.
    /// Vendor floor takes precedence — NVENC never takes the GPU path.
    #[test]
    fn same_luid_nvidia_nvenc_selects_cpu_staged_fallback() {
        let path = select_encode_path(LUID_CAPTURE, LUID_CAPTURE, EncoderVendor::NvidiaNvenc);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "same LUID + NvidiaNvenc must select CpuStagedFallback (vendor floor)"
        );
    }

    // ── T-PS-03: different LUID + IntelQsv → CpuStagedFallback ─────────────

    /// Scenario S-07 row 3 (spec #1205): cross-adapter QSV → CPU fallback.
    /// LUID mismatch is sufficient to reject the GPU path even for Intel QSV.
    #[test]
    fn different_luid_intel_qsv_selects_cpu_staged_fallback() {
        let path = select_encode_path(LUID_CAPTURE, LUID_NVIDIA, EncoderVendor::IntelQsv);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "different LUIDs + IntelQsv must select CpuStagedFallback (cross-adapter)"
        );
    }

    // ── T-PS-04: different LUID + NvidiaNvenc → CpuStagedFallback (belt+suspenders) ─

    /// Belt-and-suspenders: cross-adapter NVENC also falls back.
    /// Both the LUID mismatch and the vendor floor independently reject GpuResident.
    #[test]
    fn different_luid_nvidia_nvenc_selects_cpu_staged_fallback() {
        let path = select_encode_path(LUID_CAPTURE, LUID_NVIDIA, EncoderVendor::NvidiaNvenc);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "cross-adapter + NvidiaNvenc must select CpuStagedFallback"
        );
    }

    // ── T-PS-05: same LUID + Amd → CpuStagedFallback ────────────────────────

    /// AMD encoder is not an approved GPU-resident vendor — always CPU fallback.
    #[test]
    fn same_luid_amd_selects_cpu_staged_fallback() {
        let path = select_encode_path(LUID_CAPTURE, LUID_CAPTURE, EncoderVendor::Amd);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "same LUID + Amd must select CpuStagedFallback"
        );
    }

    // ── T-PS-06: same LUID + Unknown → CpuStagedFallback ────────────────────

    /// Unknown vendor is safe-side fallback.
    #[test]
    fn same_luid_unknown_vendor_selects_cpu_staged_fallback() {
        let path = select_encode_path(LUID_CAPTURE, LUID_CAPTURE, EncoderVendor::Unknown);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "same LUID + Unknown must select CpuStagedFallback"
        );
    }
}
