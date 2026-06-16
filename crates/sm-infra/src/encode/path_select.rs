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

// ── D3D negotiation-rejection fallback (PR-2 seam; TASK-05) ─────────────────

/// Step name returned by [`negotiate_gpu_path`] on rejection, for log pinning.
///
/// The WARN log must name the failed step so operators can diagnose why the
/// GPU-resident path fell back to CPU-staged (REQ-05).
///
/// PR-3: the variants are now constructed from live MFT COM results in
/// [`crate::encode::gpu_path`] (`set_d3d_manager` → `SetD3dManager`,
/// `setup_mft_input_dxgi` → `DxgiInputNegotiation`), so they are no longer
/// dead code on the non-test lib target — the PR-2 `allow(dead_code)` is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum D3dNegotiationStep {
    /// `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER)` — i.e. `METransformSetD3DManager`.
    SetD3dManager,
    /// `MFCreateDXGISurfaceBuffer` / `SetInputType` with DXGI NV12 surface.
    DxgiInputNegotiation,
}

impl std::fmt::Display for D3dNegotiationStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetD3dManager => write!(f, "METransformSetD3DManager"),
            Self::DxgiInputNegotiation => {
                write!(f, "MFCreateDXGISurfaceBuffer / SetInputType DXGI")
            }
        }
    }
}

