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
//! - `new()`: validates config, runs a DESTRUCTIVE probe on the caller thread
//!   (`CoInitializeEx` + `MFStartup` + `MFTEnumEx` + per-candidate
//!   `ActivateObject` + `try_setup_output_type` + `ShutdownObject`). The winning
//!   `IMFActivate` is retained; its `IMFTransform` is discarded after the probe.
//! - `start(rx, tx)`: transfers the `IMFActivate` to a spawned OS thread. The
//!   encoder thread calls `ActivateObject` itself and owns the `IMFTransform`
//!   entirely — no cross-thread COM transfer of `IMFTransform` or `ICodecAPI`.
//! - `stop()`: idempotent. Sets stop flag and joins the handle.
//! - `Drop`: calls `stop()` + `MFShutdown` + `CoUninitialize` on the caller thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType, IMFTransform, MEEndOfStream,
    METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE, MF_E_SHUTDOWN, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFShutdown,
    MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO,
    MFT_TRANSFORM_CLSID_Attribute, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
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
// hardware MFT factory pointers (IMFActivate). IMFTransform and ICodecAPI are
// NOT transferred cross-thread; they live entirely on the encoder thread.
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
    /// Set by `flush()`; consumed by pump_loop via swap(false, AcqRel) after the NeedInput
    /// inner loop. Fires exactly one MFT_MESSAGE_COMMAND_DRAIN per flag-set transition.
    // allow: drain_pending only read by pump_loop under hw-encoder feature
    #[allow(dead_code)]
    drain_pending: AtomicBool,
    /// PHASE 0 EMPIRICAL: Set by `flush_state()`; consumed by pump_loop via swap(false, AcqRel).
    /// Fires FLUSH+BEGIN_STREAMING+START_OF_STREAM — the same 3-message sequence as setup_mft.
    /// Unlike DRAIN (async), FLUSH is synchronous: no DrainComplete event follows.
    // allow: flush_state_pending only read by pump_loop under hw-encoder feature
    #[allow(dead_code)]
    flush_state_pending: AtomicBool,
    /// PHASE 0 EMPIRICAL: Set by `force_idr_via_gop_toggle()`; consumed by pump_loop via
    /// swap(false, AcqRel). Toggles CODECAPI_AVEncMPVGOPSize=1 before next ProcessInput,
    /// then restores original GOP size after.
    // allow: force_idr_gop_pending only read by pump_loop under hw-encoder feature
    #[allow(dead_code)]
    force_idr_gop_pending: AtomicBool,
}

