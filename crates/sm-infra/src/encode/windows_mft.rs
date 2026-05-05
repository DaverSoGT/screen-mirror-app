#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Windows hardware H.264 encoder backed by Media Foundation Transform (MFT).
//!
//! # Overview
//!
//! [`WindowsMftH264Encoder`] wraps an async `IMFTransform` behind the [`VideoEncoder`]
//! domain trait. It probes all hardware H.264 MFTs returned by
//! `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)` and selects the
//! first candidate whose output-type negotiation succeeds (strategy E: clone
//! `GetOutputAvailableType(0,0)` and overlay `FRAME_SIZE + FRAME_RATE + AVG_BITRATE`).
//! This handles systems where vendor MFTs (e.g. AMD) are registered but have no
//! backing hardware. Returns `EncoderError::InitFailed` if no candidate succeeds.
//!
//! # Thread model
//!
//! - `new()`: validates config, performs synchronous MFT enumeration and candidate
//!   probing (`CoInitializeEx` + `MFStartup` + `MFTEnumEx` + per-candidate probe).
//! - `start(rx, tx)`: spawns one OS thread that owns the MFT pump loop.
//! - `stop()`: idempotent. Sets stop flag and joins the handle.
//! - `Drop`: calls `stop()` + `MFShutdown` + `CoUninitialize` on the caller thread.

// ── Phase 0 v3: AV trace macro ────────────────────────────────────────────────
// Flushed per-line so a 0xC0000005 AV does not swallow the trail.
#[cfg(debug_assertions)]
macro_rules! av_trace {
    ($($arg:tt)*) => {{
        use std::io::Write;
        eprintln!("[av] {}", format!($($arg)*));
        let _ = std::io::stderr().flush();
    }};
}
#[cfg(not(debug_assertions))]
macro_rules! av_trace {
    ($($arg:tt)*) => {};
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFTransform, MEEndOfStream, METransformDrainComplete,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_SHUTDOWN,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{Interface, PWSTR};

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

// ── Pump-loop timing constants ────────────────────────────────────────────────

/// Sleep duration per idle iteration in the NO_WAIT polling loop.
/// Bounds stop latency to ≤ 2 ms worst case (spec R5/S5.1, design DD6).
const POLLING_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);

/// Maximum wait for the next frame in the NeedInput service path.
///
/// WHY: ≤50ms is load-bearing for T-NEW-2 (`mft_stop_during_active_encode_returns_within_deadline`).
/// When stop() is called mid-encode the pump_loop may be inside this wait; it exits within
/// FRAME_RECV_TIMEOUT (≤50ms) and reaches the top-of-loop stop check. Option A (Phase 1 user
/// decision). See spec OQ-5 + design DD7.
const FRAME_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

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
    /// Performs synchronous MFT enumeration and candidate probing on the caller's thread:
    /// `CoInitializeEx` → `MFStartup` → `MFTEnumEx` → per-candidate probe
    /// (`ActivateObject` + `MF_TRANSFORM_ASYNC_UNLOCK` + `GetOutputAvailableType(0,0)`
    /// + `SetOutputType` with Strategy E).
    ///
    /// Returns:
    /// - `Err(InvalidConfig(_))` for `bitrate_bps == 0` or `framerate == 0`.
    /// - `Err(InitFailed(_))` if no hardware H.264 MFT candidate passes the probe.
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
        av_trace!("start: ENTER");
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

        av_trace!("start: before thread spawn");
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
        av_trace!("start: after thread spawn — handle stored");

        self.handle = Some(handle);
        av_trace!("start: EXIT Ok");
        Ok(())
    }

    fn stop(&mut self) -> Result<(), EncoderError> {
        av_trace!(
            "stop: ENTER handle_is_some={} com_initialized={}",
            self.handle.is_some(),
            self.com_initialized
        );
        av_trace!("stop: before stop_atomic.store(true)");
        self.state.stop.store(true, Ordering::Release);
        av_trace!("stop: after stop_atomic.store(true)");
        if let Some(h) = self.handle.take() {
            av_trace!("stop: before thread join");
            let join_result = h.join();
            av_trace!(
                "stop: after thread join result={}",
                if join_result.is_ok() { "Ok" } else { "Err(panic)" }
            );
        } else {
            av_trace!("stop: no handle — thread was never started or already stopped");
        }
        av_trace!("stop: EXIT Ok");
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
        av_trace!(
            "Drop: ENTER handle_is_some={} com_initialized={} mft_is_some={} codec_api_is_some={}",
            self.handle.is_some(),
            self.com_initialized,
            self.mft.is_some(),
            self.codec_api.is_some()
        );
        // Join encoder thread first. If start() was never called this is a no-op.
        av_trace!("Drop: before stop()");
        let _ = self.stop();
        av_trace!("Drop: after stop()");

        // SAFETY: COM Release() on IMFTransform / ICodecAPI MUST execute while the
        // Media Foundation runtime is still alive. Rust's automatic field-drop runs
        // AFTER this Drop body returns, so we explicitly drop the Option<COMInterface>
        // fields here. If we let MFShutdown() run first, the subsequent automatic
        // Release() would land on memory torn down by MF → access violation 0xc0000005
        // (see explore #583 Bucket B diagnosis).
        av_trace!(
            "Drop: before codec_api.take() (is_some={})",
            self.codec_api.is_some()
        );
        drop(self.codec_api.take());
        av_trace!("Drop: after codec_api.take() (now None)");
        // SAFETY: same ordering contract as above for the IMFTransform handle.
        // Order between codec_api and mft does not matter — both are sibling COM
        // pointers; only their position relative to MFShutdown matters.
        av_trace!("Drop: before mft.take() (is_some={})", self.mft.is_some());
        drop(self.mft.take());
        av_trace!("Drop: after mft.take() (now None)");

        if self.com_initialized {
            av_trace!("Drop: com_initialized=true — before MFShutdown");
            unsafe {
                // SAFETY: MFShutdown is the documented teardown for MFStartup.
                // Process-global refcount per A4; safe to call here AFTER all MF
                // interface refs above have been Release()d.
                let _ = MFShutdown();
                av_trace!("Drop: after MFShutdown — before CoUninitialize");
                // SAFETY: CoUninitialize matches the CoInitializeEx in init_mft_sync()
                // on this same thread. Safe AFTER all COM refs above are released.
                CoUninitialize();
            }
            av_trace!("Drop: after CoUninitialize");
            self.com_initialized = false;
        } else {
            av_trace!("Drop: com_initialized=false — skipping MFShutdown/CoUninitialize");
        }
        av_trace!("Drop: EXIT");
    }
}

