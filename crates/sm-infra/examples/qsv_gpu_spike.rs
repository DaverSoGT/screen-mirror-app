//! GPU-resident capture→encode validation spike (SDD `qsv-igpu-pipeline-perf`, TASK-01).
//!
//! # WHAT THIS IS
//!
//! A throwaway GO/NO-GO **measurement harness** — NOT production code. It exercises the
//! GPU-resident path end-to-end on a real Intel-QSV iGPU and measures `capture_fps` /
//! `encode_fps` so we can decide whether to build the full L1+L2 pipeline (TASK-06..08)
//! or fall back to the previously-scoped L3 adaptive relief valve.
//!
//! It deliberately does NOT do TDD: the whole thing is hardware-bound D3D11/DXGI/MFT
//! interop that cannot be honestly unit-tested without an iGPU. The empirical sender-log
//! `capture_fps` reading IS the test (design §Validation Spike, tasks GATE A).
//!
//! # THE PIPELINE THIS PROVES (per frame, all on the WGC capture thread)
//!
//! ```text
//!   WGC BGRA texture (iGPU, windows-capture's own device)
//!     → VideoProcessorBlt BGRA→NV12 (BT.601 limited range) into an NV12 ID3D11Texture2D
//!       → MFCreateDXGISurfaceBuffer(nv12_texture) → IMFSample
//!         → IMFTransform::ProcessInput  (HW MFT, IMFDXGIDeviceManager + SET_D3D_MANAGER set first)
//!           → drain ProcessOutput
//!   NO CPU readback, NO rayon convert, NO re-upload — fully GPU-resident.
//! ```
//!
//! # WHY THE SPIKE RUNS ON THE CAPTURE THREAD (and production will not)
//!
//! windows-capture 2.0.0 exposes `frame.device()`, `frame.device_context()` and
//! `frame.as_raw_texture()` (verified — see D2 resolution logged at startup). Inside
//! `on_frame_arrived` we already hold WGC's own ID3D11Device + the live BGRA texture, so
//! the spike does the whole GPU chain right there on WGC's device — no cross-thread
//! keyed-mutex hand-off is needed to MEASURE feasibility.
//!
//! Production (TASK-08) is different: the HW MFT lives on a dedicated encoder thread
//! (windows_mft.rs COM contract), so it WILL need the capture-thread `CopyResource` into a
//! shared keyed-mutex texture (design D1). The spike proves the expensive parts work
//! (VideoProcessorBlt color, SET_D3D_MANAGER acceptance, DXGI-surface ProcessInput,
//! sustained fps); the keyed-mutex plumbing is mechanical and added later.
//!
//! # USAGE (run on the Intel-QSV machine)
//!
//! ```text
//! RUST_LOG=debug cargo run -p sm-infra --example qsv_gpu_spike --features hw-encoder
//! ```
//!
//! Play the SAME fullscreen 60fps content used for the QSV2 gate, full-screen, on the
//! primary monitor, then read the per-second `capture_fps` / `encode_fps` lines. See the
//! RUN GUIDE printed at the end of this file's `main`.
//!
//! # ACCEPTANCE (GATE A)
//!
//! Skip the first 10 warmup samples. GO if steady-state `capture_fps` is sustained
//! ≥ ~55 fps at native 1440p with jitter materially below the QSV2 20.9→60.1 spread.
//! NO-GO otherwise (iGPU ceiling), or if any GPU/DXGI/MFT step fails un-recoverably —
//! that failure is itself a NO-GO signal and the harness logs it and exits cleanly.

#![cfg(all(target_os = "windows", feature = "hw-encoder"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_RESOURCE_MISC_SHARED,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFTransform, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample, MFMediaType_Video,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFStartup,
    MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFSTARTUP_FULL,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::core::Interface;

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

// ── Production parity constants (reuse sender defaults) ────────────────────────
// SENDER_ENCODER_FRAMERATE = 60 (src-tauri sender.rs), EncoderConfig bitrate default = 4 Mbps.
const TARGET_FRAMERATE: u32 = 60;
const TARGET_BITRATE_BPS: u32 = 4_000_000;

/// How long the spike runs before tearing down and exiting (seconds).
const RUN_SECONDS: u64 = 30;

// ── COM guards (mirror windows_mft.rs run_encoder_thread teardown order) ────────

