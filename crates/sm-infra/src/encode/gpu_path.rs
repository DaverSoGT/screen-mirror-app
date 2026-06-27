#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! GPU-resident BGRA→NV12 + DXGI-surface MFT input (encoder-thread only).
//!
//! # Overview
//!
//! This module implements the GPU-resident leg of the capture→encode pipeline
//! (REQ-01, design D3/D4/D5). When the path-selection gate
//! ([`crate::encode::path_select::select_encode_path`]) returns
//! [`crate::encode::path_select::EncodePath::GpuResident`] AND the runtime D3D
//! negotiation succeeds, the encoder feeds the hardware MFT an
//! `MFCreateDXGISurfaceBuffer`-backed NV12 sample whose pixels never left GPU
//! memory: a `VideoProcessorBlt` converts the captured BGRA texture to NV12 on
//! the iGPU, and that NV12 texture is wrapped directly into the `IMFSample`.
//!
//! The implementation is a faithful port of the hardware-validated spike
//! (`examples/qsv_gpu_spike.rs::GpuPipeline`): same struct layout, same
//! `CONTENT_DESC` / view descriptors, same BT.601 limited-range colorspace
//! `_bitfield` setup. The spike ran the chain on the WGC capture thread using
//! WGC's own device; production runs it EXCLUSIVELY on the encoder thread
//! (REQ-07, design THREAD CONTRACT). The cross-thread keyed-mutex texture
//! hand-off that bridges the capture thread to this module is TASK-08 / PR-4 —
//! this module owns only the encoder-thread COM objects and their use.
//!
//! # Thread model
//!
//! Every COM object created here ([`GpuEncodePipeline`], its `ID3D11Video*`
//! interfaces, the `IMFDXGIDeviceManager`, and the per-frame DXGI-surface
//! `IMFSample`) is created on, and used only from, the encoder thread that owns
//! the `IMFTransform`. No interface created here is transferred across a thread
//! boundary. This honors the single-thread COM contract documented in
//! `windows_mft.rs` (the same contract `ComSend` protects on the activate side).
//!
//! # Fallback contract
//!
//! Negotiation is fallible by design (REQ-05, S-04). [`set_d3d_manager`] and
//! [`setup_mft_input_dxgi`] return the failing
//! [`crate::encode::path_select::D3dNegotiationStep`] on rejection so the caller
//! can degrade to the CPU-staged path with a `warn` log instead of aborting the
//! session. None of this code panics on a driver rejection.

use std::time::Duration;

use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIKeyedMutex};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateSample, MFMediaType_Video, MFT_MESSAGE_SET_D3D_MANAGER, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
};
use windows::core::Interface;

use sm_domain::encode::{EncoderConfig, EncoderError};

use crate::encode::path_select::{D3dNegotiationStep, EncodePath, negotiate_gpu_path};

/// Keyed-mutex acquire/release key (design D1). The producer (capture thread)
/// releases with this key after `CopyResource`; the consumer (encoder thread)
/// acquires with the same key before reading. A single shared key is sufficient
/// for the strictly-alternating one-frame-at-a-time hand-off.
pub(crate) const KEYED_MUTEX_KEY: u64 = 0;

/// Create a hardware `ID3D11Device` (BGRA + video support) and return it alongside
/// its adapter LUID packed as `i64` (matching `select_encode_path`'s signature).
///
/// Used by the encoder thread to build the shared device the GPU pipeline and the
/// keyed-mutex consumer run on. The LUID is the encode-adapter LUID published into
/// the [`crate::encode::gpu_handoff::GpuHandoff`] so the capture thread can run the
/// path-selection gate.
///
/// Returns `EncodeFailed` on any creation/LUID-read failure (never panics).
pub(crate) fn create_shared_d3d_device() -> Result<(ID3D11Device, i64), EncoderError> {
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: D3D11CreateDevice with a null adapter selects the default hardware
    // adapter. BGRA + VIDEO support flags are required for the video processor +
    // DXGI-surface MFT input. Out-param `device` is written on Ok.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|e| EncoderError::InitFailed(format!("D3D11CreateDevice: 0x{:08X}", e.code().0)))?;
    let device = device
        .ok_or_else(|| EncoderError::InitFailed("D3D11CreateDevice returned null device".into()))?;
    let luid = adapter_luid_i64(&device)?;
    Ok((device, luid))
}