// ── Synchronous MFT initialisation (design §5 steps 1–6) ─────────────────────

fn init_mft_sync() -> Result<(IMFTransform, ICodecAPI), EncoderError> {
    av_trace!("init_mft_sync: ENTER");
    // Step 1: CoInitializeEx on caller thread (MTA).
    // SAFETY: Paired with CoUninitialize in Drop.
    // CoInitializeEx returns HRESULT directly (not Result).
    // S_OK (0) and S_FALSE (1, already initialised on this apartment) are both acceptable.
    av_trace!("init_mft_sync: before CoInitializeEx");
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    av_trace!("init_mft_sync: CoInitializeEx hr=0x{:08X}", hr.0);
    if hr.is_err() {
        av_trace!("init_mft_sync: CoInitializeEx FAILED — returning Err");
        return Err(EncoderError::InitFailed(format!(
            "CoInitializeEx: 0x{:08X}",
            hr.0
        )));
    }

    // Step 2: MFStartup (process-global refcount per proposal A4).
    av_trace!("init_mft_sync: before MFStartup");
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        av_trace!("init_mft_sync: MFStartup FAILED 0x{:08X}", e.code().0);
        // SAFETY: CoUninitialize paired with CoInitializeEx above.
        unsafe { CoUninitialize() };
        return Err(EncoderError::InitFailed(format!(
            "MFStartup: 0x{:08X}",
            e.code().0
        )));
    }
    av_trace!("init_mft_sync: MFStartup OK");

    // Steps 3–5: Enumerate IMFActivate candidates and probe each with full output-type
    // negotiation. MFTEnumEx(MFT_ENUM_FLAG_HARDWARE) returns vendor MFTs even when
    // their hardware is absent (e.g. AMD MFT on a non-AMD system). We must probe the
    // full setup_mft output-type path to select the winner, not merely ActivateObject.
    // Phase 0 v2 evidence: Host B has 3 candidates — pactivates[0] and [1] are
    // AMDh264Encoder (no AMD GPU), [2] is NVIDIA H.264 Encoder MFT. Only [2] accepts
    // strategy E (FRAME_SIZE + FRAME_RATE + AVG_BITRATE on cloned slot[0] type).
    av_trace!("init_mft_sync: before enumerate_activates");
    let activates_result = enumerate_activates();
    match &activates_result {
        Ok(v) => av_trace!("init_mft_sync: enumerate_activates OK count={}", v.len()),
        Err(e) => av_trace!("init_mft_sync: enumerate_activates ERR {e}"),
    }

    av_trace!("init_mft_sync: before probe_and_select_mft");
    let result = match activates_result {
        Ok(activates) => probe_and_select_mft(activates),
        Err(e) => Err(e),
    };

    match result {
        Ok((mft, codec_api)) => {
            av_trace!("init_mft_sync: probe_and_select_mft OK — winner obtained");
            av_trace!("init_mft_sync: before query_interface ICodecAPI (already done in probe)");
            av_trace!("init_mft_sync: EXIT Ok");
            Ok((mft, codec_api))
        }
        Err(err) => {
            av_trace!("init_mft_sync: probe_and_select_mft ERR {err}");
            av_trace!("init_mft_sync: before MFShutdown (cleanup)");
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            av_trace!("init_mft_sync: after MFShutdown+CoUninitialize (cleanup) — EXIT Err");
            Err(err)
        }
    }
}

/// Enumerate hardware H.264 MFT candidates from MFTEnumEx (steps 3–4).
///
/// Returns all candidates as a `Vec<IMFActivate>` without activating them.
/// The raw COM array is freed after cloning all references. If the array is
/// empty or null, returns `EncoderError::InitFailed`.
fn enumerate_activates() -> Result<Vec<IMFActivate>, EncoderError> {
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

    // SAFETY: MFTEnumEx writes a COM-allocated array to pactivates; must be freed via
    // CoTaskMemFree once we have cloned all IMFActivate refs into a Vec.
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

    // Clone each IMFActivate from the raw array into a Vec (AddRef via clone).
    // SAFETY: pactivates[0..count] are valid Option<IMFActivate> per MFTEnumEx contract.
    let activates: Vec<IMFActivate> = unsafe {
        let slice = std::slice::from_raw_parts(pactivates, count as usize);
        slice.iter().filter_map(|opt| opt.clone()).collect()
    };

    // SAFETY: pactivates was CoTaskMemAlloc'd by MFTEnumEx; free after cloning all refs.
    unsafe { CoTaskMemFree(Some(pactivates as *const _)) };

    if activates.is_empty() {
        return Err(EncoderError::InitFailed(
            "MFTEnumEx returned only null IMFActivate entries".into(),
        ));
    }

    Ok(activates)
}

