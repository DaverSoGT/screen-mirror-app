#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Windows hardware H.264 encoder backed by Media Foundation Transform (MFT).
//!
//! # Overview
//!
//! [`WindowsMftH264Encoder`] wraps an async `IMFTransform` behind the [`VideoEncoder`]
//! domain trait. It selects the first hardware H.264 MFT returned by
//! `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)`, returning
//! `EncoderError::InitFailed` if none is available (so the factory can fall back
//! to `WindowsOpenH264Encoder`).
//!
//! # Thread model
//!
//! - `new()`: validates config, performs synchronous MFT enumeration and activation
//!   (`CoInitializeEx` + `MFStartup` + `MFTEnumEx` + `ActivateObject`).
//! - `start(rx, tx)`: spawns one OS thread that owns the MFT pump loop.
//! - `stop()`: idempotent. Sets stop flag and joins the handle.
//! - `Drop`: calls `stop()` + `MFShutdown` + `CoUninitialize` on the caller thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFTransform, MEEndOfStream, METransformHaveOutput,
    METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NONE, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFShutdown,
    MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::Interface;

use sm_domain::encode::{EncodedPacket, EncoderConfig, EncoderError, VideoEncoder};

// ── COM interface thread-transfer wrapper ─────────────────────────────────────

/// Newtype that marks a COM interface as `Send` for the purpose of transferring
/// ownership from the constructor thread to the encoder thread.
///
/// # Safety
/// COM interfaces are apartment-threaded by default, but hardware MFTs are
/// registered as free-threaded (MTA). Both the constructor (`new`) and the
/// encoder thread call `CoInitializeEx(COINIT_MULTITHREADED)` to join the MTA,
/// so cross-thread use of these MTA-registered interfaces is safe per Windows
/// COM rules. Never use this wrapper for STA-registered objects.
struct ComSend<T>(T);

impl<T> ComSend<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

// SAFETY: See safety contract on ComSend above — only used with MTA-registered
// hardware MFT interfaces (IMFTransform, ICodecAPI).
unsafe impl<T> Send for ComSend<T> {}

// ── Shared cross-thread state ─────────────────────────────────────────────────

/// Shared atomics between the caller and the encoder OS thread.
struct MftEncoderShared {
    /// Set by `request_keyframe()`; cleared (swap → false) before the next `ProcessInput`.
    keyframe_pending: AtomicBool,
    /// Non-zero means a new target bitrate is pending. 0 = no change.
    pending_bitrate: AtomicU32,
    /// Monotonically increasing count of encoded packets dropped due to backpressure.
    dropped: AtomicU64,
    /// Set by `stop()` / `Drop`. Checked at the top of each pump-loop iteration.
    stop: AtomicBool,
}