struct CoUninitGuard;
impl Drop for CoUninitGuard {
    fn drop(&mut self) {
        // SAFETY: paired with CoInitializeEx on this thread (no-op if init failed, per docs).
        unsafe { CoUninitialize() };
    }
}

// ── Adapter LUID helper (for device-path logging + own-device fallback parity) ──

/// Read the adapter LUID backing an `ID3D11Device` (via IDXGIDevice → adapter → desc).
///
/// Returned as a packed `i64` so it matches the future `select_encode_path(capture_luid,
/// encode_luid, ..)` pure-fn signature (design D6). On any failure returns `None` and the
/// caller logs that the LUID could not be resolved (non-fatal for the spike).
fn adapter_luid_i64(device: &ID3D11Device) -> Option<i64> {
    // SAFETY: ID3D11Device always implements IDXGIDevice; GetAdapter/GetDesc are read-only.
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let adapter = dxgi.GetAdapter().ok()?;
        let desc = adapter.GetDesc().ok()?;
        let LUID { LowPart, HighPart } = desc.AdapterLuid;
        Some(((HighPart as i64) << 32) | (LowPart as i64))
    }
}

// ── GPU pipeline state (created lazily on the first frame, reused thereafter) ───

/// All GPU + MFT objects the spike drives on the WGC capture thread.
///
/// Created once on the first `on_frame_arrived` (when we first see the real capture
/// dimensions + WGC's device) and reused for every subsequent frame. Every COM object
/// here is created on, and used only from, the WGC capture thread — honoring the
/// single-thread COM contract (no cross-thread transfer).
struct GpuPipeline {
    width: u32,
    height: u32,
    // Video processor chain (BGRA → NV12).
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    vp_enum: ID3D11VideoProcessorEnumerator,
    video_processor: ID3D11VideoProcessor,
    // Reusable NV12 destination texture (DEFAULT usage, sharable, render-target-able).
    nv12_tex: ID3D11Texture2D,
    nv12_out_view: ID3D11VideoProcessorOutputView,
    // Media Foundation transform (HW H.264 encoder) + its event generator.
    mft: IMFTransform,
    // Held to keep the async-MFT event model alive for the MFT's lifetime. The spike drains
    // ProcessOutput synchronously (best-effort) rather than waiting on METransformHaveOutput,
    // so it is not polled here — production (TASK-08) uses it via the windows_mft.rs pump loop.
    _event_gen: IMFMediaEventGenerator,
    // Kept alive for the MFT's lifetime (ResetDevice'd onto the shared device).
    _device_manager: IMFDXGIDeviceManager,
}

impl GpuPipeline {
    /// Build the entire GPU + MFT chain on WGC's own device. Returns the pipeline plus
    /// two negotiation facts the spike must report: whether SET_D3D_MANAGER was accepted
    /// and whether the DXGI-surface NV12 input type negotiated.
    ///
    /// # Safety
    /// All COM calls run on the WGC capture thread that owns `device` / `context`.
    unsafe fn build(
        device: &ID3D11Device,
        _context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> windows::core::Result<(Self, bool, bool)> {
        // ── 1. Video device + context from WGC's own device ────────────────────
        let video_device: ID3D11VideoDevice = device.cast()?;
        let immediate_ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext()? };
        let video_context: ID3D11VideoContext = immediate_ctx.cast()?;

        // ── 2. Video processor enumerator + processor: BGRA in, NV12 out ───────
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: TARGET_FRAMERATE, Denominator: 1 },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL { Numerator: TARGET_FRAMERATE, Denominator: 1 },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let vp_enum = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        let video_processor = unsafe { video_device.CreateVideoProcessor(&vp_enum, 0)? };