/// Iterate IMFActivate candidates and return the first one that passes the full
/// output-type negotiation probe (DD-A, DD-D, DD-E).
///
/// For each candidate:
///   1. `ActivateObject::<IMFTransform>` — if Err, log + `ShutdownObject` + skip.
///   2. `MF_TRANSFORM_ASYNC_UNLOCK = 1` — required before any other MFT call.
///      Set here (not in `setup_mft`) so the probe uses the same activation state.
///   3. `try_setup_output_type` — Strategy E probe (DD-B). If Err, log + `ShutdownObject` + skip.
///   4. If Ok: cast to `ICodecAPI`; if Err, `ShutdownObject` + skip.
///   5. Winner found — return `(IMFTransform, ICodecAPI)`.
///
/// If all candidates fail, returns `EncoderError::InitFailed` with the last error.
fn probe_and_select_mft(
    activates: Vec<IMFActivate>,
) -> Result<(IMFTransform, ICodecAPI), EncoderError> {
    // Placeholder dimensions for the probe. These must be valid (non-zero) so
    // try_setup_output_type calls SetOutputType with a real resolution.
    // We use 1920×1080 (the sentinel-zero fallback) and a standard config.
    const PROBE_W: u32 = 1920;
    const PROBE_H: u32 = 1080;
    const PROBE_FPS: u32 = 30;
    const PROBE_BPS: u32 = 4_000_000;

    let count = activates.len();
    av_trace!("probe_and_select_mft: ENTER count={}", count);
    let mut last_err = EncoderError::InitFailed("no hardware MFT candidates enumerated".into());

    for (i, activate) in activates.iter().enumerate() {
        // ── Log candidate identity for diagnostics ────────────────────────────

        // Friendly name via GetAllocatedString (IMFActivate inherits IMFAttributes).
        // SAFETY: GetAllocatedString allocates with CoTaskMemAlloc; freed with CoTaskMemFree.
        let friendly_name: String = unsafe {
            let mut pwstr = PWSTR::null();
            let mut cch: u32 = 0;
            match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pwstr, &mut cch) {
                Ok(()) if !pwstr.is_null() => {
                    let name = pwstr.to_string().unwrap_or_else(|_| "(utf16 error)".into());
                    CoTaskMemFree(Some(pwstr.0 as *const _));
                    name
                }
                _ => "(unknown)".into(),
            }
        };

        // CLSID via GetGUID.
        // SAFETY: GetGUID reads from the internal attribute store; no allocation.
        let clsid_str: String = unsafe {
            match activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) {
                Ok(guid) => format!(
                    "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
                    guid.data1,
                    guid.data2,
                    guid.data3,
                    guid.data4[0],
                    guid.data4[1],
                    guid.data4[2],
                    guid.data4[3],
                    guid.data4[4],
                    guid.data4[5],
                    guid.data4[6],
                    guid.data4[7],
                ),
                Err(_) => "(no CLSID)".into(),
            }
        };

        av_trace!("probe[{i}/{count}]: \"{friendly_name}\" {clsid_str}");
        tracing::info!(
            "probe_and_select_mft: candidate [{i}/{count}] \"{friendly_name}\" {clsid_str}"
        );

        // ── Step 1: ActivateObject ────────────────────────────────────────────
        // SAFETY: ActivateObject is the documented way to instantiate an IMFTransform
        // from an IMFActivate pointer obtained via MFTEnumEx.
        av_trace!("probe[{i}]: before ActivateObject");
        let mft: IMFTransform = match unsafe { activate.ActivateObject() } {
            Ok(m) => {
                av_trace!("probe[{i}]: ActivateObject OK");
                m
            }
            Err(e) => {
                av_trace!("probe[{i}]: ActivateObject FAILED 0x{:08X}", e.code().0);
                tracing::warn!(
                    "probe_and_select_mft: candidate [{i}] ActivateObject failed \
                     (0x{:08X}); trying next",
                    e.code().0
                );
                // ShutdownObject releases any partially-initialised GPU resources (DD-D).
                // SAFETY: ShutdownObject on an activate that failed ActivateObject is a no-op
                // per Windows docs (it can only free what was allocated).
                av_trace!("probe[{i}]: before ShutdownObject (ActivateObject failed)");
                let _ = unsafe { activate.ShutdownObject() };
                av_trace!("probe[{i}]: after ShutdownObject (ActivateObject failed)");
                last_err = EncoderError::InitFailed(format!(
                    "ActivateObject[{i}] ({friendly_name}): 0x{:08X}",
                    e.code().0
                ));
                continue;
            }
        };

        // ── Step 2: MF_TRANSFORM_ASYNC_UNLOCK ────────────────────────────────
        // Must be set before any other call on an async (hardware) MFT.
        // Set here so the probe operates on the same activation state as production.
        // setup_mft MUST NOT re-set this (it is already set by the time setup_mft runs).
        av_trace!("probe[{i}]: before GetAttributes");
        let attrs = match unsafe { mft.GetAttributes() } {
            Ok(a) => {
                av_trace!("probe[{i}]: GetAttributes OK");
                a
            }
            Err(e) => {
                av_trace!("probe[{i}]: GetAttributes FAILED 0x{:08X}", e.code().0);
                tracing::warn!(
                    "probe_and_select_mft: candidate [{i}] GetAttributes failed \
                     (0x{:08X}); trying next",
                    e.code().0
                );
                av_trace!("probe[{i}]: before ShutdownObject (GetAttributes failed)");
                let _ = unsafe { activate.ShutdownObject() };
                av_trace!("probe[{i}]: after ShutdownObject (GetAttributes failed)");
                last_err = EncoderError::InitFailed(format!(
                    "GetAttributes[{i}] ({friendly_name}): 0x{:08X}",
                    e.code().0
                ));
                continue;
            }
        };
        av_trace!("probe[{i}]: before MF_TRANSFORM_ASYNC_UNLOCK SetUINT32");
        if let Err(e) = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) } {
            av_trace!("probe[{i}]: ASYNC_UNLOCK FAILED 0x{:08X}", e.code().0);
            tracing::warn!(
                "probe_and_select_mft: candidate [{i}] MF_TRANSFORM_ASYNC_UNLOCK failed \
                 (0x{:08X}); trying next",
                e.code().0
            );
            av_trace!("probe[{i}]: before ShutdownObject (ASYNC_UNLOCK failed)");
            let _ = unsafe { activate.ShutdownObject() };
            av_trace!("probe[{i}]: after ShutdownObject (ASYNC_UNLOCK failed)");
            last_err = EncoderError::InitFailed(format!(
                "MF_TRANSFORM_ASYNC_UNLOCK[{i}] ({friendly_name}): 0x{:08X}",
                e.code().0
            ));
            continue;
        }
        av_trace!("probe[{i}]: ASYNC_UNLOCK OK");

        // ── Step 3: Output-type negotiation probe (DD-B via DD-E helper) ─────
        // Calls GetOutputAvailableType(0, 0) + overlay FRAME_SIZE + FRAME_RATE +
        // AVG_BITRATE + SetOutputType. NVENC's slot[0] pre-sets INTERLACE_MODE=2
        // (Progressive) and MPEG2_PROFILE=77 (Main); do NOT overlay them.
        av_trace!("probe[{i}]: before try_setup_output_type (probe pass)");
        if let Err(e) = try_setup_output_type(&mft, PROBE_W, PROBE_H, PROBE_FPS, PROBE_BPS) {
            av_trace!("probe[{i}]: try_setup_output_type FAILED: {e}");
            tracing::warn!(
                "probe_and_select_mft: candidate [{i}] \"{friendly_name}\" {clsid_str} \
                 rejected output-type negotiation ({e}); trying next"
            );
            av_trace!("probe[{i}]: before ShutdownObject (output-type failed)");
            let _ = unsafe { activate.ShutdownObject() };
            av_trace!("probe[{i}]: after ShutdownObject (output-type failed)");
            last_err = e;
            continue;
        }
        av_trace!("probe[{i}]: try_setup_output_type OK");

        // ── Step 4: Cast to ICodecAPI ─────────────────────────────────────────
        // SAFETY: IMFTransform for hardware video encoders implements ICodecAPI per Windows docs.
        av_trace!("probe[{i}]: before ICodecAPI cast");
        let codec_api = match mft.cast::<ICodecAPI>() {
            Ok(c) => {
                av_trace!("probe[{i}]: ICodecAPI cast OK");
                c
            }
            Err(e) => {
                av_trace!("probe[{i}]: ICodecAPI cast FAILED 0x{:08X}", e.code().0);
                tracing::warn!(
                    "probe_and_select_mft: candidate [{i}] \"{friendly_name}\" ICodecAPI cast \
                     failed (0x{:08X}); trying next",
                    e.code().0
                );
                av_trace!("probe[{i}]: before ShutdownObject (ICodecAPI cast failed)");
                let _ = unsafe { activate.ShutdownObject() };
                av_trace!("probe[{i}]: after ShutdownObject (ICodecAPI cast failed)");
                last_err = EncoderError::InitFailed(format!(
                    "ICodecAPI cast[{i}] ({friendly_name}): 0x{:08X}",
                    e.code().0
                ));
                continue;
            }
        };

        // ── Winner ────────────────────────────────────────────────────────────
        av_trace!(
            "probe[{i}]: WINNER \"{friendly_name}\" {clsid_str} — handing IMFTransform out"
        );
        tracing::info!(
            "probe_and_select_mft: selected candidate [{i}] \"{friendly_name}\" {clsid_str}"
        );
        av_trace!("probe_and_select_mft: EXIT Ok winner_index={i}");
        return Ok((mft, codec_api));
    }

    av_trace!("probe_and_select_mft: no winner — EXIT Err");
    Err(last_err)
}

