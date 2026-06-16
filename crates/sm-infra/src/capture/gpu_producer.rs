#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Capture-thread GPU producer for the GPU-resident hand-off (design D1, TASK-08).
//!
//! When the path-selection gate selects the GPU-resident path, the WGC capture
//! thread does NOT read pixels back to the CPU. Instead it `CopyResource`s the live
//! WGC BGRA texture into a reusable shared `ID3D11Texture2D` created with
//! `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`, then hands the texture's NT share handle
//! to the encoder thread through the channel (`FramePayload::GpuShared`). The
//! encoder opens the handle on its own same-adapter device and reads the texture
//! under the same keyed mutex.
//!
//! All COM objects here are created on, and used only from, the WGC capture thread
//! (the thread that runs `on_frame_arrived`). The keyed-mutex texture's share handle
//! is the ONLY thing that crosses to the encoder thread.

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

/// Owns the capture-thread side of the keyed-mutex hand-off: one reusable shared
/// BGRA texture (+ its NT share handle and keyed mutex) created on WGC's device.
///
/// Created lazily on the first GPU frame (once the real capture dimensions and WGC's
/// device are known) and reused for every subsequent frame.
pub(crate) struct GpuProducer {
    width: u32,
    height: u32,
    /// WGC's own device (NOT owned — borrowed each frame; we keep the immediate
    /// context for `CopyResource`).
    context: ID3D11DeviceContext,
    /// Reusable shared BGRA destination texture (keyed-mutex, NT-handle sharable).
    shared_tex: ID3D11Texture2D,
    /// Keyed mutex guarding `shared_tex` against the encoder-thread consumer.
    keyed: IDXGIKeyedMutex,
    /// NT share handle for `shared_tex`, stored as `isize` so `GpuProducer` stays
    /// `Send` (a raw `*mut c_void` HANDLE is not `Send`). Reconstructed into `HANDLE`
    /// only to send (as the same `isize`) and to close on drop. The producer + this
    /// handle live entirely on the WGC capture thread; the value (an `isize`) is what
    /// crosses to the encoder, which opens its own reference via `OpenSharedResource1`.
    share_handle: isize,
}

impl GpuProducer {
    /// Build the producer on WGC's device for `width`×`height` BGRA frames.
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
            width,
            height,
            context: context.clone(),
            shared_tex,
            keyed,
            share_handle: share_handle.0 as isize,
        })
    }

    /// Dimensions this producer's shared texture was built for.
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The NT share handle as an `isize` for `FramePayload::GpuShared`.
    ///
    /// The handle stays owned by this `GpuProducer` (closed on drop); the encoder
    /// opens its OWN reference via `OpenSharedResource1`, so the value can be sent
    /// every frame without transferring ownership.
    pub(crate) fn share_handle(&self) -> isize {
        self.share_handle
    }

    /// Copy the live WGC BGRA texture into the shared keyed-mutex texture.
    ///
    /// Acquires the keyed mutex (blocking until the encoder released the previous
    /// frame), `CopyResource`s, and releases — the producer half of the D1 hand-off.
    /// The acquire/release pair is balanced on BOTH the success and error paths so a
    /// failed copy cannot leave the mutex held (which would deadlock the consumer).
    ///
    /// # Safety
    /// `wgc_tex` MUST be the live WGC BGRA texture for this frame
    /// (`frame.as_raw_texture()`), same dimensions/format as the shared texture.
    pub(crate) unsafe fn copy_frame(&self, wgc_tex: &ID3D11Texture2D) -> Result<(), EncoderError> {
        // SAFETY: AcquireSync blocks until the consumer's ReleaseSync (or succeeds
        // immediately on the first frame, when the mutex starts unlocked at key 0).
        unsafe { self.keyed.AcquireSync(KEYED_MUTEX_KEY, u32::MAX) }.map_err(|e| {
            EncoderError::EncodeFailed(format!("producer AcquireSync: 0x{:08X}", e.code().0))
        })?;
        // CopyResource has no return value; it cannot fail at the API level for two
        // matching DEFAULT textures (driver removal surfaces later via the encoder).
        // SAFETY: both resources are valid same-format same-size textures on `context`'s
        // device; CopyResource is a GPU-side copy with no CPU mapping.
        let copy_result = (|| -> Result<(), EncoderError> {
            let src: ID3D11Resource = wgc_tex.cast().map_err(|e| {
                EncoderError::EncodeFailed(format!("cast src ID3D11Resource: 0x{:08X}", e.code().0))
            })?;
            let dst: ID3D11Resource = self.shared_tex.cast().map_err(|e| {
                EncoderError::EncodeFailed(format!("cast dst ID3D11Resource: 0x{:08X}", e.code().0))
            })?;
            unsafe { self.context.CopyResource(&dst, &src) };
            Ok(())
        })();
        // SAFETY: ReleaseSync is paired with the AcquireSync above; it MUST run on
        // both the Ok and Err copy paths so the consumer's AcquireSync does not
        // deadlock. The consumer acquires with the same KEYED_MUTEX_KEY.
        let release = unsafe { self.keyed.ReleaseSync(KEYED_MUTEX_KEY) };
        copy_result?;
        release.map_err(|e| {
            EncoderError::EncodeFailed(format!("producer ReleaseSync: 0x{:08X}", e.code().0))
        })
    }
}

impl Drop for GpuProducer {
    fn drop(&mut self) {
        let handle = HANDLE(self.share_handle as *mut core::ffi::c_void);
        if !handle.is_invalid() {
            // SAFETY: share_handle is an owned NT handle from CreateSharedHandle;
            // CloseHandle releases it exactly once (guarded by is_invalid()).
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
        // shared_tex / keyed / context drop here → COM Release.
    }
}

/// Resolve the adapter LUID of WGC's device (the capture LUID for the gate).
///
/// # Safety
/// `device` MUST be WGC's live device (`frame.device()`).
pub(crate) unsafe fn capture_adapter_luid(device: &ID3D11Device) -> Result<i64, EncoderError> {
    adapter_luid_i64(device)
}