/// Read the adapter LUID backing an `ID3D11Device`, packed as `i64`.
///
/// Mirrors the spike's `adapter_luid_i64` helper so capture and encode LUIDs share
/// one packing convention (`(HighPart << 32) | LowPart`).
pub(crate) fn adapter_luid_i64(device: &ID3D11Device) -> Result<i64, EncoderError> {
    // SAFETY: ID3D11Device always implements IDXGIDevice; GetAdapter/GetDesc are
    // read-only and the returned adapter is released when it drops.
    unsafe {
        let dxgi: IDXGIDevice = device.cast().map_err(|e| {
            EncoderError::InitFailed(format!("cast IDXGIDevice: 0x{:08X}", e.code().0))
        })?;
        let adapter = dxgi.GetAdapter().map_err(|e| {
            EncoderError::InitFailed(format!("IDXGIDevice::GetAdapter: 0x{:08X}", e.code().0))
        })?;
        let desc = adapter.GetDesc().map_err(|e| {
            EncoderError::InitFailed(format!("IDXGIAdapter::GetDesc: 0x{:08X}", e.code().0))
        })?;
        let LUID { LowPart, HighPart } = desc.AdapterLuid;
        Ok(((HighPart as i64) << 32) | (i64::from(LowPart)))
    }
}

/// Run the live encoder-thread D3D negotiation and build the GPU pipeline.
///
/// This is the production replacement for the PR-2 `negotiate_gpu_path(None)`
/// stub: it performs the real encoder-thread negotiation (TASK-07) —
/// `IMFDXGIDeviceManager`, `METransformSetD3DManager`, and DXGI-surface NV12
/// input — driving any live rejection into the TASK-05 fallback branch.
///
/// # Behaviour
///
/// * `shared_device == None` — no producer is wired yet (PR-3 runtime state: the
///   capture-thread keyed-mutex texture hand-off lands in TASK-08/PR-4). There is
///   no shared D3D11 device to negotiate against, so the session runs on the
///   CPU-staged path. Returns `(CpuStagedFallback, None)` with a debug log; this
///   is NOT a rejection (no WARN) — it is the expected absence of a GPU producer.
/// * `shared_device == Some(device)` — build the pipeline and arm the MFT. On
///   success returns `(GpuResident, Some(pipeline))`. On any rejection, emits the
///   canonical WARN via [`negotiate_gpu_path`] (REQ-05) and returns
///   `(CpuStagedFallback, None)` — the session degrades gracefully, no panic.
///
/// The `(EncodePath, Option<GpuEncodePipeline>)` pair is the single authority the
/// encoder thread uses to decide whether the GPU `GpuShared` arm is live.
pub(crate) fn negotiate_gpu_path_runtime(
    shared_device: Option<&ID3D11Device>,
    mft: &IMFTransform,
    width: u32,
    height: u32,
    config: &EncoderConfig,
) -> (EncodePath, Option<GpuEncodePipeline>) {
    let Some(device) = shared_device else {
        // No GPU producer wired (PR-3). The CPU-staged path drives the session.
        tracing::debug!(
            target: "sm_infra::encode::gpu_path",
            "no shared D3D device available — GPU producer not wired (PR-4); using CpuStagedFallback"
        );
        return (EncodePath::CpuStagedFallback, None);
    };

    match GpuEncodePipeline::build(device, mft, width, height, config) {
        Ok(pipeline) => {
            tracing::info!(
                target: "sm_infra::encode::gpu_path",
                width,
                height,
                "GPU-resident pipeline negotiated (SET_D3D_MANAGER + DXGI NV12 input accepted)"
            );
            (EncodePath::GpuResident, Some(pipeline))
        }
        Err((step, hr)) => {
            // Live rejection → TASK-05 negotiation-fallback branch: negotiate_gpu_path
            // emits the canonical WARN (step + HRESULT) and returns CpuStagedFallback.
            let fallback = negotiate_gpu_path(Some((step, hr)));
            (fallback, None)
        }
    }
}

