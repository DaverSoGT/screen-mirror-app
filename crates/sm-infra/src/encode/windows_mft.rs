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
//! - `new()`: validates config, spawns a short-lived `"sm-mft-probe"` OS thread that
//!   calls `CoInitializeEx(MTA)` + `MFStartup` + `MFTEnumEx` + per-candidate
//!   `ActivateObject` + `try_setup_output_type` + `ShutdownObject`, then
//!   `MFShutdown` + `CoUninitialize` before the thread exits. The winning
//!   `IMFActivate` is sent back to the caller via a channel. The calling thread
//!   never touches COM or MF during construction.
//! - `start(rx, tx)`: transfers the `IMFActivate` to a spawned OS thread. The
//!   encoder thread calls `ActivateObject` itself and owns the `IMFTransform`
//!   entirely — no cross-thread COM transfer of `IMFTransform` or `ICodecAPI`.
//! - `stop()`: idempotent. Sets stop flag and joins the handle.
//! - `Drop`: calls `stop()` and releases `IMFActivate`. MF/COM teardown happens on the probe thread during `new()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame,
    ICodecAPI,
    IMFActivate,
    IMFMediaEventGenerator,
    IMFMediaType,
    IMFTransform,
    MEEndOfStream,
    METransformDrainComplete,
    METransformHaveOutput,
    METransformNeedInput,
    MF_E_NO_EVENTS_AVAILABLE,
    MF_E_SHUTDOWN,
    MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT,
    MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK,
    MF_VERSION,
    MFCreateMediaType,
    MFCreateMemoryBuffer,
    MFCreateSample,
    MFMediaType_Video,
    MFSTARTUP_FULL,
    // MFSampleExtension_CleanPoint: READ path in collect_output for IDR detection
    // (defense-in-depth alongside annex_b_contains_idr byte scanning). DD7: the write
    // path (CleanPoint=1 on input sample) was deleted — falsified by P1 probe (#807).
    MFSampleExtension_CleanPoint,
    MFShutdown,
    MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO,
    MFT_TRANSFORM_CLSID_Attribute,
    MFTEnumEx,
    MFVideoFormat_H264,
    MFVideoFormat_NV12,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::{Interface, PWSTR};

use sm_domain::encode::{EncodedPacket, EncoderConfig, EncoderError, VideoEncoder};

// Shared observability seam — cadence predicate defined in capture::windows (no hw-encoder
// gate) so both the encode and capture production gates call the same tested function.
use crate::capture::interval_elapsed;

// ── FramePayload dispatch seam (PR-2) ─────────────────────────────────────────
//
// The capture→encoder channel still carries `CaptureFrame` at the public
// VideoEncoder::start() boundary (sm-domain trait is frozen). Inside pump_loop
// we convert each received frame to `FramePayload` and match on the variant.
// This introduces the routing seam without changing the external API.
//
// In PR-2 the capture side ONLY produces `Cpu` frames; the `GpuShared` arm
// is a `todo!()` stub that is safe because no such variant can be constructed
// from production code in this PR.
use crate::encode::frame_payload::FramePayload;

// ── A1: GOP size cap ──────────────────────────────────────────────────────────

/// GOP size cap sent to the hardware encoder via `CODECAPI_AVEncMPVGOPSize`.
///
/// At 30 fps this is a 2-second keyframe interval. See design Decision (a).
/// Value satisfies `GOP_SIZE_A1 <= 60` as required by REQ-A1-1.
// verified present in windows 0.62.2 (imported from Win32_Media_MediaFoundation)
const GOP_SIZE_FRAMES: u32 = 60;

// ── COM interface thread-transfer wrapper ─────────────────────────────────────

/// Newtype that marks a COM interface as `Send` for the purpose of transferring
/// ownership from the constructor thread to the encoder thread.
///
/// # Safety
/// COM interfaces are apartment-threaded by default, but hardware MFTs are
/// registered as free-threaded (MTA). The probe thread (spawned by `new()`) and
/// the encoder thread each call `CoInitializeEx(COINIT_MULTITHREADED)` to join
/// the MTA independently. The calling thread (`new()` itself) never calls
/// `CoInitializeEx`, so cross-thread transfer of MTA-registered factory pointers
/// is safe per Windows COM rules. Never use this wrapper for STA-registered objects.
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

// ── Encoder vendor identity ───────────────────────────────────────────────────

/// Identity of the winning hardware encoder MFT, detected at probe time.
///
/// Determined by matching the MFT CLSID obtained from `MFT_TRANSFORM_CLSID_Attribute`
/// during `probe_and_select_mft`. Retained for INFO/WARN diagnostic logging only —
/// does NOT drive IDR mechanism dispatch (DD5, Slice 6 R2).
///
/// Mid-stream IDR is vendor-uniform via `CODECAPI_AVEncVideoForceKeyFrame` (VT_UI4=1)
/// called BEFORE `ProcessInput`. See P2 evidence #809.
///
/// See explore #803 and design DD5.
///
/// `pub(crate)` so [`crate::encode::path_select`] can consume it in the
/// path-selection gate without widening the public API (PR-2 seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncoderVendor {
    /// Intel Quick Sync Video H.264 MFT — CLSID `{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}`.
    IntelQsv,
    /// NVIDIA NVENC H.264 MFT — CLSID `{60F44560-5A20-4857-BFEF-D29773CB8040}`.
    NvidiaNvenc,
    /// AMD H.264 MFT (`AMDh264Encoder`).
    ///
    /// Detected via the PCI vendor-ID attribute `VEN_1002` rather than a stable
    /// CLSID prefix. AMD has historically rotated its MFT CLSID across driver
    /// versions (no rigorous public source documents a stable AMD H.264 CLSID
    /// as of 2026-05), so the canonical detection path is the PCI vendor-ID
    /// rather than the CLSID. For AMF SDK context see
    /// <https://gpuopen.com/advanced-media-framework/>.
    Amd,
    /// Any other vendor (fallback). Treated as IntelQsv for mechanism routing.
    Unknown,
}

impl EncoderVendor {
    /// Classify the MFT vendor from the CLSID and optional PCI vendor-ID.
    ///
    /// Priority:
    /// 1. **CLSID exact-prefix match** (NVENC `{60F44560-`, Intel QSV `{4BE8D3C0-`).
    /// 2. **PCI vendor-ID prefix match** (`VEN_10DE` NVIDIA, `VEN_8086` Intel,
    ///    `VEN_1002` AMD) — rotation-insurance fallback.
    /// 3. `Unknown` when neither signal resolves.
    ///
    /// # CLSID wins on disagreement
    ///
    /// If the CLSID matches a known prefix but the vendor-ID disagrees, the CLSID
    /// is authoritative. The CLSID is obtained via `GetGUID` (a structured 16-byte
    /// value), is registered per-vendor in the Windows COM registry, and is what
    /// `MFTEnumEx` keys on to activate the transform. Disagreement implies either
    /// a Windows misregistration bug or a forked/repackaged MFT — in either case
    /// the more specific signal (CLSID) is trusted. The vendor-ID is a fallback
    /// for the case where the CLSID has rotated AWAY from any prefix we know
    /// about, not a tie-breaker.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // CLSID exact match — NVENC.
    /// assert_eq!(
    ///     EncoderVendor::detect("{60F44560-5033-...}", None),
    ///     EncoderVendor::NvidiaNvenc,
    /// );
    ///
    /// // CLSID unknown, vendor-ID resolves AMD.
    /// assert_eq!(
    ///     EncoderVendor::detect("(no CLSID)", Some("VEN_1002")),
    ///     EncoderVendor::Amd,
    /// );
    ///
    /// // CLSID unknown, vendor-ID disagrees with no known prefix.
    /// assert_eq!(
    ///     EncoderVendor::detect("(no CLSID)", Some("VEN_FFFF")),
    ///     EncoderVendor::Unknown,
    /// );
    /// ```
    pub(crate) fn detect(clsid: &str, vendor_id: Option<&str>) -> Self {
        // Stage 1: CLSID exact-prefix match (authoritative).
        // NVENC: {60F44560-5A20-4857-BFEF-D29773CB8040} — confirmed in C0/C0.b probe logs (Slice 6).
        if clsid.starts_with("{60F44560-") {
            return Self::NvidiaNvenc;
        }
        // Intel QSV: {4BE8D3C0-0515-4A37-AD55-E4BAE19AF471} — from Slice 4 archive explore.
        if clsid.starts_with("{4BE8D3C0-") {
            return Self::IntelQsv;
        }

        // Stage 2: vendor-ID prefix fallback (rotation-insurance).
        // starts_with used to tolerate trailing &DEV_xxxx suffixes (R1.4).
        if let Some(vid) = vendor_id {
            if vid.starts_with("VEN_10DE") {
                return Self::NvidiaNvenc;
            }
            if vid.starts_with("VEN_8086") {
                return Self::IntelQsv;
            }
            if vid.starts_with("VEN_1002") {
                return Self::Amd;
            }
        }

        Self::Unknown
    }
}

// ── Shared cross-thread state ─────────────────────────────────────────────────