impl Default for MftEncoderShared {
    fn default() -> Self {
        Self {
            keyframe_pending: AtomicBool::new(false),
            pending_bitrate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }
}

// ── H264 profile constant ─────────────────────────────────────────────────────

// DR4: eAVEncH264VProfile_Main = 77 (confirmed present in MediaFoundation 0.62.2).
// Using the newtype value directly to be explicit about the integer.
const H264_PROFILE_MAIN: u32 = eAVEncH264VProfile_Main.0 as u32;

// ── WindowsMftH264Encoder ─────────────────────────────────────────────────────

/// Windows hardware H.264 encoder via Media Foundation Transform.
///
/// See module documentation for construction contract and thread model.
pub struct WindowsMftH264Encoder {
    config: EncoderConfig,
    state: Arc<MftEncoderShared>,
    /// COM interfaces obtained in `new()`, consumed by `start()`.
    /// `None` after `start()` has transferred them to the encoder thread.
    mft: Option<IMFTransform>,
    codec_api: Option<ICodecAPI>,
    /// `Some` while the encoder thread is running; `None` before `start` and after `stop`.
    handle: Option<JoinHandle<()>>,
    /// Tracks whether `new()` performed `CoInitializeEx` + `MFStartup`.
    /// When `true`, `Drop` must call `MFShutdown` + `CoUninitialize` on this thread.
    com_initialized: bool,
}

impl std::fmt::Debug for WindowsMftH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsMftH264Encoder")
            .field("config", &self.config)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

// SAFETY: `WindowsMftH264Encoder` is used from a single owner thread.
// `mft` and `codec_api` (both `Option<COM interface>`) are transferred to the encoder
// thread inside `start()` and set to `None` on the caller side immediately after.
// Once `start()` has been called the struct never accesses these COM pointers again.
// All remaining shared state (`state: Arc<MftEncoderShared>`) consists entirely of
// atomics, which are `Send + Sync`. `JoinHandle<()>` and `EncoderConfig` are `Send`.
// The caller is responsible for not calling methods from multiple threads simultaneously
// (consistent with the rest of the encoder API). The static test `adapter_is_send_sync`
// asserts these impls at compile time.
unsafe impl Send for WindowsMftH264Encoder {}
unsafe impl Sync for WindowsMftH264Encoder {}

impl VideoEncoder for WindowsMftH264Encoder {
    /// Construct and validate an encoder configuration.
    ///
    /// Performs synchronous MFT enumeration on the caller's thread:
    /// `CoInitializeEx` → `MFStartup` → `MFTEnumEx` → `ActivateObject`.
    ///
    /// Returns:
    /// - `Err(InvalidConfig(_))` for `bitrate_bps == 0` or `framerate == 0`.
    /// - `Err(InitFailed(_))` if no hardware H.264 MFT is available.
    fn new(config: EncoderConfig) -> Result<Self, EncoderError>
    where
        Self: Sized,
    {
        // Validation gate — matches WindowsOpenH264Encoder.
        if config.bitrate_bps == 0 {
            return Err(EncoderError::InvalidConfig(
                "bitrate_bps must be > 0".into(),
            ));
        }
        if config.framerate == 0 {
            return Err(EncoderError::InvalidConfig("framerate must be > 0".into()));
        }

        // Synchronous MFT initialisation (design §5 steps 1–6).
        let (mft, codec_api) = init_mft_sync()?;

        Ok(Self {
            config,
            state: Arc::new(MftEncoderShared::default()),
            mft: Some(mft),
            codec_api: Some(codec_api),
            handle: None,
            com_initialized: true,
        })
    }

    fn start(
        &mut self,
        rx: Receiver<sm_domain::CaptureFrame>,
        tx: SyncSender<EncodedPacket>,
    ) -> Result<(), EncoderError> {
        let mft = self.mft.take().ok_or_else(|| {
            EncoderError::Internal("start() called after IMFTransform was already consumed".into())
        })?;
        let codec_api = self.codec_api.take().ok_or_else(|| {
            EncoderError::Internal("start() called after ICodecAPI was already consumed".into())
        })?;
        let config = self.config.clone();
        let state = Arc::clone(&self.state);

        state.stop.store(false, Ordering::Release);

        // Wrap COM interfaces in ComSend so the closure is Send.
        // SAFETY: Both this thread and the encoder thread initialise as MTA
        // (CoInitializeEx(COINIT_MULTITHREADED)), so MTA-registered hardware
        // MFT interfaces cross thread boundaries safely (see ComSend docs).
        let mft_send = ComSend(mft);
        let codec_api_send = ComSend(codec_api);

        let handle = std::thread::spawn(move || {
            // into_inner() unwraps ComSend<T> → T inside the thread.
            // The closure captures ComSend<IMFTransform> and ComSend<ICodecAPI> (both Send).
            run_encoder_thread(
                mft_send.into_inner(),
                codec_api_send.into_inner(),
                config,
                state,
                rx,
                tx,
            );
        });

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), EncoderError> {
        self.state.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        Ok(())
    }

    fn request_keyframe(&self) {
        self.state.keyframe_pending.store(true, Ordering::Release);
    }