/// Output-type negotiation helper (DD-B / DD-E — single source of truth).
///
/// Phase 0 v2 evidence (Host B): NVENC's `GetOutputAvailableType(0, 0)` returns a base
/// type with `INTERLACE_MODE = 2` (Progressive) and `MPEG2_PROFILE = 77` (Main) already
/// set. Overlaying those attributes invalidates the negotiation envelope (AMD evidence:
/// Strategy A–D all rejected on candidates without hardware). PAR is absent from the
/// slot and must stay absent (NVENC infers 1:1). The only three caller-controlled
/// attributes are `MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE`, and `MF_MT_AVG_BITRATE`.
///
/// Called from:
/// - `probe_and_select_mft` (DD-A probe step) — determines which candidate wins.
/// - `setup_mft` (production path) — configures the selected MFT for the real session.
///
/// No retries. No `DeleteItem`. Strategy E is the contract (Phase 0 v2).
fn try_setup_output_type(
    mft: &IMFTransform,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<(), EncoderError> {
    av_trace!(
        "try_setup_output_type: ENTER w={w} h={h} fps={framerate} bps={bitrate_bps}"
    );
    // Step 7b: Clone NVENC's advertised output type at slot 0 and overlay the three
    // caller-controlled attributes (FRAME_SIZE, FRAME_RATE, AVG_BITRATE). Per Phase 0
    // v2 evidence: NVENC's slot[0] pre-sets INTERLACE_MODE = 2 (Progressive) and
    // MPEG2_PROFILE = 77 (Main); overlaying those would invalidate the negotiation
    // envelope. PAR is absent and stays absent (NVENC infers 1:1).
    av_trace!("try_setup_output_type: before GetOutputAvailableType(0,0)");
    let out_type: IMFMediaType = unsafe { mft.GetOutputAvailableType(0, 0) }.map_err(|e| {
        av_trace!(
            "try_setup_output_type: GetOutputAvailableType FAILED 0x{:08X}",
            e.code().0
        );
        EncoderError::InitFailed(format!(
            "GetOutputAvailableType(0,0): 0x{:08X}",
            e.code().0
        ))
    })?;
    av_trace!("try_setup_output_type: GetOutputAvailableType OK");

    unsafe {
        av_trace!("try_setup_output_type: before SetUINT64 FrameSize {w}x{h}");
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| {
                av_trace!(
                    "try_setup_output_type: SetUINT64 FrameSize FAILED 0x{:08X}",
                    e.code().0
                );
                EncoderError::InitFailed(format!("SetUINT64 FrameSize: 0x{:08X}", e.code().0))
            })?;
        av_trace!("try_setup_output_type: SetUINT64 FrameSize OK");

        av_trace!("try_setup_output_type: before SetUINT64 FrameRate {framerate}/1");
        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1)
            .map_err(|e| {
                av_trace!(
                    "try_setup_output_type: SetUINT64 FrameRate FAILED 0x{:08X}",
                    e.code().0
                );
                EncoderError::InitFailed(format!("SetUINT64 FrameRate: 0x{:08X}", e.code().0))
            })?;
        av_trace!("try_setup_output_type: SetUINT64 FrameRate OK");

        av_trace!("try_setup_output_type: before SetUINT32 Bitrate {bitrate_bps}");
        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
            .map_err(|e| {
                av_trace!(
                    "try_setup_output_type: SetUINT32 Bitrate FAILED 0x{:08X}",
                    e.code().0
                );
                EncoderError::InitFailed(format!("SetUINT32 Bitrate: 0x{:08X}", e.code().0))
            })?;
        av_trace!("try_setup_output_type: SetUINT32 Bitrate OK");

        av_trace!("try_setup_output_type: before SetOutputType(0, out_type, 0)");
        mft.SetOutputType(0, &out_type, 0).map_err(|e| {
            av_trace!(
                "try_setup_output_type: SetOutputType FAILED 0x{:08X}",
                e.code().0
            );
            EncoderError::InitFailed(format!("SetOutputType: 0x{:08X}", e.code().0))
        })?;
        av_trace!("try_setup_output_type: SetOutputType OK");
    }

    av_trace!("try_setup_output_type: EXIT Ok");
    Ok(())
}

