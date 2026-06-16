#![cfg(all(target_os = "windows", feature = "hw-encoder"))]
//! Cross-thread coordination for the GPU-resident capture→encode hand-off.
//!
//! The capture thread (producer) and the encoder thread (consumer) live in
//! separate OS threads with their own D3D11 devices (design D1: keyed-mutex
//! texture hand-off across two same-adapter devices). [`GpuHandoff`] is the small
//! lock-free record they share to coordinate the one-time path-selection gate
//! WITHOUT transferring any COM interface across the thread boundary:
//!
//! * The **encoder thread** publishes its encode-adapter LUID + vendor at init via
//!   [`GpuHandoff::publish_encode_luid`].
//! * The **capture thread**, on its first frame, reads WGC's device LUID (the
//!   capture LUID), runs `select_encode_path(capture_luid, encode_luid, vendor)`
//!   ONCE, and publishes the result via [`GpuHandoff::resolve_path`]. From then on
//!   it produces `GpuShared` or `Cpu` accordingly.
//!
//! Only scalars cross the boundary (two `i64` LUIDs, a vendor tag, and an
//! `EncodePath` tag) — no `ID3D11Device`. The keyed-mutex `ID3D11Texture2D` itself
//! crosses by share `HANDLE` (an `isize` in `FramePayload::GpuShared`), opened on
//! the consumer's device with `OpenSharedResource1`. This keeps each thread's COM
//! objects thread-local (REQ-07).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};

use crate::encode::path_select::EncodePath;
use crate::encode::windows_mft::EncoderVendor;

/// Sentinel LUID meaning "not yet published".
const LUID_UNSET: i64 = i64::MIN;

// EncodePath tag values for the AtomicU8 slot.
const PATH_UNRESOLVED: u8 = 0;
const PATH_CPU: u8 = 1;
const PATH_GPU: u8 = 2;

/// Lock-free coordination record shared by the capture and encoder threads.
///
/// Created once in the sender wiring and handed (by `Arc`) to both the
/// `WindowsCaptureSource` (producer) and the encoder thread (consumer).
#[derive(Debug)]
pub struct GpuHandoff {
    /// Encode-adapter LUID, published by the encoder thread at init. `LUID_UNSET`
    /// until published.
    encode_luid: AtomicI64,
    /// Encoder vendor tag (see [`vendor_to_tag`]), published with the encode LUID.
    vendor_tag: AtomicU8,
    /// Set once the encoder has published its LUID + vendor.
    encode_published: AtomicBool,
    /// The resolved [`EncodePath`], decided ONCE by the capture thread on its first
    /// frame. `PATH_UNRESOLVED` until resolved.
    resolved_path: AtomicU8,
}

impl Default for GpuHandoff {
    fn default() -> Self {
        Self {
            encode_luid: AtomicI64::new(LUID_UNSET),
            vendor_tag: AtomicU8::new(0),
            encode_published: AtomicBool::new(false),
            resolved_path: AtomicU8::new(PATH_UNRESOLVED),
        }
    }
}

impl GpuHandoff {
    /// Construct an unresolved hand-off (no LUIDs published, path unresolved).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Encoder thread: publish the encode-adapter LUID + vendor at init.
    ///
    /// Called once from `run_encoder_thread` after the shared D3D11 device is
    /// created and its adapter LUID read. Idempotent on repeated calls (last write
    /// wins) but expected exactly once per session.
    pub(crate) fn publish_encode_luid(&self, encode_luid: i64, vendor: EncoderVendor) {
        self.encode_luid.store(encode_luid, Ordering::Release);
        self.vendor_tag
            .store(vendor_to_tag(vendor), Ordering::Release);
        self.encode_published.store(true, Ordering::Release);
    }

    /// Capture thread: has the encoder published its LUID + vendor yet?
    ///
    /// Returns `Some((encode_luid, vendor))` once available; `None` if the encoder
    /// has not reached its init publish point yet (the capture thread then keeps
    /// producing `Cpu` until the gate can be resolved).
    pub(crate) fn encode_luid(&self) -> Option<(i64, EncoderVendor)> {
        if !self.encode_published.load(Ordering::Acquire) {
            return None;
        }
        let luid = self.encode_luid.load(Ordering::Acquire);
        let vendor = tag_to_vendor(self.vendor_tag.load(Ordering::Acquire));
        Some((luid, vendor))
    }

