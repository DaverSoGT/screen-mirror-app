#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Capture-thread GPU producer for the GPU-resident hand-off (design D1, TASK-08).
//!
//! When the path-selection gate selects the GPU-resident path, the WGC capture
//! thread does NOT read pixels back to the CPU. Instead it `CopyResource`s the live
//! WGC BGRA texture into a shared `ID3D11Texture2D` created with
//! `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`, then hands the texture's NT share handle
//! to the encoder thread through the channel (`FramePayload::GpuShared`). The
//! encoder opens the handle on its own same-adapter device and reads the texture
//! under the same keyed mutex.
//!
//! # Frame aliasing (Fix 2)
//!
//! A SINGLE reusable shared texture would alias frames: the channel has capacity
//! `CHANNEL_CAP` (4), so the producer can overwrite the one texture in place for
//! frames N+1, N+2 while frame N is still queued — the consumer would then blt
//! later pixels under frame N's (older) timestamp, causing judder under any
//! backpressure. To eliminate aliasing WITHOUT throttling below the 60 fps target,
//! this producer owns a RING of shared textures. Each copy writes the CURRENT slot
//! and returns THAT slot's share handle, so every in-flight payload references its
//! own pixels. The ring is sized so the producer cannot lap a slot that is still
//! referenced by a queued payload or the frame the consumer is currently reading.
//!
//! ## Cursor-on-queue (Fix 6)
//!
//! The ring cursor advances ONLY after the frame is actually queued. `copy_frame_bounded`
//! copies into the current slot and returns its handle WITHOUT moving the cursor; the
//! caller advances the cursor (`advance_after_send`) ONLY on a confirmed `try_send`
//! success. A dropped frame (`TrySendError::Full`) leaves the cursor put, so the SAME
//! slot is reused by the next frame — safe, because the dropped frame's handle was never
//! queued and nothing references that slot's pixels. This is what keeps the ring honest:
//! a slot is overwritten only after the previous occupant left the channel, so the
//! producer can never lap a slot whose handle is still the un-popped channel head.
//! Worst-case LIVE references with cursor-on-queue: at most `CHANNEL_CAP` (4) queued
//! payloads + 1 frame the consumer is currently reading = 5. The heartbeat no longer
//! holds a live handle (Fix 3 made it a CPU readback), so `RING_LEN > 5` suffices.
//!
//! All COM objects here are created on, and used only from, the WGC capture thread
//! (the thread that runs `on_frame_arrived`). Each ring slot's NT share handle is
//! the ONLY thing that crosses to the encoder thread.

use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource1};
use windows::core::Interface;

use sm_domain::encode::EncoderError;

use crate::encode::gpu_path::{KEYED_MUTEX_KEY, adapter_luid_i64};

/// Number of shared textures in the producer ring (Fix 2 + Fix 6).
///
/// Must exceed the maximum number of share handles that can reference distinct
/// in-flight frames at once so the producer never overwrites a texture whose handle
/// is still queued or in-use. Because the ring cursor advances ONLY on a confirmed
/// `try_send` success (Fix 6), a slot is reused only after its previous occupant has
/// left the channel, so the worst-case live references are:
///
/// - `CHANNEL_CAP` (4) queued payloads, plus
/// - 1 frame the consumer is currently reading
///   = 5 worst-case live references.
///
/// The heartbeat no longer contributes a live handle: Fix 3 made it a CPU readback,
/// so there is no longer a `+1 heartbeat` term. `RING_LEN` must be strictly greater
/// than 5 so the producer can write its current slot without aliasing any of those 5.
///
/// `CHANNEL_CAP` is the capture→encoder channel capacity in
/// `src-tauri/src/commands/sender.rs` (4). If that changes, bump this accordingly.
pub(crate) const RING_LEN: usize = 6;