    fn set_bitrate(&self, bps: u32) -> Result<(), EncoderError> {
        if bps == 0 {
            return Err(EncoderError::InvalidConfig(
                "bitrate_bps must be > 0".into(),
            ));
        }
        self.state.pending_bitrate.store(bps, Ordering::Release);
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for WindowsMftH264Encoder {
    fn drop(&mut self) {
        // Join encoder thread if still running.
        let _ = self.stop();

        if self.com_initialized {
            // SAFETY: MFShutdown paired with MFStartup in new(); refcount managed by
            // Microsoft runtime per A4 in proposal. CoUninitialize paired with CoInitializeEx.
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            self.com_initialized = false;
        }
    }
}

// ── Synchronous MFT initialisation (design §5 steps 1–6) ─────────────────────

fn init_mft_sync() -> Result<(IMFTransform, ICodecAPI), EncoderError> {
    // Step 1: CoInitializeEx on caller thread (MTA).
    // SAFETY: Paired with CoUninitialize in Drop.
    // CoInitializeEx returns HRESULT directly (not Result).
    // S_OK (0) and S_FALSE (1, already initialised on this apartment) are both acceptable.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        return Err(EncoderError::InitFailed(format!(
            "CoInitializeEx: 0x{:08X}",
            hr.0
        )));
    }

    // Step 2: MFStartup (process-global refcount per proposal A4).
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        // SAFETY: CoUninitialize paired with CoInitializeEx above.
        unsafe { CoUninitialize() };
        return Err(EncoderError::InitFailed(format!(
            "MFStartup: 0x{:08X}",
            e.code().0
        )));
    }

    // Steps 3–4: MFTEnumEx + ActivateObject.
    match enumerate_and_activate() {
        Ok(mft) => {
            // Step 5: Cast to ICodecAPI.
            // SAFETY: IMFTransform for hardware video encoders implements ICodecAPI per Windows docs.
            match mft.cast::<ICodecAPI>() {
                Ok(codec_api) => Ok((mft, codec_api)),
                Err(e) => {
                    unsafe {
                        let _ = MFShutdown();
                        CoUninitialize();
                    }
                    Err(EncoderError::InitFailed(format!(
                        "ICodecAPI cast failed: 0x{:08X}",
                        e.code().0
                    )))
                }
            }
        }
        Err(err) => {
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            Err(err)
        }
    }
}

/// Enumerate hardware H.264 MFTs and activate the first one (steps 3–4).
fn enumerate_and_activate() -> Result<IMFTransform, EncoderError> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut pactivates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    // SAFETY: MFTEnumEx writes a COM-allocated array to pactivates; must be freed via CoTaskMemFree.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            Some(&input_info),
            Some(&output_info),
            &mut pactivates as *mut _ as _,
            &mut count,
        )
        .map_err(|e| EncoderError::InitFailed(format!("MFTEnumEx: 0x{:08X}", e.code().0)))?;
    }

    if count == 0 || pactivates.is_null() {
        return Err(EncoderError::InitFailed(
            "no hardware MFT H264 encoder registered".into(),
        ));
    }

    // Activate the first hardware MFT.
    // SAFETY: pactivates[0] is a valid IMFActivate pointer when count > 0 (MFTEnumEx contract).
    let mft: IMFTransform = unsafe {
        let activate_opt = &*pactivates;
        let activate = activate_opt.as_ref().ok_or_else(|| {
            EncoderError::InitFailed("MFTEnumEx returned null IMFActivate[0]".into())
        })?;
        activate.ActivateObject().map_err(|e| {
            EncoderError::InitFailed(format!("ActivateObject: 0x{:08X}", e.code().0))
        })?
    };

    // SAFETY: pactivates was allocated by MFTEnumEx (CoTaskMemAlloc); free with CoTaskMemFree.
    unsafe { CoTaskMemFree(Some(pactivates as *const _)) };

    Ok(mft)
}

// ── Encoder thread ────────────────────────────────────────────────────────────

/// RAII guard that calls `CoUninitialize` on drop (paired with encoder thread's CoInitializeEx).
struct CoUninitGuard;

impl Drop for CoUninitGuard {
    fn drop(&mut self) {
        // SAFETY: Paired with CoInitializeEx in run_encoder_thread.
        unsafe { CoUninitialize() };
    }
}