/// Emit the canonical negotiation-rejection WARN and resolve the fallback path
/// (REQ-05, S-04).
///
/// This is the single source of truth for the negotiation-fallback log + result:
/// * `Some((step, hr))` — a driver rejected `step` with HRESULT `hr`. Emits a
///   `warn` log naming the step + HRESULT and returns `CpuStagedFallback`. Never
///   panics. The live production caller is
///   [`crate::encode::gpu_path::negotiate_gpu_path_runtime`], which passes the real
///   `(step, hr)` from `set_d3d_manager` / `setup_mft_input_dxgi`.
/// * `None` — no rejection. Returns `GpuResident`. This arm is the success sentinel
///   used by the TASK-05 unit tests; production code reaches the success case
///   through `negotiate_gpu_path_runtime` (which returns the built pipeline), so it
///   does not call this with `None`.
///
/// # Arguments
///
/// * `inject_rejection` — `None` (no rejection → `GpuResident`); `Some((step, hr))`
///   for a real or simulated driver rejection of `step` with Windows HRESULT `hr`.
pub(crate) fn negotiate_gpu_path(
    inject_rejection: Option<(D3dNegotiationStep, u32)>,
) -> EncodePath {
    match inject_rejection {
        None => {
            // Success sentinel: no rejection was reported. Production reaches the
            // success path via negotiate_gpu_path_runtime (which returns the built
            // pipeline); this branch backs the TASK-05 no-rejection unit test.
            EncodePath::GpuResident
        }
        Some((step, hr)) => {
            tracing::warn!(
                target: "sm_infra::encode::path_select",
                step = %step,
                hresult = format!("0x{:08X}", hr).as_str(),
                "D3D negotiation rejected — falling back to CpuStagedFallback (REQ-05)"
            );
            EncodePath::CpuStagedFallback
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{D3dNegotiationStep, EncodePath, negotiate_gpu_path, select_encode_path};
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

    // ── TASK-04: NVENC byte-identical config-pinning regression ──────────────

    /// T-PS-NVENC-01 (TASK-04, REQ-02, S-06): on a cross-adapter NVENC machine
    /// (AMD iGPU captures, NVIDIA dGPU encodes) the gate selects CpuStagedFallback.
    ///
    /// Synthetic LUID topology mirrors the real NVENC machine: AMD iGPU LUID ≠
    /// NVIDIA dGPU LUID AND vendor == NvidiaNvenc → both gate conditions fail.
    #[test]
    fn nvenc_cross_adapter_selects_cpu_staged_fallback_task04() {
        // AMD capture adapter LUID (synthetic — mimics the real NVENC machine topology
        // where the iGPU is AMD and the encoder is on the NVIDIA dGPU).
        let luid_amd: i64 = 0x0000_1002_0000_0001_u64 as i64;
        // NVIDIA encode adapter LUID (different from AMD capture LUID).
        let luid_nvidia: i64 = 0x0000_10DE_0000_0001_u64 as i64;

        let path = select_encode_path(luid_amd, luid_nvidia, EncoderVendor::NvidiaNvenc);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "cross-adapter NVENC (AMD iGPU + NVIDIA dGPU) must select CpuStagedFallback"
        );
    }

    /// T-PS-NVENC-02 (TASK-04, REQ-02): same-adapter NVENC also falls back.
    ///
    /// The vendor floor (NvidiaNvenc) independently rejects GpuResident even
    /// when the adapter LUIDs happen to match.
    #[test]
    fn nvenc_same_adapter_also_selects_cpu_staged_fallback_task04() {
        let luid: i64 = 0x0000_10DE_0000_0002_u64 as i64;
        let path = select_encode_path(luid, luid, EncoderVendor::NvidiaNvenc);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "same-adapter NvidiaNvenc must select CpuStagedFallback (vendor floor)"
        );
    }

    /// T-PS-NVENC-03 (TASK-04, REQ-02): when gate selects CpuStagedFallback,
    /// the result is NOT GpuResident — asserts the DXGI-manager seam is unreachable.
    ///
    /// In production code, GpuResident is the only arm that would call
    /// IMFDXGIDeviceManager / METransformSetD3DManager.  Since NVENC always yields
    /// CpuStagedFallback, those COM calls are structurally unreachable on NVENC.
    /// This test pins that contract: any future refactor that returns GpuResident
    /// for NVENC would break this assertion before reaching the COM layer.
    #[test]
    fn nvenc_path_result_is_never_gpu_resident_so_d3d_manager_is_unreachable_task04() {
        let luid_amd: i64 = 0x0000_1002_ABCD_0001_u64 as i64;
        let luid_nvidia: i64 = 0x0000_10DE_ABCD_0002_u64 as i64;

        let path = select_encode_path(luid_amd, luid_nvidia, EncoderVendor::NvidiaNvenc);

        // Not GpuResident ⟹ no IMFDXGIDeviceManager / METransformSetD3DManager call
        // is reachable on the NVENC code path (design §NVENC-Protection Proof, REQ-02).
        assert_ne!(
            path,
            EncodePath::GpuResident,
            "NVENC path must never reach GpuResident (D3D manager seam unreachable)"
        );
    }

    // ── TASK-05: D3D negotiation-rejection fallback ───────────────────────────
    //
    // Tests for the `negotiate_gpu_path` seam (REQ-05, S-04).
    // All CI-runnable: error-code injection into the setup seam.

    /// T-NEG-01 (TASK-05, REQ-05): injecting a SetD3dManager rejection selects fallback.
    ///
    /// Simulates the MFT rejecting `METransformSetD3DManager` (e.g., driver returns
    /// MF_E_INVALIDTYPE / 0xC00D36B4).  Asserts CpuStagedFallback is returned.
    #[test]
    #[tracing_test::traced_test]
    fn d3d_set_manager_rejection_selects_cpu_staged_fallback_task05() {
        // Simulate METransformSetD3DManager rejection: MF_E_INVALIDTYPE = 0xC00D36B4.
        let path = negotiate_gpu_path(Some((D3dNegotiationStep::SetD3dManager, 0xC00D36B4)));

        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "SetD3dManager rejection must degrade to CpuStagedFallback (REQ-05)"
        );
    }

    /// T-NEG-02 (TASK-05, REQ-05): warn log is emitted with the failed step name.
    ///
    /// The WARN log must contain the step name so operators can identify which
    /// negotiation stage failed (REQ-05 "log the rejection reason at warn level").
    #[test]
    #[tracing_test::traced_test]
    fn d3d_rejection_emits_warn_log_with_step_name_task05() {
        let _ = negotiate_gpu_path(Some((D3dNegotiationStep::SetD3dManager, 0xC00D36B4)));

        // `logs_contain` is injected by tracing_test::traced_test.
        assert!(
            logs_contain("METransformSetD3DManager"),
            "warn log must name the failed negotiation step (METransformSetD3DManager)"
        );
        assert!(
            logs_contain("CpuStagedFallback"),
            "warn log must mention CpuStagedFallback (operator visibility)"
        );
    }

    /// T-NEG-03 (TASK-05, REQ-05): DXGI input negotiation rejection also falls back.
    ///
    /// Simulates `SetInputType` with DXGI NV12 surface rejected by the MFT
    /// (e.g., driver returns MF_E_INVALIDMEDIATYPE / 0xC00D36B6).
    #[test]
    #[tracing_test::traced_test]
    fn dxgi_input_rejection_selects_cpu_staged_fallback_task05() {
        let path = negotiate_gpu_path(Some((
            D3dNegotiationStep::DxgiInputNegotiation,
            0xC00D36B6, // MF_E_INVALIDMEDIATYPE
        )));

        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "DXGI input negotiation rejection must degrade to CpuStagedFallback (REQ-05)"
        );
    }

    /// T-NEG-04 (TASK-05, REQ-05): negotiation rejection does NOT panic.
    ///
    /// The pipeline must continue running after a negotiation failure — the warn log
    /// is the only observable side effect; the function returns normally.
    #[test]
    fn d3d_negotiation_rejection_does_not_panic_task05() {
        // This would catch any unreachable!() / panic!() in the rejection path.
        let path = negotiate_gpu_path(Some((D3dNegotiationStep::SetD3dManager, 0x80004005)));
        assert_eq!(path, EncodePath::CpuStagedFallback);
    }

    /// T-NEG-05 (TASK-05): happy path (no rejection) → negotiate_gpu_path returns GpuResident stub.
    ///
    /// Verifies the no-injection branch works correctly (PR-2 stub: always GpuResident
    /// until PR-3 replaces with real COM calls).
    #[test]
    fn no_rejection_returns_gpu_resident_stub_task05() {
        let path = negotiate_gpu_path(None);
        assert_eq!(
            path,
            EncodePath::GpuResident,
            "no injection must return GpuResident (PR-2 stub — real COM added in PR-3)"
        );
    }
}