// ── Encoder thread ────────────────────────────────────────────────────────────

/// RAII guard that calls `CoUninitialize` on drop (paired with encoder thread's CoInitializeEx).
struct CoUninitGuard;

impl Drop for CoUninitGuard {
    fn drop(&mut self) {
        // SAFETY: Paired with CoInitializeEx in run_encoder_thread. If init failed
        // on this thread, this call is a documented no-op (apartment refcount
        // was never incremented; decrementing stays at 0). See DD10 and Microsoft
        // COM docs: "CoUninitialize on a thread where CoInitializeEx returned an error
        // does not corrupt other threads' apartments."
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
    av_trace!("run_encoder_thread: ENTER (encoder thread)");
    // SAFETY: CoInitializeEx returns HRESULT directly (not Result). S_OK (0) and
    // S_FALSE (1, already initialised on this apartment) both pass is_err() == false.
    // We install the CoUninitGuard BEFORE checking co_hr so the un-init runs even
    // on failure paths below. Per Microsoft docs, CoUninitialize on a thread whose
    // CoInitializeEx returned an error (apartment refcount stayed 0) is a no-op —
    // it does NOT corrupt other threads' apartments. Verified safe. (See DD10.)
    av_trace!("run_encoder_thread: before CoInitializeEx");
    let co_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    av_trace!("run_encoder_thread: CoInitializeEx hr=0x{:08X}", co_hr.0);
    // SAFETY: paired with CoInitializeEx above (or no-op if init failed, per docs).
    let _co_guard = CoUninitGuard;

    if co_hr.is_err() {
        av_trace!("run_encoder_thread: CoInitializeEx FAILED — thread exit");
        tracing::error!("encoder thread CoInitializeEx failed: 0x{:08X}", co_hr.0);
        return;
    }
    av_trace!(
        "run_encoder_thread: CoInitializeEx OK config={}x{} @{}fps {}bps",
        config.width, config.height, config.framerate, config.bitrate_bps
    );
    tracing::debug!(
        "encoder thread CoInitializeEx OK; config: {}x{} @ {}fps {}bps",
        config.width,
        config.height,
        config.framerate,
        config.bitrate_bps
    );

    // Steps 7b–7h: Setup output/input media types and streaming messages.
    // (MF_TRANSFORM_ASYNC_UNLOCK was already set during probe_and_select_mft.)
    av_trace!("run_encoder_thread: before setup_mft");
    if let Err(e) = setup_mft(&mft, &config) {
        av_trace!("run_encoder_thread: setup_mft FAILED: {e} — thread exit");
        tracing::error!("MFT setup failed: {e}");
        // MFShutdown for the thread's MF context is not needed here because
        // the caller thread's new() owns MFStartup/MFShutdown (step 2 and Drop).
        return;
    }
    av_trace!("run_encoder_thread: setup_mft OK");
    tracing::debug!("setup_mft OK; entering pump_loop");

    av_trace!("run_encoder_thread: before IMFMediaEventGenerator cast");
    let event_gen: IMFMediaEventGenerator = match mft.cast() {
        Ok(g) => {
            av_trace!("run_encoder_thread: IMFMediaEventGenerator cast OK");
            g
        }
        Err(e) => {
            av_trace!("run_encoder_thread: IMFMediaEventGenerator cast FAILED 0x{:08X} — thread exit", e.code().0);
            tracing::error!("IMFMediaEventGenerator cast failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    // Per OQ-NEW-1 resolution (DD5): detect Annex-B vs AVCC at first packet in
    // collect_output, not during init. The probe_output_format approach (submit
    // a 16×16 NV12 frame during setup) corrupted the MFT event pipeline on
    // hardware encoders (see explore #583 Bucket A diagnosis). Removed in Phase 4.
    let mut output_format_known: Option<bool> = None; // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB

    // Step 8: Pump loop.
    av_trace!("run_encoder_thread: before pump_loop");
    pump_loop(
        &mft,
        &codec_api,
        &event_gen,
        &state,
        rx,
        tx,
        &mut output_format_known,
        &config,
    );
    av_trace!("run_encoder_thread: after pump_loop returned");

    // Steps 9a–9e: Notify end of stream and release.
    av_trace!("run_encoder_thread: before END_OF_STREAM + END_STREAMING messages");
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    av_trace!("run_encoder_thread: after cleanup messages — mft/codec_api/event_gen about to drop");
    // mft, codec_api, event_gen are dropped here (COM Release via Drop).
    // MFShutdown for the thread is NOT called here — the caller thread's Drop handles it.
    // CoUninitGuard calls CoUninitialize when this function returns.
    av_trace!("run_encoder_thread: EXIT (CoUninitGuard will fire CoUninitialize)");
}

/// Resolve effective (width, height) from config, applying 1920×1080 fallback for sentinel zeros.
///
/// Sentinel `0` triggers the fallback: adapters that do not know the capture
/// dimensions at construction time pass zero, and `setup_mft` uses the 1920×1080
/// default. Production callers should supply real screen dimensions via
/// `EncoderConfig { width: cap_w, height: cap_h, ..EncoderConfig::default() }`.
///
/// See design DD3 and spec R3.
fn effective_dimensions(config: &EncoderConfig) -> (u32, u32) {
    let w = if config.width == 0 {
        1920
    } else {
        config.width
    };
    let h = if config.height == 0 {
        1080
    } else {
        config.height
    };
    (w, h)
}

/// Setup MFT media types and streaming messages (steps 7b–7h).
///
/// `MF_TRANSFORM_ASYNC_UNLOCK` is NOT set here — it is set once in
/// `probe_and_select_mft` during the candidate probe (before `ActivateObject`
/// returns the winning IMFTransform). Setting it again here would be idempotent
/// on most MFTs but is cleaner to do once. See DD-A / DD-B.
fn setup_mft(mft: &IMFTransform, config: &EncoderConfig) -> Result<(), EncoderError> {
    av_trace!("setup_mft: ENTER");
    // Sentinel-zero triggers 1920×1080 fallback per DD3; production callers supply
    // real dimensions via EncoderConfig.width / EncoderConfig.height.
    // See effective_dimensions() for the fallback policy.
    let (w, h) = effective_dimensions(config);
    av_trace!("setup_mft: effective dims w={w} h={h} fps={} bps={}", config.framerate, config.bitrate_bps);

    // Step 7b: SetOutputType FIRST (MFT requirement: output before input).
    // Delegates to try_setup_output_type (DD-E single source of truth) using the
    // real session config dimensions, framerate, and bitrate.
    // NOTE: probe_and_select_mft already called try_setup_output_type once (probe pass).
    // This is the SECOND call — DOUBLE NEGOTIATION (H-AV2 suspect).
    av_trace!("setup_mft: DOUBLE NEGOTIATION — calling try_setup_output_type again (2nd call after probe)");
    if let Err(e) = try_setup_output_type(mft, w, h, config.framerate, config.bitrate_bps) {
        av_trace!("setup_mft: try_setup_output_type FAILED: {e}");
        return Err(e);
    }
    av_trace!("setup_mft: try_setup_output_type OK (2nd call)");

    // Step 7c: SetInputType (NV12).
    av_trace!("setup_mft: before MFCreateMediaType (input type)");
    let in_type: IMFMediaType = unsafe { MFCreateMediaType() }.map_err(|e| {
        EncoderError::InitFailed(format!("MFCreateMediaType(in): 0x{:08X}", e.code().0))
    })?;
    av_trace!("setup_mft: MFCreateMediaType OK");

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
        av_trace!("setup_mft: before SetInputType");
        mft.SetInputType(0, &in_type, 0)
            .map_err(|e| EncoderError::InitFailed(format!("SetInputType: 0x{:08X}", e.code().0)))?;
        av_trace!("setup_mft: SetInputType OK");
    }

    // Steps 7f–7h: Send streaming messages.
    // (Async unlock was set in probe_and_select_mft during init — not repeated here.)
    av_trace!("setup_mft: before COMMAND_FLUSH");
    unsafe {
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("COMMAND_FLUSH: 0x{:08X}", e.code().0))
            })?;
        av_trace!("setup_mft: COMMAND_FLUSH OK — before BEGIN_STREAMING");
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("BEGIN_STREAMING: 0x{:08X}", e.code().0))
            })?;
        av_trace!("setup_mft: BEGIN_STREAMING OK — before START_OF_STREAM");
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            .map_err(|e| {
                EncoderError::InitFailed(format!("START_OF_STREAM: 0x{:08X}", e.code().0))
            })?;
        av_trace!("setup_mft: START_OF_STREAM OK");
    }

    av_trace!("setup_mft: EXIT Ok");
    Ok(())
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