fn run_encoder_thread(
    mft: IMFTransform,
    codec_api: ICodecAPI,
    config: EncoderConfig,
    state: Arc<MftEncoderShared>,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
) {
    // SAFETY: encoder thread must be MTA for IMFTransform cross-thread use per DD9.
    // S_FALSE (already MTA) is acceptable.
    let co_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let _co_guard = CoUninitGuard; // CoUninitialize on thread exit regardless of init result

    if co_hr.is_err() {
        tracing::error!("encoder thread CoInitializeEx failed: 0x{:08X}", co_hr.0);
        return;
    }

    // Steps 7b–7h: Setup media types, unlock async, get event generator, send messages.
    if let Err(e) = setup_mft(&mft, &config) {
        tracing::error!("MFT setup failed: {e}");
        // MFShutdown for the thread's MF context is not needed here because
        // the caller thread's new() owns MFStartup/MFShutdown (step 2 and Drop).
        return;
    }

    let event_gen: IMFMediaEventGenerator = match mft.cast() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("IMFMediaEventGenerator cast failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    // Step 7i: Probe output format (Annex-B vs AVCC).
    let output_is_avcc = probe_output_format(&mft, &event_gen);

    // Step 8: Pump loop.
    pump_loop(
        &mft,
        &codec_api,
        &event_gen,
        &state,
        rx,
        tx,
        output_is_avcc,
        &config,
    );

    // Steps 9a–9e: Notify end of stream and release.
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    // mft, codec_api, event_gen are dropped here (COM Release via Drop).
    // MFShutdown for the thread is NOT called here — the caller thread's Drop handles it.
    // CoUninitGuard calls CoUninitialize when this function returns.
}

/// Setup MFT media types and streaming messages (steps 7b–7h).
fn setup_mft(mft: &IMFTransform, config: &EncoderConfig) -> Result<(), EncoderError> {
    use windows::Win32::Media::MediaFoundation::IMFMediaType;

    // Frame dimensions are not in EncoderConfig — use hardcoded 1920×1080 as default.
    // The MFT will accept any input frame at SetInputType time; actual frame dimensions
    // are constrained only by the output type set here. For the V1 use case (1080p30),
    // this is correct. A future enhancement could pass dimensions via EncoderConfig.
    let w: u32 = 1920;
    let h: u32 = 1080;

    // Step 7b: SetOutputType FIRST (MFT requirement: output before input).
    let out_type: IMFMediaType = unsafe { MFCreateMediaType() }.map_err(|e| {
        EncoderError::InitFailed(format!("MFCreateMediaType(out): 0x{:08X}", e.code().0))
    })?;

    unsafe {
        out_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetGUID Major(out): 0x{:08X}", e.code().0))
            })?;
        out_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .map_err(|e| EncoderError::InitFailed(format!("SetGUID H264: 0x{:08X}", e.code().0)))?;
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT32 bitrate: 0x{:08X}", e.code().0))
            })?;
        // MFSetAttributeSize encodes (w,h) into a u64: high 32 bits = w, low 32 bits = h.
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameSize: 0x{:08X}", e.code().0))
            })?;
        // MFSetAttributeRatio encodes (num,den) into a u64: high 32 = num, low 32 = den.
        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((config.framerate as u64) << 32) | 1)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameRate: 0x{:08X}", e.code().0))
            })?;
        out_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 PAR: 0x{:08X}", e.code().0))
            })?;
        out_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT32 Interlace: 0x{:08X}", e.code().0))
            })?;
        out_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_MAIN)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT32 Profile: 0x{:08X}", e.code().0))
            })?;
        mft.SetOutputType(0, &out_type, 0).map_err(|e| {
            EncoderError::InitFailed(format!("SetOutputType: 0x{:08X}", e.code().0))
        })?;
    }

    // Step 7c: SetInputType (NV12).
    let in_type: IMFMediaType = unsafe { MFCreateMediaType() }.map_err(|e| {
        EncoderError::InitFailed(format!("MFCreateMediaType(in): 0x{:08X}", e.code().0))
    })?;

    unsafe {
        in_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetGUID Major(in): 0x{:08X}", e.code().0))
            })?;
        in_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| EncoderError::InitFailed(format!("SetGUID NV12: 0x{:08X}", e.code().0)))?;
        in_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameSize(in): 0x{:08X}", e.code().0))
            })?;
        in_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((config.framerate as u64) << 32) | 1)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameRate(in): 0x{:08X}", e.code().0))
            })?;
        in_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 PAR(in): 0x{:08X}", e.code().0))
            })?;
        in_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT32 Interlace(in): 0x{:08X}", e.code().0))
            })?;
        mft.SetInputType(0, &in_type, 0)
            .map_err(|e| EncoderError::InitFailed(format!("SetInputType: 0x{:08X}", e.code().0)))?;
    }

    // Step 7d: MF_TRANSFORM_ASYNC_UNLOCK (required for async hardware MFTs).
    let attrs = unsafe { mft.GetAttributes() }
        .map_err(|e| EncoderError::InitFailed(format!("GetAttributes: 0x{:08X}", e.code().0)))?;
    unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.map_err(|e| {
        EncoderError::InitFailed(format!("MF_TRANSFORM_ASYNC_UNLOCK: 0x{:08X}", e.code().0))
    })?;

    // Steps 7f–7h: Send streaming messages.
    unsafe {
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("COMMAND_FLUSH: 0x{:08X}", e.code().0))
            })?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("BEGIN_STREAMING: 0x{:08X}", e.code().0))
            })?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("START_OF_STREAM: 0x{:08X}", e.code().0))
            })?;
    }

    Ok(())
}