/// Shared atomics between the caller and the encoder OS thread.
struct MftEncoderShared {
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
    /// Set by `request_keyframe_via_force_keyframe_icodecapi()` (Slice 6 R2).
    ///
    /// Consumed by pump_loop in the NeedInput service path: BEFORE calling `submit_frame()`,
    /// swap(false, AcqRel) to consume the flag; if true, call
    /// `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` on the encoder's
    /// ICodecAPI interface.
    ///
    /// WHY BEFORE ProcessInput: Per Chromium `media_foundation_video_encode_accelerator_win.cc`
    /// lines 2299-2307 and FFmpeg `libavcodec/mfenc.c::mf_send_frame()`, the canonical
    /// production sequence calls SetValue(CODECAPI_AVEncVideoForceKeyFrame) BEFORE the
    /// ProcessInput call for the target frame. The Slice 4 SWAP-FIRE pattern used AFTER
    /// ProcessInput — that is the known-wrong timing this probe corrects.
    ///
    /// VT_UI4 (not VT_BOOL): per research #808, the correct VARIANT type is VT_UI4=1.
    /// Slice 4 used VT_BOOL, which may also have contributed to falsification on Intel QSV.
    ///
    /// CODECAPI_AVEncVideoForceKeyFrame is a REQUIRED HCK Win8+ certification property for
    /// hardware encoder MFTs (Microsoft docs). NVENC MUST implement it to be certified.
    ///
    /// The property auto-resets to 0 after the next ProcessInput per MS docs — no manual
    /// cleanup needed after the call.
    ///
    /// SetValue rejection is non-fatal (warn + continue) per DD13 convention.
    // allow: force_keyframe_icodecapi_pending only read by pump_loop under hw-encoder feature
    #[allow(dead_code)]
    force_keyframe_icodecapi_pending: AtomicBool,
}