    /// Capture thread: record the path decided by `select_encode_path`. Called once
    /// after the capture LUID is known on the first frame.
    ///
    /// **CPU is sticky.** A `CpuStagedFallback` resolution — whether it originates
    /// here or from [`Self::degrade_to_cpu`] — is FINAL for the session: a later
    /// `resolve_path(GpuResident)` MUST NOT override it. This closes the TOCTOU where
    /// a capture frame arriving between the encoder's `publish_encode_luid` and its
    /// `degrade_to_cpu` could otherwise write `GpuResident` AFTER the encoder forced
    /// CPU, stranding the session (producer emits `GpuShared` forever while the
    /// consumer has no pipeline). We use compare-exchange so the CPU latch always wins:
    ///
    /// * resolving `GpuResident` succeeds only while the slot is still `UNRESOLVED`;
    /// * resolving `CpuStagedFallback` always lands (it is the terminal state).
    pub(crate) fn resolve_path(&self, path: EncodePath) {
        match path {
            // GpuResident may only be written when nobody has resolved yet. If a CPU
            // decision (resolve_path or degrade_to_cpu) already landed, the CAS fails
            // and the CPU latch stands — no GPU write can clobber a CPU degrade.
            EncodePath::GpuResident => {
                let _ = self.resolved_path.compare_exchange(
                    PATH_UNRESOLVED,
                    PATH_GPU,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            // CpuStagedFallback is the terminal state: store unconditionally so it
            // overrides a prior GpuResident as well as an unresolved slot.
            EncodePath::CpuStagedFallback => {
                self.resolved_path.store(PATH_CPU, Ordering::Release);
            }
        }
    }

    /// Read the resolved path: `None` until the capture thread has resolved it.
    pub(crate) fn resolved_path(&self) -> Option<EncodePath> {
        match self.resolved_path.load(Ordering::Acquire) {
            PATH_GPU => Some(EncodePath::GpuResident),
            PATH_CPU => Some(EncodePath::CpuStagedFallback),
            _ => None,
        }
    }

    /// Force the path to CPU-staged for the remaining session lifetime.
    ///
    /// Used by the device-lost / negotiation-failure degradation path (REQ-05): once
    /// the GPU arm hits an unrecoverable device-removed error, the producer must stop
    /// emitting `GpuShared` and feed CPU frames for the rest of the session (no
    /// per-frame retry).
    ///
    /// This is the terminal CPU latch: it stores `PATH_CPU` unconditionally, and
    /// [`Self::resolve_path`] guarantees no later `GpuResident` write can override it
    /// (GPU resolution only succeeds from the `UNRESOLVED` state). Callable from EITHER
    /// thread — the encoder thread on a device-lost / negotiation failure, the capture
    /// thread on a producer/copy error — and remains correct under concurrent races.
    pub fn degrade_to_cpu(&self) {
        self.resolved_path.store(PATH_CPU, Ordering::Release);
    }
}

/// Map an [`EncoderVendor`] to a stable `u8` tag for the atomic slot.
fn vendor_to_tag(vendor: EncoderVendor) -> u8 {
    match vendor {
        EncoderVendor::IntelQsv => 1,
        EncoderVendor::NvidiaNvenc => 2,
        EncoderVendor::Amd => 3,
        EncoderVendor::Unknown => 4,
    }
}

/// Inverse of [`vendor_to_tag`].
fn tag_to_vendor(tag: u8) -> EncoderVendor {
    match tag {
        1 => EncoderVendor::IntelQsv,
        2 => EncoderVendor::NvidiaNvenc,
        3 => EncoderVendor::Amd,
        _ => EncoderVendor::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_handoff_returns_none() {
        let h = GpuHandoff::new();
        assert!(h.encode_luid().is_none(), "no LUID published yet");
        assert!(h.resolved_path().is_none(), "path not resolved yet");
    }

    #[test]
    fn publish_then_read_round_trips_luid_and_vendor() {
        let h = GpuHandoff::new();
        h.publish_encode_luid(0x1234_5678, EncoderVendor::IntelQsv);
        let (luid, vendor) = h.encode_luid().expect("published");
        assert_eq!(luid, 0x1234_5678);
        assert_eq!(vendor, EncoderVendor::IntelQsv);
    }

    #[test]
    fn resolve_path_round_trips_gpu_then_cpu_terminal() {
        // From UNRESOLVED, GpuResident lands; CpuStagedFallback then overrides it
        // (CPU is the terminal state).
        let h = GpuHandoff::new();
        h.resolve_path(EncodePath::GpuResident);
        assert_eq!(h.resolved_path(), Some(EncodePath::GpuResident));
        h.resolve_path(EncodePath::CpuStagedFallback);
        assert_eq!(h.resolved_path(), Some(EncodePath::CpuStagedFallback));
    }

    #[test]
    fn degrade_to_cpu_overrides_gpu() {
        let h = GpuHandoff::new();
        h.resolve_path(EncodePath::GpuResident);
        h.degrade_to_cpu();
        assert_eq!(
            h.resolved_path(),
            Some(EncodePath::CpuStagedFallback),
            "device-lost degradation must force CPU for the rest of the session"
        );
    }

    #[test]
    fn cpu_is_sticky_against_a_later_gpu_resolve() {
        // The TOCTOU guard (Fix 4): once any party resolves CPU, a late GpuResident
        // write from the other thread must NOT override it. This is the exact race
        // where capture's resolve_path(GpuResident) could otherwise land AFTER the
        // encoder's degrade_to_cpu and strand the session.
        let h = GpuHandoff::new();
        h.degrade_to_cpu();
        h.resolve_path(EncodePath::GpuResident);
        assert_eq!(
            h.resolved_path(),
            Some(EncodePath::CpuStagedFallback),
            "a late GpuResident write must not clobber a prior CPU degrade"
        );

        // Same invariant when the CPU decision came via resolve_path, not degrade.
        let h2 = GpuHandoff::new();
        h2.resolve_path(EncodePath::CpuStagedFallback);
        h2.resolve_path(EncodePath::GpuResident);
        assert_eq!(
            h2.resolved_path(),
            Some(EncodePath::CpuStagedFallback),
            "CPU resolved via resolve_path is just as sticky as degrade_to_cpu"
        );
    }

    #[test]
    fn gpu_resolve_lands_only_from_unresolved() {
        // Positive case: from UNRESOLVED, a GpuResident resolution succeeds.
        let h = GpuHandoff::new();
        assert_eq!(h.resolved_path(), None);
        h.resolve_path(EncodePath::GpuResident);
        assert_eq!(h.resolved_path(), Some(EncodePath::GpuResident));
    }

    #[test]
    fn vendor_tag_round_trips_all_variants() {
        for v in [
            EncoderVendor::IntelQsv,
            EncoderVendor::NvidiaNvenc,
            EncoderVendor::Unknown,
        ] {
            assert_eq!(tag_to_vendor(vendor_to_tag(v)), v);
        }
    }
}