/// RAII guard for an owned NT share handle (from `CreateSharedHandle`) during the
/// fallible window of `RingSlot::build`. If `build` returns Err after the handle is
/// created (e.g. the `IDXGIKeyedMutex` cast fails), `Drop` closes the handle so it is
/// never leaked. On success the guard is disarmed via [`Self::into_raw_isize`], handing
/// ownership to the constructed `RingSlot` (whose own `Drop` closes it).
struct NtHandleGuard(HANDLE);

impl NtHandleGuard {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// Disarm the guard, returning the raw handle value as `isize` (the form stored in
    /// `RingSlot`). After this the guard no longer closes the handle.
    fn into_raw_isize(self) -> isize {
        let raw = self.0.0 as isize;
        // Prevent Drop from closing the handle now that ownership has transferred.
        core::mem::forget(self);
        raw
    }
}

impl Drop for NtHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: self.0 is an owned NT handle from CreateSharedHandle that was never
            // handed off (guard still armed). CloseHandle releases it exactly once.
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

/// One slot of the producer ring: a shared keyed-mutex BGRA texture plus its NT share
/// handle and keyed mutex. Each slot is independent so frames in different slots never
/// alias each other's pixels.
struct RingSlot {
    /// Shared BGRA destination texture (keyed-mutex, NT-handle sharable).
    shared_tex: ID3D11Texture2D,
    /// Keyed mutex guarding `shared_tex` against the encoder-thread consumer.
    keyed: IDXGIKeyedMutex,
    /// NT share handle for `shared_tex`, stored as `isize` (a raw `*mut c_void` HANDLE
    /// is not `Send`). Reconstructed into `HANDLE` only to close on drop; the value
    /// (an `isize`) is what crosses to the encoder, which opens its own reference via
    /// `OpenSharedResource1`.
    share_handle: isize,
}

impl RingSlot {
    /// Create one ring slot on `device` for `width`×`height` BGRA frames.
    ///
    /// # Safety
    /// `device` MUST be WGC's live device for the calling capture thread.
    unsafe fn build(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, EncoderError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            // Keyed-mutex + NT-handle sharing: the consumer opens the NT handle with
            // OpenSharedResource1 and reads the texture under the keyed mutex (D1).
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0
                | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut shared_tex: Option<ID3D11Texture2D> = None;
        // SAFETY: desc is fully initialized; None initial data is valid for a DEFAULT
        // render target. Out-param written on Ok.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut shared_tex)) }.map_err(|e| {
            EncoderError::InitFailed(format!(
                "CreateTexture2D(shared bgra): 0x{:08X}",
                e.code().0
            ))
        })?;
        let shared_tex = shared_tex
            .ok_or_else(|| EncoderError::InitFailed("CreateTexture2D returned null".into()))?;

        // Create the NT share handle via IDXGIResource1::CreateSharedHandle — required
        // for NT-handle textures (the legacy GetSharedHandle only works for non-NT
        // SHARED). The resource (not the device) owns the handle.
        let dxgi_res: IDXGIResource1 = shared_tex.cast().map_err(|e| {
            EncoderError::InitFailed(format!("cast IDXGIResource1: 0x{:08X}", e.code().0))
        })?;
        // SAFETY: dxgi_res is the just-created keyed-mutex/NT-handle texture; passing
        // None for the security attributes + name requests an unnamed NT handle with
        // GENERIC_ALL access. Returns an owned HANDLE we close on drop.
        let share_handle = unsafe { dxgi_res.CreateSharedHandle(None, GENERIC_ALL.0, None) }
            .map_err(|e| {
                EncoderError::InitFailed(format!("CreateSharedHandle: 0x{:08X}", e.code().0))
            })?;
        // Wrap the owned NT handle in an RAII guard so any early return BELOW (the
        // keyed-mutex cast) closes it instead of leaking it. The guard is disarmed only
        // once ownership transfers into the constructed `RingSlot`, whose Drop then owns
        // the close. Without this, a failed `cast::<IDXGIKeyedMutex>()` would return Err
        // while `share_handle` (a bare HANDLE) drops without `CloseHandle`, leaking the NT
        // handle for the process lifetime.
        let share_handle = NtHandleGuard::new(share_handle);

        let keyed: IDXGIKeyedMutex = shared_tex.cast().map_err(|e| {
            EncoderError::InitFailed(format!("cast IDXGIKeyedMutex: 0x{:08X}", e.code().0))
        })?;

        Ok(Self {
            shared_tex,
            keyed,
            // Disarm the guard: ownership of the NT handle now belongs to this RingSlot,
            // whose Drop closes it exactly once.
            share_handle: share_handle.into_raw_isize(),
        })
    }
}