impl Default for MftEncoderShared {
    fn default() -> Self {
        Self {
            pending_bitrate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            drain_pending: AtomicBool::new(false),
            force_keyframe_icodecapi_pending: AtomicBool::new(false),
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
    /// Vendor identity of the winning MFT, detected during `new()` via CLSID matching.
    ///
    /// Read by `backend_name()` to return the canonical backend token (`"hw_nvenc"`,
    /// `"hw_intel_qsv"`, or `"hw_unknown"`). Also available for future diagnostic use
    /// (vendor-specific quirk flags, metrics). See `EncoderVendor`.
    vendor: EncoderVendor,
    /// The winning `IMFActivate` selected during the destructive probe in `new()`.
    ///
    /// `IMFActivate` is an MTA-registered COM factory pointer; it is safe to transfer
    /// across MTA threads (see `ComSend` safety contract). Transferred to the encoder
    /// thread in `start()` via `ComSend`. The encoder thread calls `ActivateObject` on
    /// it to produce a fresh `IMFTransform` that lives ENTIRELY on that thread —
    /// no cross-thread COM for `IMFTransform` or `ICodecAPI` (root cause of AVs in
    /// commit ccd2e43, see sdd/.../phase0v3-final-root-cause).
    mft_activate_factory: Option<IMFActivate>,
    /// `Some` while the encoder thread is running; `None` before `start` and after `stop`.
    handle: Option<JoinHandle<()>>,
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
// `mft_activate_factory` (`Option<IMFActivate>`) is transferred to the encoder thread
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
    /// Spawns a short-lived `"sm-mft-probe"` OS thread that runs the MFT probe
    /// (`CoInitializeEx(MTA)` → `MFStartup` → `MFTEnumEx` → per-candidate
    /// `ActivateObject` / `try_setup_output_type` / `ShutdownObject` → `MFShutdown`
    /// → `CoUninitialize`). The calling thread never touches COM or MF, which avoids
    /// `RPC_E_CHANGED_MODE` when the caller is already STA-initialized (e.g. the Tauri
    /// main thread after WebView2 init).
    ///
    /// Returns:
    /// - `Err(InvalidConfig(_))` for `bitrate_bps == 0` or `framerate == 0`.
    /// - `Err(InitFailed(_))` if no hardware H.264 MFT candidate passes the probe,
    ///   if the probe thread times out (5 s), or if it panics.
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

        // Off-main-thread probe: the Tauri main thread is STA-initialized by tao/wry
        // for WebView2; calling CoInitializeEx(MTA) on it returns RPC_E_CHANGED_MODE
        // (0x80010106). Running the probe on a fresh thread avoids this entirely.
        let (activate, vendor) = run_probe_on_isolated_thread(config.clone())?;

        Ok(Self {
            config,
            state: Arc::new(MftEncoderShared::default()),
            vendor,
            mft_activate_factory: Some(activate),
            handle: None,
        })
    }

    fn start(
        &mut self,
        rx: Receiver<sm_domain::CaptureFrame>,
        tx: SyncSender<EncodedPacket>,
    ) -> Result<(), EncoderError> {
        let activate = self.mft_activate_factory.take().ok_or_else(|| {
            EncoderError::Internal("start() called after IMFActivate was already consumed".into())
        })?;
        let config = self.config.clone();
        let state = Arc::clone(&self.state);
        // Pass vendor so run_encoder_thread can call select_encode_path at init (PR-2 seam).
        let vendor = self.vendor;

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
            run_encoder_thread(activate_send.into_inner(), config, state, vendor, rx, tx);
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

    /// Request a forced mid-stream IDR frame.
    ///
    /// Arms `force_keyframe_icodecapi_pending`; pump_loop consumes the flag with
    /// `swap(false, AcqRel)` on the next NeedInput credit and calls
    /// `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` BEFORE
    /// `ProcessInput`.
    ///
    /// **Latency**: ~0ms on NVENC (IDR at idx 0); ~33ms on Intel QSV (IDR at idx 1,
    /// 1 in-flight frame latency at 30fps). Both within `assert_keyframe_within_next_n_frames(30)`
    /// tolerance. Vendor-uniform via `CODECAPI_AVEncVideoForceKeyFrame`; see Phase 0 P2
    /// evidence (engram #809), research finding (engram #808), Microsoft HCK property table
    /// for Win8+ hardware MFTs.
    fn request_keyframe(&self) {
        self.state
            .force_keyframe_icodecapi_pending
            .store(true, std::sync::atomic::Ordering::Release);
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

    fn backend_name(&self) -> &'static str {
        match self.vendor {
            EncoderVendor::NvidiaNvenc => "hw_nvenc",
            EncoderVendor::IntelQsv => "hw_intel_qsv",
            EncoderVendor::Amd => "hw_amd",
            EncoderVendor::Unknown => "hw_unknown",
        }
    }
}

impl Drop for WindowsMftH264Encoder {
    fn drop(&mut self) {
        // Join encoder thread first. If start() was never called this is a no-op.
        let _ = self.stop();

        // mft_activate_factory: if start() was never called, the IMFActivate is still here.
        // Release the COM ref. MF/COM teardown was already done by the probe thread
        // inside new(); IMFActivate::Release does not require an active MFStartup on
        // the calling thread (it only decrements a COM refcount).
        // If start() was called, mft_activate_factory is already None.
        drop(self.mft_activate_factory.take());
    }
}

// ── Synchronous MFT initialisation (design §5 steps 1–6) ─────────────────────

/// Probe-thread MFT enumeration. Returns the winning `IMFActivate` (not an `IMFTransform`).
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
fn init_mft_sync(config: &EncoderConfig) -> Result<(IMFActivate, EncoderVendor), EncoderError> {
    // Step 1: CoInitializeEx on the probe thread (MTA).
    // SAFETY: Paired with `CoUninitialize` in the probe closure's teardown.
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
        Ok((activate, vendor)) => Ok((activate, vendor)),
        Err(err) => {
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
            Err(err)
        }
    }
}

// ── Probe-thread isolation (REQ-MFT-1..REQ-MFT-8) ───────────────────────────

/// Wall-clock cap for the probe thread. ~25x the worst observed real-probe latency.
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `init_mft_sync` on a fresh OS thread so `CoInitializeEx(MTA)` succeeds even
/// when the caller's apartment is STA (Tauri main thread + WebView2).
///
/// Returns `(IMFActivate, EncoderVendor)` on success; maps any failure (probe error,
/// timeout, thread panic) into `EncoderError::InitFailed`.
fn run_probe_on_isolated_thread(
    config: EncoderConfig,
) -> Result<(IMFActivate, EncoderVendor), EncoderError> {
    run_probe_on_isolated_thread_with(PROBE_TIMEOUT, move || init_mft_sync(&config))
}

fn classify_probe_receive<T>(
    received: Result<Result<T, EncoderError>, std::sync::mpsc::RecvTimeoutError>,
    timeout: std::time::Duration,
) -> Result<T, EncoderError> {
    match received {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(error),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(EncoderError::InitFailed(format!(
            "probe thread timeout after {}s",
            timeout.as_secs()
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(EncoderError::InitFailed(
            "probe thread panicked before sending result".into(),
        )),
    }
}

/// Testability seam — same machinery as `run_probe_on_isolated_thread`, but the
/// probe body and timeout are injected. Tests use this to simulate panic, error,
/// and timeout without touching real Media Foundation.
///
/// `F` must produce `ComSend`-compatible values; `IMFActivate` is MTA-safe.
#[cfg_attr(not(test), allow(dead_code))]
fn run_probe_on_isolated_thread_with<F>(
    timeout: std::time::Duration,
    probe: F,
) -> Result<(IMFActivate, EncoderVendor), EncoderError>
where
    F: FnOnce() -> Result<(IMFActivate, EncoderVendor), EncoderError> + Send + 'static,
{
    use std::sync::mpsc;
    type ProbeOut = Result<(ComSend<IMFActivate>, EncoderVendor), EncoderError>;
    let (tx, rx) = mpsc::channel::<ProbeOut>();

    // Variant a: the probe closure owns the full Co/MF lifecycle on the spawned thread.
    // init_mft_sync already handles CoUninitialize on its error arms; calling
    // CoUninitialize again after a failed probe is a documented no-op per DD10
    // (apartment refcount was 0 on those paths). Zero correctness cost, avoids
    // refactoring every error arm of init_mft_sync.
    let _handle = std::thread::Builder::new()
        .name("sm-mft-probe".into())
        .spawn(move || {
            let co_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if co_hr.is_err() {
                let _ = tx.send(Err(EncoderError::InitFailed(format!(
                    "probe thread CoInitializeEx: 0x{:08X}",
                    co_hr.0
                ))));
                // Nothing to tear down — refcount stayed at 0.
                return;
            }
            // From here we must always run MFShutdown+CoUninitialize before exit.
            let result = probe().map(|(act, v)| (ComSend(act), v));
            let _ = tx.send(result);
            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
        })
        .map_err(|e| EncoderError::InitFailed(format!("probe thread spawn: {e}")))?;
    // detached not joined — see REQ-MFT-15

    classify_probe_receive(rx.recv_timeout(timeout), timeout)
        .map(|(activate_send, vendor)| (activate_send.into_inner(), vendor))
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

// ── Vendor-ID attribute extraction helper ────────────────────────────────────
/// Reads the optional `MFT_ENUM_HARDWARE_VENDOR_ID_Attribute` from an
/// `IMFActivate`, returning `None` if the attribute is absent, the string
/// is null, or UTF-16 decode fails. Encapsulates the `unsafe` COM dance
/// (GetAllocatedString + PWSTR::to_string + CoTaskMemFree) so the probe
/// loop stays readable.
fn read_vendor_id_attribute(activate: &IMFActivate) -> Option<String> {
    // SAFETY: GetAllocatedString writes a CoTaskMemAlloc'd PWSTR on Ok(())
    // which we MUST CoTaskMemFree exactly once. On Err the pointer is left
    // null (per windows-rs docs) so no free is needed on the failure path.
    unsafe {
        let mut pwstr = PWSTR::null();
        let mut cch: u32 = 0;
        match activate.GetAllocatedString(
            &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
            &mut pwstr,
            &mut cch,
        ) {
            Ok(()) if !pwstr.is_null() => {
                let s = pwstr.to_string().ok();
                CoTaskMemFree(Some(pwstr.0 as *const _));
                s
            }
            _ => None,
        }
    }
}

/// Reads the optional `MFT_FRIENDLY_NAME_Attribute` from an `IMFActivate`.
///
/// Returns `Some` whenever the attribute is present and the string pointer is
/// non-null — decoding to either the UTF-16 name or the sentinel
/// `"(utf16 error)"` on a UTF-16 decode failure.  Returns `None` only when
/// the attribute is absent, `GetAllocatedString` returns `Err`, or the
/// returned pointer is null.
///
/// Memory: `CoTaskMemFree` is called exactly once, on the success (`Ok(())` +
/// non-null) path.  No allocation is made on `Err`/null paths so no free is
/// needed there (per windows-rs docs).
fn read_friendly_name_attribute(activate: &IMFActivate) -> Option<String> {
    // SAFETY: GetAllocatedString writes a CoTaskMemAlloc'd PWSTR on Ok(())
    // which we MUST CoTaskMemFree exactly once. On Err the pointer is left
    // null (per windows-rs docs) so no free is needed on the failure path.
    unsafe {
        let mut pwstr = PWSTR::null();
        let mut cch: u32 = 0;
        match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pwstr, &mut cch) {
            Ok(()) if !pwstr.is_null() => {
                let name = pwstr.to_string().unwrap_or_else(|_| "(utf16 error)".into());
                CoTaskMemFree(Some(pwstr.0 as *const _));
                Some(name)
            }
            _ => None,
        }
    }
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
) -> Result<(IMFActivate, EncoderVendor), EncoderError> {
    // Use the session's real config dimensions for the probe so the
    // SetOutputType call validates the actual session parameters.
    let (probe_w, probe_h) = effective_dimensions(config);
    let probe_fps = config.framerate;
    let probe_bps = config.bitrate_bps;

    let count = activates.len();
    let mut last_err = EncoderError::InitFailed("no hardware MFT candidates enumerated".into());

    for (i, activate) in activates.iter().enumerate() {
        // ── Log candidate identity for diagnostics ────────────────────────────

        // Friendly name via the encapsulated helper (mirrors read_vendor_id_attribute).
        let friendly_name: String =
            read_friendly_name_attribute(activate).unwrap_or_else(|| "(unknown)".into());

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

        // ── Vendor detection (Slice 6 R2) ────────────────────────────────────
        // Match CLSID string to EncoderVendor for diagnostic logging only.
        // Mid-stream IDR is vendor-uniform via CODECAPI_AVEncVideoForceKeyFrame (P2 #809).
        // See explore #803 and design DD5.
        let vendor_id_str = read_vendor_id_attribute(activate);
        let vendor = EncoderVendor::detect(&clsid_str, vendor_id_str.as_deref());

        // DD3 three-site logging: source="clsid" | "vendor_id_fallback" | debug on absent.
        match vendor {
            EncoderVendor::NvidiaNvenc | EncoderVendor::IntelQsv | EncoderVendor::Amd => {
                // Determine which stage resolved the vendor.
                let clsid_matched =
                    clsid_str.starts_with("{60F44560-") || clsid_str.starts_with("{4BE8D3C0-");
                if clsid_matched {
                    tracing::info!(
                        target: "encoder.vendor",
                        vendor = ?vendor,
                        source = "clsid",
                        clsid = %clsid_str,
                        "vendor detected"
                    );
                } else if let Some(ref vid) = vendor_id_str {
                    tracing::info!(
                        target: "encoder.vendor",
                        vendor = ?vendor,
                        source = "vendor_id_fallback",
                        clsid = %clsid_str,
                        vendor_id = %vid,
                        "vendor detected"
                    );
                }
            }
            EncoderVendor::Unknown => {
                if vendor_id_str.is_none() {
                    tracing::debug!(
                        target: "encoder.vendor",
                        clsid = %clsid_str,
                        "vendor-id attribute absent — fallback unavailable"
                    );
                }
                tracing::warn!(
                    target: "encoder.vendor",
                    clsid = %clsid_str,
                    "vendor Unknown — CLSID not in known-vendor table and vendor-ID did not resolve"
                );
            }
        }

        tracing::info!(
            "probe_and_select_mft: selected candidate [{i}] \"{friendly_name}\" {clsid_str}"
        );
        return Ok((activate.clone(), vendor));
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

/// RAII guard that calls `MFShutdown` on drop (paired with encoder thread's MFStartup).
///
/// Must be constructed AFTER `CoUninitGuard` so it drops BEFORE it —
/// i.e. `MFShutdown` runs before `CoUninitialize`, matching the teardown order in
/// `init_mft_sync` and the Microsoft Media Foundation documentation.
struct MfShutdownGuard;

impl Drop for MfShutdownGuard {
    fn drop(&mut self) {
        // SAFETY: Paired with MFStartup in run_encoder_thread. MFStartup/MFShutdown
        // use a process-global reference count; this call decrements it exactly once
        // for the encoder thread's MFStartup. Per MS docs, MFShutdown is safe to call
        // from any thread and is a no-op when the refcount would go negative.
        unsafe {
            let _ = MFShutdown();
        };
    }
}

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
    vendor: EncoderVendor,
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

    // Step 1b: MFStartup on the encoder thread — this thread owns its own MF lifecycle.
    // The probe thread started and shut down MF during new(); MF is not alive here.
    // SAFETY: MFStartup initialises Media Foundation for this thread's MF calls. Paired
    // with MFShutdown via MfShutdownGuard. MfShutdownGuard is constructed AFTER
    // CoUninitGuard so it drops FIRST, preserving the teardown order: MFShutdown then
    // CoUninitialize (matching init_mft_sync's precedent and Microsoft docs).
    let _mf_guard = match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        Ok(_) => MfShutdownGuard,
        Err(e) => {
            tracing::error!("encoder thread MFStartup failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    tracing::debug!(
        "encoder thread CoInitializeEx+MFStartup OK; config: {}x{} @ {}fps {}bps",
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
        // MFShutdown is handled by MfShutdownGuard; CoUninitialize by CoUninitGuard.
        // Both guards drop automatically when this function returns.
        return;
    }
    tracing::debug!("setup_mft OK; entering pump_loop");

    // Path-selection gate + live D3D negotiation: evaluate ONCE at init.
    //
    // TASK-08/PR-4 will supply the real capture/encode adapter LUIDs (from the
    // capture device and the MFT adapter) and the shared keyed-mutex D3D11 device
    // produced by the capture-thread CopyResource hand-off. In PR-3 there is no
    // GPU producer yet, so:
    //   1. The gate still runs and logs the selected path (placeholder LUIDs).
    //   2. negotiate_gpu_path_runtime receives `None` for the shared device and
    //      therefore returns CpuStagedFallback + no pipeline — the session runs on
    //      the CPU-staged path exactly as before. Production behaviour is UNCHANGED.
    //
    // The returned `gpu_pipeline: Option<GpuEncodePipeline>` is threaded into
    // pump_loop. The FramePayload::GpuShared arm routes through it (TASK-06/07
    // GPU code) instead of `todo!()`; the arm is unreachable at runtime until a
    // producer exists (PR-4), but it compiles and links the real GPU path.
    let gpu_pipeline = {
        use crate::encode::gpu_path::negotiate_gpu_path_runtime;
        use crate::encode::path_select::{EncodePath, select_encode_path};

        // Placeholder LUIDs until PR-4 wires real adapter LUID reads. With both 0
        // the LUID-equality check passes (0 == 0); the vendor floor still applies,
        // so only an IntelQsv encoder selects GpuResident here.
        let placeholder_capture_luid: i64 = 0;
        let placeholder_encode_luid: i64 = 0;
        let selected =
            select_encode_path(placeholder_capture_luid, placeholder_encode_luid, vendor);
        tracing::info!(
            target: "sm_infra::encode::windows_mft",
            path = ?selected,
            vendor = ?vendor,
            "encode path selected at init"
        );

        let (w, h) = effective_dimensions(&config);
        match selected {
            EncodePath::GpuResident => {
                // Run the live encoder-thread D3D negotiation. In PR-3 the shared
                // device is None (no producer), so this returns CpuStagedFallback
                // with no pipeline; in PR-4 it builds the real GPU pipeline or
                // degrades via the TASK-05 fallback branch (WARN + CpuStagedFallback).
                let (negotiated, pipeline) = negotiate_gpu_path_runtime(None, &mft, w, h, &config);
                tracing::debug!(
                    target: "sm_infra::encode::windows_mft",
                    negotiated = ?negotiated,
                    gpu_pipeline_active = pipeline.is_some(),
                    "GpuResident selected; live negotiation complete"
                );
                pipeline
            }
            EncodePath::CpuStagedFallback => {
                // Existing CPU path — the GPU pipeline is never built.
                None
            }
        }
    };

    // Step 5: Cast to ICodecAPI — done here on the encoder thread, after setup_mft.
    // SAFETY: IMFTransform for hardware video encoders implements ICodecAPI per Windows docs.
    let codec_api: ICodecAPI = match mft.cast() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("encoder thread ICodecAPI cast failed: 0x{:08X}", e.code().0);
            return;
        }
    };

    // A1: cap GOP size to GOP_SIZE_FRAMES. Non-fatal if the driver rejects it (REQ-A1-2).
    // Set before pump_loop starts so the GOP schedule takes effect from the first IDR.
    // Consistent with the existing CODECAPI_AVEncCommonMeanBitRate rejection handling.
    let v = make_variant_u32(GOP_SIZE_FRAMES);
    // SAFETY: SetValue on a valid ICodecAPI is always safe; VARIANT is stack-allocated VT_UI4.
    if let Err(e) = unsafe { codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &v) } {
        tracing::warn!(
            target: "sm_infra::encode::windows_mft",
            "ICodecAPI::SetValue(CODECAPI_AVEncMPVGOPSize, {GOP_SIZE_FRAMES}) rejected \
             (non-fatal, encoding continues): 0x{:08X}",
            e.code().0
        );
    }

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

    // Step 8: Pump loop. mft is passed by value (owned) so it can be returned after
    // the stream ends. pump_loop takes ownership of mft, codec_api, and event_gen.
    // gpu_pipeline is `Some` only when the GPU-resident path was negotiated (PR-4
    // runtime); in PR-3 it is always `None` and the GpuShared arm is unreachable.
    let mft = pump_loop(
        mft,
        codec_api,
        event_gen,
        &state,
        rx,
        tx,
        &mut output_format_known,
        &config,
        gpu_pipeline,
    );

    // Steps 9a–9e: Notify end of stream and release.
    // WHY: pump_loop returns the IMFTransform handle. Send end-of-stream messages
    // before dropping it to cleanly finalize the encoder session.
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
    }
    // mft is dropped here (COM Release via Drop).
    // codec_api and event_gen were moved into pump_loop and dropped inside it.
    // activate is dropped here (COM Release via Drop).
    // MfShutdownGuard calls MFShutdown and CoUninitGuard calls CoUninitialize when
    // this function returns — encoder thread owns its own MF lifecycle end-to-end.
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
///
/// Snapshot of pending codec settings for the SWAP-FIRE pattern (bitrate change channel).
///
/// The struct is `Copy` so callers can pass it by value to `fire_pending_codec_settings`
/// and `restore_pending_codec` without lifetime concerns.
#[derive(Copy, Clone)]
struct CodecApiSwap {
    /// `Some(bps)` when a `set_bitrate(bps)` was pending at SWAP time; `None` otherwise.
    new_bitrate: Option<u32>,
}

/// SWAP step (DD1): read pending codec atomics BEFORE `ProcessInput`.
///
/// Clears `pending_bitrate` via `swap` so that each pending request is consumed
/// exactly once. The caller must call [`fire_pending_codec_settings`] AFTER
/// `ProcessInput` returns `Ok(())`.
fn swap_pending_codec_settings(state: &MftEncoderShared) -> CodecApiSwap {
    let raw_bps = state.pending_bitrate.swap(0, Ordering::AcqRel);
    let new_bitrate = if raw_bps != 0 { Some(raw_bps) } else { None };
    tracing::trace!(
        target: "sm_infra::encode::windows_mft",
        new_bitrate = ?new_bitrate,
        "pump_loop: swap captured new_bitrate={:?}",
        new_bitrate
    );
    CodecApiSwap { new_bitrate }
}

/// FIRE step (DD1): invoke `ICodecAPI::SetValue` calls AFTER `ProcessInput` succeeds.
///
/// Bitrate rejection is non-fatal (warn + continue).
///
/// Mid-stream IDR mechanism (Slice 6 R2 — replaces Slice 5 Mechanism G):
/// Vendor-uniform via `CODECAPI_AVEncVideoForceKeyFrame` (VT_UI4=1) called
/// BEFORE `IMFTransform::ProcessInput`. Empirical evidence: Phase 0 P2
/// (engram #809) — IDR at idx 0 on NVENC, idx 1 on Intel QSV (1-frame
/// in-flight latency, within `assert_keyframe_within_next_n_frames(30)`).
/// Reference: Microsoft HCK property table for Win8+ hardware encoder MFTs.
/// Production refs: Chromium media_foundation_video_encode_accelerator_win.cc
/// and FFmpeg libavcodec/mfenc.c::mf_send_frame() use the same sequence.
fn fire_pending_codec_settings(codec_api: &ICodecAPI, swap: &CodecApiSwap) {
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
    tracing::trace!(
        target: "sm_infra::encode::windows_mft",
        "pump_loop: fire applied"
    );
}

/// RESTORE step (DD3): re-arm pending codec atomics on early-return paths where
/// `ProcessInput` was NOT called (frame dropped, timeout, disconnect-without-drain).
///
/// - `pending_bitrate`: `compare_exchange(0, bps, AcqRel, Acquire)` — only restores
///   if the slot is still empty (no newer `set_bitrate` call); preserves last-write-wins
///   semantics (R6).
fn restore_pending_codec(state: &MftEncoderShared, swap: &CodecApiSwap) {
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
    reason = "pump_loop owns mft, codec_api, event_gen plus config, state, rx, tx, format state \
              and the optional GPU pipeline — design §5a one-function pump shape; 9 args accepted \
              over struct decomposition for clarity"
)]
fn pump_loop(
    mft: IMFTransform,
    initial_codec_api: ICodecAPI,
    initial_event_gen: IMFMediaEventGenerator,
    state: &MftEncoderShared,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
    output_format_known: &mut Option<bool>, // None until first packet sniffed; Some(true)=AVCC, Some(false)=AnnexB
    config: &EncoderConfig,
    // GPU-resident pipeline, `Some` only when the GPU path was negotiated (PR-4
    // runtime). In PR-3 this is always `None`, so the FramePayload::GpuShared arm
    // is structurally present but unreachable (no producer constructs GpuShared).
    gpu_pipeline: Option<crate::encode::gpu_path::GpuEncodePipeline>,
) -> IMFTransform {
    use crate::encode::bgra_to_nv12::{Nv12, convert as nv12_convert};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let codec_api = initial_codec_api;
    let event_gen = initial_event_gen;

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

    // Permanent once-shot for disconnect-triggered DRAIN. After upstream channel closes,
    // F2 wake-up post-DrainComplete (BEGIN_STREAMING + START_OF_STREAM) re-emits NeedInput;
    // re-entering the disconnect branch would re-fire DRAIN indefinitely (~12× spam observed
    // in v0.1.0 / v0.2.0). The first disconnect-DRAIN is the legitimate flush; subsequent
    // re-fires add no signal — the channel stays closed until the encoder thread exits.
    let mut disconnect_drained = false;

    // Change-detection sentinels for DD8 counter snapshot logging (spec R7/S7.2).
    let mut last_logged_ni: u32 = u32::MAX;
    let mut last_logged_ho: u32 = u32::MAX;
    let mut iter_count: u64 = 0;

    // Encode-rate diagnostic: count packets forwarded downstream and log fps once
    // per ~1 s of wall-clock. This reveals whether the capture+encode pipeline is
    // genuinely slow (corroborating the muxer real-DTS-delta fix) or whether the
    // bottleneck is elsewhere. Log only — no encoder behavior is changed.
    let mut fps_frame_count: u32 = 0;
    let mut fps_window_start = std::time::Instant::now();
    const FPS_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
    // I2 (D-PPT-2): per-second convert timing accumulator. Shares the encode_fps window
    // so all encoder-thread metrics reset at the same tick (one timestamp, one reset).
    let mut convert_stats = ConvertStats::default();
    // I3 (D-PPT-3): snapshot of state.dropped at the last encode window boundary, used
    // to compute per-interval drop delta for the enc_to_sender channel.
    let mut last_dropped_enc_snapshot: u64 = 0;

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
                &mft,
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
                        Ok(()) => {
                            // Encode-rate diagnostic: count only successfully forwarded frames.
                            // The window emission below runs unconditionally so encode_fps still
                            // reflects true throughput even when this send did not succeed.
                            fps_frame_count += 1;
                        }
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            state.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            tracing::info!("pump_loop: packet channel disconnected, exiting");
                            return mft;
                        }
                    }