        // BT.601 limited-range output so the HW MFT receives studio-swing NV12, matching the
        // CPU rayon path's color (bgra_to_nv12.rs). The D3D11_VIDEO_PROCESSOR_COLOR_SPACE
        // bitfield packs: bit0 Usage(0=playback), bit1 RGB_Range(0=full 0-255 for our BGRA
        // input), bit2 YCbCr_Matrix(0=BT.601), bit3 YCbCr_xvYCC(0), bits4-5 Nominal_Range
        // (1=16-235 limited for the NV12 output). Color exactness is GATE-judged (design D4),
        // not byte-pinned — if QSV rejects this it's a NO-GO signal we log.
        // Input (RGB full range): all bits 0.
        let in_colorspace = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        // Output (NV12 BT.601 limited): Nominal_Range = 1 at bit offset 4 → 1 << 4 = 0x10.
        let out_colorspace = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x10 };
        unsafe {
            video_context.VideoProcessorSetStreamColorSpace(&video_processor, 0, &in_colorspace);
            video_context.VideoProcessorSetOutputColorSpace(&video_processor, &out_colorspace);
        }

        // ── 3. NV12 destination texture + output view ──────────────────────────
        // DEFAULT usage, render-target bind (VP writes it), SHARED so it could later feed a
        // cross-device hand-off (production keyed-mutex texture; here just for parity).
        let nv12_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut nv12_tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12_tex))? };
        let nv12_tex = nv12_tex.expect("CreateTexture2D succeeded but returned null");

        let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut nv12_out_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &nv12_tex,
                &vp_enum,
                &out_view_desc,
                Some(&mut nv12_out_view),
            )?
        };
        let nv12_out_view = nv12_out_view.expect("CreateVideoProcessorOutputView returned null");

        // ── 4. IMFDXGIDeviceManager on WGC's device ────────────────────────────
        let mut reset_token: u32 = 0;
        let mut device_manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)? };
        let device_manager = device_manager.expect("MFCreateDXGIDeviceManager returned null");
        // ResetDevice ties the manager to WGC's own ID3D11Device — the GPU stays on one device.
        unsafe { device_manager.ResetDevice(device, reset_token)? };

        // ── 5. Select + activate a hardware H.264 MFT ──────────────────────────
        let mft = unsafe { select_hardware_h264_mft()? };

        // Async unlock MUST be set before any other call on an async HW MFT.
        let attrs = unsafe { mft.GetAttributes()? };
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)? };

        // ── 6. SET_D3D_MANAGER — hand the device manager to the MFT ────────────
        // A rejection (HRESULT error) is a NO-GO signal for the GPU path, but we keep going
        // so the rest of the spike can report what else fails. Report acceptance to the log.
        let set_d3d_manager_accepted = unsafe {
            mft.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                device_manager.as_raw() as usize,
            )
        }
        .is_ok();

        // ── 7. Negotiate output (H.264) then input (NV12 via DXGI surfaces) ────
        unsafe { setup_mft_output(&mft, width, height)? };
        let dxgi_input_negotiated = unsafe { setup_mft_input_nv12(&mft, width, height) }.is_ok();

        // Begin streaming.
        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let event_gen: IMFMediaEventGenerator = mft.cast()?;

        Ok((
            Self {
                width,
                height,
                video_device,
                video_context,
                vp_enum,
                video_processor,
                nv12_tex,
                nv12_out_view,
                mft,
                _event_gen: event_gen,
                _device_manager: device_manager,
            },
            set_d3d_manager_accepted,
            dxgi_input_negotiated,
        ))
    }

    /// Run one frame: VideoProcessorBlt BGRA→NV12, wrap NV12 in a DXGI-surface MFSample,
    /// ProcessInput, then best-effort drain ProcessOutput. Returns `Ok(())` on success.
    ///
    /// # Safety
    /// Runs on the WGC capture thread that owns all the COM objects + `bgra_tex`.
    unsafe fn process_frame(
        &self,
        bgra_tex: &ID3D11Texture2D,
        timestamp: Duration,
    ) -> windows::core::Result<()> {
        // 1. Input view over the live WGC BGRA texture.
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
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                bgra_tex,
                &self.vp_enum,
                &in_view_desc,
                Some(&mut in_view),
            )?
        };
        let in_view = in_view.expect("CreateVideoProcessorInputView returned null");

        // 2. VideoProcessorBlt: BGRA → NV12 on the GPU.
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
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
        };
        unsafe {
            self.video_context.VideoProcessorBlt(
                &self.video_processor,
                &self.nv12_out_view,
                0,
                &[stream],
            )?
        };

        // 3. Wrap the NV12 texture in a DXGI-surface-backed IMFSample.
        let buffer = unsafe {
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12_tex, 0, false)?
        };
        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buffer)?;
            let ts_100ns = (timestamp.as_nanos() as i64) / 100;
            sample.SetSampleTime(ts_100ns)?;
            sample.SetSampleDuration(10_000_000 / TARGET_FRAMERATE as i64)?;
        }

        // 4. ProcessInput into the HW MFT (the GPU-resident input the spike must prove).
        unsafe { self.mft.ProcessInput(0, &sample, 0)? };

        // 5. Best-effort drain. NEED_MORE_INPUT is expected/benign on an async MFT.
        let _ = unsafe { self.drain_output() };
        Ok(())
    }

    /// Best-effort single ProcessOutput drain. Ignores NEED_MORE_INPUT.
    ///
    /// # Safety
    /// Runs on the WGC capture thread that owns the MFT.
    unsafe fn drain_output(&self) -> windows::core::Result<()> {
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        match unsafe {
            self.mft
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Enumerate hardware H.264 encoder MFTs and activate the first one.
///
/// # Safety
/// MF must be initialized (MFStartup) on the calling thread.
unsafe fn select_hardware_h264_mft() -> windows::core::Result<IMFTransform> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates_ptr: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
        std::ptr::null_mut();
    let mut count: u32 = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output_info),
            &mut activates_ptr,
            &mut count,
        )?
    };
    if count == 0 || activates_ptr.is_null() {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_FAIL,
        ));
    }
    // Take the first activate, then free the CoTaskMem array.
    let activates = unsafe { std::slice::from_raw_parts(activates_ptr, count as usize) };
    let first = activates[0]
        .clone()
        .expect("MFTEnumEx returned a null activate in a non-empty array");
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(activates_ptr as *const _));
    }
    unsafe { first.ActivateObject::<IMFTransform>() }
}