impl Default for MftEncoderShared {
    fn default() -> Self {
        Self {
            keyframe_pending: AtomicBool::new(false),
            pending_bitrate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            drain_pending: AtomicBool::new(false),
            flush_state_pending: AtomicBool::new(false),
            force_idr_gop_pending: AtomicBool::new(false),
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
    /// The winning `IMFActivate` selected during the destructive probe in `new()`.
    ///
    /// `IMFActivate` is an MTA-registered COM factory pointer; it is safe to transfer
    /// across MTA threads (see `ComSend` safety contract). Transferred to the encoder
    /// thread in `start()` via `ComSend`. The encoder thread calls `ActivateObject` on
    /// it to produce a fresh `IMFTransform` that lives ENTIRELY on that thread —
    /// no cross-thread COM for `IMFTransform` or `ICodecAPI` (root cause of AVs in
    /// commit ccd2e43, see sdd/.../phase0v3-final-root-cause).
    winning_activate: Option<IMFActivate>,
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
// `winning_activate` (`Option<IMFActivate>`) is transferred to the encoder thread
// inside `start()` via `ComSend` and set to `None` on the caller side immediately
// after. `IMFActivate` is an MTA-registered factory pointer; cross-thread transfer
// is safe per Windows COM rules for MTA-registered objects (see `ComSend` docs).
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
    /// Performs a synchronous DESTRUCTIVE probe on the caller's thread:
    /// `CoInitializeEx` → `MFStartup` → `MFTEnumEx` → per-candidate probe
    /// (`ActivateObject`, `MF_TRANSFORM_ASYNC_UNLOCK`, `GetOutputAvailableType(0,0)`,
    /// `SetOutputType` with Strategy E, then `ShutdownObject`). The winning
    /// candidate's `IMFTransform` is discarded after the probe; only the `IMFActivate` is retained.
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

        // Synchronous MFT probe (design §5 steps 1–5).
        // Returns only the winning IMFActivate — the IMFTransform from the probe is
        // ShutdownObject'd immediately. The encoder thread re-activates from the
        // IMFActivate to produce a fresh IMFTransform that lives entirely on that thread.
        // This eliminates cross-thread IMFTransform transfer, the root cause of AVs in
        // commit ccd2e43 (phase0v3 trace: H-AV3 confirmed — NVENC singleton corruption
        // when IMFTransform is used from a different thread than ActivateObject).
        let activate = init_mft_sync(&config)?;

        Ok(Self {
            config,
            state: Arc::new(MftEncoderShared::default()),
            winning_activate: Some(activate),
            handle: None,
            com_initialized: true,
        })
    }

    fn start(
        &mut self,
        rx: Receiver<sm_domain::CaptureFrame>,
        tx: SyncSender<EncodedPacket>,
    ) -> Result<(), EncoderError> {
        let activate = self.winning_activate.take().ok_or_else(|| {
            EncoderError::Internal("start() called after IMFActivate was already consumed".into())
        })?;
        let config = self.config.clone();
        let state = Arc::clone(&self.state);

        state.stop.store(false, Ordering::Release);

        // Wrap IMFActivate in ComSend so the closure is Send.
        // SAFETY: IMFActivate is an MTA-registered COM factory; cross-thread transfer
        // is safe when both threads join the MTA via CoInitializeEx(COINIT_MULTITHREADED).
        // The encoder thread calls ActivateObject on the activate to produce a fresh
        // IMFTransform that lives ENTIRELY on that thread — no cross-thread IMFTransform
        // or ICodecAPI transfer (root cause of AVs in commit ccd2e43).
        let activate_send = ComSend(activate);

        let handle = std::thread::spawn(move || {
            // into_inner() unwraps ComSend<IMFActivate> → IMFActivate inside the thread.
            run_encoder_thread(activate_send.into_inner(), config, state, rx, tx);
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
        // Join encoder thread first. If start() was never called this is a no-op.
        let _ = self.stop();

        // winning_activate: if start() was never called, the IMFActivate is still here.
        // Drop it before MFShutdown to release the COM ref while MF is still alive.
        // If start() was called, winning_activate is already None (transferred to thread).
        drop(self.winning_activate.take());

        if self.com_initialized {
            unsafe {
                // SAFETY: MFShutdown is the documented teardown for MFStartup.
                // Process-global refcount per A4; safe to call here AFTER all MF
                // interface refs above have been Release()d.
                let _ = MFShutdown();
                // SAFETY: CoUninitialize matches the CoInitializeEx in init_mft_sync()
                // on this same thread. Safe AFTER all COM refs above are released.
                CoUninitialize();
            }
            self.com_initialized = false;
        }
    }
}

// ── Synchronous MFT initialisation (design §5 steps 1–6) ─────────────────────

/// Caller-thread MFT probe. Returns the winning `IMFActivate` (not an `IMFTransform`).
///
/// Each candidate is activated, probed with `try_setup_output_type`, then immediately
/// `ShutdownObject`'d — including the winner. The returned `IMFActivate` is transferred
/// to the encoder thread in `start()`, which calls `ActivateObject` to produce a fresh
/// `IMFTransform` that lives entirely on that thread.
///
/// WHY: NVENC's `IMFTransform` — despite MTA registration — AVs when used from a
/// different thread than the one that called `ActivateObject` (phase0v3 trace: H-AV3
/// confirmed). This destructive-probe approach eliminates all cross-thread COM transfer
/// for `IMFTransform` and `ICodecAPI`.
fn init_mft_sync(config: &EncoderConfig) -> Result<IMFActivate, EncoderError> {
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

    // Steps 3–5: Enumerate IMFActivate candidates and probe each with full output-type
    // negotiation. MFTEnumEx(MFT_ENUM_FLAG_HARDWARE) returns vendor MFTs even when
    // their hardware is absent (e.g. AMD MFT on a non-AMD system). Probe selects the
    // winner; winner's IMFTransform is ShutdownObject'd after probe.
    // Phase 0 v2 evidence: Host B has 3 candidates — pactivates[0] and [1] are
    // AMDh264Encoder (no AMD GPU), [2] is NVIDIA H.264 Encoder MFT. Only [2] accepts
    // strategy E (FRAME_SIZE + FRAME_RATE + AVG_BITRATE on cloned slot[0] type).
    let activates_result = enumerate_activates();

    let result = match activates_result {
        Ok(activates) => probe_and_select_mft(activates, config),
        Err(e) => Err(e),
    };

    match result {
        Ok(activate) => Ok(activate),
        Err(err) => {
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
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

/// Iterate IMFActivate candidates and return the FIRST one that passes the full
/// output-type negotiation probe (DD-A, DD-D, DD-E). This is a DESTRUCTIVE probe:
/// the winner's `IMFTransform` is `ShutdownObject`'d after the probe, just like
/// the rejected candidates. Only the `IMFActivate` (factory pointer) is returned.
///
/// For each candidate:
///   1. `ActivateObject::<IMFTransform>` — if Err, log + `ShutdownObject` + skip.
///   2. `MF_TRANSFORM_ASYNC_UNLOCK = 1` — required before any other MFT call.
///   3. `try_setup_output_type` — Strategy E probe (DD-B). If Err, log + `ShutdownObject` + skip.
///   4. Winner found — `ShutdownObject` the temporary IMFTransform, return the `IMFActivate`.
///      The encoder thread will call `ActivateObject` again to get a fresh `IMFTransform`
///      on its own thread, eliminating cross-thread COM transfer entirely.
///
/// If all candidates fail, returns `EncoderError::InitFailed` with the last error.
///
/// Note: `ICodecAPI` cast is NOT done here — it is done on the encoder thread after
/// the fresh `ActivateObject` call, where the IMFTransform will actually be used.
fn probe_and_select_mft(
    activates: Vec<IMFActivate>,
    config: &EncoderConfig,
) -> Result<IMFActivate, EncoderError> {
    // Use the session's real config dimensions for the probe so the
    // SetOutputType call validates the actual session parameters.
    let (probe_w, probe_h) = effective_dimensions(config);
    let probe_fps = config.framerate;
    let probe_bps = config.bitrate_bps;

    let count = activates.len();
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

        tracing::info!(
            "probe_and_select_mft: candidate [{i}/{count}] \"{friendly_name}\" {clsid_str}"
        );

        // ── Step 1: ActivateObject ────────────────────────────────────────────
        // SAFETY: ActivateObject is the documented way to instantiate an IMFTransform
        // from an IMFActivate pointer obtained via MFTEnumEx.
        let mft: IMFTransform = match unsafe { activate.ActivateObject() } {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "probe_and_select_mft: candidate [{i}] ActivateObject failed \
                     (0x{:08X}); trying next",
                    e.code().0
                );
                // ShutdownObject releases any partially-initialised GPU resources (DD-D).
                // SAFETY: ShutdownObject on an activate that failed ActivateObject is a no-op
                // per Windows docs (it can only free what was allocated).
                let _ = unsafe { activate.ShutdownObject() };
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
        let attrs = match unsafe { mft.GetAttributes() } {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(
                    "probe_and_select_mft: candidate [{i}] GetAttributes failed \
                     (0x{:08X}); trying next",
                    e.code().0
                );
                let _ = unsafe { activate.ShutdownObject() };
                last_err = EncoderError::InitFailed(format!(
                    "GetAttributes[{i}] ({friendly_name}): 0x{:08X}",
                    e.code().0
                ));
                continue;
            }
        };
        if let Err(e) = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) } {
            tracing::warn!(
                "probe_and_select_mft: candidate [{i}] MF_TRANSFORM_ASYNC_UNLOCK failed \
                 (0x{:08X}); trying next",
                e.code().0
            );
            let _ = unsafe { activate.ShutdownObject() };
            last_err = EncoderError::InitFailed(format!(
                "MF_TRANSFORM_ASYNC_UNLOCK[{i}] ({friendly_name}): 0x{:08X}",
                e.code().0
            ));
            continue;
        }

        // ── Step 3: Output-type negotiation probe (DD-B via DD-E helper) ─────
        // Calls GetOutputAvailableType(0, 0) + overlay FRAME_SIZE + FRAME_RATE +
        // AVG_BITRATE + SetOutputType. NVENC's slot[0] pre-sets INTERLACE_MODE=2
        // (Progressive) and MPEG2_PROFILE=77 (Main); do NOT overlay them.
        if let Err(e) = try_setup_output_type(&mft, probe_w, probe_h, probe_fps, probe_bps) {
            tracing::warn!(
                "probe_and_select_mft: candidate [{i}] \"{friendly_name}\" {clsid_str} \
                 rejected output-type negotiation ({e}); trying next"
            );
            let _ = unsafe { activate.ShutdownObject() };
            last_err = e;
            continue;
        }

        // ── Step 4 (WINNER) — destructive shutdown of the probe IMFTransform ──
        // ShutdownObject releases the temporary IMFTransform obtained above.
        // The encoder thread will call ActivateObject again to get a fresh
        // IMFTransform that lives entirely on its own thread, eliminating cross-thread
        // COM transfer of IMFTransform (root cause of AVs).
        // ICodecAPI cast is NOT done here — it happens on the encoder thread.
        // Drop mft first so the COM ref is released before ShutdownObject.
        drop(mft);
        let _ = unsafe { activate.ShutdownObject() };

        tracing::info!(
            "probe_and_select_mft: selected candidate [{i}] \"{friendly_name}\" {clsid_str}"
        );
        return Ok(activate.clone());
    }

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
    // Step 7b: Clone NVENC's advertised output type at slot 0 and overlay the three
    // caller-controlled attributes (FRAME_SIZE, FRAME_RATE, AVG_BITRATE). Per Phase 0
    // v2 evidence: NVENC's slot[0] pre-sets INTERLACE_MODE = 2 (Progressive) and
    // MPEG2_PROFILE = 77 (Main); overlaying those would invalidate the negotiation
    // envelope. PAR is absent and stays absent (NVENC infers 1:1).
    let out_type: IMFMediaType = unsafe { mft.GetOutputAvailableType(0, 0) }.map_err(|e| {
        EncoderError::InitFailed(format!("GetOutputAvailableType(0,0): 0x{:08X}", e.code().0))
    })?;

    unsafe {
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameSize: 0x{:08X}", e.code().0))
            })?;

        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT64 FrameRate: 0x{:08X}", e.code().0))
            })?;

        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
            .map_err(|e| {
                EncoderError::InitFailed(format!("SetUINT32 Bitrate: 0x{:08X}", e.code().0))
            })?;

        mft.SetOutputType(0, &out_type, 0).map_err(|e| {
            EncoderError::InitFailed(format!("SetOutputType: 0x{:08X}", e.code().0))
        })?;
    }

    Ok(())
}