                    // Per-second observability window: emit encode_fps, convert throughput, and
                    // the enc_to_sender drop delta. Checked unconditionally after the send match
                    // (mirroring capture/windows.rs) so the window still ticks during sustained
                    // enc→sender backpressure — exactly when drop-rate visibility matters most.
                    let now = std::time::Instant::now();
                    let elapsed = now.duration_since(fps_window_start);
                    if interval_elapsed(fps_window_start, now, FPS_LOG_INTERVAL) {
                        let fps = fps_frame_count as f64 / elapsed.as_secs_f64();
                        // CAPT-OBS-6: encode_fps event field names and cadence MUST remain
                        // unchanged. Do NOT alter the fields or message below.
                        tracing::info!(
                            target: "sm_infra::encode::windows_mft",
                            encode_fps = %format!("{fps:.1}"),
                            frames = fps_frame_count,
                            "encode throughput"
                        );

                        // I2 (D-PPT-2): emit convert throughput alongside encode_fps so all
                        // encoder-thread metrics share one window tick (CAPT-OBS-2).
                        tracing::info!(
                            target: "sm_infra::encode::windows_mft",
                            convert_fps = %format!("{:.1}", convert_stats.fps(elapsed)),
                            convert_us = convert_stats.mean_us(),
                            frames = convert_stats.frames,
                            "convert throughput"
                        );

                        // I3 (D-PPT-3): emit per-interval encode drop delta (CAPT-OBS-4).
                        // state.dropped accumulates both drops from the TrySendError::Full arm
                        // above and dimension-mismatch drops from the drop site in the NeedInput
                        // service pass below — includes dim-mismatch drops per the D-PPT-3
                        // dual-use note. Splitting the counter is out of Slice 1 scope.
                        let current_enc_dropped = state.dropped.load(Ordering::Relaxed);
                        let (enc_delta, new_enc_last) =
                            compute_drop_delta(current_enc_dropped, last_dropped_enc_snapshot);
                        if enc_delta > 0 {
                            tracing::info!(
                                target: "sm_infra::encode::windows_mft",
                                channel_drops = enc_delta,
                                channel = "enc_to_sender",
                                "encode channel drops"
                            );
                        }
                        last_dropped_enc_snapshot = new_enc_last;

                        // Reset all window-shared accumulators at the same boundary.
                        convert_stats.reset();
                        fps_frame_count = 0;
                        fps_window_start = std::time::Instant::now();
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
                        return mft;
                    } else if reason.contains("ProcessOutput: 0x80004005") {
                        // E_UNEXPECTED on vendor priming: consume credit, log warn, continue.
                        // This is expected during HW MFT startup before any frame is submitted.
                        tracing::warn!(
                            "pump_loop: ProcessOutput E_UNEXPECTED (vendor priming) — consuming credit"
                        );
                        ho_count -= 1;
                    } else {
                        tracing::error!("pump_loop: collect_output failed: {e}");
                        return mft;
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
                Ok(raw_frame) => {
                    // FramePayload dispatch seam: wrap the received CaptureFrame as
                    // FramePayload::Cpu so the routing match below is the single
                    // authoritative dispatch point. The channel carries CaptureFrame
                    // at the frozen VideoEncoder::start() boundary, so only the Cpu
                    // variant can be produced here today; PR-4 adds a GpuShared producer
                    // on the capture side via the keyed-mutex texture hand-off.
                    let payload = FramePayload::Cpu(raw_frame);
                    let frame = match payload {
                        FramePayload::Cpu(f) => f,
                        FramePayload::GpuShared {
                            handle,
                            width,
                            height,
                            stride: _,
                            timestamp,
                        } => {
                            // GPU-resident path (TASK-06/07): convert the shared BGRA
                            // texture to NV12 on the GPU and feed the MFT a DXGI-surface
                            // sample — no readback, no CPU convert, no MFCreateMemoryBuffer.
                            //
                            // Reachable only when a GpuEncodePipeline was negotiated AND a
                            // producer constructed GpuShared. In PR-3 neither holds, so this
                            // arm does not run at runtime; it compiles and links the real GPU
                            // code path (gpu_path::GpuEncodePipeline) instead of `todo!()`.
                            if let Some(pipe) = gpu_pipeline.as_ref() {
                                debug_assert_eq!(
                                    pipe.dimensions(),
                                    (width, height),
                                    "GpuShared frame dims must match the negotiated pipeline"
                                );
                                match submit_gpu_frame(
                                    &mft,
                                    pipe,
                                    handle,
                                    timestamp,
                                    frame_dur_100ns,
                                ) {
                                    Ok(()) => {
                                        current_ts = timestamp;
                                        ni_count -= 1;
                                        fire_pending_codec_settings(&codec_api, &swap);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            target: "sm_infra::encode::windows_mft",
                                            "pump_loop: GPU ProcessInput failed (skipping frame): {e}"
                                        );
                                        ni_count -= 1;
                                    }
                                }
                            } else {
                                // GpuShared received without a negotiated pipeline — a wiring
                                // bug (the capture side must only emit GpuShared when the gate
                                // selected GpuResident). Skip the frame; do not panic.
                                tracing::error!(
                                    target: "sm_infra::encode::windows_mft",
                                    "pump_loop: GpuShared frame received but no GPU pipeline negotiated — skipping"
                                );
                                state.dropped.fetch_add(1, Ordering::Relaxed);
                                restore_pending_codec(state, &swap);
                            }
                            // The GPU arm fully services (or skips) the frame above; continue
                            // the credit loop without falling through to the CPU convert path.
                            continue;
                        }
                    };
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
                    // I2 (D-PPT-2): bracket nv12_convert to accumulate per-frame convert latency.
                    let t0 = std::time::Instant::now();
                    nv12_convert(&frame, &mut nv12_scratch);
                    convert_stats.record(t0.elapsed());

                    // Consume force_keyframe_icodecapi_pending BEFORE submit_frame /
                    // ProcessInput — canonical Chromium + FFmpeg ordering (research #808).
                    // Vendor-uniform: both NVENC (IDR idx 0) and Intel QSV (IDR idx 1) honor
                    // this property BEFORE ProcessInput; P2 evidence in engram #809.
                    // The property auto-resets to 0 after ProcessInput per MS docs.
                    if state
                        .force_keyframe_icodecapi_pending
                        .swap(false, Ordering::AcqRel)
                    {
                        let v = make_variant_u32(1);
                        // SAFETY: SetValue on a valid ICodecAPI is always safe;
                        // the VARIANT is stack-allocated and correctly typed VT_UI4.
                        unsafe {
                            if let Err(e) =
                                codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v)
                            {
                                // Non-fatal — driver rejection is acceptable (DD13 convention).
                                tracing::warn!(
                                    target: "sm_infra::encode::windows_mft",
                                    "pump_loop: ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame) \
                                     rejected: 0x{:08X} (non-fatal, encoding continues)",
                                    e.code().0
                                );
                            } else {
                                tracing::debug!(
                                    target: "sm_infra::encode::windows_mft",
                                    "pump_loop: ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, \
                                     VT_UI4=1) issued BEFORE ProcessInput"
                                );
                            }
                        }
                    }