/// Probe output format (step 7i). Returns `true` if output is AVCC (requires rewrite).
fn probe_output_format(mft: &IMFTransform, event_gen: &IMFMediaEventGenerator) -> bool {
    // Submit a minimal synthetic 16×16 NV12 frame and inspect the first output bytes.
    // If bytes start with [0x00, 0x00, 0x00, 0x01] → Annex-B (no rewrite needed).
    // Otherwise → AVCC (length-prefix format; rewrite needed).
    let probe_w: u32 = 16;
    let probe_h: u32 = 16;
    let y_size = (probe_w * probe_h) as usize;
    let uv_size = (probe_w * probe_h / 2) as usize;
    let frame_bytes = vec![0u8; y_size + uv_size]; // black NV12 frame

    // Try to push one synthetic frame and get one output sample.
    let probe_result = try_probe(mft, event_gen, &frame_bytes);

    match probe_result {
        Some(bytes) if bytes.len() >= 4 => {
            // Check for Annex-B start code.
            let is_annex_b =
                bytes[0..4] == [0x00, 0x00, 0x00, 0x01] || bytes[0..3] == [0x00, 0x00, 0x01];
            !is_annex_b // true = AVCC
        }
        _ => false, // Assume Annex-B if probe fails or returns too few bytes
    }
}

/// Attempt to send a synthetic frame and collect the first output sample.
fn try_probe(
    mft: &IMFTransform,
    event_gen: &IMFMediaEventGenerator,
    frame_bytes: &[u8],
) -> Option<Vec<u8>> {
    use std::time::Duration;

    // Wait for NeedInput, submit a synthetic frame, wait for HaveOutput.
    for _ in 0..32 {
        let event = unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NONE) }.ok()?;
        let event_type = unsafe { event.GetType() }.ok()?;

        if event_type == METransformNeedInput.0 as u32 {
            // Build a minimal IMFSample with the probe frame.
            let sample = build_imfsample(frame_bytes, Duration::ZERO, 33_333_333).ok()?;
            unsafe { mft.ProcessInput(0, &sample, 0) }.ok()?;
        } else if event_type == METransformHaveOutput.0 as u32 {
            let mut output = MFT_OUTPUT_DATA_BUFFER::default();
            let mut status: u32 = 0;
            if let Ok(()) =
                unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) }
            {
                if let Some(sample) = output.pSample.take() {
                    return extract_bytes(&sample).ok();
                }
            }
            return None;
        }
    }
    None
}

