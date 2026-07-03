#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Asynchronous GPU→CPU readback for the GPU-resident heartbeat snapshot (judder Fix 7).
//!
//! # Why this exists
//!
//! On the GPU-resident path the static-content heartbeat needs a CPU-backed copy of the
//! last frame to re-inject when the screen freezes (see `HeartbeatFrame::Cpu` in
//! `windows.rs`). The previous implementation produced that copy with the `windows-capture`
//! crate's `frame.buffer()`, which performs a SYNCHRONOUS, BLOCKING GPU→CPU readback ON the
//! WGC `on_frame_arrived` callback thread: it creates a `D3D11_USAGE_STAGING` texture,
//! `CopyResource`s into it, then `Map`s it WITHOUT `D3D11_MAP_FLAG_DO_NOT_WAIT` — a full GPU
//! pipeline flush + CPU stall. Because the WGC frame pool is size 1, that stall delayed the
//! NEXT frame's delivery and produced non-uniform capture cadence (visible judder), and it
//! ran ~10x/sec even during active motion when the heartbeat never fires — pure waste.
//!
//! # The fix
//!
//! This type owns its OWN pool of TWO (ping-pong) `D3D11_USAGE_STAGING` textures (CPU read
//! access) and performs the readback ASYNCHRONOUSLY so it NEVER blocks the callback thread:
//!
//! * **Issue** a non-blocking `CopyResource(staging[write], wgc_tex)` (GPU submit, returns
//!   immediately). Mark that slot pending.
//! * **Try-map** the OTHER slot — the one a PREVIOUS readback wrote (if pending) — with
//!   `Map(.., D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, ..)`. If it returns
//!   `DXGI_ERROR_WAS_STILL_DRAWING` the GPU copy has not finished: we SKIP (no stall, the
//!   snapshot stays as-is). If `Ok`, we copy the pixels into a tight BGRA buffer (respecting
//!   `RowPitch`, row by row) and `Unmap`. The indices ping-pong.
//!
//! The snapshot is therefore at most ~1 readback-interval (~100ms) stale, which is fine:
//! the heartbeat consumes it only when the screen is static, and a slightly-stale snapshot
//! of a static screen equals the current frame.
//!
//! All COM objects here are created on, and used only from, the WGC capture thread (the
//! thread that runs `on_frame_arrived`), so the immediate context is used single-threaded.

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows::core::Interface;

use sm_domain::encode::EncoderError;

/// Number of staging textures in the ping-pong pool. Two is sufficient: one slot is being
/// written by the current readback's `CopyResource` while the OTHER (written one interval
/// ago) is try-mapped — so the map never targets a copy that was issued this same call.
const PINGPONG_LEN: usize = 2;

/// Pure ping-pong index advance: given the slot just WRITTEN, return the slot to READ next
/// time. With two slots this is simply the other index. Isolated as a free function so the
/// alternation is unit-testable without live D3D (a regression that mapped the slot just
/// written — reading a copy issued THIS call, defeating the async-ness — is caught here).
#[inline]
fn other_slot(idx: usize) -> usize {
    (idx + 1) % PINGPONG_LEN
}

/// Pure decision: do the staging textures need to be (re)created for `(width, height)`?
///
/// Returns `true` when there is no cached size yet (first frame) or the frame dimensions
/// changed (resolution change). Isolated so the recreate-on-resize choice is unit-testable
/// without allocating D3D textures.
#[inline]
fn staging_needs_rebuild(cached: Option<(u32, u32)>, width: u32, height: u32) -> bool {
    cached != Some((width, height))
}

/// Copy `height` rows of `width * 4` BGRA bytes from a mapped staging surface (whose source
/// stride is `row_pitch`, which may exceed `width * 4`) into a TIGHT destination buffer
/// (stride exactly `width * 4`). Mirrors how `windows-capture`'s `FrameBuffer` strips row
/// padding (`as_nopadding_buffer`): copy row by row, advancing the source by `row_pitch` and
/// the destination by `width * 4`.
///
/// `src` must contain at least `height * row_pitch` bytes; `dst` is resized to exactly
/// `height * width * 4`. A pure slice-to-slice copy so the pitch handling is unit-testable
/// without a real mapped GPU surface. Returns the tight row width (`width * 4`) actually
/// copied per row, for the caller to record as the snapshot stride.
fn copy_rows_tight(src: &[u8], dst: &mut Vec<u8>, width: u32, height: u32, row_pitch: u32) -> u32 {
    let tight_stride = (width as usize) * 4;
    let pitch = row_pitch as usize;
    let rows = height as usize;
    dst.clear();
    dst.reserve(tight_stride * rows);
    for y in 0..rows {
        let start = y * pitch;
        // Defensive: never read past the mapped slice. A correctly-sized staging surface
        // always satisfies `start + tight_stride <= src.len()`, but clamp rather than panic
        // on a short/odd buffer (best-effort snapshot; a partial row is harmless).
        let end = start.saturating_add(tight_stride);
        if end <= src.len() {
            dst.extend_from_slice(&src[start..end]);
        } else if start < src.len() {
            dst.extend_from_slice(&src[start..]);
            break;
        } else {
            break;
        }
    }
    tight_stride as u32
}