// WHY: async MFT spec — no flush, no NOTIFY_BEGIN_STREAMING resend (R4/R5, DD8)
fn renegotiate_output_type(
    mft: &IMFTransform,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<(), EncoderError> {
    let out_type: IMFMediaType = unsafe { mft.GetOutputAvailableType(0, 0) }.map_err(|e| {
        EncoderError::EncodeFailed(format!(
            "renegotiate: GetOutputAvailableType: 0x{:08X}",
            e.code().0
        ))
    })?;

    unsafe {
        out_type
            .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
            .map_err(|e| {
                EncoderError::EncodeFailed(format!(
                    "renegotiate: SetUINT64 FrameSize: 0x{:08X}",
                    e.code().0
                ))
            })?;

        out_type
            .SetUINT64(&MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1)
            .map_err(|e| {
                EncoderError::EncodeFailed(format!(
                    "renegotiate: SetUINT64 FrameRate: 0x{:08X}",
                    e.code().0
                ))
            })?;

        out_type
            .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)
            .map_err(|e| {
                EncoderError::EncodeFailed(format!(
                    "renegotiate: SetUINT32 Bitrate: 0x{:08X}",
                    e.code().0
                ))
            })?;

        mft.SetOutputType(0, &out_type, 0).map_err(|e| {
            EncoderError::EncodeFailed(format!("renegotiate: SetOutputType: 0x{:08X}", e.code().0))
        })?;
    }

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
    activate: IMFActivate,
    config: EncoderConfig,
    state: Arc<MftEncoderShared>,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
) {
    // Step 1: CoInitializeEx on the encoder thread (MTA).
    // SAFETY: CoInitializeEx returns HRESULT directly (not Result). S_OK (0) and
    // S_FALSE (1, already initialised on this apartment) both pass is_err() == false.
    // We install the CoUninitGuard BEFORE checking co_hr so the un-init runs even
    // on failure paths below. Per Microsoft docs, CoUninitialize on a thread whose
    // CoInitializeEx returned an error (apartment refcount stayed 0) is a no-op —
    // it does NOT corrupt other threads' apartments. Verified safe. (See DD10.)
    let co_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // SAFETY: paired with CoInitializeEx above (or no-op if init failed, per docs).
    let _co_guard = CoUninitGuard;

    if co_hr.is_err() {
        tracing::error!("encoder thread CoInitializeEx failed: 0x{:08X}", co_hr.0);
        return;
    }
    tracing::debug!(
        "encoder thread CoInitializeEx OK; config: {}x{} @ {}fps {}bps",
        config.width,
        config.height,
        config.framerate,
        config.bitrate_bps
    );

    // Step 2: ActivateObject — produce a fresh IMFTransform ENTIRELY ON THIS THREAD.
    // WHY: NVENC's IMFTransform AVs when used from a different thread than ActivateObject.
    // The caller thread (init_mft_sync / probe_and_select_mft) only validated the activate;
    // it ShutdownObject'd its probe IMFTransform immediately. This re-activation creates a
    // new, thread-local IMFTransform with no cross-thread COM history (phase0v3 H-AV3 fix).
    // SAFETY: ActivateObject is the documented way to instantiate an IMFTransform from
    // an IMFActivate obtained via MFTEnumEx. The activate is an MTA-registered factory;
    // both this thread and the caller thread are MTA — transfer is safe (see ComSend docs).
    let mft: IMFTransform = match unsafe { activate.ActivateObject() } {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("encoder thread ActivateObject failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    // Step 3: MF_TRANSFORM_ASYNC_UNLOCK — required before any other call on an async MFT.
    // Set here on the same thread that will use the MFT, NOT in the probe (the probe's
    // IMFTransform was ShutdownObject'd; this is a fresh activation).
    let attrs = match unsafe { mft.GetAttributes() } {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("encoder thread GetAttributes failed: 0x{:08X}", e.code().0);
            return;
        }
    };
    if let Err(e) = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) } {
        tracing::error!(
            "encoder thread MF_TRANSFORM_ASYNC_UNLOCK failed: 0x{:08X}",
            e.code().0
        );
        return;
    }

    // Steps 4–6 (via setup_mft): output-type negotiation, input type, streaming messages.
    // setup_mft calls try_setup_output_type at its start (same-thread — no cross-thread AV).
    if let Err(e) = setup_mft(&mft, &config) {
        tracing::error!("MFT setup failed: {e}");
        // MFShutdown for the thread's MF context is not needed here because
        // the caller thread's new() owns MFStartup/MFShutdown (step 2 and Drop).
        return;
    }
    tracing::debug!("setup_mft OK; entering pump_loop");

    // Step 5: Cast to ICodecAPI — done here on the encoder thread, after setup_mft.
    // SAFETY: IMFTransform for hardware video encoders implements ICodecAPI per Windows docs.
    let codec_api: ICodecAPI = match mft.cast() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("encoder thread ICodecAPI cast failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    let event_gen: IMFMediaEventGenerator = match mft.cast() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("IMFMediaEventGenerator cast failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    // Per OQ-NEW-1 resolution (DD5): detect Annex-B vs AVCC at first packet in
    // collect_output, not during init. The probe_output_format approach (submit
    // a 16×16 NV12 frame during setup) corrupted the MFT event pipeline on
    // hardware encoders (see explore #583 Bucket A diagnosis). Removed in Phase 4.
    let mut output_format_known: Option<bool> = None; // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB

    // Step 8: Pump loop. mft, codec_api, event_gen all owned by this thread.
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

    // Steps 9a–9e: Notify end of stream and release.
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    // mft, codec_api, event_gen, activate are dropped here (COM Release via Drop).
    // MFShutdown for the thread is NOT called here — the caller thread's Drop handles it.
    // CoUninitGuard calls CoUninitialize when this function returns.
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
/// `MF_TRANSFORM_ASYNC_UNLOCK` is set by the encoder thread in `run_encoder_thread`
/// BEFORE calling this function (using the fresh `IMFTransform` produced by
/// `ActivateObject` on the encoder thread). This function starts with output-type
/// negotiation (`try_setup_output_type`) — same-thread as the subsequent pump_loop,
/// so no cross-thread COM state corruption is possible (contrast with commit ccd2e43
/// where the output type was set on the caller thread and the pump ran on the encoder
/// thread, triggering H-AV2 in the phase0v3 trace).
fn setup_mft(mft: &IMFTransform, config: &EncoderConfig) -> Result<(), EncoderError> {
    // Sentinel-zero triggers 1920×1080 fallback per DD3; production callers supply
    // real dimensions via EncoderConfig.width / EncoderConfig.height.
    // See effective_dimensions() for the fallback policy.
    let (w, h) = effective_dimensions(config);

    // Step 7b: SetOutputType (output-type negotiation — SAME THREAD as pump_loop).
    // WHY this is here (and not on the caller thread): the probe in probe_and_select_mft
    // called try_setup_output_type and then IMMEDIATELY ShutdownObject'd the IMFTransform.
    // This is a fresh IMFTransform (new ActivateObject call in run_encoder_thread); its
    // output type must be negotiated here, on the thread that will drive the pump_loop.
    // This is the ONLY SetOutputType call for this IMFTransform — no double-negotiation.
    try_setup_output_type(mft, w, h, config.framerate, config.bitrate_bps)?;

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

    // Steps 7f–7h: Send streaming messages.
    // (Async unlock was set by run_encoder_thread before calling this function — not repeated here.)
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

/// Snapshot of pending codec settings read atomically from [`MftEncoderShared`]
/// before a `ProcessInput` call (DD1 SWAP step).
///
/// The struct is `Copy` so callers can pass it by value to `fire_pending_codec_settings`
/// and `restore_pending_codec` without lifetime concerns.
#[derive(Copy, Clone)]
struct CodecApiSwap {
    /// `true` when `request_keyframe()` was pending at SWAP time.
    force_keyframe: bool,
    /// `Some(bps)` when a `set_bitrate(bps)` was pending at SWAP time; `None` otherwise.
    new_bitrate: Option<u32>,
}

/// SWAP step (DD1): read pending codec atomics BEFORE `ProcessInput`.
///
/// Clears `keyframe_pending` and `pending_bitrate` via `swap` so that each
/// pending request is consumed exactly once. The caller must call
/// [`fire_pending_codec_settings`] AFTER `ProcessInput` returns `Ok(())`.
fn swap_pending_codec_settings(state: &MftEncoderShared) -> CodecApiSwap {
    let force_keyframe = state.keyframe_pending.swap(false, Ordering::AcqRel);
    let raw_bps = state.pending_bitrate.swap(0, Ordering::AcqRel);
    let new_bitrate = if raw_bps != 0 { Some(raw_bps) } else { None };
    tracing::trace!(
        target: "sm_infra::encode::windows_mft",
        force_keyframe,
        new_bitrate = ?new_bitrate,
        "pump_loop: swap captured force_keyframe={} new_bitrate={:?}",
        force_keyframe,
        new_bitrate
    );
    CodecApiSwap {
        force_keyframe,
        new_bitrate,
    }
}

/// FIRE step (DD1): invoke `ICodecAPI::SetValue` calls AFTER `ProcessInput` succeeds.
///
/// Order: bitrate first, then keyframe — matches original `apply_pending_codec_settings`
/// ordering. Bitrate rejection is non-fatal (warn + continue). Keyframe SetValue failure
/// is also non-fatal — `MFSampleExtension_CleanPoint` is the authoritative IDR trigger
/// for NVENC (DD2).
fn fire_pending_codec_settings(codec_api: &ICodecAPI, swap: &CodecApiSwap) {
    // Bitrate first (matches original ordering).
    if let Some(bps) = swap.new_bitrate {
        let v = make_variant_u32(bps);
        unsafe {
            if let Err(e) = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) {
                // Non-fatal — driver rejection is acceptable (DD13).
                tracing::warn!(
                    target: "sm_infra::encode::windows_mft",
                    "ICodecAPI::SetValue(bitrate) rejected: 0x{:08X}",
                    e.code().0
                );
            }
        }
    }
    // Keyframe second.
    if swap.force_keyframe {
        // SAFETY: VARIANT constructed with VT_BOOL type per CODECAPI contract.
        let v = make_variant_bool(true);
        unsafe {
            let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
        }
    }
    tracing::trace!(
        target: "sm_infra::encode::windows_mft",
        "pump_loop: fire applied"
    );
}

/// RESTORE step (DD3): re-arm pending codec atomics on early-return paths where
/// `ProcessInput` was NOT called (frame dropped, timeout, disconnect-without-drain).
///
/// - `keyframe_pending`: unconditional `store(true, Release)` — idempotent re-arm.
/// - `pending_bitrate`: `compare_exchange(0, bps, AcqRel, Acquire)` — only restores
///   if the slot is still empty (no newer `set_bitrate` call); preserves last-write-wins
///   semantics (R6).
fn restore_pending_codec(state: &MftEncoderShared, swap: &CodecApiSwap) {
    if swap.force_keyframe {
        state.keyframe_pending.store(true, Ordering::Release);
    }
    if let Some(bps) = swap.new_bitrate {
        // Only restore if no newer set_bitrate() overwrote the slot.
        let _ = state
            .pending_bitrate
            .compare_exchange(0, bps, Ordering::AcqRel, Ordering::Acquire);
    }
    tracing::trace!(
        target: "sm_infra::encode::windows_mft",
        "pump_loop: restore_pending_codec on early-return"
    );
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
    // Configured frame dimensions (sentinel-zero → fallback). Frames whose dimensions
    // don't match are DROPPED at submission time: NVENC's MFT trusts the input buffer
    // size to match the negotiated output frame size, and a smaller-than-expected NV12
    // buffer causes an out-of-bounds read inside the driver → 0xC0000005 AV. The guard
    // is defensive — well-behaved producers send matching dims.
    let (cfg_w, cfg_h) = effective_dimensions(config);

    // Dual-arm counters tracking pending MFT credits (spec R2, design DD1/DD2).
    // Stack-local — no atomics needed; only the pump thread reads/writes these.
    let mut ni_count: u32 = 0; // pending METransformNeedInput credits
    let mut ho_count: u32 = 0; // pending METransformHaveOutput credits

    // F1 drain-state guard (DD14, R16): tracks whether the MFT is currently between
    // MFT_MESSAGE_COMMAND_DRAIN and the corresponding METransformDrainComplete event.
    // Stack-local — sole owner is pump_loop; no struct field needed (DD9 preserved).
    // SET on every ProcessMessage(COMMAND_DRAIN) call; CLEAR on METransformDrainComplete.
    // GUARD at top of `while ni_count > 0` loop — MUST precede SWAP (GUARD-BEFORE-SWAP).
    let mut draining = false;

    // PHASE 0 EMPIRICAL: tracks whether GOPSize needs restoring after the next ProcessInput.
    // Set when force_idr_gop_pending is consumed; cleared after the next successful submit.
    let mut gop_restore_needed = false;
    // Saved GOP size for restore after GOP-toggle IDR attempt. Default 30 (30fps).
    let mut saved_gop_size: u32 = 30;

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
                    // DD14 CLEAR: drain window is closed; NeedInput credits are safe to service again.
                    draining = false;
                    tracing::trace!(
                        target: "sm_infra::encode::windows_mft",
                        "draining = false (DrainComplete)"
                    );
                    // F2 (Mode 3, R16/DD17): wake MFT for new input. Intel QSV does NOT auto-emit
                    // NeedInput post-DrainComplete; explicit BEGIN_STREAMING + START_OF_STREAM wakes
                    // it up. This lets flush+continue cadence work (test cadence + production
                    // long-running streams that flush periodically).
                    unsafe {
                        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                    }
                    tracing::trace!(
                        target: "sm_infra::encode::windows_mft",
                        "post-drain: BEGIN_STREAMING + START_OF_STREAM sent — MFT resumed"
                    );
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
            match collect_output(
                mft,
                output_format_known,
                current_ts,
                &mut seq,
                cfg_w,
                cfg_h,
                config.framerate,
                config.bitrate_bps,
            ) {
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
                    if reason.contains("renegotiate") {
                        tracing::error!("pump_loop: renegotiation failed: {e}");
                        return;
                    } else if reason.contains("ProcessOutput: 0x80004005") {
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
            // DD14 F1 GUARD (R16): if the MFT is between COMMAND_DRAIN and DrainComplete,
            // discard accumulated NeedInput credits — ProcessInput returns MF_E_NOTACCEPTING
            // during this window. The MFT re-emits NeedInput after DrainComplete per the
            // Microsoft MFT contract. GUARD MUST precede SWAP (GUARD-BEFORE-SWAP ordering):
            // atomics remain armed for the first post-DrainComplete NeedInput credit.
            if draining {
                tracing::trace!(
                    target: "sm_infra::encode::windows_mft",
                    "pump_loop: skipping NeedInput credits during drain"
                );
                ni_count = 0;
                break;
            }

            // DD1 SWAP: read pending codec atomics BEFORE ProcessInput.
            let swap = swap_pending_codec_settings(state);

            // WHY: FRAME_RECV_TIMEOUT ≤50ms is load-bearing for T-NEW-2
            // (`mft_stop_during_active_encode_returns_within_deadline`). When stop()
            // is called during active encode, the loop exits this wait within 50ms
            // and reaches the top-of-loop stop check. Option A (Phase 1 user decision).
            // See spec OQ-5 + design DD7. DO NOT increase beyond 50ms.
            match rx.recv_timeout(FRAME_RECV_TIMEOUT) {
                Ok(frame) => {
                    if frame.width != cfg_w || frame.height != cfg_h {
                        tracing::warn!(
                            "pump_loop: frame dim mismatch — configured {}x{}, got {}x{}; dropping frame to avoid NVENC driver AV",
                            cfg_w,
                            cfg_h,
                            frame.width,
                            frame.height
                        );
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                        // Do NOT consume the NeedInput credit — break out so the next
                        // poll iteration re-evaluates and we don't busy-loop on bad input.
                        // Restore both codec atomics (DD3) so the next submitted frame
                        // still carries the IDR hint and pending bitrate change.
                        restore_pending_codec(state, &swap);
                        break;
                    }
                    current_ts = frame.timestamp;
                    nv12_convert(&frame, &mut nv12_scratch);
                    match submit_frame(
                        mft,
                        &nv12_scratch,
                        frame.timestamp,
                        frame_dur_100ns,
                        swap.force_keyframe,
                    ) {
                        Ok(()) => {
                            // Decrement AFTER successful ProcessInput (spec OQ-1, design DD2).
                            ni_count -= 1;
                            // DD1 FIRE: ICodecAPI::SetValue AFTER ProcessInput — eliminates the
                            // race window where QSV withdraws NeedInput readiness (Mode 1 fix).
                            fire_pending_codec_settings(codec_api, &swap);
                            // PHASE 0 EMPIRICAL — Mechanism A restore: after the first successful
                            // ProcessInput following a GOP=1 toggle, restore original GOP size.
                            if gop_restore_needed {
                                let gop_orig = make_variant_u32(saved_gop_size);
                                unsafe {
                                    let _ =
                                        codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop_orig);
                                }
                                gop_restore_needed = false;
                                tracing::info!(
                                    "pump_loop: GOP size restored to {saved_gop_size} after force_idr_via_gop_toggle"
                                );
                            }
                        }
                        Err(e) => {
                            // Check for MF_E_NOTACCEPTING — indicates counter desync (design DD5).
                            let reason = e.to_string();
                            if reason.contains("ProcessInput: 0xC00D36B5") {
                                // MF_E_NOTACCEPTING should NEVER happen when counters are correct
                                // AND the drain-state guard (DD14) is active. Under DD1+DD14 this
                                // assert is sound for both Mode 1 (codec_api race) and Mode 2
                                // (flush+continue ProcessInput) — both are eliminated by this commit.
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
                    // Restore both codec atomics (DD3) so the next submitted frame carries
                    // the pending IDR hint and bitrate change.
                    restore_pending_codec(state, &swap);
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Upstream closed — drain MFT and continue looping (do NOT consume credit).
                    // Restore both codec atomics (DD3) — encoder shutting down, but be consistent.
                    tracing::info!("pump_loop: frame channel disconnected, sending COMMAND_DRAIN");
                    restore_pending_codec(state, &swap);
                    unsafe {
                        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                    }
                    // DD14 SET site #2: mark drain window open BEFORE break.
                    draining = true;
                    tracing::trace!(
                        target: "sm_infra::encode::windows_mft",
                        "draining = true (disconnect)"
                    );
                    break;
                }
            }
        }

        // DD3/DD4: consume explicit flush() signal — fires exactly one COMMAND_DRAIN per flag-set.
        // swap(false, AcqRel) resets atomically; subsequent flush() after DrainComplete re-arms.
        // WHY: explicit flush() vs disconnect-DRAIN — fires DRAIN exactly once via swap.
        if state.drain_pending.swap(false, Ordering::AcqRel) {
            tracing::info!("pump_loop: explicit flush() — sending COMMAND_DRAIN");
            unsafe {
                let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            }
            // DD14 SET site #1: mark drain window open (explicit flush path).
            draining = true;
            tracing::trace!(
                target: "sm_infra::encode::windows_mft",
                "draining = true (explicit flush)"
            );
        }

        // PHASE 0 EMPIRICAL — Mechanism C-prime: consume flush_state() signal.
        // Sends FLUSH+BEGIN_STREAMING+START_OF_STREAM — the same 3-message sequence as
        // setup_mft (lines 911-922). FLUSH is synchronous (no DrainComplete follows).
        // NOT setting draining=true because FLUSH has no async completion event.
        // Counters reset because FLUSH discards all internal encoder state.
        if state.flush_state_pending.swap(false, Ordering::AcqRel) {
            tracing::info!(
                "pump_loop: flush_state() — sending COMMAND_FLUSH + BEGIN_STREAMING + START_OF_STREAM"
            );
            unsafe {
                let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }
            // FLUSH discards all in-flight state; reset counters to match fresh state.
            ni_count = 0;
            ho_count = 0;
            tracing::trace!(
                target: "sm_infra::encode::windows_mft",
                "flush_state: COMMAND_FLUSH + BEGIN_STREAMING + START_OF_STREAM sent; counters reset"
            );
        }

        // PHASE 0 EMPIRICAL — Mechanism A: consume force_idr_via_gop_toggle() signal.
        // Sets CODECAPI_AVEncMPVGOPSize=1 so the next ProcessInput submits a GOP head (= IDR).
        // Saves the prior GOP size; restore is done after the NEXT successful ProcessInput
        // (gop_restore_needed flag, checked inside the NI loop on the following iteration).
        if state.force_idr_gop_pending.swap(false, Ordering::AcqRel) && !gop_restore_needed {
            tracing::info!(
                "pump_loop: force_idr_via_gop_toggle() — reading current GOP size, setting GOP=1"
            );
            // Try to read the current GOP size for restore; fall back to framerate default.
            // SAFETY: GetValue returns a VARIANT; we read the VT_UI4 (ulVal) arm.
            saved_gop_size = unsafe {
                codec_api
                    .GetValue(&CODECAPI_AVEncMPVGOPSize)
                    .ok()
                    .map(|v| (*v.Anonymous.Anonymous).Anonymous.ulVal)
                    .unwrap_or(if config.framerate > 0 {
                        config.framerate
                    } else {
                        30
                    })
            };
            let gop1 = make_variant_u32(1);
            unsafe {
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop1);
            }
            gop_restore_needed = true;
            tracing::trace!(
                target: "sm_infra::encode::windows_mft",
                "force_idr_gop: GOP=1 set; saved_gop_size={saved_gop_size}; restore pending after next ProcessInput"
            );
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
///
/// When `force_keyframe` is `true`, sets `MFSampleExtension_CleanPoint = 1` on
/// the input sample. This is the Microsoft-documented mechanism for requesting
/// an IDR — required for NVIDIA NVENC, which silently ignores the alternative
/// `CODECAPI_AVEncVideoForceKeyFrame` path.
fn submit_frame(
    mft: &IMFTransform,
    nv12: &crate::encode::bgra_to_nv12::Nv12,
    timestamp: std::time::Duration,
    duration_100ns: i64,
    force_keyframe: bool,
) -> Result<(), EncoderError> {
    let sample = build_imfsample(&nv12.buf, timestamp, duration_100ns)?;
    if force_keyframe {
        // SAFETY: SetUINT32 on a valid IMFSample is safe; CleanPoint is a documented
        // attribute on the IMFAttributes interface that IMFSample inherits.
        unsafe {
            sample
                .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                .map_err(|e| {
                    EncoderError::EncodeFailed(format!(
                        "SetUINT32(CleanPoint): 0x{:08X}",
                        e.code().0
                    ))
                })?;
        }
    }
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
#[allow(clippy::too_many_arguments)] // WHY: DD3 passes 4 config scalars directly; &EncoderConfig rejected (sm-domain frozen, avoids coupling)
fn collect_output(
    mft: &IMFTransform,
    output_format_known: &mut Option<bool>, // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB
    frame_timestamp: std::time::Duration,
    seq: &mut u64,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<Option<EncodedPacket>, EncoderError> {
    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
    let mut status: u32 = 0;

    match unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) } {
        Ok(()) => {
            tracing::trace!(dw_status = output.dwStatus, status, "ProcessOutput Ok");
        }
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
            tracing::trace!(
                dw_status = output.dwStatus,
                status,
                hr = format!("0x{:08X}", e.code().0).as_str(),
                "ProcessOutput NEED_MORE_INPUT"
            );
            return Ok(None);
        }
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
            tracing::trace!(
                dw_status = output.dwStatus,
                status,
                hr = format!("0x{:08X}", e.code().0).as_str(),
                "ProcessOutput STREAM_CHANGE — renegotiating"
            );
            *output_format_known = None;
            renegotiate_output_type(mft, w, h, framerate, bitrate_bps)?;
            return Ok(None);
        }
        Err(e) => {
            tracing::trace!(
                dw_status = output.dwStatus,
                status,
                hr = format!("0x{:08X}", e.code().0).as_str(),
                "ProcessOutput Err"
            );
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
    // Fallback: some vendor MFTs emit IDR access units without setting CleanPoint,
    // so we also scan the Annex-B bitstream for an IDR NAL (type 5) as authoritative.
    // SAFETY: GetUINT32 on a valid IMFSample is always safe.
    let clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) } != 0;
    let is_keyframe = clean_point || annex_b_contains_idr(&annex_b);

    let pkt = EncodedPacket {
        data: Arc::from(annex_b.into_boxed_slice()),
        is_keyframe,
        timestamp: frame_timestamp,
        sequence: *seq,
    };
    *seq += 1;

    Ok(Some(pkt))
}

/// Scan an Annex-B bitstream for an IDR NAL unit (type 5).
///
/// Used as a fallback when `MFSampleExtension_CleanPoint` is absent — some vendor
/// MFTs (e.g. NVIDIA NVENC) emit forced IDR samples without setting that attribute.
/// The bitstream is authoritative: H.264 emulation prevention guarantees that real
/// start codes never appear inside NAL payloads, so a byte scan is correct.
fn annex_b_contains_idr(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if data[i + 2] == 0x01 {
                if data[i + 3] & 0x1F == 5 {
                    return true;
                }
                i += 4;
                continue;
            }
            if data[i + 2] == 0x00 && i + 4 < data.len() && data[i + 3] == 0x01 {
                if data[i + 4] & 0x1F == 5 {
                    return true;
                }
                i += 5;
                continue;
            }
        }
        i += 1;
    }
    false
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

// ── Inherent methods ──────────────────────────────────────────────────────────

impl WindowsMftH264Encoder {
    /// Signal end-of-burst to the encoder pump loop, requesting a `MFT_MESSAGE_COMMAND_DRAIN`.
    ///
    /// **Single-shot**: After `COMMAND_DRAIN` fires, Intel QSV enters a non-accepting state
    /// until `METransformDrainComplete` fires (~250 ms empirically, Phase 0 trace #710). The
    /// encoder is effectively terminal per session on Intel QSV — do not call `flush()` mid-stream.
    ///
    /// **Async**: Returns immediately. The DRAIN fires on the next pump_loop iteration after the
    /// NeedInput inner loop completes. Failures (e.g. vendor error) surface via `pkt_rx.recv_timeout`.
    ///
    /// **Concurrency-safe**: Backed by `Arc<AtomicBool>`. Multiple concurrent calls collapse to at
    /// most one `COMMAND_DRAIN` per pump iteration (last `store(true)` wins; swap-once consumption).
    ///
    /// **Latency**: Empirically ~250 ms on Intel QSV (Host A, Phase 0 trace #710). Plan recv_timeout
    /// deadlines accordingly (spec S11.x uses 3–5 s — sufficient margin).
    ///
    /// **Production callers MUST NOT call this method.** It is a test affordance for single-burst
    /// short-stream tests. Production callers rely on the channel-disconnect DRAIN path at
    /// pump_loop shutdown.
    pub fn flush(&self) {
        self.state.drain_pending.store(true, Ordering::Release);
    }

    /// PHASE 0 EMPIRICAL: triggers `MFT_MESSAGE_COMMAND_FLUSH` + `BEGIN_STREAMING` +
    /// `START_OF_STREAM` on the next pump_loop iteration.
    ///
    /// Unlike `flush()` (which sends `COMMAND_DRAIN` and waits for async `DrainComplete`),
    /// `COMMAND_FLUSH` is SYNCHRONOUS — it discards all internal encoder state immediately.
    /// No `METransformDrainComplete` event follows. The encoder is in the same state as
    /// just before `BEGIN_STREAMING` was first sent (matching `setup_mft` at lines 911-922).
    ///
    /// **Hypothesis (Mechanism C-prime)**: if `COMMAND_FLUSH` is the missing piece that
    /// `setup_mft` sends but `DrainComplete/F2` does not, then the first frame after this
    /// sequence should be IDR on Intel QSV. If the C-prime probe PASSES, this method
    /// becomes the production mechanism for `request_keyframe()`. Otherwise it is reverted.
    ///
    /// **Probe**: `phase0_intel_qsv_idr_via_flush_resume_first_frame_is_idr` in
    /// `crates/sm-infra/tests/windows_mft_encode.rs`.
    pub fn flush_state(&self) {
        self.state
            .flush_state_pending
            .store(true, Ordering::Release);
    }

    /// PHASE 0 EMPIRICAL: forces the next frame to be IDR via `CODECAPI_AVEncMPVGOPSize=1` toggle.
    ///
    /// When consumed by pump_loop: saves the current GOP size (or uses `config.framerate`
    /// as default), sets `CODECAPI_AVEncMPVGOPSize=1` (every frame becomes a GOP head = IDR),
    /// submits the next frame normally, then restores the original GOP size.
    ///
    /// **Hypothesis (Mechanism A)**: per Intel MSDK docs, GOP=1 forces every frame to be
    /// IDR. Microsoft warns "before recording only" but that is unverified empirically.
    /// If the A probe PASSES, this becomes an alternative production mechanism.
    /// Otherwise it is reverted.
    ///
    /// **Probe**: `phase0_intel_qsv_idr_via_gop_toggle_first_frame_is_idr` in
    /// `crates/sm-infra/tests/windows_mft_encode.rs`.
    pub fn force_idr_via_gop_toggle(&self) {
        self.state
            .force_idr_gop_pending
            .store(true, Ordering::Release);
    }
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
    /// `winning_activate` is `None`, so any call to `start()` will return
    /// `Err(Internal(_))` rather than accessing an invalid COM pointer.
    fn new_for_validation_test() -> Self {
        Self {
            config: EncoderConfig::default(),
            state: Arc::new(MftEncoderShared::default()),
            winning_activate: None,
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

    // ─── annex_b_contains_idr — fallback for vendor MFTs without CleanPoint ────

    #[test]
    fn annex_b_contains_idr_detects_idr_with_4byte_start_code() {
        // 4-byte start code + NAL byte 0x65 (forbidden_zero=0, nal_ref_idc=3, type=5).
        let data = vec![0x00u8, 0x00, 0x00, 0x01, 0x65, 0xAB, 0xCD];
        assert!(annex_b_contains_idr(&data));
    }

    #[test]
    fn annex_b_contains_idr_detects_idr_with_3byte_start_code() {
        // 3-byte start code + NAL byte 0x25 (nal_ref_idc=1, type=5).
        let data = vec![0x00u8, 0x00, 0x01, 0x25, 0xAB];
        assert!(annex_b_contains_idr(&data));
    }

    #[test]
    fn annex_b_contains_idr_detects_idr_after_sps_pps_prefix() {
        // SPS (type 7) + PPS (type 8) + IDR slice (type 5) — typical IDR access unit.
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x00, // PPS (type 8)
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, // IDR slice (type 5)
        ];
        assert!(annex_b_contains_idr(&data));
    }

    #[test]
    fn annex_b_contains_idr_returns_false_for_p_frame_only() {
        // Only NAL type 1 (non-IDR slice).
        let data = vec![0x00u8, 0x00, 0x00, 0x01, 0x41, 0xAB, 0xCD, 0xEF];
        assert!(!annex_b_contains_idr(&data));
    }

    #[test]
    fn annex_b_contains_idr_returns_false_for_too_short_input() {
        // Below minimum to hold a start code + NAL byte.
        assert!(!annex_b_contains_idr(&[]));
        assert!(!annex_b_contains_idr(&[0x00, 0x00, 0x01]));
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