/// Negotiate the MFT output type (H.264) at native resolution / production bitrate.
///
/// # Safety
/// `mft` is a freshly activated HW MFT on the calling thread.
unsafe fn setup_mft_output(mft: &IMFTransform, w: u32, h: u32) -> windows::core::Result<()> {
    let out_type = unsafe { MFCreateMediaType()? };
    unsafe {
        out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out_type.SetUINT32(&MF_MT_AVG_BITRATE, TARGET_BITRATE_BPS)?;
        out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))?;
        out_type.SetUINT64(&MF_MT_FRAME_RATE, ((TARGET_FRAMERATE as u64) << 32) | 1)?;
        out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mft.SetOutputType(0, &out_type, 0)?;
    }
    Ok(())
}

/// Negotiate the NV12 input type so the MFT accepts DXGI-surface NV12 samples.
///
/// # Safety
/// `mft` output type must already be set; runs on the calling thread.
unsafe fn setup_mft_input_nv12(mft: &IMFTransform, w: u32, h: u32) -> windows::core::Result<()> {
    let in_type = unsafe { MFCreateMediaType()? };
    unsafe {
        in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        in_type.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))?;
        in_type.SetUINT64(&MF_MT_FRAME_RATE, ((TARGET_FRAMERATE as u64) << 32) | 1)?;
        in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mft.SetInputType(0, &in_type, 0)?;
    }
    Ok(())
}

// ── WGC capture handler — drives the GPU pipeline + measures fps ───────────────

/// Flags passed into the WGC handler: the run deadline + a stop flag.
struct SpikeFlags {
    stop: Arc<AtomicBool>,
}

/// The spike's WGC handler. Lives on the WGC capture thread. Builds the GPU pipeline on
/// the first frame, then runs + measures every subsequent frame.
struct SpikeHandler {
    stop: Arc<AtomicBool>,
    pipeline: Option<GpuPipeline>,
    /// `true` once we have logged the one-time negotiation facts.
    logged_negotiation: bool,
    /// Whether the GPU path is viable. Once a hard failure is seen we stop touching the GPU
    /// and just let the session wind down (the failure itself is the NO-GO signal).
    gpu_failed: bool,

    // fps measurement (mirrors capture/windows.rs throughput logging).
    capture_frames: u32,
    encode_frames: u32,
    fps_window_start: Instant,
    sample_index: u64,
    encode_errors: Arc<AtomicU64>,
}

// SAFETY: `SpikeHandler` holds COM interfaces (IMFTransform, ID3D11Video*, etc.) that are
// `!Send`. `start_free_threaded` requires `Self: Send` as a STATIC bound, but it constructs
// the handler INSIDE the spawned WGC thread (via an Arc<Mutex<Self>> channel — see
// windows-capture capture.rs) and invokes `new()` + every `on_frame_arrived` on that one
// thread. The COM objects are created on, used on, and dropped on the WGC capture thread —
// they NEVER actually cross a thread boundary. Marking the handler `Send` satisfies the
// bound without violating COM apartment rules. This is the same single-thread-COM contract
// the production encoder thread relies on (windows_mft.rs ComSend). Spike-only.
unsafe impl Send for SpikeHandler {}