/// All GPU + device-manager COM objects driven on the encoder thread for the
/// GPU-resident path.
///
/// Built once at MFT setup time (when the shared D3D11 device and the real
/// capture dimensions are known) and reused for every subsequent frame. The
/// reusable `nv12_tex` is the `VideoProcessorBlt` destination; it is re-wrapped
/// into a fresh DXGI-surface `IMFSample` per frame (the texture is shared so a
/// future keyed-mutex consumer can read it, mirroring the spike).
///
/// Field order and the video-processor chain mirror the validated spike
/// `GpuPipeline` exactly (TASK-06 spike-reference contract).
pub(crate) struct GpuEncodePipeline {
    width: u32,
    height: u32,
    // The shared D3D11 device this pipeline was built on. Held so the GpuShared
    // arm can OpenSharedResource the capture-thread keyed-mutex texture (PR-4)
    // on the same device the video processor and MFT use.
    device: ID3D11Device,
    // Video-processor chain (BGRA in → NV12 out, BT.601 limited).
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    vp_enum: ID3D11VideoProcessorEnumerator,
    video_processor: ID3D11VideoProcessor,
    // Reusable NV12 destination texture (DEFAULT usage, render-target, sharable)
    // and its video-processor output view.
    nv12_tex: ID3D11Texture2D,
    nv12_out_view: ID3D11VideoProcessorOutputView,
    // Held for the MFT lifetime: ResetDevice'd onto the shared device and handed
    // to the MFT via MFT_MESSAGE_SET_D3D_MANAGER. Dropping it before the MFT is
    // torn down would invalidate the device manager the encoder is using.
    _device_manager: IMFDXGIDeviceManager,
}

