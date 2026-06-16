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
//! this producer owns a RING of shared textures. Each `copy_frame` advances to the
//! next slot and returns THAT slot's share handle, so every in-flight payload
//! references its own pixels. The ring is sized so the producer cannot lap a slot
//! that is still referenced by a queued payload, the heartbeat snapshot, or the
//! frame the consumer is currently reading.
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

/// Number of shared textures in the producer ring (Fix 2).
///
/// Must exceed the maximum number of share handles that can reference distinct
/// in-flight frames at once so the producer never overwrites a texture whose handle
/// is still queued or in-use. Worst-case live references:
///
/// - `CHANNEL_CAP` (4) queued payloads, plus
/// - 1 frame the consumer is currently reading, plus
/// - 1 handle held in the heartbeat snapshot (`last_frame`)
///   = 6 worst-case live references.
///
/// `CHANNEL_CAP` is the capture→encoder channel capacity in
/// `src-tauri/src/commands/sender.rs` (4). If that changes, bump this accordingly.
pub(crate) const RING_LEN: usize = 6;

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

        let keyed: IDXGIKeyedMutex = shared_tex.cast().map_err(|e| {
            EncoderError::InitFailed(format!("cast IDXGIKeyedMutex: 0x{:08X}", e.code().0))
        })?;

        Ok(Self {
            shared_tex,
            keyed,
            share_handle: share_handle.0 as isize,
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

    /// Copy the live WGC BGRA texture into the NEXT ring slot with a BOUNDED keyed-mutex
    /// acquire, returning that slot's share handle for `FramePayload::GpuShared`
    /// (Fix 2 + Fix 5).
    ///
    /// Acquires the slot's keyed mutex with `timeout_ms` (NOT `INFINITE`), `CopyResource`s,
    /// releases, advances the round-robin cursor, and returns the per-frame handle.
    /// Returning a distinct handle per frame (the round-robin ring) is what removes frame
    /// aliasing (Fix 2). The bounded wait is what keeps the WGC `on_frame_arrived` callback
    /// thread from hanging forever if a stalled or dead consumer holds the slot's mutex
    /// (Fix 5). The acquire/release pair is balanced on BOTH the success and error paths
    /// so a failed copy cannot leave the mutex held (which would deadlock the consumer).
    ///
    /// Failure classification:
    /// * `WAIT_TIMEOUT` (`0x102`) → [`CopyError::Timeout`] — consumer stalled holding the
    ///   slot; the caller degrades to CPU for the session instead of blocking.
    /// * `WAIT_ABANDONED_0` (`0x80`) → [`CopyError::Abandoned`] — the consumer thread died
    ///   while holding the mutex. windows-rs surfaces this as an `Err` HRESULT (the wait
    ///   did not return `S_OK`); we DO NOT treat the slot as acquired (an abandoned mutex
    ///   leaves the texture in an undefined state), and the caller degrades.
    /// * any other HRESULT (cast, device-removed, ReleaseSync) → [`CopyError::Encoder`].
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

        // SAFETY: bounded AcquireSync. Any non-S_OK result (WAIT_TIMEOUT, WAIT_ABANDONED,
        // device-removed) surfaces as Err; we classify and degrade rather than block or
        // assume ownership of an abandoned/contended mutex.
        let acquire = unsafe {
            self.ring[slot_idx]
                .keyed
                .AcquireSync(KEYED_MUTEX_KEY, timeout_ms)
        };
        if let Err(e) = acquire {
            let hr = e.code().0 as u32;
            return Err(classify_acquire_error(hr));
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

        let handle = self.ring[slot_idx].share_handle;
        self.next = (self.next + 1) % RING_LEN;
        Ok(handle)
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

/// `WAIT_TIMEOUT` HRESULT-packed value as windows-rs reports it from `AcquireSync`.
const WAIT_TIMEOUT_HR: u32 = 0x0000_0102;
/// `WAIT_ABANDONED_0` packed value (consumer died holding the keyed mutex).
const WAIT_ABANDONED_HR: u32 = 0x0000_0080;

/// Classify a non-`S_OK` `AcquireSync` HRESULT into a [`CopyError`] (Fix 5).
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
        // CHANNEL_CAP (4) queued + 1 consumer-in-use + 1 heartbeat snapshot = 6.
        // RING_LEN must be at least that so the producer never laps a live slot.
        // A const assertion makes this a compile-time guard against shrinking the ring
        // below the channel capacity without re-evaluating the aliasing argument.
        const CHANNEL_CAP: usize = 4;
        const WORST_CASE_LIVE: usize = CHANNEL_CAP + 1 /* consumer */ + 1 /* heartbeat */;
        const {
            assert!(
                RING_LEN >= WORST_CASE_LIVE,
                "RING_LEN must cover the worst-case live reference count (CHANNEL_CAP + 2)"
            );
        }
    }

    #[test]
    fn classify_acquire_error_maps_timeout_and_abandoned() {
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