impl GraphicsCaptureApiHandler for SpikeHandler {
    type Flags = SpikeFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        // MF must be started on THIS (the WGC capture) thread so the MFT lives here.
        // SAFETY: paired with MFShutdown via the guard stored on the OS thread's stack —
        // here we leak the guard intentionally: the process exits right after the run, and
        // the WGC thread owns MF for its whole lifetime. (Spike-only shortcut.)
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self {
            stop: ctx.flags.stop,
            pipeline: None,
            logged_negotiation: false,
            gpu_failed: false,
            capture_frames: 0,
            encode_frames: 0,
            fps_window_start: Instant::now(),
            sample_index: 0,
            encode_errors: Arc::new(AtomicU64::new(0)),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.stop.load(Ordering::Relaxed) {
            capture_control.stop();
            return Ok(());
        }

        self.capture_frames += 1;

        let timestamp = frame
            .timestamp()
            .ok()
            .and_then(|ts| u64::try_from(ts.Duration).ok())
            .map(|d| Duration::from_nanos(d * 100))
            .unwrap_or(Duration::ZERO);

        if !self.gpu_failed {
            let desc = *frame.desc();
            let (w, h) = (desc.Width, desc.Height);

            // D2 PRIMARY path: windows-capture 2.0.0 exposes its own device/texture/context.
            let device = frame.device();
            let context = frame.device_context();
            let bgra_tex = frame.as_raw_texture();

            // Lazily build the GPU pipeline on the first frame (now that we know real dims).
            if self.pipeline.is_none() {
                match unsafe { GpuPipeline::build(device, context, w, h) } {
                    Ok((pipe, set_d3d_ok, dxgi_in_ok)) => {
                        let luid = adapter_luid_i64(device);
                        tracing::info!(
                            target: "qsv_gpu_spike",
                            device_path = "windows-capture (D2 PRIMARY: frame.device exposed)",
                            adapter_luid = ?luid,
                            width = w,
                            height = h,
                            set_d3d_manager_accepted = set_d3d_ok,
                            dxgi_nv12_input_negotiated = dxgi_in_ok,
                            "GPU pipeline initialized"
                        );
                        if !set_d3d_ok {
                            tracing::warn!(
                                target: "qsv_gpu_spike",
                                "SET_D3D_MANAGER was REJECTED by the MFT — GPU input may be a NO-GO"
                            );
                        }
                        if !dxgi_in_ok {
                            tracing::warn!(
                                target: "qsv_gpu_spike",
                                "DXGI NV12 input type negotiation FAILED — GPU input may be a NO-GO"
                            );
                        }
                        self.pipeline = Some(pipe);
                        self.logged_negotiation = true;
                    }
                    Err(e) => {
                        tracing::error!(
                            target: "qsv_gpu_spike",
                            hr = format!("0x{:08X}", e.code().0),
                            "GPU pipeline build FAILED — NO-GO signal. {e}"
                        );
                        self.gpu_failed = true;
                    }
                }
            }

            // Run the GPU chain for this frame.
            if let Some(pipe) = &self.pipeline {
                debug_assert_eq!((pipe.width, pipe.height), (w, h));
                match unsafe { pipe.process_frame(bgra_tex, timestamp) } {
                    Ok(()) => self.encode_frames += 1,
                    Err(e) => {
                        let n = self.encode_errors.fetch_add(1, Ordering::Relaxed) + 1;
                        // Log the first few then go quiet; a sustained failure is a NO-GO.
                        if n <= 5 {
                            tracing::error!(
                                target: "qsv_gpu_spike",
                                hr = format!("0x{:08X}", e.code().0),
                                "process_frame FAILED (GPU/MFT step) — NO-GO signal. {e}"
                            );
                        }
                        if n >= 30 {
                            tracing::error!(
                                target: "qsv_gpu_spike",
                                "30+ GPU frame failures — declaring GPU path NO-GO, stopping GPU work"
                            );
                            self.gpu_failed = true;
                        }
                    }
                }
            }
        }

        // ── Per-second throughput logging (mirrors capture/windows.rs style) ───
        let now = Instant::now();
        let elapsed = now.duration_since(self.fps_window_start);
        if elapsed >= Duration::from_secs(1) {
            let cap_fps = self.capture_frames as f64 / elapsed.as_secs_f64();
            let enc_fps = self.encode_frames as f64 / elapsed.as_secs_f64();
            self.sample_index += 1;
            // The first 10 samples are warmup — the GATE reads steady-state AFTER these.
            let warmup = self.sample_index <= 10;
            tracing::info!(
                target: "qsv_gpu_spike",
                sample = self.sample_index,
                warmup,
                capture_fps = %format!("{cap_fps:.1}"),
                encode_fps = %format!("{enc_fps:.1}"),
                encode_errors = self.encode_errors.load(Ordering::Relaxed),
                "throughput"
            );
            self.capture_frames = 0;
            self.encode_frames = 0;
            self.fps_window_start = now;
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing → stdout, controlled by RUST_LOG (default debug for the spike).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    print_run_guide();

    // The spike thread that spawns WGC needs an MTA apartment for the HW MFT factory.
    // SAFETY: paired with CoUninitialize via the guard; S_OK / S_FALSE both pass.
    let co_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let _co_guard = CoUninitGuard;
    if co_hr.is_err() {
        return Err(format!("CoInitializeEx failed: 0x{:08X}", co_hr.0).into());
    }

    let monitor = Monitor::primary()?;
    let stop = Arc::new(AtomicBool::new(false));

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        SpikeFlags { stop: Arc::clone(&stop) },
    );

    tracing::info!(target: "qsv_gpu_spike", run_seconds = RUN_SECONDS, "starting capture");
    let control = SpikeHandler::start_free_threaded(settings)
        .map_err(|e| format!("WGC start failed: {e}"))?;

    // Let it run, then signal stop.
    std::thread::sleep(Duration::from_secs(RUN_SECONDS));
    stop.store(true, Ordering::Relaxed);
    // Give the handler one more frame to observe the stop flag, then tear down.
    std::thread::sleep(Duration::from_millis(300));
    let _ = control.stop();

    tracing::info!(
        target: "qsv_gpu_spike",
        "spike complete — read the steady-state capture_fps (warmup=false samples) above"
    );
    print_verdict_guide();
    Ok(())
}

fn print_run_guide() {
    eprintln!("════════════════════════════════════════════════════════════════════");
    eprintln!(" QSV GPU-resident spike — GO/NO-GO measurement (qsv-igpu-pipeline-perf)");
    eprintln!("════════════════════════════════════════════════════════════════════");
    eprintln!(" 1. Run on the Intel-QSV machine, native 1440p primary monitor.");
    eprintln!(" 2. Play the SAME fullscreen 60fps content used for the QSV2 gate,");
    eprintln!("    full-screen on the primary monitor, BEFORE/while this runs.");
    eprintln!(" 3. Recommended invocation:");
    eprintln!("       RUST_LOG=debug cargo run -p sm-infra \\");
    eprintln!("         --example qsv_gpu_spike --features hw-encoder");
    eprintln!(" 4. Watch the per-second `throughput` lines (capture_fps / encode_fps).");
    eprintln!("────────────────────────────────────────────────────────────────────");
}

fn print_verdict_guide() {
    eprintln!("────────────────────────────────────────────────────────────────────");
    eprintln!(" HOW TO READ THE RESULT (GATE A):");
    eprintln!("   • Skip the first 10 samples (warmup=true).");
    eprintln!("   • GO  : steady-state capture_fps sustained >= ~55 fps at 1440p,");
    eprintln!("           jitter well below the QSV2 20.9->60.1 spread, AND");
    eprintln!("           set_d3d_manager_accepted=true + dxgi_nv12_input_negotiated=true,");
    eprintln!("           with encode_errors staying ~0.");
    eprintln!("   • NO-GO: capture_fps below ~55, or large jitter, or any of the");
    eprintln!("           negotiation flags false / encode_errors climbing.");
    eprintln!("   The device path is logged once at init as `device_path` — it tells us");
    eprintln!("   WHICH approach worked (windows-capture D2 PRIMARY vs own-device).");
    eprintln!("════════════════════════════════════════════════════════════════════");
}