impl GpuEncodePipeline {
    /// Build the encoder-thread GPU pipeline + `IMFDXGIDeviceManager` on
    /// `device`, then arm the MFT via `MFT_MESSAGE_SET_D3D_MANAGER`.
    ///
    /// On success the MFT has accepted the D3D device manager and the returned
    /// pipeline is ready to convert + wrap frames. On failure the function
    /// returns the [`D3dNegotiationStep`] that was rejected (alongside the
    /// HRESULT) so the caller can fall back to the CPU-staged path with a `warn`
    /// log (REQ-05). This function does NOT log — the caller owns the WARN so the
    /// step/HRESULT formatting stays in one place (`path_select::negotiate_gpu_path`).
    ///
    /// `device` MUST be the D3D11 device shared with the capture source (same
    /// adapter LUID — verified by the gate before this is called). `device` and
    /// the resulting pipeline are used exclusively on the calling (encoder) thread.
    pub(crate) fn build(
        device: &ID3D11Device,
        mft: &IMFTransform,
        width: u32,
        height: u32,
        config: &EncoderConfig,
    ) -> Result<Self, (D3dNegotiationStep, u32)> {
        let framerate = if config.framerate == 0 {
            30
        } else {
            config.framerate
        };

        // ── 1. Video device + context from the shared D3D11 device ────────────
        // SAFETY: ID3D11Device implements ID3D11VideoDevice; GetImmediateContext
        // returns the device's immediate context, castable to ID3D11VideoContext.
        // All three live on the encoder thread that owns `device`.
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let immediate_ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let video_context: ID3D11VideoContext = immediate_ctx
            .cast()
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;

        // ── 2. Video processor enumerator + processor: BGRA in, NV12 out ──────
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: framerate,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: framerate,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: video_device is valid; content_desc is a fully-initialized
        // by-value descriptor. Both calls return owned COM interfaces.
        let vp_enum = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let video_processor = unsafe { video_device.CreateVideoProcessor(&vp_enum, 0) }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;

        // BT.601 limited-range output so the HW MFT receives studio-swing NV12,
        // matching the CPU rayon path's color (bgra_to_nv12.rs, BT.601 limited).
        // The D3D11_VIDEO_PROCESSOR_COLOR_SPACE bitfield packs (per spike D4):
        //   bit0 Usage(0=playback), bit1 RGB_Range(0=full 0-255 for BGRA input),
        //   bit2 YCbCr_Matrix(0=BT.601), bit3 YCbCr_xvYCC(0),
        //   bits4-5 Nominal_Range(1=16-235 limited for the NV12 output).
        // Color exactness is GATE-judged (design D4), not byte-pinned.
        // Input (RGB full range): all bits 0.
        let in_colorspace = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        // Output (NV12 BT.601 limited): Nominal_Range = 1 at bit offset 4 → 1 << 4 = 0x10.
        let out_colorspace = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x10 };
        // SAFETY: video_processor + colorspace descriptors are valid; these are
        // state-setters with no out-params and cannot fail (void return).
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&video_processor, 0, &in_colorspace);
            video_context.VideoProcessorSetOutputColorSpace(&video_processor, &out_colorspace);
        }

        // ── 3. NV12 destination texture + output view ─────────────────────────
        // DEFAULT usage, render-target bind (the VP writes it), SHARED so a future
        // keyed-mutex consumer (PR-4) could read it cross-device. Mirrors the spike.
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut nv12_tex: Option<ID3D11Texture2D> = None;
        // SAFETY: nv12_desc is fully initialized; passing None for initial data
        // is valid for a DEFAULT-usage render target. Out-param written on Ok.
        unsafe { device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12_tex)) }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let nv12_tex = nv12_tex.ok_or((D3dNegotiationStep::SetD3dManager, 0))?;

        let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut nv12_out_view: Option<ID3D11VideoProcessorOutputView> = None;
        // SAFETY: nv12_tex + vp_enum + out_view_desc are valid; out-param written on Ok.
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &nv12_tex,
                &vp_enum,
                &out_view_desc,
                Some(&mut nv12_out_view),
            )
        }
        .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let nv12_out_view = nv12_out_view.ok_or((D3dNegotiationStep::SetD3dManager, 0))?;

        // ── 4. IMFDXGIDeviceManager on the shared device ──────────────────────
        let mut reset_token: u32 = 0;
        let mut device_manager: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: out-params (reset_token, device_manager) written on Ok per the
        // MFCreateDXGIDeviceManager contract.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;
        let device_manager = device_manager.ok_or((D3dNegotiationStep::SetD3dManager, 0))?;
        // ResetDevice ties the manager to the shared ID3D11Device — the frame
        // stays on one device end-to-end.
        // SAFETY: device + reset_token are the matched pair from above.
        unsafe { device_manager.ResetDevice(device, reset_token) }
            .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))?;

        // ── 5. SET_D3D_MANAGER — hand the device manager to the MFT (REQ-05) ──
        // On rejection this is the negotiation-fallback trigger (TASK-05 branch):
        // return the SetD3dManager step so the caller degrades to CPU-staged.
        set_d3d_manager(mft, &device_manager)?;

        // ── 6. Negotiate the NV12 DXGI-surface input type (TASK-07) ───────────
        // The MFT output type (H.264) is already negotiated by setup_mft before
        // this is called; here we (re)assert the NV12 input type so the MFT will
        // accept DXGI-surface samples. Rejection is the DxgiInputNegotiation step.
        //
        // Carry-forward #5 (partial-negotiation rollback): SET_D3D_MANAGER already
        // armed the IMFDXGIDeviceManager on the MFT. If the DXGI input type is now
        // rejected and we just returned Err, the caller would fall back to the
        // CPU-staged path while the MFT is still in a hybrid state (D3D manager armed
        // + possibly a mutated input type) and would feed it system-memory
        // MFCreateMemoryBuffer samples. So on rejection we UNDO SET_D3D_MANAGER (null
        // manager) and re-assert the system-memory NV12 input type before returning
        // the error — leaving the MFT clean for the CPU path.
        if let Err(rejection) = setup_mft_input_dxgi(mft, width, height, framerate) {
            rollback_d3d_negotiation(mft, width, height, framerate);
            return Err(rejection);
        }

        Ok(Self {
            width,
            height,
            device: device.clone(),
            video_device,
            video_context,
            vp_enum,
            video_processor,
            nv12_tex,
            nv12_out_view,
            _device_manager: device_manager,
        })
    }

    /// Open a cross-thread shared keyed-mutex BGRA texture by its NT share handle on
    /// this pipeline's device (PR-4 keyed-mutex hand-off consumer, design D1).
    ///
    /// The handle comes from `FramePayload::GpuShared`. The producer (capture thread)
    /// created the texture with `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX` and a NT
    /// handle (`CreateSharedHandle`), so it is opened with `OpenSharedResource1`
    /// (NOT the legacy `OpenSharedResource`, which only accepts non-NT
    /// `SHARED`-flag handles). The opened texture exposes an `IDXGIKeyedMutex`;
    /// [`Self::convert_shared_bgra_to_nv12`] acquires/releases it around the blt.
    ///
    /// Returns `EncodeFailed` on an invalid handle rather than panicking.
    ///
    /// # Safety
    /// `handle` MUST be a live D3D11 keyed-mutex NT share handle produced on a device
    /// that shares the same adapter LUID as this pipeline's device (the gate
    /// guarantees same-adapter). Passing an arbitrary handle is undefined.
    unsafe fn open_shared_bgra(&self, handle: isize) -> Result<ID3D11Texture2D, EncoderError> {
        // ID3D11Device1 exposes OpenSharedResource1 for NT keyed-mutex handles.
        let device1: ID3D11Device1 = self.device.cast().map_err(|e| {
            EncoderError::EncodeFailed(format!("cast ID3D11Device1: 0x{:08X}", e.code().0))
        })?;
        // SAFETY: OpenSharedResource1<T> takes a NT HANDLE and writes an owned T
        // (ID3D11Texture2D, IID inferred from the type) on Ok. The caller's safety
        // contract guarantees a live, same-adapter keyed-mutex NT share handle.
        let tex: ID3D11Texture2D = unsafe {
            device1.OpenSharedResource1(windows::Win32::Foundation::HANDLE(
                handle as *mut core::ffi::c_void,
            ))
        }
        .map_err(|e| {
            EncoderError::EncodeFailed(format!("OpenSharedResource1(bgra): 0x{:08X}", e.code().0))
        })?;
        Ok(tex)
    }

    /// Acquire the keyed mutex on the shared BGRA texture, run `VideoProcessorBlt`
    /// BGRA→NV12, and release the keyed mutex — the full consumer side of the D1
    /// keyed-mutex hand-off.
    ///
    /// The `AcquireSync`/`ReleaseSync` pair is balanced on BOTH the success and the
    /// error path (no leaked lock that would deadlock the producer's next acquire).
    ///
    /// `blt_hold` (GATE B metric #3) is an out-param: the wall-clock span the consumer
    /// HELD the keyed mutex (AcquireSync → ReleaseSync, the whole blt). The producer's
    /// non-blocking acquire (Fix 1) skips a frame whenever this hold overlaps a capture
    /// callback, so a long hold here directly maps to producer skips. Written on every
    /// path that takes the lock.
    ///
    /// # Safety
    /// `bgra_tex` MUST be a texture opened via [`Self::open_shared_bgra`] that
    /// exposes an `IDXGIKeyedMutex` (created with `SHARED_KEYEDMUTEX`).
    unsafe fn convert_shared_bgra_to_nv12(
        &self,
        bgra_tex: &ID3D11Texture2D,
        blt_hold: &mut Duration,
    ) -> Result<(), EncoderError> {
        let keyed: IDXGIKeyedMutex = bgra_tex.cast().map_err(|e| {
            EncoderError::EncodeFailed(format!("cast IDXGIKeyedMutex: 0x{:08X}", e.code().0))
        })?;
        // Read the RAW HRESULT from AcquireSync rather than the safe `-> Result<()>`
        // wrapper. The INFINITE wait cannot return WAIT_TIMEOUT, but it CAN return
        // WAIT_ABANDONED_0 (0x80) if the producer thread dies while holding the mutex —
        // and 0x80 is a NON-NEGATIVE HRESULT, so windows-rs `.ok()` would wrongly report
        // it as success and we would blt an undefined-state texture under the prior
        // timestamp. Reading the numeric code lets us reject abandoned explicitly.
        // SAFETY: `keyed` is a live IDXGIKeyedMutex; the AcquireSync vtable slot takes
        // (this, key, milliseconds) and returns the raw HRESULT.
        let hr = unsafe {
            (Interface::vtable(&keyed).AcquireSync)(
                Interface::as_raw(&keyed),
                KEYED_MUTEX_KEY,
                u32::MAX,
            )
        };
        // Named HRESULT codes for the raw AcquireSync classification. These mirror the
        // producer side in `capture::gpu_producer`; they are Win32 ABI-fixed values, kept
        // local to each side to avoid a capture<->encode coupling for three constants.
        const S_OK_HR: u32 = 0x0000_0000;
        const WAIT_ABANDONED_HR: u32 = 0x0000_0080;
        const HRESULT_FAILURE_BIT: u32 = 0x8000_0000;
        // Default the out-param to zero so the abandoned / failed early-returns below report
        // "no hold" (we did not blt). It is overwritten with the real span on the held path.
        *blt_hold = Duration::ZERO;
        match hr.0 as u32 {
            S_OK_HR => {} // mutex held, proceed to blt + release.
            WAIT_ABANDONED_HR => {
                // WAIT_ABANDONED_0: per Win32 we DID acquire the mutex (the prior owner
                // died), so we MUST release it to not leave it held forever, but the
                // texture contents are undefined — do NOT blt; surface the error so the
                // encoder degrades.
                // SAFETY: WAIT_ABANDONED_0 grants ownership, so this ReleaseSync is the
                // balanced counterpart of the acquire above.
                let _ = unsafe { keyed.ReleaseSync(KEYED_MUTEX_KEY) };
                return Err(EncoderError::EncodeFailed(
                    "KeyedMutex AcquireSync: WAIT_ABANDONED_0 (producer died holding the mutex)"
                        .into(),
                ));
            }
            // Any negative HRESULT is a genuine acquire failure; we do NOT hold the mutex,
            // so we must NOT ReleaseSync.
            other if other & HRESULT_FAILURE_BIT != 0 => {
                return Err(EncoderError::EncodeFailed(format!(
                    "KeyedMutex AcquireSync: 0x{other:08X}"
                )));
            }
            // Other non-negative codes (e.g. S_FALSE) still mean we hold the mutex.
            _ => {}
        }
        // GATE B metric #3: time the span the mutex is HELD (here through ReleaseSync).
        // This is exactly the window during which a producer try-acquire would skip.
        let hold_started = std::time::Instant::now();
        let blt = self.gpu_bgra_to_nv12(bgra_tex);
        // SAFETY: ReleaseSync is paired with the AcquireSync above; it MUST run on
        // both the Ok and Err blt paths so the producer's next AcquireSync does not
        // deadlock. We use the same key so the texture returns to the unlocked state.
        let release = unsafe { keyed.ReleaseSync(KEYED_MUTEX_KEY) };
        *blt_hold = hold_started.elapsed();
        blt?;
        release.map_err(|e| {
            EncoderError::EncodeFailed(format!("KeyedMutex ReleaseSync: 0x{:08X}", e.code().0))
        })
    }

    /// Consume one cross-thread shared keyed-mutex BGRA frame end-to-end: open the
    /// share handle on the encoder device, acquire the keyed mutex, run
    /// `VideoProcessorBlt` BGRA→NV12 on the GPU, and release the keyed mutex.
    ///
    /// This is the single PR-4 entry point the encoder pump calls per `GpuShared`
    /// frame. The opened `ID3D11Texture2D` (one COM ref per frame) is released when
    /// it drops at the end of this function — no per-frame COM leak (design D1; the
    /// PR-3 `ManuallyDrop` input-view leak is NOT reintroduced because
    /// `gpu_bgra_to_nv12` drops its input view internally).
    ///
    /// `blt_hold` (GATE B metric #3) reports the keyed-mutex hold span for this frame.
    ///
    /// # Safety
    /// `handle` MUST be a live keyed-mutex NT share handle for a BGRA texture on a
    /// device sharing this pipeline's adapter LUID (gate-guaranteed same-adapter).
    pub(crate) unsafe fn consume_shared_bgra(
        &self,
        handle: isize,
        blt_hold: &mut Duration,
    ) -> Result<(), EncoderError> {
        // SAFETY: handle contract forwarded from the caller (FramePayload::GpuShared).
        let bgra_tex = unsafe { self.open_shared_bgra(handle) }?;
        // SAFETY: bgra_tex was opened above with SHARED_KEYEDMUTEX, so it exposes an
        // IDXGIKeyedMutex. AcquireSync/ReleaseSync are balanced inside.
        unsafe { self.convert_shared_bgra_to_nv12(&bgra_tex, blt_hold) }
        // bgra_tex (and its keyed mutex cast) drop here → COM Release, no leak.
    }

    /// Convert a BGRA texture to NV12 on the GPU via `VideoProcessorBlt`, writing
    /// into this pipeline's reusable NV12 destination texture.
    ///
    /// This is the TASK-06 L2 conversion entry point: pure GPU work, no CPU
    /// readback, no rayon convert. The input view is created per call over the
    /// supplied source texture (the source identity changes each frame in PR-4).
    pub(crate) fn gpu_bgra_to_nv12(
        &self,
        src_bgra_tex: &ID3D11Texture2D,
    ) -> Result<(), EncoderError> {
        // Input view over the source BGRA texture.
        let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0, // 0 → use the texture's own format (BGRA8).
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: src_bgra_tex + vp_enum + in_view_desc are valid; out-param on Ok.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                src_bgra_tex,
                &self.vp_enum,
                &in_view_desc,
                Some(&mut in_view),
            )
        }
        .map_err(|e| {
            EncoderError::EncodeFailed(format!(
                "CreateVideoProcessorInputView: 0x{:08X}",
                e.code().0
            ))
        })?;
        let in_view = in_view.ok_or_else(|| {
            EncoderError::EncodeFailed("CreateVideoProcessorInputView returned null".into())
        })?;

        // VideoProcessorBlt: BGRA → NV12 on the GPU.
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];
        // SAFETY: video_processor + nv12_out_view are valid; `streams` borrows the
        // input view for the duration of the call.
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.video_processor,
                &self.nv12_out_view,
                0,
                &streams,
            )
        }
        .map_err(|e| {
            EncoderError::EncodeFailed(format!("VideoProcessorBlt: 0x{:08X}", e.code().0))
        });
        // SAFETY: `D3D11_VIDEO_PROCESSOR_STREAM` is a windows-rs FFI struct with no
        // `Drop`, and `ManuallyDrop` inhibits the inner `Option<...InputView>`'s
        // destructor — so windows-rs requires the caller to release COM-interface
        // fields manually. We drop `pInputSurface` here (releasing the one COM ref
        // created above) on BOTH the success and error paths, before returning, to
        // avoid leaking one `ID3D11VideoProcessorInputView` per converted frame.
        // `pInputSurfaceRight` holds `None` (no COM ref) but is dropped for symmetry.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurfaceRight);
        }
        result
    }

    /// Wrap this pipeline's NV12 texture in a DXGI-surface-backed `IMFSample`
    /// (TASK-07 L1), additive alongside the CPU `MFCreateMemoryBuffer` path.
    ///
    /// Call AFTER [`Self::gpu_bgra_to_nv12`] has converted the current frame into
    /// `nv12_tex`. The returned sample references the texture directly — no CPU
    /// memory buffer is allocated and no readback occurs.
    pub(crate) fn build_dxgi_imfsample(
        &self,
        timestamp: Duration,
        duration_100ns: i64,
    ) -> Result<IMFSample, EncoderError> {
        // SAFETY: nv12_tex is a valid ID3D11Texture2D; MFCreateDXGISurfaceBuffer
        // wraps subresource 0 (FALSE = not a bottom-up surface). Owned buffer on Ok.
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12_tex, 0, false) }
                .map_err(|e| {
                EncoderError::EncodeFailed(format!(
                    "MFCreateDXGISurfaceBuffer: 0x{:08X}",
                    e.code().0
                ))
            })?;

        let sample = unsafe { MFCreateSample() }.map_err(|e| {
            EncoderError::EncodeFailed(format!("MFCreateSample: 0x{:08X}", e.code().0))
        })?;

        // SAFETY: sample + buffer are valid owned interfaces; AddBuffer/SetSample*
        // are standard MF sample construction calls.
        unsafe {
            sample.AddBuffer(&buffer).map_err(|e| {
                EncoderError::EncodeFailed(format!("AddBuffer(dxgi): 0x{:08X}", e.code().0))
            })?;
            let ts_100ns = timestamp.as_nanos() as i64 / 100;
            sample.SetSampleTime(ts_100ns).map_err(|e| {
                EncoderError::EncodeFailed(format!("SetSampleTime(dxgi): 0x{:08X}", e.code().0))
            })?;
            sample.SetSampleDuration(duration_100ns).map_err(|e| {
                EncoderError::EncodeFailed(format!("SetSampleDuration(dxgi): 0x{:08X}", e.code().0))
            })?;
        }

        Ok(sample)
    }

    /// Configured frame dimensions this pipeline was built for.
    ///
    /// Used by the caller to debug-assert the incoming frame geometry matches
    /// (mirrors the spike's `debug_assert_eq!` parity check).
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Hand the `IMFDXGIDeviceManager` to the MFT via
/// `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER)` (`METransformSetD3DManager`).
///
/// This is the TASK-07 D3D-manager wiring and the live trigger for the TASK-05
/// negotiation-fallback branch: on rejection it returns
/// `(D3dNegotiationStep::SetD3dManager, hresult)` so the caller degrades to the
/// CPU-staged path with a `warn` log (REQ-05). It never panics.
pub(crate) fn set_d3d_manager(
    mft: &IMFTransform,
    device_manager: &IMFDXGIDeviceManager,
) -> Result<(), (D3dNegotiationStep, u32)> {
    // SAFETY: SET_D3D_MANAGER takes the device-manager interface pointer as the
    // ULONG_PTR message parameter per the Media Foundation contract. The manager
    // outlives this call (it is held by GpuEncodePipeline for the MFT lifetime).
    unsafe {
        mft.ProcessMessage(
            MFT_MESSAGE_SET_D3D_MANAGER,
            device_manager.as_raw() as usize,
        )
    }
    .map_err(|e| (D3dNegotiationStep::SetD3dManager, e.code().0 as u32))
}