/// One ping-pong staging texture plus whether it currently holds a GPU copy that has not yet
/// been mapped/consumed.
struct StagingSlot {
    tex: ID3D11Texture2D,
    /// `true` once a `CopyResource` has targeted this slot and it has NOT yet been mapped.
    /// A slot is only try-mapped when pending; a successful (or still-drawing) map path
    /// leaves it consumed on success and pending on still-drawing.
    pending: bool,
}

/// Owns the WGC-side asynchronous heartbeat readback: a ping-pong pool of
/// `D3D11_USAGE_STAGING` textures (CPU read) + the WGC device's immediate context.
///
/// Built lazily on the first GPU frame and rebuilt if the frame dimensions change. All use
/// is single-threaded on the WGC callback thread.
pub(crate) struct AsyncReadback {
    /// WGC's device, used to (re)create the staging textures.
    device: ID3D11Device,
    /// WGC's immediate context, used for `CopyResource` / `Map` / `Unmap`.
    context: ID3D11DeviceContext,
    /// Ping-pong staging textures. `None` until the first `readback` call (or after a
    /// dimension change forces a rebuild on the next call).
    slots: Option<[StagingSlot; PINGPONG_LEN]>,
    /// Dimensions the current `slots` were created for. `None` when `slots` is `None`.
    dims: Option<(u32, u32)>,
    /// Index of the slot the NEXT `CopyResource` will write. The OTHER slot is the one a
    /// previous readback wrote and that this call try-maps.
    write_idx: usize,
}

impl AsyncReadback {
    /// Create a readback owner bound to WGC's device + immediate context. The staging
    /// textures are created lazily on the first `readback` call (when real dims are known).
    ///
    /// # Safety
    /// `device` / `context` MUST be WGC's live device + immediate context for the calling
    /// capture thread (from `frame.device()` / `frame.device_context()`), the SAME device
    /// that owns the frame texture passed to `readback` — `CopyResource` requires both
    /// resources to live on the same device.
    pub(crate) unsafe fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Self {
        Self {
            device: device.clone(),
            context: context.clone(),
            slots: None,
            dims: None,
            write_idx: 0,
        }
    }