                    match submit_frame(&mft, &nv12_scratch, frame.timestamp, frame_dur_100ns) {
                        Ok(()) => {
                            // Decrement AFTER successful ProcessInput (spec OQ-1, design DD2).
                            ni_count -= 1;
                            // DD1 FIRE: ICodecAPI::SetValue AFTER ProcessInput — eliminates the
                            // race window where QSV withdraws NeedInput readiness (Mode 1 fix).
                            fire_pending_codec_settings(&codec_api, &swap);
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
                                return mft;
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
                    restore_pending_codec(state, &swap);
                    if !disconnect_drained {
                        tracing::info!(
                            "pump_loop: frame channel disconnected, sending COMMAND_DRAIN"
                        );
                        unsafe {
                            let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                        }
                        // DD14 SET site #2: mark drain window open BEFORE break.
                        draining = true;
                        // Permanent once-shot: do NOT re-fire even after DrainComplete + F2 wake-up.
                        disconnect_drained = true;
                        tracing::trace!(
                            target: "sm_infra::encode::windows_mft",
                            "draining = true (disconnect)"
                        );
                    }
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
        if iter_count.is_multiple_of(1000) {
            tracing::debug!(ni_count, ho_count, iter_count, "pump_loop: heartbeat");
        }
    }
    tracing::debug!("pump_loop exited cleanly");
    mft
}

/// Submit one NV12 frame as an `IMFSample` to `ProcessInput`.
///
/// The caller is responsible for firing any pending `ICodecAPI::SetValue` calls
/// (e.g. `CODECAPI_AVEncVideoForceKeyFrame`) BEFORE calling this function, following
/// the canonical Chromium + FFmpeg ordering (research #808).
///
/// The `MFSampleExtension_CleanPoint` attribute is READ (not written) in `collect_output`
/// for IDR detection — the output-side read path is unchanged (DD7).
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

/// Submit one GPU-resident frame to `ProcessInput` (TASK-06/07 GPU path).
///
/// Opens the cross-thread shared BGRA texture by its handle on the encoder-thread
/// device (PR-3: plain `OpenSharedResource`, no keyed-mutex acquire), runs
/// `VideoProcessorBlt` BGRA→NV12 on the GPU, wraps the NV12 texture in an
/// `MFCreateDXGISurfaceBuffer` sample, and feeds it to the MFT — no GPU→CPU
/// readback, no rayon convert, no `MFCreateMemoryBuffer`. The CPU `submit_frame`
/// path above is untouched and remains the byte-identical fallback (REQ-08).
///
/// Reachable only when a `GpuEncodePipeline` was negotiated and a producer emitted
/// `FramePayload::GpuShared`; in PR-3 there is no producer, so this is compiled and
/// linked but not exercised at runtime (the keyed-mutex producer lands in PR-4).
///
/// TODO(PR-4): switch to `OpenSharedResource1` + `IDXGIKeyedMutex::AcquireSync`/
/// `ReleaseSync` around the blt, per design D1 (PR-3 opens a plain shared texture
/// without any keyed-mutex synchronization).
fn submit_gpu_frame(
    mft: &IMFTransform,
    pipe: &crate::encode::gpu_path::GpuEncodePipeline,
    shared_handle: isize,
    timestamp: std::time::Duration,
    duration_100ns: i64,
) -> Result<(), EncoderError> {
    // SAFETY: shared_handle is a live, same-adapter D3D11 share handle per the
    // FramePayload::GpuShared contract (the capture thread produced it on a device
    // that shares this pipeline's adapter LUID — enforced by the path-selection gate).
    // PR-3 opens it via plain OpenSharedResource (no IDXGIKeyedMutex::AcquireSync).
    let bgra_tex = unsafe { pipe.open_shared_bgra(shared_handle) }?;
    pipe.gpu_bgra_to_nv12(&bgra_tex)?;
    let sample = pipe.build_dxgi_imfsample(timestamp, duration_100ns)?;
    unsafe {
        mft.ProcessInput(0, &sample, 0).map_err(|e| {
            EncoderError::EncodeFailed(format!("ProcessInput(dxgi): 0x{:08X}", e.code().0))
        })
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

// make_variant_bool() remains deleted — VT_BOOL is not used anywhere.
// CODECAPI_AVEncVideoForceKeyFrame uses make_variant_u32(1) (VT_UI4=1, the correct
// VARIANT type per MS docs and research #808).

// ── Inherent methods ──────────────────────────────────────────────────────────

impl WindowsMftH264Encoder {
    /// Signal end-of-burst to the encoder pump loop, requesting a `MFT_MESSAGE_COMMAND_DRAIN`.
    ///
    /// `flush()` triggers `MFT_MESSAGE_COMMAND_DRAIN`. After `METransformDrainComplete`,
    /// pump_loop resumes via `BEGIN_STREAMING + START_OF_STREAM` (Slice 4 DD17/F2) —
    /// `flush()` is **SAFE mid-stream** and is **NOT terminal** on Intel QSV.
    ///
    /// **Latency**: Empirically ~250 ms drain roundtrip (Phase 0 trace #710). Plan
    /// `recv_timeout` deadlines accordingly.
    ///
    /// **Production callers MUST NOT call this method.** It is a test affordance for
    /// single-burst short-stream tests. Production callers force IDR via
    /// `request_keyframe()` and rely on the channel-disconnect DRAIN path at shutdown.
    ///
    /// **Async**: Returns immediately. The DRAIN fires on the next pump_loop iteration
    /// after the NeedInput inner loop completes.
    ///
    /// **Concurrency-safe**: Backed by `Arc<AtomicBool>`. Multiple concurrent calls collapse
    /// to at most one `COMMAND_DRAIN` per pump iteration (swap-once consumption).
    pub fn flush(&self) {
        self.state.drain_pending.store(true, Ordering::Release);
    }

    /// Force an IDR frame via `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)`
    /// called BEFORE the next `ProcessInput`.
    ///
    /// This is the production mid-stream IDR mechanism for the Slice 6 R2 architecture.
    /// It is identical to the trait `request_keyframe()` impl but named explicitly for
    /// direct use in Phase 0 probes (which live in a separate integration-test crate and
    /// cannot access `pub(crate)` items). Production callers SHOULD prefer the trait method.
    ///
    /// **Latency**: ~0ms on NVENC (IDR at idx 0); ~33ms on Intel QSV (IDR at idx 1).
    /// Both within `assert_keyframe_within_next_n_frames(30)` tolerance (P2 evidence #809).
    ///
    /// **HCK compliance**: `CODECAPI_AVEncVideoForceKeyFrame` is a REQUIRED Win8+ hardware
    /// encoder MFT certification property (research #808). Both NVENC and Intel QSV implement
    /// it. SetValue rejection is non-fatal — pump_loop WARNs and continues.
    ///
    /// **Phase 0 probes**: `phase0_nvenc_force_keyframe_via_codecapi_before_processinput`
    /// and `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput`
    /// in `crates/sm-infra/tests/windows_mft_encode.rs` (`#[ignore]`-gated).
    pub fn request_keyframe_via_force_keyframe_icodecapi(&self) {
        self.state
            .force_keyframe_icodecapi_pending
            .store(true, Ordering::Release);
    }
}

// ── Observability seams (perf-pipeline-throughput Slice 1) ───────────────────
//
// Pure helpers extracted for unit-testability (D-PPT-5, D-PPT-6). No COM, no
// heap allocation, no locks — safe to call on the encoder pump_loop hot path.

/// Per-second accumulator for NV12 convert timing (I2, D-PPT-5).
///
/// Accumulates a frame count and a total microsecond sum across one 1-second window.
/// At the window boundary, callers read `mean_us()` and `fps()`, emit the log event,
/// then call `reset()`. Follows the `FpsTracker` pure-struct-with-cfg(test)-accessor
/// precedent (render/fps_tracker.rs).
#[derive(Default)]
struct ConvertStats {
    /// Number of `nv12_convert` calls recorded in the current window.
    frames: u32,
    /// Cumulative duration of all recorded calls in the current window, in microseconds.
    total_us: u64,
}

impl ConvertStats {
    /// Record one convert duration into the current window.
    #[inline]
    fn record(&mut self, dur: std::time::Duration) {
        self.frames += 1;
        self.total_us += dur.as_micros() as u64;
    }

    /// Per-frame mean convert latency in microseconds over the current window.
    /// Returns 0 when no frames have been recorded (avoids divide-by-zero).
    #[inline]
    fn mean_us(&self) -> u64 {
        if self.frames == 0 {
            0
        } else {
            self.total_us / self.frames as u64
        }
    }

    /// Frames-per-second computed over the provided elapsed window duration.
    /// Returns 0.0 when no frames have been recorded.
    #[inline]
    fn fps(&self, window: std::time::Duration) -> f64 {
        if self.frames == 0 {
            0.0
        } else {
            self.frames as f64 / window.as_secs_f64()
        }
    }

    /// Reset the accumulator to prepare for the next 1-second window.
    #[inline]
    fn reset(&mut self) {
        self.frames = 0;
        self.total_us = 0;
    }
}

/// Compute the per-interval drop delta for a monotonically-increasing drop counter.
///
/// Returns `(delta, new_last)` where `delta = current.saturating_sub(last)` and
/// `new_last = current`. Used by pump_loop (I3, D-PPT-3) to surface the
/// enc_to_sender drop rate without cumulative totals in the per-second log event.
#[inline]
fn compute_drop_delta(current: u64, last: u64) -> (u64, u64) {
    (current.saturating_sub(last), current)
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
    /// `mft_activate_factory: None` means `start()` returns `Err(Internal(_))` rather
    /// than accessing an invalid COM pointer. Drop only calls `stop()` (a no-op when
    /// `handle` is None) and drops the None factory — no COM or MF calls are made.
    /// The test encoder MUST NOT be started.
    fn new_for_validation_test() -> Self {
        Self {
            config: EncoderConfig::default(),
            state: Arc::new(MftEncoderShared::default()),
            vendor: EncoderVendor::Unknown,
            mft_activate_factory: None,
            handle: None,
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

    // ─── Slice 6 R2: ForceKeyFrame atomic flag semantics (spec R18, S19-S21) ──
    //
    // CI-runnable: new_for_validation_test() bypasses COM/MFT init.
    // Tests verify the AtomicBool mechanics powering the vendor-uniform IDR mechanism.

    // S19 — flag defaults to false on construction.
    #[test]
    fn force_keyframe_icodecapi_pending_defaults_to_false_on_construction() {
        let enc = WindowsMftH264Encoder::new_for_validation_test();
        assert!(
            !enc.state
                .force_keyframe_icodecapi_pending
                .load(Ordering::Acquire),
            "force_keyframe_icodecapi_pending must be false immediately after construction"
        );
    }

    // S20 — calling request_keyframe() sets the flag to true.
    #[test]
    fn request_keyframe_sets_force_keyframe_icodecapi_pending_to_true() {
        let enc = WindowsMftH264Encoder::new_for_validation_test();
        enc.request_keyframe();
        assert!(
            enc.state
                .force_keyframe_icodecapi_pending
                .load(Ordering::Acquire),
            "force_keyframe_icodecapi_pending must be true after request_keyframe()"
        );
    }

    // S21 — swap(false, AcqRel) returns true (was set) and leaves false (consumed once).
    #[test]
    fn force_keyframe_icodecapi_pending_swap_consumes_to_false() {
        let enc = WindowsMftH264Encoder::new_for_validation_test();
        enc.request_keyframe(); // arm the flag
        let previous = enc
            .state
            .force_keyframe_icodecapi_pending
            .swap(false, Ordering::AcqRel); // simulate pump_loop NeedInput consume
        assert!(
            previous,
            "swap must return true (flag was set by request_keyframe)"
        );
        assert!(
            !enc.state
                .force_keyframe_icodecapi_pending
                .load(Ordering::Acquire),
            "flag must be false after swap consume (one-shot semantics)"
        );
    }

    // ── T.A.1 (RED → GREEN): EncoderVendor::detect — 9 unit tests ──────────────
    //
    // Priority contract: CLSID exact-prefix (stage 1) → vendor-ID prefix (stage 2) →
    // Unknown (catch-all). All tests are pure string comparisons; no COM, no GPU.

    // SC-DETECT-1 / R1.2, R1.3: CLSID exact-prefix → NvidiaNvenc
    #[test]
    fn detect_clsid_match_nvenc() {
        assert_eq!(
            EncoderVendor::detect("{60F44560-5A20-4857-BFEF-D29773CB8040}", None),
            EncoderVendor::NvidiaNvenc
        );
    }

    // SC-DETECT-3 / R1.2, R1.3: CLSID exact-prefix → IntelQsv
    #[test]
    fn detect_clsid_match_intel() {
        assert_eq!(
            EncoderVendor::detect("{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}", None),
            EncoderVendor::IntelQsv
        );
    }

    // SC-DETECT-4 / R1.4, R1.6: vendor-ID fallback → Amd
    #[test]
    fn detect_vendor_id_only_amd() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_1002")),
            EncoderVendor::Amd
        );
    }

    // SC-DETECT-2 / R1.4: vendor-ID fallback → NvidiaNvenc (CLSID rotation)
    #[test]
    fn detect_vendor_id_only_nvenc_fallback() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_10DE")),
            EncoderVendor::NvidiaNvenc
        );
    }

    // R1.4: vendor-ID fallback → IntelQsv
    #[test]
    fn detect_vendor_id_only_intel_fallback() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_8086")),
            EncoderVendor::IntelQsv
        );
    }

    // SC-DETECT-7 / R1.4: no CLSID, no vendor-ID → Unknown
    #[test]
    fn detect_unknown_both_absent() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", None),
            EncoderVendor::Unknown
        );
    }

    // SC-DETECT-6 / R1.4: unrecognized vendor-ID + malformed sub-cases → all Unknown
    #[test]
    fn detect_unknown_vendor_id_not_recognized() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_FFFF")),
            EncoderVendor::Unknown
        );
        // Malformed: prefix only (no digits after VEN_)
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_")),
            EncoderVendor::Unknown
        );
        // Malformed: digits without prefix
        assert_eq!(
            EncoderVendor::detect("", Some("1002")),
            EncoderVendor::Unknown
        );
    }

    // SC-DETECT-8 / R1.5, DD4: CLSID wins when vendor-ID disagrees
    #[test]
    fn detect_clsid_wins_on_disagreement() {
        // CLSID says NVENC, vendor-ID says AMD — CLSID must win.
        assert_eq!(
            EncoderVendor::detect("{60F44560-5A20-4857-BFEF-D29773CB8040}", Some("VEN_1002")),
            EncoderVendor::NvidiaNvenc
        );
    }

    // R1.4 prefix-match tolerance: VEN_1002 with trailing device suffix → Amd
    #[test]
    fn detect_vendor_id_suffix_tolerated() {
        assert_eq!(
            EncoderVendor::detect("(no CLSID)", Some("VEN_1002&DEV_xxxx")),
            EncoderVendor::Amd
        );
    }

    // ─── T.B.3: mft_encoder_reports_hw_backend_name (HW-gated, #[ignore]) ────
    //
    // Requires a real MFT host (Windows machine with NVENC or Intel QSV).
    // Tagged `#[ignore]` to match the existing 46 HW-gated tests pattern.
    // On HW: constructs a real `WindowsMftH264Encoder` and asserts the returned
    // backend name starts with `"hw_"`, covering all three vendor branches.
    // Note: "hw_amd" also passes the starts_with("hw_") assertion.

    #[test]
    #[ignore = "requires real MFT hardware (NVENC or Intel QSV) — run manually on HW host"]
    fn mft_encoder_reports_hw_backend_name() {
        let enc = WindowsMftH264Encoder::new(EncoderConfig::default())
            .expect("MFT encoder construction must succeed on HW host");
        let name = enc.backend_name();
        assert!(
            name.starts_with("hw_"),
            "MFT encoder backend_name must start with 'hw_', got: {name:?}"
        );
    }

    // ─── A1: GOP size cap structural constant guard ───────────────────────────
    //
    // CI-runnable: asserts the named constant exists and equals the design value.
    // Real hardware IDR cadence is verified empirically (manual check — see task A1-5).
    // REQ-A1-5: guards against accidental constant deletion or value drift.

    #[test]
    fn gop_size_constant_is_60_frames() {
        // Structural guard: asserts the GOP constant exists and equals the design value.
        // Real hardware IDR cadence is verified empirically (manual check — see task A1-5).
        assert_eq!(GOP_SIZE_FRAMES, 60u32);
    }

    // ─── sender-mft-hw-init: probe-thread seam tests (SC-2, SC-3, SC-4, SC-5) ─

    // SC-2: Err returned from the probe closure is propagated unchanged (REQ-MFT-7).
    #[test]
    fn probe_thread_failure_propagates_err_unchanged() {
        use std::time::Duration;
        let r = run_probe_on_isolated_thread_with(Duration::from_secs(5), || {
            Err(EncoderError::InitFailed("simulated".into()))
        });
        assert!(
            matches!(&r, Err(EncoderError::InitFailed(s)) if s == "simulated"),
            "expected InitFailed(\"simulated\"), got {r:?}"
        );
    }

    // SC-3: timeout path returns InitFailed with "probe thread timeout" in message (REQ-MFT-6).
    #[test]
    fn probe_thread_timeout_returns_init_failed() {
        use std::time::Duration;
        let r = run_probe_on_isolated_thread_with(Duration::from_secs(1), || {
            std::thread::sleep(Duration::from_secs(3));
            Err(EncoderError::InitFailed("late".into()))
        });
        assert!(
            matches!(&r, Err(EncoderError::InitFailed(s)) if s.contains("probe thread timeout")),
            "expected InitFailed containing \"probe thread timeout\", got {r:?}"
        );
    }

    // SC-4: probe closure panic is caught via channel Disconnected; caller gets InitFailed (REQ-MFT-6).
    #[test]
    fn probe_thread_panic_returns_init_failed() {
        use std::time::Duration;
        let r = run_probe_on_isolated_thread_with(Duration::from_secs(5), || {
            std::panic::resume_unwind(Box::new("simulated probe unwind"))
        });
        assert!(
            matches!(&r, Err(EncoderError::InitFailed(s)) if s.contains("panicked before sending result")),
            "expected InitFailed containing \"panicked before sending result\", got {r:?}"
        );
    }

    #[test]
    fn probe_receive_disconnect_maps_to_the_existing_init_failure() {
        use std::{sync::mpsc::RecvTimeoutError, time::Duration};

        let r = classify_probe_receive::<()>(Err(RecvTimeoutError::Disconnected), Duration::ZERO);

        assert!(
            matches!(&r, Err(EncoderError::InitFailed(s)) if s == "probe thread panicked before sending result"),
            "expected the existing disconnected-probe InitFailed, got {r:?}"
        );
    }

    #[test]
    fn probe_receive_success_and_timeout_remain_distinct() {
        use std::{sync::mpsc::RecvTimeoutError, time::Duration};

        assert!(classify_probe_receive(Ok(Ok(())), Duration::ZERO).is_ok());
        let timeout = classify_probe_receive::<()>(Err(RecvTimeoutError::Timeout), Duration::ZERO);
        assert!(
            matches!(&timeout, Err(EncoderError::InitFailed(s)) if s.contains("probe thread timeout")),
            "expected timeout to remain distinct from disconnect, got {timeout:?}"
        );
    }

    // SC-5: new() must not corrupt the caller's COM apartment (REQ-MFT-8).
    // The calling thread enters STA (mimicking Tauri main thread + WebView2).
    // After new() returns (Ok or Err), re-calling CoInitializeEx(STA) must return
    // S_FALSE (0x1) — apartment still STA and untouched, not RPC_E_CHANGED_MODE.
    // CI-runnable: purely checks COM apartment semantics, no real GPU needed.
    #[test]
    fn new_does_not_corrupt_caller_sta_apartment() {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
        };

        // Enter STA — mimics Tauri main thread after tao/wry WebView2 init.
        let sta_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert!(
            sta_hr.is_ok() || sta_hr.0 == 0x1,
            "must be able to enter STA on a fresh test thread, got 0x{:08X}",
            sta_hr.0
        );

        let config = EncoderConfig {
            bitrate_bps: 4_000_000,
            framerate: 30,
            ..EncoderConfig::default()
        };
        // new() may return Ok or Err depending on hardware availability.
        // What matters is the caller apartment state after the call.
        let _result = WindowsMftH264Encoder::new(config);

        // Verify: re-entering STA returns S_FALSE (0x1) meaning the apartment is
        // still STA and has NOT been changed to MTA by new().
        let recheck_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        assert_ne!(
            recheck_hr.0, 0x80010106_u32 as i32,
            "caller apartment must not have been changed to MTA (RPC_E_CHANGED_MODE)"
        );
        assert_eq!(
            recheck_hr.0, 0x1,
            "caller apartment must still be STA (S_FALSE = already initialized)"
        );

        unsafe { CoUninitialize() }; // for the recheck CoInitializeEx
        unsafe { CoUninitialize() }; // for the initial STA init
    }

    // ── Observability seam tests (WU-A RED — perf-pipeline-throughput Slice 1) ──
    //
    // These tests are RED until Phase 2 adds ConvertStats and interval_elapsed.
    // All tests are pure (no COM, no hardware, no tracing subscriber).

    /// Task 1.1 [RED]: ConvertStats accumulates durations and computes mean_us and fps correctly.
    #[test]
    fn convert_stats_record_and_mean_us() {
        use std::time::{Duration, Instant};

        let mut stats = ConvertStats::default();
        // Record 3 durations: 10 ms, 20 ms, 30 ms → total 60 ms, mean = 20 ms = 20_000 us.
        stats.record(Duration::from_millis(10));
        stats.record(Duration::from_millis(20));
        stats.record(Duration::from_millis(30));
        assert_eq!(stats.mean_us(), 20_000, "mean_us must be total_us / frames");

        // fps over a 3-second window: 3 frames / 3 s = 1.0 fps
        let fps = stats.fps(Duration::from_secs(3));
        assert!(
            (fps - 1.0_f64).abs() < 1e-9,
            "fps must be frames / elapsed_secs, got {fps}"
        );
        let _ = Instant::now(); // ensure Instant is in scope (compile smoke)
    }

    /// Task 1.2 [RED]: ConvertStats::reset zeroes all accumulated state.
    #[test]
    fn convert_stats_reset_zeroes_state() {
        let mut stats = ConvertStats::default();
        stats.record(std::time::Duration::from_millis(5));
        stats.reset();
        assert_eq!(stats.frames, 0, "frames must be 0 after reset");
        assert_eq!(stats.total_us, 0, "total_us must be 0 after reset");
    }

    /// Pinning guard: ConvertStats with zero frames must not divide by zero.
    ///
    /// Production code fires mean_us()/fps() at the per-second window boundary.  On the
    /// first window after startup (or after a reset), it is possible that no convert calls
    /// occurred (quiet first second: D-PPT-5 / spec CAPT-OBS-2 "First frame in window").
    /// This test pins the guard so a future removal would cause a failure before a panic
    /// reaches production.
    #[test]
    fn convert_stats_zero_frame_guard_returns_zero() {
        use std::time::Duration;

        let stats = ConvertStats::default();
        assert_eq!(
            stats.mean_us(),
            0,
            "mean_us() must return 0 when frames == 0 (divide-by-zero guard)"
        );
        assert_eq!(
            stats.fps(Duration::from_secs(1)),
            0.0,
            "fps() must return 0.0 when frames == 0 (divide-by-zero guard)"
        );
    }

    /// Task 1.3 [RED]: interval_elapsed returns false below threshold and true at/above threshold.
    #[test]
    fn window_gate_below_threshold_returns_false_and_at_or_above_returns_true() {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        // Simulate "now" 500 ms after start — below the 1 s threshold.
        let below = start + Duration::from_millis(500);
        assert!(
            !interval_elapsed(start, below, Duration::from_secs(1)),
            "500 ms elapsed must return false for a 1 s threshold"
        );

        // Simulate "now" 1001 ms after start — above the threshold.
        let above = start + Duration::from_millis(1001);
        assert!(
            interval_elapsed(start, above, Duration::from_secs(1)),
            "1001 ms elapsed must return true for a 1 s threshold"
        );

        // Exactly at the boundary — inclusive (>= semantics, matching FPS_LOG_INTERVAL).
        let exact = start + Duration::from_secs(1);
        assert!(
            interval_elapsed(start, exact, Duration::from_secs(1)),
            "exactly 1 s elapsed must return true (inclusive boundary)"
        );
    }

    // ── TASK-04: NVENC byte-identical config-pinning regression ──────────────
    //
    // These tests pin the NVENC encoder configuration constants and assert that
    // the CPU-staged sample construction path is selected for NVENC — ensuring
    // the GPU-resident path (PR-3) cannot inadvertently activate on NVENC machines.
    //
    // Satisfies: REQ-02, S-06, design §NVENC-Protection Proof.

    /// T-MFT-NVENC-01 (TASK-04): NVENC selects CpuStagedFallback via path gate.
    ///
    /// Asserts the gate returns CpuStagedFallback for NvidiaNvenc regardless
    /// of LUID equality — the vendor floor takes precedence.
    #[test]
    fn nvenc_path_gate_selects_cpu_staged_fallback_task04() {
        use crate::encode::path_select::{EncodePath, select_encode_path};

        // Simulate NVENC machine with same-adapter LUID (belt-and-suspenders: vendor
        // floor alone should reject GpuResident even when LUID matches).
        let luid_nvidia: i64 = 0x0000_10DE_CAFE_0001_u64 as i64;
        let path = select_encode_path(luid_nvidia, luid_nvidia, EncoderVendor::NvidiaNvenc);
        assert_eq!(
            path,
            EncodePath::CpuStagedFallback,
            "NvidiaNvenc must always select CpuStagedFallback (REQ-02 vendor floor)"
        );
    }

    /// T-MFT-NVENC-02 (TASK-04): default EncoderConfig constants match pre-change reference.
    ///
    /// Config-regression guard. Pins the generic encoder defaults that must remain
    /// stable across this change (REQ-02): GOP_SIZE_FRAMES (60), the default
    /// bitrate (4 Mbps), and the default framerate (30fps). These are not
    /// NVENC-specific — they are the shared EncoderConfig defaults used by every
    /// encode path.
    ///
    /// Any refactor that accidentally changes these constants will break this test
    /// BEFORE the change reaches CI hardware measurement.
    #[test]
    fn default_encoder_config_constants_match_pre_change_reference_task04() {
        // GOP_SIZE_FRAMES — CODECAPI_AVEncMPVGOPSize sent to the MFT.
        // Pre-change value: 60 frames = a 1-second keyframe interval at the
        // sender's 60fps (design §A1).
        assert_eq!(
            GOP_SIZE_FRAMES, 60u32,
            "GOP_SIZE_FRAMES must be 60 (pre-change reference)"
        );

        // Default EncoderConfig bitrate — 4 Mbps.
        let cfg = EncoderConfig::default();
        assert_eq!(
            cfg.bitrate_bps, 4_000_000,
            "default bitrate_bps must be 4_000_000 bps (pre-change reference)"
        );

        // Default framerate — 30fps (sender overrides to 60, but the config default is pinned).
        assert_eq!(
            cfg.framerate, 30u32,
            "default framerate must be 30fps (pre-change reference)"
        );
    }

    // NOTE: A former T-MFT-NVENC-03 test
    // (`cpu_staged_path_uses_memory_buffer_not_dxgi_surface_task04`) was removed.
    // Its docstring promised it pinned `build_imfsample` as MFCreateMemoryBuffer-
    // backed, but exercising that contract requires MFStartup + COM init (no
    // unit-test harness has that), and its only real assertion — NVENC →
    // CpuStagedFallback via `select_encode_path` — duplicated
    // `nvenc_path_gate_selects_cpu_staged_fallback_task04` above. The remaining
    // `matches!(payload, FramePayload::Cpu(_))` check was tautological (the value
    // had just been constructed as Cpu). Removed rather than left misleading.
}