/// Build an `IMFSample` wrapping raw NV12 bytes.
fn build_imfsample(
    data: &[u8],
    timestamp: std::time::Duration,
    duration_100ns: i64,
) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, EncoderError> {
    let total = data.len() as u32;
    let buffer = unsafe { MFCreateMemoryBuffer(total) }.map_err(|e| {
        EncoderError::EncodeFailed(format!("MFCreateMemoryBuffer: 0x{:08X}", e.code().0))
    })?;

    // SAFETY: Lock/Unlock are explicitly paired. data is valid for the duration of the copy.
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .map_err(|e| EncoderError::EncodeFailed(format!("Lock: 0x{:08X}", e.code().0)))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        // SAFETY: Unlock paired with Lock above per DR3.
        buffer
            .Unlock()
            .map_err(|e| EncoderError::EncodeFailed(format!("Unlock: 0x{:08X}", e.code().0)))?;
        buffer.SetCurrentLength(total).map_err(|e| {
            EncoderError::EncodeFailed(format!("SetCurrentLength: 0x{:08X}", e.code().0))
        })?;
    }

    let sample = unsafe { MFCreateSample() }
        .map_err(|e| EncoderError::EncodeFailed(format!("MFCreateSample: 0x{:08X}", e.code().0)))?;

    unsafe {
        sample
            .AddBuffer(&buffer)
            .map_err(|e| EncoderError::EncodeFailed(format!("AddBuffer: 0x{:08X}", e.code().0)))?;
        let ts_100ns = timestamp.as_nanos() as i64 / 100;
        sample.SetSampleTime(ts_100ns).map_err(|e| {
            EncoderError::EncodeFailed(format!("SetSampleTime: 0x{:08X}", e.code().0))
        })?;
        sample.SetSampleDuration(duration_100ns).map_err(|e| {
            EncoderError::EncodeFailed(format!("SetSampleDuration: 0x{:08X}", e.code().0))
        })?;
    }

    Ok(sample)
}

/// Extract raw bytes from an `IMFSample`.
fn extract_bytes(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
) -> Result<Vec<u8>, EncoderError> {
    use windows::Win32::Media::MediaFoundation::IMFMediaBuffer;

    let total = unsafe { sample.GetTotalLength() }
        .map_err(|e| EncoderError::EncodeFailed(format!("GetTotalLength: 0x{:08X}", e.code().0)))?;

    let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| {
        EncoderError::EncodeFailed(format!("ConvertToContiguousBuffer: 0x{:08X}", e.code().0))
    })?;

    let mut out = vec![0u8; total as usize];

    // SAFETY: Lock/Unlock explicitly paired per DR3.
    unsafe {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len: u32 = 0;
        buffer
            .Lock(&mut ptr, None, Some(&mut cur_len))
            .map_err(|e| EncoderError::EncodeFailed(format!("Lock(out): 0x{:08X}", e.code().0)))?;
        let len = cur_len as usize;
        out.truncate(len);
        std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
        // SAFETY: Unlock paired with Lock above per DR3.
        buffer.Unlock().map_err(|e| {
            EncoderError::EncodeFailed(format!("Unlock(out): 0x{:08X}", e.code().0))
        })?;
    }

    Ok(out)
}

// ── Pump loop (design §5a) ────────────────────────────────────────────────────

#[expect(
    clippy::too_many_arguments,
    reason = "pump_loop takes mft, codec_api, event_gen, config, state, rx, tx + avcc flag — design §5a one-function pump shape"
)]
fn pump_loop(
    mft: &IMFTransform,
    codec_api: &ICodecAPI,
    event_gen: &IMFMediaEventGenerator,
    state: &MftEncoderShared,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
    output_is_avcc: bool,
    config: &EncoderConfig,
) {
    use crate::encode::bgra_to_nv12::{Nv12, convert as nv12_convert};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let mut nv12_scratch = Nv12::new(1, 1);
    let mut seq: u64 = 0;
    // Track current frame timestamp for EncodedPacket (R5: timestamp passthrough).
    let mut current_ts = Duration::ZERO;
    // Frame duration in 100ns units for MFT sample.
    let frame_dur_100ns = if config.framerate > 0 {
        10_000_000i64 / config.framerate as i64
    } else {
        333_333 // 30fps fallback
    };

    loop {
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // Blocking wait for next MFT event.
        let event = match unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NONE) } {
            Ok(e) => e,
            Err(e) => {
                let code = e.code().0 as u32;
                if code == 0x8000_4004 {
                    // E_ABORT — graceful shutdown signal.
                    break;
                }
                tracing::error!("GetEvent failed: 0x{:08X}", code);
                break;
            }
        };

        let event_type = match unsafe { event.GetType() } {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("GetType failed: 0x{:08X}", e.code().0);
                break;
            }
        };

        if event_type == METransformNeedInput.0 as u32 {
            // Apply pending keyframe request BEFORE ProcessInput (design §7, DD10).
            if state.keyframe_pending.swap(false, Ordering::AcqRel) {
                // SAFETY: VARIANT constructed with VT_BOOL type per CODECAPI contract.
                let v = make_variant_bool(true);
                unsafe {
                    let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
                }
            }

            // Apply pending bitrate change (design §7, DD11).
            let new_bps = state.pending_bitrate.swap(0, Ordering::AcqRel);
            if new_bps != 0 {
                let v = make_variant_u32(new_bps);
                unsafe {
                    if let Err(e) = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) {
                        // Log warn but do NOT crash — driver rejection is non-fatal (DD11).
                        tracing::warn!(
                            "ICodecAPI::SetValue(bitrate) rejected: 0x{:08X}",
                            e.code().0
                        );
                    }
                }
            }

            // Fetch next frame (50ms timeout to remain responsive to stop flag).
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(frame) => {
                    current_ts = frame.timestamp;
                    nv12_convert(&frame, &mut nv12_scratch);
                    if let Err(e) =
                        submit_frame(mft, &nv12_scratch, frame.timestamp, frame_dur_100ns)
                    {
                        tracing::warn!("ProcessInput failed: {e}");
                        // Skip frame, continue — mirrors openh264 behaviour.
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // No frame available; MFT will re-emit NeedInput.
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Upstream closed — drain and exit.
                    unsafe {
                        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                    }
                    continue;
                }
            }
        } else if event_type == METransformHaveOutput.0 as u32 {
            match collect_output(mft, output_is_avcc, current_ts, &mut seq) {
                Ok(Some(pkt)) => match tx.try_send(pkt) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                },
                Ok(None) => {} // need-more-input or stream-change
                Err(e) => {
                    tracing::error!("collect_output: {e}");
                    break;
                }
            }
        } else if event_type == MEEndOfStream.0 as u32 {
            break;
        }
    }
}