/// Roll back a partial GPU negotiation so the MFT is clean for the CPU-staged path
/// (PR-4 carry-forward #5).
///
/// Called when `SET_D3D_MANAGER` succeeded but the subsequent DXGI input
/// negotiation was rejected. Sends `SET_D3D_MANAGER` with a NULL manager pointer to
/// disarm the `IMFDXGIDeviceManager`, then re-asserts the system-memory NV12 input
/// type so the CPU path's `MFCreateMemoryBuffer` samples are accepted. Both calls
/// are best-effort: any failure is logged at `warn` but does not change the outcome
/// (the caller is already falling back to CPU and will surface its own degradation).
fn rollback_d3d_negotiation(mft: &IMFTransform, w: u32, h: u32, framerate: u32) {
    // Disarm the D3D manager (NULL pointer param).
    // SAFETY: SET_D3D_MANAGER with a 0 ULONG_PTR clears the device manager per the
    // MF contract; it is the documented way to detach a previously-set manager.
    if let Err(e) = unsafe { mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0) } {
        tracing::warn!(
            target: "sm_infra::encode::gpu_path",
            "rollback: clearing SET_D3D_MANAGER (null) failed: 0x{:08X} (continuing CPU fallback)",
            e.code().0
        );
    }
    // Re-assert the system-memory NV12 input type (same attributes as setup_mft's
    // CPU input type) so the CPU path is accepted on a clean MFT.
    if let Err((step, hr)) = setup_mft_input_dxgi(mft, w, h, framerate) {
        tracing::warn!(
            target: "sm_infra::encode::gpu_path",
            "rollback: re-asserting CPU NV12 input type failed at {step}: 0x{hr:08X} (continuing CPU fallback)"
        );
    }
}

/// Negotiate the NV12 input type so the MFT accepts DXGI-surface NV12 samples.
///
/// Ported from the spike's `setup_mft_input_nv12`. On rejection returns
/// `(D3dNegotiationStep::DxgiInputNegotiation, hresult)` so the caller degrades
/// to the CPU-staged path (REQ-05, S-04). The byte layout of the input media
/// type matches `setup_mft`'s system-memory NV12 type exactly (same attributes)
/// — the only difference is that the MFT now has a D3D manager, so it advertises
/// DXGI-surface input support for the same NV12 subtype.
pub(crate) fn setup_mft_input_dxgi(
    mft: &IMFTransform,
    w: u32,
    h: u32,
    framerate: u32,
) -> Result<(), (D3dNegotiationStep, u32)> {
    let in_type: IMFMediaType = unsafe { MFCreateMediaType() }
        .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;

    // SAFETY: in_type is a fresh owned media type; every Set* call writes one
    // attribute into its store. SetInputType applies it to stream 0.
    unsafe {
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        in_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        in_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
        mft.SetInputType(0, &in_type, 0)
            .map_err(|e| (D3dNegotiationStep::DxgiInputNegotiation, e.code().0 as u32))?;
    }

    Ok(())
}