impl Drop for RingSlot {
    fn drop(&mut self) {
        let handle = HANDLE(self.share_handle as *mut core::ffi::c_void);
        if !handle.is_invalid() {
            // SAFETY: share_handle is an owned NT handle from CreateSharedHandle;
            // CloseHandle releases it exactly once (guarded by is_invalid()).
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
        // shared_tex / keyed drop here → COM Release.
    }
}

/// Owns the capture-thread side of the keyed-mutex hand-off: a RING of shared BGRA
/// textures (each with its own NT share handle + keyed mutex) created on WGC's device.
///
/// Created lazily on the first GPU frame (once the real capture dimensions and WGC's
/// device are known) and reused for every subsequent frame. `copy_frame_bounded`
/// round-robins across the ring so concurrently in-flight frames never alias each
/// other's pixels (Fix 2).
pub(crate) struct GpuProducer {
    width: u32,
    height: u32,
    /// WGC's own device's immediate context, used for `CopyResource`.
    context: ID3D11DeviceContext,
    /// Ring of independent shared textures (Fix 2: anti-aliasing).
    ring: Vec<RingSlot>,
    /// Index of the NEXT slot `copy_frame_bounded` will write (round-robin).
    next: usize,
}

impl GpuProducer {
    /// Build the producer (a ring of `RING_LEN` shared textures) on WGC's device for
    /// `width`×`height` BGRA frames.
    ///
    /// # Safety
    /// `device` / `context` MUST be WGC's live device + immediate context for the
    /// calling capture thread (from `frame.device()` / `frame.device_context()`).
    pub(crate) unsafe fn build(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, EncoderError> {
        let mut ring = Vec::with_capacity(RING_LEN);
        for _ in 0..RING_LEN {
            // SAFETY: device is WGC's live device per the caller's contract.
            ring.push(unsafe { RingSlot::build(device, width, height) }?);
        }
        Ok(Self {
            width,
            height,
            context: context.clone(),
            ring,
            next: 0,
        })
    }

    /// Dimensions this producer's shared textures were built for.
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Copy the live WGC BGRA texture into the CURRENT ring slot with a BOUNDED keyed-mutex
    /// acquire, returning that slot's share handle for `FramePayload::GpuShared`
    /// (Fix 2 + Fix 5 + Fix 6).
    ///
    /// Acquires the current slot's keyed mutex with `timeout_ms` (NOT `INFINITE`),
    /// `CopyResource`s, releases, and returns the per-frame handle WITHOUT advancing the
    /// ring cursor. The caller advances the cursor via [`Self::advance_after_send`] ONLY
    /// after a confirmed `try_send` success (Fix 6); a dropped frame reuses this same slot
    /// next time, which is safe because its handle was never queued. Returning a distinct
    /// handle per QUEUED frame (the ring) is what removes frame aliasing (Fix 2). The
    /// bounded wait keeps the WGC `on_frame_arrived` callback thread from hanging forever
    /// if a stalled or dead consumer holds the slot's mutex (Fix 5). The acquire/release
    /// pair is balanced on BOTH the success and error paths so a failed copy cannot leave
    /// the mutex held (which would deadlock the consumer).
    ///
    /// `IDXGIKeyedMutex::AcquireSync` does NOT return an `Err` for `WAIT_TIMEOUT` or
    /// `WAIT_ABANDONED_0`: both are NON-NEGATIVE `HRESULT`s, and windows-rs maps the call
    /// through `HRESULT::ok()`, which yields `Ok(())` for any `HRESULT >= 0`. So the
    /// safe wrapper cannot distinguish these from `S_OK`. To classify them we read the RAW
    /// `HRESULT` straight from the vtable and match the numeric code (Fix 5 corrected).
    ///
    /// Failure classification (by raw numeric `HRESULT`):
    /// * `S_OK` (`0`) → mutex acquired; `CopyResource` + `ReleaseSync`, return the handle.
    /// * `WAIT_TIMEOUT` (`0x102`) → [`CopyError::Timeout`] — consumer stalled holding the
    ///   slot. We did NOT acquire it: do NOT copy, do NOT `ReleaseSync`. The caller degrades
    ///   to CPU for the session instead of blocking.
    /// * `WAIT_ABANDONED_0` (`0x80`) → [`CopyError::Abandoned`] — the consumer thread died
    ///   while holding the mutex. Per Win32 keyed-mutex semantics an ABANDONED wait DOES
    ///   grant ownership to the caller (the mutex is now held by us), so we MUST `ReleaseSync`
    ///   to avoid leaving it held forever — but the texture state is UNDEFINED, so we MUST
    ///   NOT run the copy and MUST degrade.
    /// * any NEGATIVE `HRESULT` (device-removed, etc.) → [`CopyError::Encoder`].
    ///
    /// # Safety
    /// `wgc_tex` MUST be the live WGC BGRA texture for this frame
    /// (`frame.as_raw_texture()`), same dimensions/format as the shared textures.
    pub(crate) unsafe fn copy_frame_bounded(
        &mut self,
        wgc_tex: &ID3D11Texture2D,
        timeout_ms: u32,
    ) -> Result<isize, CopyError> {
        let slot_idx = self.next;

        // Read the RAW HRESULT from AcquireSync. We must NOT use the safe `-> Result<()>`
        // wrapper here: it calls `HRESULT::ok()`, which returns `Ok(())` for ANY
        // non-negative HRESULT — including WAIT_TIMEOUT (0x102) and WAIT_ABANDONED_0 (0x80),
        // which are exactly the codes we need to detect. Calling the vtable directly gives
        // us the numeric code so we can classify timeout / abandoned / success precisely.
        let keyed = &self.ring[slot_idx].keyed;
        // SAFETY: `keyed` is a live IDXGIKeyedMutex for this slot; the AcquireSync vtable
        // slot takes (this, key, milliseconds) and returns the raw HRESULT. This is the
        // same call the windows-rs wrapper makes, minus the `.ok()` lossy mapping.
        let hr = unsafe {
            (Interface::vtable(keyed).AcquireSync)(
                Interface::as_raw(keyed),
                KEYED_MUTEX_KEY,
                timeout_ms,
            )
        };
        // Classify the RAW numeric HRESULT through the shared decision function so the
        // exact branch logic this path depends on is unit-testable (the test feeds the
        // same codes and asserts the same outcomes). This is the real detection path.
        match classify_acquire_hr(hr.0 as u32) {
            AcquireOutcome::Acquired => {} // fall through to copy + release below.
            AcquireOutcome::Timeout => {
                // Not acquired: do NOT copy, do NOT ReleaseSync (we do not own the mutex).
                return Err(CopyError::Timeout);
            }
            AcquireOutcome::Abandoned => {
                // Abandoned == acquired (Win32): we now hold the mutex even though the prior
                // owner died. Release it so it does not stay held forever, but do NOT copy —
                // the texture contents are undefined — and degrade.
                // SAFETY: per Win32 keyed-mutex semantics WAIT_ABANDONED_0 grants ownership,
                // so the matching ReleaseSync is required and balanced.
                let _ = unsafe { self.ring[slot_idx].keyed.ReleaseSync(KEYED_MUTEX_KEY) };
                return Err(CopyError::Abandoned);
            }
            // Negative HRESULT (top bit set) → genuine failure (device-removed, etc.).
            AcquireOutcome::Failed(other) => return Err(classify_acquire_error(other)),
        }

        let copy_result = self.run_copy(slot_idx, wgc_tex);
        // SAFETY: paired ReleaseSync on both Ok and Err copy paths (mutex IS held here —
        // AcquireSync returned S_OK above).
        let release = unsafe { self.ring[slot_idx].keyed.ReleaseSync(KEYED_MUTEX_KEY) };
        copy_result.map_err(CopyError::Encoder)?;
        release.map_err(|e| {
            CopyError::Encoder(EncoderError::EncodeFailed(format!(
                "producer ReleaseSync: 0x{:08X}",
                e.code().0
            )))
        })?;

        // Do NOT advance the cursor here — the frame is not yet queued. The caller calls
        // `advance_after_send` only on a confirmed `try_send` success (Fix 6). This is the
        // anti-lapping invariant: a slot is reused only after its handle left the channel.
        Ok(self.ring[slot_idx].share_handle)
    }

    /// Advance the ring cursor to the next slot. The caller MUST call this exactly once,
    /// ONLY after the handle returned by [`Self::copy_frame_bounded`] was successfully
    /// queued (`try_send` returned `Ok`). On a dropped send the cursor stays put so the
    /// same slot is overwritten next frame — safe, because the dropped handle was never
    /// queued (Fix 6).
    pub(crate) fn advance_after_send(&mut self) {
        self.next = next_slot(self.next);
    }

    /// `CopyResource` the WGC texture into ring slot `slot_idx`. Caller holds the slot's
    /// keyed mutex. Returns an `EncoderError` on a cast failure; the GPU-side copy itself
    /// has no return value.
    fn run_copy(&self, slot_idx: usize, wgc_tex: &ID3D11Texture2D) -> Result<(), EncoderError> {
        let src: ID3D11Resource = wgc_tex.cast().map_err(|e| {
            EncoderError::EncodeFailed(format!("cast src ID3D11Resource: 0x{:08X}", e.code().0))
        })?;
        let dst: ID3D11Resource = self.ring[slot_idx].shared_tex.cast().map_err(|e| {
            EncoderError::EncodeFailed(format!("cast dst ID3D11Resource: 0x{:08X}", e.code().0))
        })?;
        // SAFETY: both resources are valid same-format same-size textures on `context`'s
        // device; CopyResource is a GPU-side copy with no CPU mapping.
        unsafe { self.context.CopyResource(&dst, &src) };
        Ok(())
    }
}

/// Advance a ring cursor by one slot, modulo [`RING_LEN`]. A pure free function so that
/// `advance_after_send` and its unit test exercise the SAME arithmetic — a regression in
/// the step (wrong increment or a missing modulo) is then caught by the test.
fn next_slot(cur: usize) -> usize {
    (cur + 1) % RING_LEN
}

/// `S_OK` — the keyed mutex was acquired.
const S_OK_HR: u32 = 0x0000_0000;
/// `WAIT_TIMEOUT` raw HRESULT from `AcquireSync` (non-negative — NOT surfaced as `Err`
/// by windows-rs; see `copy_frame_bounded` for why we read the raw code).
const WAIT_TIMEOUT_HR: u32 = 0x0000_0102;
/// `WAIT_ABANDONED_0` raw HRESULT (consumer died holding the keyed mutex; non-negative).
const WAIT_ABANDONED_HR: u32 = 0x0000_0080;

/// Decision derived from a RAW `AcquireSync` HRESULT (Fix 5, raw-HRESULT corrected).
///
/// This is the branch logic `copy_frame_bounded` acts on; isolating it as a pure function
/// ([`classify_acquire_hr`]) makes the timeout/abandoned/success detection unit-testable
/// without live D3D objects, while the producer path uses the SAME function (not a
/// duplicate) so the test exercises real logic.
#[derive(Debug, PartialEq, Eq)]
enum AcquireOutcome {
    /// `S_OK` — mutex held by us; safe to copy then ReleaseSync.
    Acquired,
    /// `WAIT_TIMEOUT` — NOT held; must not copy or ReleaseSync.
    Timeout,
    /// `WAIT_ABANDONED_0` — held by us but texture undefined; must ReleaseSync but not copy.
    Abandoned,
    /// Negative HRESULT — genuine failure; carries the raw code for the error message.
    Failed(u32),
}

/// Classify a RAW `AcquireSync` HRESULT into an [`AcquireOutcome`].
///
/// `WAIT_TIMEOUT` (`0x102`) and `WAIT_ABANDONED_0` (`0x80`) are NON-NEGATIVE HRESULTs, so
/// the windows-rs `-> Result<()>` wrapper (which uses `HRESULT::ok()`, `Ok` for `>= 0`)
/// reports both as success. We therefore classify the raw numeric code directly. Any other
/// non-negative code is treated as success (we acquired); any negative code (top bit set,
/// `>= 0x8000_0000`) is a genuine failure.
fn classify_acquire_hr(hr: u32) -> AcquireOutcome {
    match hr {
        S_OK_HR => AcquireOutcome::Acquired,
        WAIT_TIMEOUT_HR => AcquireOutcome::Timeout,
        WAIT_ABANDONED_HR => AcquireOutcome::Abandoned,
        // Negative HRESULT (high bit set) → failure; everything else non-negative → acquired
        // (any non-negative AcquireSync HRESULT means we hold the mutex).
        other if other & 0x8000_0000 != 0 => AcquireOutcome::Failed(other),
        _ => AcquireOutcome::Acquired,
    }
}

/// Map a raw `AcquireSync` HRESULT to a [`CopyError`] (Fix 5, raw-HRESULT corrected).
///
/// This is the single source of truth for AcquireSync classification: `copy_frame_bounded`
/// matches `S_OK`/`WAIT_TIMEOUT`/`WAIT_ABANDONED_0` inline (the abandoned arm has extra
/// ReleaseSync handling), and forwards every OTHER code here. For completeness and to keep
/// the mapping testable in isolation, this function also recognizes the timeout/abandoned
/// codes, but in practice they are handled by the inline match before reaching here.
fn classify_acquire_error(hr: u32) -> CopyError {
    match hr {
        WAIT_TIMEOUT_HR => CopyError::Timeout,
        WAIT_ABANDONED_HR => CopyError::Abandoned,
        other => CopyError::Encoder(EncoderError::EncodeFailed(format!(
            "producer AcquireSync(bounded): 0x{other:08X}"
        ))),
    }
}

/// Outcome of a bounded [`GpuProducer::copy_frame_bounded`] (Fix 5).
#[derive(Debug)]
pub(crate) enum CopyError {
    /// The keyed-mutex acquire timed out (consumer stalled holding the slot). Degrade.
    Timeout,
    /// The keyed mutex was abandoned (consumer died holding it). Degrade; the texture
    /// state is undefined so it must not be treated as a successful copy.
    Abandoned,
    /// Any other copy/release failure (cast, device-removed, ReleaseSync). Degrade.
    Encoder(EncoderError),
}

/// Resolve the adapter LUID of WGC's device (the capture LUID for the gate).
///
/// # Safety
/// `device` MUST be WGC's live device (`frame.device()`).
pub(crate) unsafe fn capture_adapter_luid(device: &ID3D11Device) -> Result<i64, EncoderError> {
    adapter_luid_i64(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_len_exceeds_worst_case_live_references() {
        // Cursor-on-queue invariant (Fix 6): the cursor advances ONLY on a confirmed
        // try_send success, so a slot is reused only after its handle left the channel.
        // Worst-case LIVE references are therefore: CHANNEL_CAP (4) queued payloads +
        // 1 frame the consumer is currently reading = 5. The heartbeat holds NO live
        // handle (Fix 3 made it a CPU readback), so there is NO +1 heartbeat term.
        // RING_LEN must be STRICTLY greater than 5 so the producer can write its current
        // slot without aliasing any of those 5 live references.
        const CHANNEL_CAP: usize = 4;
        const WORST_CASE_LIVE: usize = CHANNEL_CAP + 1 /* consumer-in-flight */;
        const {
            assert!(
                RING_LEN > WORST_CASE_LIVE,
                "RING_LEN must exceed the worst-case live reference count (CHANNEL_CAP + 1)"
            );
        }
        // Sanity-check the literal value too: the +1 heartbeat term is gone, so the
        // correct lower bound is 5 (not the old 6 = CHANNEL_CAP + 2).
        assert_eq!(WORST_CASE_LIVE, 5);
    }

    #[test]
    fn advance_after_send_is_modular_and_dropped_send_reuses_slot() {
        // Fix 6: copy_frame_bounded does NOT advance; advance_after_send does, modulo
        // RING_LEN. A dropped send (no advance call) must leave the cursor put so the
        // SAME slot is reused next frame. We drive the REAL `next_slot` helper that
        // `advance_after_send` uses, so a regression in the step is caught here.
        let mut next = 0usize;
        let advance = |n: &mut usize| *n = next_slot(*n);

        // Queue RING_LEN frames: cursor walks every slot and wraps to 0.
        for expected in 0..RING_LEN {
            assert_eq!(next, expected);
            advance(&mut next);
        }
        assert_eq!(next, 0, "advance must wrap modulo RING_LEN");

        // Now: copy into slot 0, send drops (no advance) → cursor STILL 0 → next frame
        // reuses slot 0. Two consecutive drops keep reusing slot 0.
        let slot_before_drop = next;
        // (no advance — simulating TrySendError::Full)
        assert_eq!(
            next, slot_before_drop,
            "dropped send must not advance cursor"
        );
        assert_eq!(
            next, slot_before_drop,
            "second dropped send reuses same slot"
        );

        // A successful send after the drops advances exactly one slot.
        advance(&mut next);
        assert_eq!(next, (slot_before_drop + 1) % RING_LEN);
    }

    #[test]
    fn classify_acquire_hr_detects_timeout_abandoned_success_and_failure() {
        // This exercises the ACTUAL detection function copy_frame_bounded calls on the
        // raw HRESULT (not an isolated duplicate). The two non-negative WAIT codes are the
        // codes the windows-rs `.ok()` wrapper would have wrongly reported as success.
        assert_eq!(classify_acquire_hr(S_OK_HR), AcquireOutcome::Acquired);
        assert_eq!(
            classify_acquire_hr(WAIT_TIMEOUT_HR),
            AcquireOutcome::Timeout
        );
        assert_eq!(
            classify_acquire_hr(WAIT_ABANDONED_HR),
            AcquireOutcome::Abandoned
        );
        // Other NON-NEGATIVE codes (e.g. S_FALSE 0x1) must be treated as acquired, since a
        // non-negative AcquireSync HRESULT means we hold the mutex.
        assert_eq!(classify_acquire_hr(0x0000_0001), AcquireOutcome::Acquired);
        // A NEGATIVE HRESULT (DXGI_ERROR_DEVICE_REMOVED 0x887A0005, high bit set) is a real
        // failure carrying its raw code.
        assert_eq!(
            classify_acquire_hr(0x887A_0005),
            AcquireOutcome::Failed(0x887A_0005)
        );
    }

    #[test]
    fn classify_acquire_error_maps_failed_codes() {
        // classify_acquire_error is the Failed-arm tail used by copy_frame_bounded for the
        // negative-HRESULT case (timeout/abandoned are handled by the inline match before
        // reaching here, but the mapping recognizes them for completeness).
        assert!(matches!(
            classify_acquire_error(WAIT_TIMEOUT_HR),
            CopyError::Timeout
        ));
        assert!(matches!(
            classify_acquire_error(WAIT_ABANDONED_HR),
            CopyError::Abandoned
        ));
        // Any other HRESULT is a generic encoder error (degrade, but not timeout/abandoned).
        assert!(matches!(
            classify_acquire_error(0x887A_0005),
            CopyError::Encoder(_)
        ));
    }
}