/// Submit one NV12 frame as an `IMFSample` to `ProcessInput`.
fn submit_frame(
    mft: &IMFTransform,
    nv12: &crate::encode::bgra_to_nv12::Nv12,
    timestamp: std::time::Duration,
    duration_100ns: i64,
) -> Result<(), EncoderError> {
    let sample = build_imfsample(&nv12.buf, timestamp, duration_100ns)?;
    unsafe {
        mft.ProcessInput(0, &sample, 0)
            .map_err(|e| EncoderError::EncodeFailed(format!("ProcessInput: 0x{:08X}", e.code().0)))
    }
}

/// Collect one output sample from `ProcessOutput` and build an `EncodedPacket`.
fn collect_output(
    mft: &IMFTransform,
    output_is_avcc: bool,
    frame_timestamp: std::time::Duration,
    seq: &mut u64,
) -> Result<Option<EncodedPacket>, EncoderError> {
    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
    let mut status: u32 = 0;

    match unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) } {
        Ok(()) => {}
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),
        Err(e) => {
            return Err(EncoderError::EncodeFailed(format!(
                "ProcessOutput: 0x{:08X}",
                e.code().0
            )));
        }
    }

    let sample = match output.pSample.take() {
        Some(s) => s,
        None => return Ok(None),
    };

    let raw_bytes = extract_bytes(&sample)?;

    let annex_b = if output_is_avcc {
        avcc_to_annex_b(&raw_bytes)
    } else {
        raw_bytes
    };

    // DD10: read is_keyframe from MFSampleExtension_CleanPoint attribute.
    // SAFETY: GetUINT32 on a valid IMFSample is always safe.
    let is_keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) } != 0;

    let pkt = EncodedPacket {
        data: Arc::from(annex_b.into_boxed_slice()),
        is_keyframe,
        timestamp: frame_timestamp,
        sequence: *seq,
    };
    *seq += 1;

    Ok(Some(pkt))
}