/// Apply pending keyframe and bitrate codec settings before a ProcessInput call.
///
/// Mechanical extraction from the NeedInput arm (design DD9). Module-private.
fn apply_pending_codec_settings(codec_api: &ICodecAPI, state: &MftEncoderShared) {
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
}

#[expect(
    clippy::too_many_arguments,
    reason = "pump_loop takes mft, codec_api, event_gen, config, state, rx, tx + format state — design §5a one-function pump shape"
)]
fn pump_loop(
    mft: &IMFTransform,
    codec_api: &ICodecAPI,
    event_gen: &IMFMediaEventGenerator,
    state: &MftEncoderShared,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
    output_format_known: &mut Option<bool>, // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB
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

    // Dual-arm counters tracking pending MFT credits (spec R2, design DD1/DD2).
    // Stack-local — no atomics needed; only the pump thread reads/writes these.
    let mut ni_count: u32 = 0; // pending METransformNeedInput credits
    let mut ho_count: u32 = 0; // pending METransformHaveOutput credits

    // Change-detection sentinels for DD8 counter snapshot logging (spec R7/S7.2).
    let mut last_logged_ni: u32 = u32::MAX;
    let mut last_logged_ho: u32 = u32::MAX;
    let mut iter_count: u64 = 0;

    loop {
        // Top-of-loop stop check — sole exit via stop flag (design DD1, spec R4/S4.1).
        // With POLLING_SLEEP=1ms this is reached within ≤2ms from any idle state.
        if state.stop.load(Ordering::Acquire) {
            tracing::info!("pump_loop: stop signaled, exiting");
            break;
        }

        // Non-blocking event poll (spec R1/S1.1, design DD1).
        // MF_EVENT_FLAG_NO_WAIT returns MF_E_NO_EVENTS_AVAILABLE immediately if idle.
        let event_opt = match unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                let event_type = match unsafe { event.GetType() } {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("GetType failed: 0x{:08X}", e.code().0);
                        break;
                    }
                };
                tracing::trace!("pump_loop event_type=0x{:08X}", event_type);

                if event_type == METransformNeedInput.0 as u32 {
                    ni_count = ni_count.saturating_add(1);
                } else if event_type == METransformHaveOutput.0 as u32 {
                    ho_count = ho_count.saturating_add(1);
                } else if event_type == METransformDrainComplete.0 as u32 {
                    // WHY: DrainComplete signals the encoder has flushed all in-flight frames after
                    // a COMMAND_DRAIN. Reset both counters so any subsequent stream re-arm starts
                    // clean — without this, phantom credits from the drained segment would cause
                    // over-servicing on the next input cycle. Spec R6/S6.1, design DD3.
                    // Do NOT break: top-of-loop state.stop is the sole exit point (OQ-4 resolved).
                    let old_ni = ni_count;
                    let old_ho = ho_count;
                    ni_count = 0;
                    ho_count = 0;
                    tracing::info!(
                        old_ni_count = old_ni,
                        old_ho_count = old_ho,
                        "pump_loop: DrainComplete — counters reset"
                    );
                } else if event_type == MEEndOfStream.0 as u32 {
                    tracing::info!("pump_loop: MEEndOfStream (0x{:08X}), exiting", event_type);
                    break;
                } else {
                    // Catch-all: vendor MFTs may emit MEError, MESessionStreamSinkFormatChanged,
                    // or other events not anticipated by the original design. Log loudly so
                    // the smoke transcript reveals the actual event sequence rather than
                    // silently spinning until a test timeout.
                    tracing::warn!(
                        "pump_loop received unhandled event_type=0x{:08X}; continuing loop",
                        event_type
                    );
                }
                true // got an event
            }
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                // Expected idle state — no event queued. Fall through to service pass.
                false
            }
            Err(e) if e.code() == MF_E_SHUTDOWN => {
                tracing::info!("pump_loop: MF_E_SHUTDOWN, exiting");
                break;
            }
            Err(e) if e.code().0 as u32 == 0x8000_4004 => {
                // E_ABORT — graceful shutdown signal (preserved from original code).
                tracing::info!("pump_loop: E_ABORT, exiting");
                break;
            }
            Err(e) => {
                tracing::error!("pump_loop: GetEvent unexpected error: 0x{:08X}", e.code().0);
                break;
            }
        };

        // ── Drain HaveOutput FIRST (vendor priming requirement, spec R3, design DD1) ──
        // HaveOutput credits must be consumed before NeedInput to prevent pipeline deadlock
        // on vendor MFTs that emit HaveOutput before the first NeedInput at startup.
        while ho_count > 0 {
            match collect_output(mft, output_format_known, current_ts, &mut seq) {
                Ok(Some(pkt)) => {
                    // Decrement AFTER successful COM call (spec OQ-1, design DD2).
                    ho_count -= 1;
                    match tx.try_send(pkt) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            state.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            tracing::info!("pump_loop: packet channel disconnected, exiting");
                            return;
                        }
                    }
                }
                Ok(None) => {
                    // NEED_MORE_INPUT or stream-change — consume credit (output was attempted).
                    ho_count -= 1;
                }
                Err(e) => {
                    // Detect vendor-priming E_UNEXPECTED via string prefix match (design DD4,
                    // DR-NEW-2). Recognition: EncodeFailed reason starts with "ProcessOutput: 0x80004005".
                    let reason = e.to_string();
                    if reason.contains("ProcessOutput: 0x80004005") {
                        // E_UNEXPECTED on vendor priming: consume credit, log warn, continue.
                        // This is expected during HW MFT startup before any frame is submitted.
                        tracing::warn!(
                            "pump_loop: ProcessOutput E_UNEXPECTED (vendor priming) — consuming credit"
                        );
                        ho_count -= 1;
                    } else {
                        tracing::error!("pump_loop: collect_output failed: {e}");
                        return;
                    }
                }
            }
        }

        // ── Service NeedInput (submit frames) ─────────────────────────────────────
        while ni_count > 0 {
            apply_pending_codec_settings(codec_api, state);

            // WHY: FRAME_RECV_TIMEOUT ≤50ms is load-bearing for T-NEW-2
            // (`mft_stop_during_active_encode_returns_within_deadline`). When stop()
            // is called during active encode, the loop exits this wait within 50ms
            // and reaches the top-of-loop stop check. Option A (Phase 1 user decision).
            // See spec OQ-5 + design DD7. DO NOT increase beyond 50ms.
            match rx.recv_timeout(FRAME_RECV_TIMEOUT) {
                Ok(frame) => {
                    current_ts = frame.timestamp;
                    nv12_convert(&frame, &mut nv12_scratch);
                    match submit_frame(mft, &nv12_scratch, frame.timestamp, frame_dur_100ns) {
                        Ok(()) => {
                            // Decrement AFTER successful ProcessInput (spec OQ-1, design DD2).
                            ni_count -= 1;
                        }
                        Err(e) => {
                            // Check for MF_E_NOTACCEPTING — indicates counter desync (design DD5).
                            let reason = e.to_string();
                            if reason.contains("ProcessInput: 0xC00D36B5") {
                                // MF_E_NOTACCEPTING should NEVER happen when counters are correct.
                                debug_assert!(
                                    false,
                                    "MF_E_NOTACCEPTING on serviced NeedInput credit — counter logic wrong"
                                );
                                tracing::error!(
                                    "pump_loop: MF_E_NOTACCEPTING — counter desync (should be unreachable): {e}"
                                );
                                return;
                            }
                            // Other ProcessInput errors: skip frame, consume credit, continue
                            // (mirrors openh264 behaviour for non-fatal input errors).
                            tracing::warn!("pump_loop: ProcessInput failed (skipping frame): {e}");
                            ni_count -= 1;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // No frame available within timeout — do NOT consume the NeedInput credit.
                    // The MFT retains the credit; we re-poll events on the next iteration.
                    // This path exits the inner loop so the top-of-loop stop check runs (DD2).
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Upstream closed — drain MFT and continue looping (do NOT consume credit).
                    tracing::info!("pump_loop: frame channel disconnected, sending COMMAND_DRAIN");
                    unsafe {
                        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                    }
                    break;
                }
            }
        }

        // ── Idle sleep — avoid busy-wait when nothing happened (spec R5, design DD6) ──
        if !event_opt && ni_count == 0 && ho_count == 0 {
            std::thread::sleep(POLLING_SLEEP);
        }

        // ── Heartbeat and counter snapshot logging (spec R7, design DD8) ────────────
        iter_count = iter_count.wrapping_add(1);

        // Emit counter snapshot only when values changed (change-only, no spam).
        if ni_count != last_logged_ni || ho_count != last_logged_ho {
            tracing::trace!(
                ni_count,
                ho_count,
                iter_count,
                "pump_loop: counter snapshot (on change)"
            );
            last_logged_ni = ni_count;
            last_logged_ho = ho_count;
        }

        // Periodic debug heartbeat every 1000 iterations.
        if iter_count % 1000 == 0 {
            tracing::debug!(ni_count, ho_count, iter_count, "pump_loop: heartbeat");
        }
    }
    tracing::debug!("pump_loop exited cleanly");
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
///
/// Implements per-packet Annex-B vs AVCC detection (DD5, OQ-NEW-1 option (a)).
/// `output_format_known` is `None` until the first packet arrives; afterwards
/// `Some(false)` = Annex-B (no rewrite) or `Some(true)` = AVCC (apply shim).
/// Sniffing every packet while the cache is `None` self-corrects against
/// partial first-packet (R-NEW-6).
fn collect_output(
    mft: &IMFTransform,
    output_format_known: &mut Option<bool>, // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB
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

    // Per-packet Annex-B sniff (resolves OQ-NEW-1 option (a), mitigates R-NEW-6).
    // If the packet is too short to sniff, defer and skip — do not cache.
    if raw_bytes.len() < 4 {
        tracing::warn!(
            "collect_output: packet too short to sniff format ({} bytes), deferring",
            raw_bytes.len()
        );
        return Ok(None);
    }

    let is_annex_b_now = raw_bytes[0] == 0x00
        && raw_bytes[1] == 0x00
        && raw_bytes[2] == 0x00
        && raw_bytes[3] == 0x01;

    // Cache decision on first clean observation; trust it thereafter.
    let annex_b = match (*output_format_known, is_annex_b_now) {
        (None, true) => {
            *output_format_known = Some(false); // Annex-B confirmed
            raw_bytes
        }
        (None, false) => {
            *output_format_known = Some(true); // AVCC confirmed — apply rewrite
            avcc_to_annex_b(&raw_bytes)
        }
        (Some(false), _) => raw_bytes, // cached Annex-B
        (Some(true), _) => avcc_to_annex_b(&raw_bytes), // cached AVCC (always rewrite)
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

    // ─── T2.1: effective_dimensions_returns_fallback_for_sentinel_zero ────────
    //
    // CI-runnable. Verifies that (0, 0) config triggers the 1920×1080 fallback.
    // RED until effective_dimensions() is added (T2.4).

    #[test]
    fn effective_dimensions_returns_fallback_for_sentinel_zero() {
        let cfg = EncoderConfig {
            width: 0,
            height: 0,
            ..EncoderConfig::default()
        };
        let (w, h) = effective_dimensions(&cfg);
        assert_eq!(w, 1920, "sentinel width 0 must fall back to 1920");
        assert_eq!(h, 1080, "sentinel height 0 must fall back to 1080");
    }

    // ─── T2.2: effective_dimensions_passes_through_nonzero ───────────────────
    //
    // CI-runnable. Verifies that non-zero dimensions pass through unchanged.
    // RED until effective_dimensions() is added (T2.4).

    #[test]
    fn effective_dimensions_passes_through_nonzero() {
        let cfg = EncoderConfig {
            width: 640,
            height: 480,
            ..EncoderConfig::default()
        };
        let (w, h) = effective_dimensions(&cfg);
        assert_eq!(w, 640, "non-zero width must pass through unchanged");
        assert_eq!(h, 480, "non-zero height must pass through unchanged");
    }

    // ─── T4.1 (Phase 4 advance): avcc_to_annex_b converts known AVCC payload ───
    //
    // CI-runnable pure-byte test for the rewrite shim. Placed here (in Phase 2
    // test block) because it tests a function that already exists and is already
    // GREEN — this is a defensive regression guard, not a RED→GREEN transition.

    #[test]
    fn avcc_to_annex_b_converts_known_avcc_payload() {
        // AVCC: [4-byte BE length = 5][5 bytes NAL]
        let avcc = vec![0x00u8, 0x00, 0x00, 0x05, 0x65, 0x88, 0x84, 0x00, 0x00];
        let out = avcc_to_annex_b(&avcc);
        assert_eq!(
            &out[..4],
            &[0x00u8, 0x00, 0x00, 0x01],
            "AVCC→AnnexB must produce start code 00 00 00 01"
        );
        assert_eq!(
            &out[4..9],
            &[0x65u8, 0x88, 0x84, 0x00, 0x00],
            "AVCC→AnnexB must preserve NAL payload bytes"
        );
    }

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