    /// Create the ping-pong staging textures for `width`×`height` BGRA frames.
    ///
    /// `D3D11_USAGE_STAGING` + `D3D11_CPU_ACCESS_READ`: the destination of a GPU
    /// `CopyResource` that the CPU later `Map`s for reading.
    fn build_slots(
        &self,
        width: u32,
        height: u32,
    ) -> Result<[StagingSlot; PINGPONG_LEN], EncoderError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut built: Vec<StagingSlot> = Vec::with_capacity(PINGPONG_LEN);
        for _ in 0..PINGPONG_LEN {
            let mut tex: Option<ID3D11Texture2D> = None;
            // SAFETY: desc is fully initialized; None initial data is valid for a STAGING
            // texture. Out-param written on Ok.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| {
                EncoderError::InitFailed(format!(
                    "CreateTexture2D(staging readback): 0x{:08X}",
                    e.code().0
                ))
            })?;
            let tex = tex.ok_or_else(|| {
                EncoderError::InitFailed("CreateTexture2D(staging readback) returned null".into())
            })?;
            built.push(StagingSlot {
                tex,
                pending: false,
            });
        }
        // Exactly PINGPONG_LEN were pushed; convert to a fixed array.
        let mut iter = built.into_iter();
        let s0 = iter
            .next()
            .ok_or_else(|| EncoderError::InitFailed("staging pool under-built (slot 0)".into()))?;
        let s1 = iter
            .next()
            .ok_or_else(|| EncoderError::InitFailed("staging pool under-built (slot 1)".into()))?;
        Ok([s0, s1])
    }

    /// Ensure the staging pool exists and matches `(width, height)`, rebuilding on a
    /// dimension change. Resets the ping-pong state on a (re)build.
    fn ensure_slots(&mut self, width: u32, height: u32) -> Result<(), EncoderError> {
        if staging_needs_rebuild(self.dims, width, height) {
            // SAFETY (build_slots uses self.device, WGC's live device per `new`'s contract).
            self.slots = Some(self.build_slots(width, height)?);
            self.dims = Some((width, height));
            self.write_idx = 0;
        }
        Ok(())
    }

    /// Perform ONE asynchronous readback step against the live WGC frame texture.
    ///
    /// Issues a non-blocking `CopyResource` into the current write slot, then try-maps the
    /// OTHER slot (written by a previous call). On a successful map the pixels are copied
    /// into `out` (a reusable buffer the caller owns) row by row respecting `RowPitch`, the
    /// slot is unmapped, and `Some(width * 4)` (the tight stride) is returned. On
    /// `DXGI_ERROR_WAS_STILL_DRAWING` the previous copy is not finished — returns `None`
    /// WITHOUT stalling (the caller keeps the prior snapshot). On any other error the step
    /// fails safe (returns `None`, no degrade — the heartbeat snapshot is best-effort).
    ///
    /// `out` is cleared and refilled with exactly `height * width * 4` BGRA bytes on the
    /// success path; it is left untouched on the still-drawing / first-frame / error paths.
    ///
    /// # Safety
    /// `wgc_tex` MUST be the live WGC BGRA texture for this frame (`frame.as_raw_texture()`),
    /// living on the SAME device this readback was constructed with. `width`/`height` must be
    /// the frame's current dimensions.
    pub(crate) unsafe fn readback(
        &mut self,
        wgc_tex: &ID3D11Texture2D,
        width: u32,
        height: u32,
        out: &mut Vec<u8>,
    ) -> Option<u32> {
        // Lazily (re)create the staging pool; on failure fail safe (no snapshot this call).
        if let Err(e) = self.ensure_slots(width, height) {
            tracing::warn!(
                target: "sm_infra::capture::gpu_readback",
                "staging pool build failed ({e}); skipping heartbeat readback this frame"
            );
            return None;
        }
        let slots = self.slots.as_mut()?;

        // Issue the non-blocking GPU copy into the write slot. `CopyResource` only SUBMITS
        // the copy; it does not wait for completion, so this returns immediately.
        let write_idx = self.write_idx;
        let read_idx = other_slot(write_idx);
        let src: ID3D11Resource = match wgc_tex.cast() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "sm_infra::capture::gpu_readback",
                    "cast WGC texture to ID3D11Resource failed (0x{:08X}); skipping readback",
                    e.code().0
                );
                return None;
            }
        };
        let dst: ID3D11Resource = match slots[write_idx].tex.cast() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "sm_infra::capture::gpu_readback",
                    "cast staging texture to ID3D11Resource failed (0x{:08X}); skipping readback",
                    e.code().0
                );
                return None;
            }
        };
        // SAFETY: both resources are same-format same-size BGRA textures on this context's
        // device; CopyResource is a GPU-side submit with no CPU mapping.
        unsafe { self.context.CopyResource(&dst, &src) };
        slots[write_idx].pending = true;
        // Ping-pong: the next call writes the other slot and try-maps the one we just wrote.
        self.write_idx = read_idx;

        // Try-map the OTHER slot (written by a PREVIOUS call). If it was never written
        // (first frame), there is nothing to read yet.
        if !slots[read_idx].pending {
            return None;
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: read slot's texture is a live STAGING texture on this context; the
        // DO_NOT_WAIT flag makes the Map non-blocking — it returns DXGI_ERROR_WAS_STILL_DRAWING
        // instead of stalling if the prior CopyResource has not completed.
        let map_res = unsafe {
            self.context.Map(
                &slots[read_idx].tex,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        };
        match map_res {
            Ok(()) => {
                let row_pitch = mapped.RowPitch;
                let total = (height as usize).saturating_mul(row_pitch as usize);
                // SAFETY: Map succeeded, so pData points to at least height*RowPitch readable
                // bytes for this STAGING surface. We only READ from this slice.
                let stride = {
                    let src_slice =
                        unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), total) };
                    copy_rows_tight(src_slice, out, width, height, row_pitch)
                };
                // Balanced Unmap (Map returned Ok above — the only path that maps).
                // SAFETY: paired with the successful Map on the same slot/subresource.
                unsafe { self.context.Unmap(&slots[read_idx].tex, 0) };
                // Slot consumed; it is no longer pending (the next CopyResource re-arms it).
                slots[read_idx].pending = false;
                Some(stride)
            }
            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                // GPU copy not finished — do NOT block. Nothing was mapped, so nothing to
                // Unmap. Keep the slot pending so the NEXT call retries the map.
                None
            }
            Err(e) => {
                // Any other map error: fail safe (best-effort snapshot). Nothing was mapped,
                // so nothing to Unmap. Drop the pending flag so we do not retry a bad slot
                // forever; the next CopyResource re-arms it.
                tracing::warn!(
                    target: "sm_infra::capture::gpu_readback",
                    "staging Map failed (0x{:08X}); skipping heartbeat snapshot this frame",
                    e.code().0
                );
                slots[read_idx].pending = false;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_slot_alternates_between_the_two_slots() {
        // The async-ness depends on the map ALWAYS targeting the slot the PREVIOUS call
        // wrote, never the one this call wrote. With two slots that is the other index.
        assert_eq!(other_slot(0), 1, "slot 0's partner is slot 1");
        assert_eq!(other_slot(1), 0, "slot 1's partner is slot 0");
        // Walking the ping-pong forward must visit 0,1,0,1,... — never the slot just written.
        let mut write = 0usize;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let read = other_slot(write);
            assert_ne!(read, write, "must never map the slot just written");
            seen.push(write);
            write = read; // next call writes what we just read (ping-pong)
        }
        assert_eq!(seen, vec![0, 1, 0, 1, 0, 1], "writes must alternate");
    }

    #[test]
    fn staging_rebuild_only_on_first_frame_or_resize() {
        // First frame (no cached size) always rebuilds.
        assert!(
            staging_needs_rebuild(None, 1920, 1080),
            "no cached dims → must build"
        );
        // Same dims → no rebuild (reuse the existing pool, the common per-frame case).
        assert!(
            !staging_needs_rebuild(Some((1920, 1080)), 1920, 1080),
            "unchanged dims must NOT rebuild"
        );
        // A width change (resolution change) → rebuild.
        assert!(
            staging_needs_rebuild(Some((1920, 1080)), 2560, 1080),
            "width change must rebuild"
        );
        // A height change → rebuild.
        assert!(
            staging_needs_rebuild(Some((1920, 1080)), 1920, 1440),
            "height change must rebuild"
        );
    }

    #[test]
    fn copy_rows_tight_strips_row_padding_into_a_tight_buffer() {
        // 2x2 BGRA frame (width*4 = 8 bytes/row) with a padded source pitch of 12 bytes.
        // The copy must drop the 4 padding bytes per row and produce a tight 16-byte buffer.
        let width = 2u32;
        let height = 2u32;
        let row_pitch = 12u32; // 8 real + 4 padding
        // Row 0 = [0..8] real, [8..12] padding; Row 1 = [12..20] real, [20..24] padding.
        let mut src = vec![0u8; (height * row_pitch) as usize];
        for (i, b) in src.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut dst = Vec::new();
        let stride = copy_rows_tight(&src, &mut dst, width, height, row_pitch);
        assert_eq!(stride, 8, "tight stride must be width*4");
        assert_eq!(
            dst.len(),
            16,
            "tight buffer must be height*width*4, padding stripped"
        );
        // Row 0 real bytes are src[0..8]; Row 1 real bytes are src[12..20].
        assert_eq!(&dst[0..8], &src[0..8], "row 0 real pixels preserved");
        assert_eq!(
            &dst[8..16],
            &src[12..20],
            "row 1 real pixels preserved (padding skipped)"
        );
    }

    #[test]
    fn copy_rows_tight_handles_unpadded_pitch() {
        // When row_pitch == width*4 there is no padding; the copy is a straight memcpy.
        let width = 3u32;
        let height = 2u32;
        let row_pitch = width * 4; // 12, no padding
        let src: Vec<u8> = (0..(height * row_pitch) as u8).collect();
        let mut dst = Vec::new();
        let stride = copy_rows_tight(&src, &mut dst, width, height, row_pitch);
        assert_eq!(stride, 12);
        assert_eq!(
            dst.len(),
            24,
            "no padding → dst length equals source length"
        );
        assert_eq!(dst.as_slice(), src.as_slice(), "unpadded copy is identity");
    }

    #[test]
    fn copy_rows_tight_clamps_a_short_source_without_panicking() {
        // Defensive: a source shorter than height*row_pitch must clamp (best-effort), not
        // panic. Here the second row's real region runs past the buffer end.
        let width = 2u32;
        let height = 2u32;
        let row_pitch = 12u32;
        // Only 1.5 rows of data present.
        let src = vec![7u8; 18];
        let mut dst = Vec::new();
        let _ = copy_rows_tight(&src, &mut dst, width, height, row_pitch);
        // Row 0 fully copied (8 bytes); row 1 starts at 12, only 6 bytes remain → clamped.
        assert_eq!(&dst[0..8], &[7u8; 8], "row 0 copied in full");
        assert!(dst.len() <= 16, "must not over-read past the short source");
    }
}