/// Rewrite AVCC (length-prefixed) NAL units to Annex-B (start-code-prefixed) format (§5b).
///
/// Cost: one copy per frame, O(packet_size) — negligible vs. the encode step.
fn avcc_to_annex_b(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + 16);
    let mut i = 0;
    while i + 4 <= input.len() {
        let nal_len = u32::from_be_bytes(input[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        if i + nal_len <= input.len() {
            out.extend_from_slice(&input[i..i + nal_len]);
        }
        i += nal_len;
    }
    out
}

// ── VARIANT helpers ───────────────────────────────────────────────────────────

/// Construct a `VARIANT` with type `VT_UI4` (u32 value).
/// Used for `CODECAPI_AVEncCommonMeanBitRate`.
///
/// # Safety contract
/// The VARIANT layout is: vt=VT_UI4 (19), anonymous union ulVal = value.
/// This matches the COM spec for VARIANT-encoded unsigned 32-bit integers.
fn make_variant_u32(value: u32) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: VARIANT is a union backed by ManuallyDrop fields.
    // We set vt to VT_UI4 and write the ulVal union arm directly.
    // The explicit dereference (`*v.Anonymous.Anonymous`) is required because
    // Rust does not auto-deref ManuallyDrop union fields for writes.
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_UI4;
        (*v.Anonymous.Anonymous).Anonymous.ulVal = value;
    }
    v
}

/// Construct a `VARIANT` with type `VT_BOOL` (bool value).
/// Used for `CODECAPI_AVEncVideoForceKeyFrame`.
///
/// # Safety contract
/// The VARIANT layout is: vt=VT_BOOL (11), boolVal = VARIANT_TRUE (-1) or VARIANT_FALSE (0).
fn make_variant_bool(value: bool) -> VARIANT {
    let mut v = VARIANT::default();
    // SAFETY: VARIANT is a union backed by ManuallyDrop fields.
    // We set vt to VT_BOOL and write the boolVal union arm (VARIANT_BOOL newtype).
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_BOOL;
        (*v.Anonymous.Anonymous).Anonymous.boolVal =
            if value { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    v
}

// ── Unit tests ────────────────────────────────────────────────────────────────

// ── Test-only helpers ────────────────────────────────────────────────────────

#[cfg(test)]
impl WindowsMftH264Encoder {
    /// Construct a `WindowsMftH264Encoder` with only the shared state initialised,
    /// bypassing COM init, MFStartup, and MFTEnumEx.
    ///
    /// # Purpose
    /// Enables testing methods that operate purely on the shared atomics
    /// (e.g. `set_bitrate`, `request_keyframe`) without requiring a GPU or COM
    /// apartment. The resulting encoder MUST NOT be started — it has no MFT handle.
    ///
    /// # Safety
    /// `com_initialized = false` prevents Drop from calling MFShutdown/CoUninitialize,
    /// which would be incorrect since COM was never initialised by this constructor.
    /// `mft` and `codec_api` are `None`, so any call to `start()` will return
    /// `Err(Internal(_))` rather than accessing an invalid COM pointer.
    fn new_for_validation_test() -> Self {
        Self {
            config: EncoderConfig::default(),
            state: Arc::new(MftEncoderShared::default()),
            mft: None,
            codec_api: None,
            handle: None,
            com_initialized: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_domain::encode::{EncoderConfig, EncoderError, VideoEncoder};

    // ─── T3a.1: new_rejects_zero_bitrate ──────────────────────────────────────
    // No MFT call — validation fires before any COM call.
    #[test]
    fn new_rejects_zero_bitrate() {
        let cfg = EncoderConfig {
            bitrate_bps: 0,
            ..EncoderConfig::default()
        };
        let err = WindowsMftH264Encoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig for bitrate_bps=0, got {err:?}"
        );
    }

    // ─── T3a.2: new_rejects_zero_framerate ────────────────────────────────────
    #[test]
    fn new_rejects_zero_framerate() {
        let cfg = EncoderConfig {
            framerate: 0,
            ..EncoderConfig::default()
        };
        let err = WindowsMftH264Encoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig for framerate=0, got {err:?}"
        );
    }

    // ─── T3a.3: adapter_is_send_sync ──────────────────────────────────────────
    // Zero-cost static assertion: compile = pass.
    #[test]
    fn adapter_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsMftH264Encoder>();
    }

    // ─── T8.1: set_bitrate_zero_returns_invalid_config ───────────────────────
    // CI-runnable: uses new_for_validation_test() which bypasses COM/MFT init.
    // set_bitrate() only accesses self.state (an AtomicU32) — no COM calls needed.
    #[test]
    fn set_bitrate_zero_returns_invalid_config() {
        let enc = WindowsMftH264Encoder::new_for_validation_test();
        let err = enc.set_bitrate(0).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig for set_bitrate(0), got {err:?}"
        );
    }
}
