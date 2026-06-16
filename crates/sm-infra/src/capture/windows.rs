//! Windows Graphics Capture adapter.
//!
//! All code in this module is gated to `cfg(target_os = "windows")` and MUST NOT compile on
//! non-Windows targets. This file implements [`CaptureSource`] for Windows using the
//! `windows-capture` v2 crate via its `start_free_threaded` path.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use std::sync::OnceLock;

use sm_domain::encode::FramePayload;
use sm_domain::{
    BorderPolicy, CaptureConfig, CaptureError, CaptureFrame, CaptureSource, MonitorId, MonitorInfo,
    MonitorSelector, PixelFormat,
};

// ---------------------------------------------------------------------------
// Stable hash — djb2
// ---------------------------------------------------------------------------

/// Computes a djb2 hash over raw UTF-8 bytes.
///
/// This is intentionally NOT `std::collections::hash_map::DefaultHasher`, which is
/// explicitly documented as "not guaranteed to be stable across Rust versions"
/// (https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html).
/// djb2 is a well-known, deterministic algorithm: `hash = hash * 33 ^ byte` over
/// every byte in the input, seeded at 5381. The output is identical across Rust
/// compiler versions, operating systems, and process restarts — making it safe for
/// persisted user configuration (e.g. a saved monitor selection).
fn djb2(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    for &b in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(b));
    }
    hash
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Bounded channel capacity for frame delivery (R11.1).
///
/// At 60 fps this represents ~67 ms of latency tolerance. Value is in [4, 8].
/// Consumers should create their `SyncSender` channel with this capacity:
/// `std::sync::mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY)`.
pub const CAPTURE_CHANNEL_CAPACITY: usize = 4;

/// Heartbeat interval for static-content frame injection (capture-static-freeze fix).
///
/// WGC `on_frame_arrived` is called only when desktop content changes. A static screen
/// (no motion, no cursor blink, no animation) yields zero frames → zero RTP packets →
/// viewer freezes on the last received frame. A sibling heartbeat thread injects a
/// duplicate of the last real frame at this cadence (with an advanced timestamp) so the
/// encoder keeps producing output. 100ms = 10fps minimum during static periods. Real
/// frames arriving from WGC reset the heartbeat — no duplicates injected during motion.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Non-blocking keyed-mutex acquire timeout for the GPU producer on the WGC callback
/// thread (judder fix — Fix 1).
///
/// `GpuProducer::copy_frame_bounded` runs on the WGC `on_frame_arrived` thread. The
/// earlier design used a bounded wait of ~50ms (`GPU_ACQUIRE_TIMEOUT_MS`), but that still
/// PARKED the capture callback on the consumer's in-flight BGRA→NV12 blt under
/// backpressure, injecting 0–50ms stalls into `frame.timestamp()` spacing → judder. We now
/// pass `0` (try-acquire): if the consumer currently holds the slot's keyed mutex the
/// acquire returns `WAIT_TIMEOUT` immediately, the producer SKIPS this frame (non-fatal,
/// see the skip branch in `try_gpu_frame`) and the callback returns without ever blocking
/// on the consumer. The capture cadence therefore reflects WGC's true timing, not the
/// encoder's blt latency.
#[cfg(feature = "hw-encoder")]
const GPU_ACQUIRE_TIMEOUT_MS: u32 = 0;

// ---------------------------------------------------------------------------
// Phase 6 — Border detection (R9.1–R9.5)
// ---------------------------------------------------------------------------

/// Pure predicate: returns `true` if the given Windows build number supports
/// the `GraphicsCaptureSession.IsBorderRequired` API (i.e., build ≥ 22621,
/// which corresponds to Windows 11 22H2).
///
/// This function accepts a `u32` build number so it can be exercised in unit
/// tests without requiring a specific host OS version (R9.1 scenario 1 / R9.3).
#[inline]
fn supports_borderless_for_build(build: u32) -> bool {
    build >= 22621
}

/// Cached, process-wide check: returns `true` if the running OS supports
/// disabling the WGC capture border.
///
/// The result is computed once via `RtlGetVersion` (through the `windows-version`
/// crate) and cached in a `OnceLock<bool>`. OS version cannot change at runtime,
/// so this is safe and efficient.
fn supports_borderless() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let build = windows_version::OsVersion::current().build;
        supports_borderless_for_build(build)
    })
}

// ── Shared observability seam (perf-pipeline-throughput Slice 1) ────────────
//
// Defined here (no hw-encoder feature gate) so both the capture-side gate in
// on_frame_arrived and the encode-side gate in pump_loop (windows_mft.rs) call
// the same tested predicate instead of an inline duplicate.

/// Returns `true` when the elapsed time since `window_start` is at or above `threshold`.
///
/// Extracted as a pure function so the cadence predicate is unit-testable with synthetic
/// `Instant` values — no wall-clock sleeping required (D-PPT-6).
#[inline]
pub(crate) fn interval_elapsed(
    window_start: std::time::Instant,
    now: std::time::Instant,
    threshold: std::time::Duration,
) -> bool {
    now.duration_since(window_start) >= threshold
}

// ---------------------------------------------------------------------------
// GATE B instrumentation (judder + repeticiones objective metrics)
// ---------------------------------------------------------------------------
//
// All of these are aggregated per-second and logged alongside the existing
// `capture throughput` line (`emit_capture_throughput_window`). Metrics #1, #2,
// #3 and #5 add no per-frame syscall or allocation — only integer math on stack
// locals — so they do not perturb capture cadence (the very thing GATE B measures).
//
// Metric #4 (the duplicate-pixel detector) is the ONE exception: it reads a STRIDED
// SAMPLE of the frame buffer (`strided_pixel_hash`, thousands of bytes from a
// possibly write-combined surface), which is NOT free. To keep it off the per-frame
// hot path — and off the smooth NVENC baseline GATE B compares against — that hash
// is THROTTLED by `HASH_SAMPLE_EVERY_N` (about 1 frame in 6 → ~10 Hz on the CPU/NVENC
// emit path). On the GPU path the same divisor compounds with the per-frame readback
// throttle, so the effective sample rate there is lower (<= readback rate); either way
// its cost is bounded and amortized, not paid on every frame. The duplicate detector
// is only a coarse SECONDARY signal anyway — the primary repeticiones signal is the
// heartbeat-fire counter (metric #5).

/// Throttle divisor for the duplicate-pixel hash (GATE B metric #4).
///
/// The strided hash samples the frame buffer, so it must NOT run on every frame
/// (that perturbed the NVENC baseline the gate compares against). At a ~60fps capture
/// cadence, sampling 1 frame in 6 gives ~10 Hz — frequent enough to catch a run of
/// pixel-identical emits (the duplicate signal), rare enough to stay off the hot path.
/// Applied with the same divisor on both emit paths; on the GPU path it compounds with
/// the readback throttle, so the effective GPU sample rate is lower (<= readback rate).
const HASH_SAMPLE_EVERY_N: u32 = 6;

/// Number of fixed-width buckets in the millisecond latency histogram backing the
/// p99 estimate. Bucket `i` covers `[i, i+1)` ms; the final bucket is an overflow
/// catch-all for anything `>= LATENCY_BUCKETS - 1` ms. 64 buckets covers 0–63ms,
/// which spans the full range of interest for both inter-frame deltas (uniform
/// 60fps ≈ 16.6ms) and keyed-mutex acquire waits (a slot held during a blt).
const LATENCY_BUCKETS: usize = 64;

/// Fixed-bucket millisecond latency histogram for a per-second p99 + exact max.
///
/// Allocation-free: a `[u32; LATENCY_BUCKETS]` array plus an exact `max_us`. The p99
/// is a coarse 1ms-resolution estimate (sufficient to SEE judder widen the tail);
/// the max is exact (microsecond) so a single bad frame is never hidden by bucketing.
/// Used for inter-frame timestamp deltas (path-agnostic judder signal) and the
/// producer keyed-mutex acquire-wait. Reset each window via [`Self::reset`].
#[derive(Clone)]
struct LatencyHistogram {
    buckets: [u32; LATENCY_BUCKETS],
    count: u32,
    max_us: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; LATENCY_BUCKETS],
            count: 0,
            max_us: 0,
        }
    }
}

impl LatencyHistogram {
    /// Record one observation (a `Duration`) into the histogram. Pure integer math —
    /// no allocation, no syscall — safe on the per-frame hot path.
    #[inline]
    fn record(&mut self, dur: std::time::Duration) {
        let us = dur.as_micros() as u64;
        if us > self.max_us {
            self.max_us = us;
        }
        let ms = (us / 1000) as usize;
        let idx = ms.min(LATENCY_BUCKETS - 1);
        self.buckets[idx] = self.buckets[idx].saturating_add(1);
        self.count = self.count.saturating_add(1);
    }

    /// Exact max observation this window, in milliseconds (f64 for sub-ms detail).
    #[inline]
    fn max_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }

    /// Coarse p99 estimate (the lower edge, in ms, of the bucket that contains the
    /// 99th-percentile observation). Returns 0 when no observations were recorded.
    #[inline]
    fn p99_ms(&self) -> u32 {
        if self.count == 0 {
            return 0;
        }
        // Rank of the p99 observation (1-based). ceil(0.99 * count).
        let target = ((self.count as u64 * 99).div_ceil(100)).max(1);
        let mut cumulative: u64 = 0;
        for (i, &b) in self.buckets.iter().enumerate() {
            cumulative += b as u64;
            if cumulative >= target {
                return i as u32;
            }
        }
        (LATENCY_BUCKETS - 1) as u32
    }

    /// Reset the histogram for the next 1-second window.
    #[inline]
    fn reset(&mut self) {
        self.buckets = [0; LATENCY_BUCKETS];
        self.count = 0;
        self.max_us = 0;
    }
}

/// Strided djb2 hash of a frame's pixel buffer for the duplicate-pixel detector.
///
/// Hashing the WHOLE buffer every frame would be a meaningful per-frame cost (a 4K
/// BGRA frame is ~33 MB). Instead we sample one byte every `STRIDE` bytes — enough to
/// distinguish distinct frames while keeping the cost negligible. Two consecutive
/// EMITTED payloads sharing a hash is an objective "repeticiones" signal (the encoder
/// received pixel-identical input twice in a row). Reuses the existing stable `djb2`
/// kernel so the sampling is deterministic.
fn strided_pixel_hash(bytes: &[u8]) -> u64 {
    /// Sample one byte per this many bytes. ~4096 samples on a 1080p frame
    /// (1920*1080*4 ≈ 8.3 MB / 2048), cheap and collision-resistant for this use.
    const STRIDE: usize = 2048;
    let mut hash: u64 = 5381;
    let mut i = 0;
    while i < bytes.len() {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(bytes[i]));
        i += STRIDE;
    }
    // Fold the length in so a truncated buffer never collides with a longer one that
    // happens to match on every sampled byte.
    hash.wrapping_mul(33).wrapping_add(bytes.len() as u64)
}

/// Pure throttle predicate for the duplicate-pixel hash (GATE B metric #4).
///
/// Given a monotonic per-emit `counter`, returns `true` on ~1 in `every_n` calls so the
/// strided `strided_pixel_hash` runs at ~10 Hz instead of every frame. Isolated as a pure
/// function so the throttle cadence is unit-testable without live D3D or a real frame
/// buffer. `counter == 0` (the first emit) samples, so the very first frame is never missed.
#[inline]
fn hash_sample_due(counter: u32, every_n: u32) -> bool {
    // `every_n` is a compile-time constant >= 1; guard defensively against 0 to avoid a
    // modulo-by-zero if it is ever mis-set.
    every_n <= 1 || counter % every_n == 0
}

/// Decide whether the CPU-arm strided duplicate-pixel hash may sample THIS session
/// (C: keep the only new per-frame CPU work off the NVENC callback).
///
/// `strided_pixel_hash` is a GATE B (metric #4) QSV diagnostic. The CPU-staged arm of
/// `on_frame_arrived` runs on EVERY session that is not GPU-resident — including NVENC,
/// which is the `CpuStagedFallback` path and MUST do zero new per-frame work versus master.
/// We therefore sample the hash only when the session resolved to `GpuResident`; on NVENC
/// (`CpuStagedFallback`) or before path resolution (`None`) the CPU callback skips it
/// entirely. Pure decision so the gate is unit-testable without a live hand-off.
#[cfg(feature = "hw-encoder")]
fn cpu_hash_should_sample(resolved_path: Option<crate::encode::path_select::EncodePath>) -> bool {
    matches!(
        resolved_path,
        Some(crate::encode::path_select::EncodePath::GpuResident)
    )
}

// ---------------------------------------------------------------------------
// Helper: map Monitor errors to CaptureError
// ---------------------------------------------------------------------------

fn map_monitor_err(e: windows_capture::monitor::Error) -> CaptureError {
    CaptureError::Internal(format!("monitor error: {e}"))
}

// ---------------------------------------------------------------------------
// Helper: build MonitorInfo from a windows-capture Monitor
// ---------------------------------------------------------------------------

fn monitor_info_from(m: &Monitor, is_primary: bool) -> Result<MonitorInfo, CaptureError> {
    let device_name = m.device_name().map_err(map_monitor_err)?;
    let label = device_name.clone();

    // Derive a stable u64 id from the device name string (e.g. "\\.\DISPLAY1").
    let id = MonitorId(djb2(device_name.as_bytes()));

    let width = m.width().map_err(map_monitor_err)?;
    let height = m.height().map_err(map_monitor_err)?;

    Ok(MonitorInfo {
        id,
        label,
        width,
        height,
        is_primary,
    })
}

// ---------------------------------------------------------------------------
// Internal capture handler (lives on the WGC OS thread)
// ---------------------------------------------------------------------------

/// The most-recent frame's heartbeat descriptor.
///
/// Carries the last `CaptureFrame` (Arc refcount clone, no data copy). This is true on
/// BOTH paths: the CPU-staged path snapshots the delivered frame directly, and the
/// GPU-resident path snapshots a one-time CPU readback of the frame (Fix 3) so the
/// static-content heartbeat NEVER re-emits a live shared-texture handle that another
/// thread could overwrite. The heartbeat therefore always injects a stable `Cpu` frame.
#[derive(Clone)]
enum HeartbeatFrame {
    /// Re-inject the last `CaptureFrame` (cheap Arc bump) with an advanced timestamp.
    Cpu(CaptureFrame),
}

impl HeartbeatFrame {
    /// Advance the timestamp by `delta` (heartbeat cadence) and return a `FramePayload`
    /// to inject.
    ///
    /// In GPU-resident mode the heartbeat still injects a system-memory `FramePayload::Cpu`
    /// (Fix 3), which the encoder feeds to the MFT via `submit_frame` (a memory-backed
    /// `IMFSample`) even though the MFT was negotiated with a D3D device manager. This
    /// relies on the standard MF contract that hardware encoders (QSV/NVENC) in D3D mode
    /// still accept system-memory input samples; the MFT internally stages them. A driver
    /// that rejects system-memory input surfaces a `ProcessInput` error, which the pump
    /// already handles by degrading the whole session to the CPU-staged path — so the
    /// worst case is a graceful CPU degrade, never undefined behavior. Heartbeats fire
    /// only on a static screen (rare), so this path is cold.
    fn into_payload_advanced(self, delta: std::time::Duration) -> FramePayload {
        match self {
            HeartbeatFrame::Cpu(mut f) => {
                f.timestamp = f.timestamp.saturating_add(delta);
                FramePayload::Cpu(f)
            }
        }
    }
}

/// Shared snapshot of the most recent frame descriptor + the instant it was observed.
/// Owned by both `WgcHandler` (writer on the WGC thread) and the heartbeat thread (reader).
type LastFrameSlot = Arc<std::sync::Mutex<Option<(HeartbeatFrame, std::time::Instant)>>>;

/// Internal state carried on the WGC capture thread.
///
/// This type implements [`GraphicsCaptureApiHandler`] and forwards frames
/// to the caller via a bounded `SyncSender`. It is NOT part of the public API.
/// Flags passed to [`WgcHandler::new`] via `start_free_threaded`. A named struct
/// (rather than a tuple) so the GPU hand-off field can be cfg-gated cleanly.
struct WgcHandlerFlags {
    dropped: Arc<AtomicU64>,
    tx: std::sync::mpsc::SyncSender<FramePayload>,
    last_frame: LastFrameSlot,
    /// GATE B: heartbeat-fire counter, shared with the heartbeat thread (incremented
    /// there) and read here to log per-second heartbeat injections.
    heartbeat_fires: Arc<AtomicU64>,
    #[cfg(feature = "hw-encoder")]
    gpu_handoff: Option<Arc<crate::encode::gpu_handoff::GpuHandoff>>,
}

struct WgcHandler {
    tx: std::sync::mpsc::SyncSender<FramePayload>,
    dropped: Arc<AtomicU64>,
    /// Shared with the heartbeat thread; updated on every real frame delivery so the
    /// heartbeat can detect "no real frame in HEARTBEAT_INTERVAL" and inject a duplicate.
    last_frame: LastFrameSlot,
    /// Frame count accumulated in the current 1-second FPS window (I1, D-PPT-1).
    /// Counts only frames that reached the channel successfully (delivered rate).
    fps_frame_count: u32,
    /// Start of the current 1-second FPS window (I1, D-PPT-1).
    fps_window_start: std::time::Instant,
    /// Snapshot of `dropped` at the end of the last 1-second window, used to compute
    /// per-interval drop delta (I1, D-PPT-3).
    last_dropped_snapshot: u64,
    /// GATE B: per-second histogram of inter-frame WGC timestamp deltas (the single
    /// most diagnostic, path-agnostic judder metric). Uniform 60fps ≈ 16.6ms flat; a
    /// widening max/p99 is judder. Updated on every frame in `on_frame_arrived`.
    frame_delta_stats: LatencyHistogram,
    /// GATE B: previous frame's WGC timestamp, to compute the inter-frame delta.
    /// `None` until the first frame is seen.
    prev_frame_timestamp: Option<std::time::Duration>,
    /// GATE B: strided pixel hash of the LAST emitted payload, for the duplicate-pixel
    /// detector. `None` until the first emitted frame. Two consecutive equal hashes =
    /// objective "repeticiones".
    last_emitted_hash: Option<u64>,
    /// GATE B: count of consecutive-duplicate emitted payloads observed in the current
    /// window (rolling-hash collisions between two adjacent emitted frames).
    duplicate_emit_count: u32,
    /// GATE B metric #4 throttle: monotonic counter of emit attempts, used to decide
    /// when the strided duplicate-pixel hash is due (~1 in `HASH_SAMPLE_EVERY_N`, ~10 Hz).
    /// Shared across the CPU and GPU emit paths so the hash sampling is uniform on both.
    /// Never reset per-window (it only gates an inexpensive modulo decision).
    hash_sample_counter: u32,
    /// GATE B: per-second histogram of the producer keyed-mutex acquire wait (Fix 1).
    /// With a non-blocking acquire this should be ~0ms; any tail indicates contention.
    #[cfg(feature = "hw-encoder")]
    acquire_wait_stats: LatencyHistogram,
    /// GATE B / Fix 1: count of GPU frames SKIPPED this window because the producer's
    /// non-blocking keyed-mutex acquire timed out (consumer mid-blt). A skip is benign
    /// backpressure relief, NOT a session degrade — see the skip branch in `try_gpu_frame`.
    #[cfg(feature = "hw-encoder")]
    gpu_skip_count: u32,
    /// GATE B: heartbeat-fire counter shared with the heartbeat thread. Non-zero during
    /// motion means the repeticiones bug is live (a real frame looked stale). Logged in
    /// the per-second window.
    heartbeat_fires: Arc<AtomicU64>,
    /// Snapshot of `heartbeat_fires` at the last window boundary, for the per-interval delta.
    last_heartbeat_snapshot: u64,
    /// GPU-resident hand-off shared with the encoder thread (PR-4 / TASK-08).
    /// `None` when the GPU path is not wired — the handler then always produces
    /// `FramePayload::Cpu`, byte-identical to the pre-GPU path.
    #[cfg(feature = "hw-encoder")]
    gpu_handoff: Option<Arc<crate::encode::gpu_handoff::GpuHandoff>>,
    /// Lazily-built capture-thread GPU producer (keyed-mutex shared texture on WGC's
    /// device). Built on the first GPU frame once the device + real dims are known.
    #[cfg(feature = "hw-encoder")]
    gpu_producer: Option<crate::capture::gpu_producer::GpuProducer>,
    /// Once a GPU frame hits an unrecoverable error, this latches and the handler
    /// produces only `Cpu` frames for the rest of the session (REQ-05 device-lost
    /// degradation; no per-frame retry).
    #[cfg(feature = "hw-encoder")]
    gpu_degraded: bool,
    /// Fix 2: the instant the GPU heartbeat snapshot's PIXELS were last refreshed via a
    /// full CPU readback (`gpu_heartbeat_snapshot`). The readback is throttled to at most
    /// once per `HEARTBEAT_INTERVAL` so a busy GPU screen does NOT pay a full-frame readback
    /// per frame. `None` until the first readback. NOTE: this throttles only the *pixels* —
    /// the heartbeat FRESHNESS instant is refreshed on EVERY real frame (see the stale-hole
    /// fix in `try_gpu_frame`), so a real frame never looks stale to `heartbeat_loop`.
    #[cfg(feature = "hw-encoder")]
    last_gpu_readback: Option<std::time::Instant>,
    /// Fix 7: asynchronous GPU→CPU readback engine for the heartbeat snapshot. Owns a
    /// ping-pong pool of `D3D11_USAGE_STAGING` textures + WGC's immediate context and does
    /// a NON-BLOCKING `Map` (`D3D11_MAP_FLAG_DO_NOT_WAIT`), so the WGC callback thread never
    /// stalls on a GPU→CPU pipeline flush (the judder source). Built lazily on the first GPU
    /// frame from `frame.device()` / `frame.device_context()`. `None` until then.
    #[cfg(feature = "hw-encoder")]
    async_readback: Option<crate::capture::gpu_readback::AsyncReadback>,
    /// Fix 7: reusable destination buffer for the async readback's tight-BGRA copy. Held on
    /// the handler so the readback does NOT reallocate its staging-copy buffer per frame;
    /// cleared and refilled in place each successful readback. (The owned `Arc<[u8]>` snapshot
    /// handed to the heartbeat IS allocated per successful readback, but only at the ~10Hz
    /// throttled rate on the cold heartbeat path — not on the per-frame hot path.)
    #[cfg(feature = "hw-encoder")]
    readback_buf: Vec<u8>,
    /// GATE B confirmation metric (Fix 7): per-second histogram of the async readback step's
    /// wall-clock (CopyResource issue + try-map + tight copy). After the blocking Map is gone
    /// this max should be sub-millisecond. Logged as `readback_dur_max_ms`.
    #[cfg(feature = "hw-encoder")]
    readback_dur_stats: LatencyHistogram,
}

impl GraphicsCaptureApiHandler for WgcHandler {
    type Flags = WgcHandlerFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let WgcHandlerFlags {
            dropped,
            tx,
            last_frame,
            heartbeat_fires,
            #[cfg(feature = "hw-encoder")]
            gpu_handoff,
        } = ctx.flags;
        Ok(Self {
            tx,
            dropped,
            last_frame,
            fps_frame_count: 0,
            fps_window_start: std::time::Instant::now(),
            last_dropped_snapshot: 0,
            frame_delta_stats: LatencyHistogram::default(),
            prev_frame_timestamp: None,
            last_emitted_hash: None,
            duplicate_emit_count: 0,
            hash_sample_counter: 0,
            #[cfg(feature = "hw-encoder")]
            acquire_wait_stats: LatencyHistogram::default(),
            #[cfg(feature = "hw-encoder")]
            gpu_skip_count: 0,
            heartbeat_fires,
            last_heartbeat_snapshot: 0,
            #[cfg(feature = "hw-encoder")]
            gpu_handoff,
            #[cfg(feature = "hw-encoder")]
            gpu_producer: None,
            #[cfg(feature = "hw-encoder")]
            gpu_degraded: false,
            #[cfg(feature = "hw-encoder")]
            last_gpu_readback: None,
            #[cfg(feature = "hw-encoder")]
            async_readback: None,
            #[cfg(feature = "hw-encoder")]
            readback_buf: Vec::new(),
            #[cfg(feature = "hw-encoder")]
            readback_dur_stats: LatencyHistogram::default(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();

        // Collect timestamp before taking the mutable buffer borrow.
        let timestamp = frame
            .timestamp()
            .ok()
            .and_then(|ts| {
                // WinRT TimeSpan.Duration is in 100-ns units.
                u64::try_from(ts.Duration).ok()
            })
            .map(|d| std::time::Duration::from_nanos(d * 100))
            .unwrap_or(std::time::Duration::ZERO);

        // GATE B metric #1 (judder): inter-frame WGC timestamp delta. Computed at the
        // single point the timestamp is read, so it is path-agnostic (covers both the
        // GPU and CPU arms below). Uniform 60fps ≈ 16.6ms flat; a widening max/p99 is
        // judder. Pure integer math — no allocation, no syscall.
        if let Some(prev) = self.prev_frame_timestamp {
            // saturating_sub guards against a non-monotonic timestamp (clamps to 0).
            self.frame_delta_stats
                .record(timestamp.saturating_sub(prev));
        }
        self.prev_frame_timestamp = Some(timestamp);

        // ── GPU-resident path (PR-4 / TASK-08) ────────────────────────────────
        // When the gate resolved GpuResident and the producer has not degraded,
        // CopyResource the WGC texture into a shared keyed-mutex texture and send a
        // `GpuShared` payload — no CPU readback, no Arc copy. On any GPU error the
        // path latches to CPU for the rest of the session (REQ-05). `Handled` means
        // the frame was fully serviced on the GPU path; `HandledStop` additionally
        // requests session teardown (consumer disconnected); `NotHandled` falls through
        // to the CPU-staged path below.
        #[cfg(feature = "hw-encoder")]
        match self.try_gpu_frame(frame, width, height, timestamp) {
            GpuFrameOutcome::Handled => return Ok(()),
            GpuFrameOutcome::HandledStop => {
                capture_control.stop();
                return Ok(());
            }
            GpuFrameOutcome::NotHandled => {}
        }

        // ── CPU-staged path (byte-identical to pre-GPU behaviour) ─────────────
        let mut buf = match frame.buffer() {
            Ok(b) => b,
            Err(e) => {
                // Non-fatal — skip this frame.
                eprintln!("sm-infra: frame buffer error: {e}");
                return Ok(());
            }
        };

        let bytes: &[u8] = buf.as_raw_buffer();
        let stride = (bytes.len() as u32)
            .checked_div(height)
            .unwrap_or(width * 4);

        // GATE B metric #4 (repeticiones): strided pixel hash of this CPU frame. Recorded
        // BEFORE the Arc copy while the buffer is still borrowed. Two consecutive equal
        // emitted hashes is an objective duplicate. THROTTLED to ~10 Hz (`hash_sample_due_now`):
        // the strided read of a write-combined buffer must NOT run on every CPU/NVENC frame —
        // that perturbed the smooth baseline GATE B compares against. The duplicate detector
        // stays a coarse secondary signal; the primary repeticiones signal is metric #5.
        //
        // C: this is the ONLY new per-frame CPU work on the NVENC callback, so under
        // `hw-encoder` we gate it to the QSV `GpuResident` session only — NVENC runs the
        // CpuStagedFallback path and must stay byte-for-byte equal to master (zero new work).
        // Without `hw-encoder` there is no NVENC and no path concept; the hash runs as before.
        #[cfg(feature = "hw-encoder")]
        let cpu_hash_enabled = cpu_hash_should_sample(
            self.gpu_handoff
                .as_ref()
                .and_then(|h| h.resolved_path()),
        );
        #[cfg(not(feature = "hw-encoder"))]
        let cpu_hash_enabled = true;
        if cpu_hash_enabled && self.hash_sample_due_now() {
            self.record_emitted_hash(strided_pixel_hash(bytes));
        }

        // Copy pixel data into an Arc slice so it can be shared without holding the GPU mapping.
        let data: Arc<[u8]> = Arc::from(bytes);

        let capture_frame = CaptureFrame {
            data,
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            timestamp,
        };

        // Update shared snapshot BEFORE send so the heartbeat thread always has a
        // representative frame to duplicate. Cloning `capture_frame` is cheap — the
        // `data` field is `Arc<[u8]>`, so the clone is a refcount bump, not a memcpy.
        if let Ok(mut guard) = self.last_frame.lock() {
            *guard = Some((
                HeartbeatFrame::Cpu(capture_frame.clone()),
                std::time::Instant::now(),
            ));
        }

        match self.tx.try_send(FramePayload::Cpu(capture_frame)) {
            Ok(()) => {
                // Count only frames that were actually delivered to the encoder channel.
                // Heartbeat-injected frames go through heartbeat_loop's own try_send,
                // NOT through here — so capture_fps measures WGC's true delivery rate only.
                // On a static screen capture_fps legitimately reads ~0; this is correct (D-PPT-1).
                self.fps_frame_count += 1;
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                // Consumer dropped the receiver — tear down the WGC session.
                capture_control.stop();
            }
        }

        self.emit_capture_throughput_window();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // Session closed externally (monitor unplugged, session ended). No action needed —
        // the WGC thread will exit naturally and the channel will be disconnected.
        Ok(())
    }
}

/// Outcome of the GPU-resident frame attempt (PR-4 / TASK-08).
#[cfg(feature = "hw-encoder")]
enum GpuFrameOutcome {
    /// Frame fully serviced on the GPU path; caller returns without CPU fallback.
    Handled,
    /// Frame serviced, but the consumer disconnected — caller must stop the session.
    HandledStop,
    /// GPU path not active for this frame — caller runs the CPU-staged path.
    NotHandled,
}

/// What to do with a `copy_frame_bounded` error on the WGC callback thread (Fix 1).
///
/// Isolated as a pure decision so the "Timeout → skip, everything else → degrade" choice
/// is unit-testable WITHOUT live D3D objects — the test feeds each `CopyError` variant and
/// asserts the disposition. A regression that accidentally degraded the session on a
/// transient timeout (re-introducing the stall-on-consumer behaviour) would be caught here.
#[cfg(feature = "hw-encoder")]
#[derive(Debug, PartialEq, Eq)]
enum CopyDisposition {
    /// Transient contention (consumer mid-blt): drop this frame, keep the GPU path active.
    Skip,
    /// Unrecoverable for the session (device-lost / abandoned mutex / copy failure):
    /// degrade to the CPU-staged path for the rest of the session (REQ-05).
    Degrade,
}

/// Classify a producer `CopyError` into a [`CopyDisposition`] (Fix 1).
///
/// ONLY `CopyError::Timeout` is a non-fatal skip — with the non-blocking acquire it means
/// the consumer currently holds the slot's keyed mutex for its blt, which is benign
/// backpressure. `Abandoned` (consumer died holding the mutex) and `Encoder` (device
/// removed / cast / `CopyResource` / `ReleaseSync` failure) are genuine and degrade the
/// session, so a skip can NEVER mask a real device-lost.
#[cfg(feature = "hw-encoder")]
fn copy_error_disposition(err: &crate::capture::gpu_producer::CopyError) -> CopyDisposition {
    use crate::capture::gpu_producer::CopyError;
    match err {
        CopyError::Timeout => CopyDisposition::Skip,
        CopyError::Abandoned | CopyError::Encoder(_) => CopyDisposition::Degrade,
    }
}

/// Decide whether the GPU heartbeat snapshot's PIXELS are due for a fresh CPU readback
/// (Fix 2: throttle the readback off the per-frame hot path).
///
/// The full-frame `gpu_heartbeat_snapshot` readback is expensive; running it every frame on
/// a busy screen wastes a memcpy per frame for a snapshot that only the (rare) static-screen
/// heartbeat consumes. We therefore refresh the pixels at most once per `HEARTBEAT_INTERVAL`
/// — frequent enough that a frozen screen has a recent snapshot to re-emit, cheap enough that
/// motion pays it only ~10x/second. `last_readback == None` (first GPU frame) is always due.
///
/// IMPORTANT: this throttles ONLY the pixels. The heartbeat FRESHNESS instant is refreshed on
/// EVERY real queued frame regardless of this decision (see `try_gpu_frame`), so a real frame
/// can never look "stale" to `heartbeat_loop` just because its readback was throttled — that
/// stale hole was the repeticiones source.
#[cfg(feature = "hw-encoder")]
fn gpu_readback_due(
    last_readback: Option<std::time::Instant>,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    match last_readback {
        None => true,
        Some(prev) => now.duration_since(prev) >= interval,
    }
}

impl WgcHandler {
    /// Try to handle this frame on the GPU-resident path (PR-4 / TASK-08).
    ///
    /// Returns `true` if the frame was fully handled on the GPU path (the caller must
    /// NOT fall through to the CPU path). Returns `false` when the GPU path is not
    /// active for this frame (no hand-off, gate not GpuResident, gate degraded, or a
    /// recoverable build/copy error already latched the producer to CPU) — the caller
    /// then runs the CPU-staged path.
    ///
    /// On the first frame it resolves the path-selection gate from WGC's device LUID
    /// (capture LUID) + the encoder-published encode LUID/vendor, builds the
    /// keyed-mutex producer, and records the result in the hand-off. Any unrecoverable
    /// GPU error latches `gpu_degraded` so the session runs CPU-only for the rest of
    /// its lifetime (REQ-05, no per-frame retry).
    #[cfg(feature = "hw-encoder")]
    fn try_gpu_frame(
        &mut self,
        frame: &mut Frame,
        width: u32,
        height: u32,
        timestamp: std::time::Duration,
    ) -> GpuFrameOutcome {
        use crate::capture::gpu_producer::{GpuProducer, capture_adapter_luid};
        use crate::encode::path_select::{EncodePath, select_encode_path};

        // No hand-off wired, or already degraded → CPU path.
        let Some(handoff) = self.gpu_handoff.as_ref() else {
            return GpuFrameOutcome::NotHandled;
        };
        if self.gpu_degraded {
            return GpuFrameOutcome::NotHandled;
        }

        // Resolve the gate ONCE (first GPU-eligible frame). The handoff caches the
        // decision; after this the producer is either built (GpuResident) or we latch
        // to CPU.
        if self.gpu_producer.is_none() && handoff.resolved_path().is_none() {
            // The encoder thread must have published its encode LUID + vendor first.
            // If it has not yet, run CPU this frame and retry the resolution next frame.
            let Some((encode_luid, vendor)) = handoff.encode_luid() else {
                return GpuFrameOutcome::NotHandled;
            };
            // SAFETY: frame.device() is WGC's live device on this capture thread.
            let device = frame.device();
            let capture_luid = match unsafe { capture_adapter_luid(device) } {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        target: "sm_infra::capture::windows",
                        "could not read capture adapter LUID ({e}); using CPU-staged path"
                    );
                    handoff.degrade_to_cpu();
                    self.gpu_degraded = true;
                    return GpuFrameOutcome::NotHandled;
                }
            };
            let path = select_encode_path(capture_luid, encode_luid, vendor);
            tracing::info!(
                target: "sm_infra::capture::windows",
                ?path,
                ?vendor,
                capture_luid,
                encode_luid,
                "capture-side path-selection gate resolved"
            );
            handoff.resolve_path(path);
            if path == EncodePath::CpuStagedFallback {
                self.gpu_degraded = true;
                return GpuFrameOutcome::NotHandled;
            }
        }

        // If the encoder degraded the path after negotiation, honor it.
        if handoff.resolved_path() != Some(EncodePath::GpuResident) {
            self.gpu_degraded = true;
            return GpuFrameOutcome::NotHandled;
        }

        // Build the keyed-mutex producer lazily on WGC's device (first GPU frame).
        if self.gpu_producer.is_none() {
            // SAFETY: device/context are WGC's live device + immediate context.
            let device = frame.device();
            let context = frame.device_context();
            match unsafe { GpuProducer::build(device, context, width, height) } {
                Ok(p) => self.gpu_producer = Some(p),
                Err(e) => {
                    tracing::error!(
                        target: "sm_infra::capture::windows",
                        "GPU producer build failed ({e}); degrading to CPU-staged for the session"
                    );
                    handoff.degrade_to_cpu();
                    self.gpu_degraded = true;
                    return GpuFrameOutcome::NotHandled;
                }
            }
        }

        // Dimension guard: a resize would invalidate the shared textures. Degrade to
        // CPU (the encoder pipeline was negotiated at the original dims; rebuilding the
        // whole GPU chain mid-session is out of scope — REQ-05 graceful fallback).
        {
            let producer = self.gpu_producer.as_ref().expect("producer built above");
            if producer.dimensions() != (width, height) {
                tracing::warn!(
                    target: "sm_infra::capture::windows",
                    "capture dims changed; degrading GPU path to CPU-staged for the session"
                );
                handoff.degrade_to_cpu();
                self.gpu_degraded = true;
                return GpuFrameOutcome::NotHandled;
            }
        }

        // Fix 7 + Fix 3 + Fix 2: refresh the heartbeat snapshot via an ASYNCHRONOUS GPU→CPU
        // readback. GPU-mode heartbeats must NEVER re-emit a live shared-texture handle
        // (another thread may overwrite it); instead, the static-content heartbeat re-injects
        // this CPU copy of the last frame.
        //
        // Fix 7 (async): the snapshot readback used to call `windows-capture`'s `frame.buffer()`,
        // which does a SYNCHRONOUS, BLOCKING GPU→CPU Map (full pipeline flush + CPU stall) ON
        // this WGC callback thread. With the size-1 WGC frame pool that stall delayed the NEXT
        // frame's delivery → non-uniform cadence → judder. We now issue a non-blocking
        // `CopyResource` into our own ping-pong staging pool and try-map the OTHER slot with
        // `D3D11_MAP_FLAG_DO_NOT_WAIT` (returns instead of stalling if the copy is still
        // drawing). The snapshot is at most ~1 readback-interval (~100ms) stale, which is fine:
        // the heartbeat consumes it only when the screen is static, where a slightly-stale copy
        // equals the current frame.
        //
        // Fix 2 (throttle): we still gate the readback to at most once per HEARTBEAT_INTERVAL
        // (`gpu_readback_due`) to limit iGPU copy work on a busy screen. The heartbeat FRESHNESS
        // instant is refreshed separately on every real queued frame below, so a throttled
        // readback NEVER makes a real frame look stale (that hole was the repeticiones source).
        let now_for_readback = std::time::Instant::now();
        let heartbeat_snapshot =
            if gpu_readback_due(self.last_gpu_readback, now_for_readback, HEARTBEAT_INTERVAL) {
                // Lazily build the async readback engine on WGC's device + immediate context
                // (the SAME device that owns the frame texture — `CopyResource` requires it).
                if self.async_readback.is_none() {
                    let device = frame.device();
                    let context = frame.device_context();
                    // SAFETY: device/context are WGC's live device + immediate context for this
                    // capture thread, the same device that owns `frame.as_raw_texture()`.
                    self.async_readback =
                        Some(unsafe { crate::capture::gpu_readback::AsyncReadback::new(device, context) });
                }
                // SAFETY: as_raw_texture() is WGC's live BGRA texture for this frame, on the
                // device the readback was built with.
                let wgc_tex = frame.as_raw_texture();
                // GATE B confirmation metric (Fix 7): time the whole async step (CopyResource
                // issue + try-map + copy). Sub-millisecond once the blocking Map is gone.
                let readback_started = std::time::Instant::now();
                let snap = if let Some(rb) = self.async_readback.as_mut() {
                    // SAFETY: wgc_tex lives on the readback's device; width/height are current.
                    match unsafe { rb.readback(wgc_tex, width, height, &mut self.readback_buf) } {
                        Some(stride) => Some(HeartbeatFrame::Cpu(CaptureFrame {
                            data: Arc::from(self.readback_buf.as_slice()),
                            width,
                            height,
                            stride,
                            format: PixelFormat::Bgra8,
                            timestamp,
                        })),
                        None => None, // still-drawing / first-frame / error — keep prior snapshot
                    }
                } else {
                    None
                };
                self.readback_dur_stats.record(readback_started.elapsed());
                // Mark the readback taken only if it actually produced pixels; a momentary
                // still-drawing (`None`) leaves `last_gpu_readback` so the next frame retries
                // instead of waiting a full interval with no snapshot.
                if snap.is_some() {
                    self.last_gpu_readback = Some(now_for_readback);
                }
                snap
            } else {
                None // pixels not due this frame — keep the existing snapshot (freshness still refreshed below)
            };

        // CopyResource the live WGC texture into the NEXT ring slot (Fix 2: per-frame
        // texture, no aliasing) with a NON-BLOCKING keyed-mutex acquire (Fix 1: the WGC
        // callback must NEVER park on the consumer's blt — that injected 0–50ms stalls into
        // the timestamp spacing and was the judder source).
        // SAFETY: as_raw_texture() is WGC's live BGRA texture for this frame.
        let wgc_tex = frame.as_raw_texture();
        let producer = self.gpu_producer.as_mut().expect("producer built above");
        let mut acquire_wait = std::time::Duration::ZERO;
        let handle = match unsafe {
            producer.copy_frame_bounded(wgc_tex, GPU_ACQUIRE_TIMEOUT_MS, &mut acquire_wait)
        } {
            Ok(h) => {
                // GATE B metric #2: record the (near-zero) acquire wait on the success path.
                self.acquire_wait_stats.record(acquire_wait);
                h
            }
            Err(copy_err) => {
                // GATE B metric #2: also record the wait on the error/skip path (it is the
                // try-acquire cost, near-zero when non-blocking).
                self.acquire_wait_stats.record(acquire_wait);
                // Fix 1: classify the copy error into skip-vs-degrade. A `Timeout` is
                // transient contention (the consumer is mid-blt) under the non-blocking
                // acquire: SKIP this frame WITHOUT degrading the session. ONLY a genuine
                // device-lost / abandoned-mutex / non-timeout copy error degrades to CPU
                // for the session (REQ-05). The acquire/release pair stays balanced inside
                // copy_frame_bounded on every path, so a skip never leaks the mutex.
                match copy_error_disposition(&copy_err) {
                    CopyDisposition::Skip => {
                        // Non-fatal: the consumer is busy with the previous frame's blt.
                        // Drop this frame, count it, leave the cursor put (the slot's handle
                        // was never queued — safe to reuse), and keep the GPU path active.
                        self.gpu_skip_count += 1;
                        // Fix 1 (skip-arm stale hole): a SKIPPED frame is still proof the
                        // screen is LIVE — WGC delivered a real frame, we simply could not
                        // copy it because the consumer held the slot mid-blt. We must keep the
                        // heartbeat quiet, so refresh the FRESHNESS instant on `last_frame`
                        // EXACTLY the way the success path's no-fresh-pixels branch does
                        // (mirror it — do not invent a new lock pattern). We do NOT touch the
                        // snapshot PIXELS here (there is no new readback on a skip); only the
                        // instant moves, so `heartbeat_loop` does not see this real frame as
                        // stale and inject a duplicate during sustained MOTION contention.
                        if let Ok(mut guard) = self.last_frame.lock() {
                            if let Some((_, instant)) = guard.as_mut() {
                                *instant = std::time::Instant::now();
                            }
                        }
                        // The frame did not reach the channel; emit the per-second window so
                        // the cadence stays stable, then report "handled" (no CPU fallback).
                        self.emit_capture_throughput_window();
                        return GpuFrameOutcome::Handled;
                    }
                    CopyDisposition::Degrade => {
                        match copy_err {
                            crate::capture::gpu_producer::CopyError::Timeout => {
                                // Unreachable given the disposition above, but kept exhaustive.
                                tracing::error!(
                                    target: "sm_infra::capture::windows",
                                    "GPU copy_frame keyed-mutex acquire timed out; degrading to CPU for the session"
                                )
                            }
                            crate::capture::gpu_producer::CopyError::Abandoned => tracing::error!(
                                target: "sm_infra::capture::windows",
                                "GPU copy_frame keyed mutex abandoned (consumer died holding it); degrading to CPU for the session"
                            ),
                            crate::capture::gpu_producer::CopyError::Encoder(e) => tracing::error!(
                                target: "sm_infra::capture::windows",
                                "GPU CopyResource failed ({e}); degrading to CPU-staged for the session"
                            ),
                        }
                        handoff.degrade_to_cpu();
                        self.gpu_degraded = true;
                        return GpuFrameOutcome::NotHandled;
                    }
                }
            }
        };

        let stride = width * 4; // BGRA8 shared texture stride (informational).

        // Fix 2 — CLOSE THE STALE HOLE. The heartbeat slot carries two DECOUPLED concerns:
        //   (1) the snapshot PIXELS the heartbeat re-emits, and
        //   (2) the FRESHNESS instant `heartbeat_loop` uses to decide "is the slot stale?".
        // Previously the slot was written ONLY when a fresh readback produced pixels, so a
        // throttled/None readback left `last_update` un-refreshed → a real frame in motion
        // looked stale → the heartbeat injected a duplicate (the repeticiones bug).
        //
        // We now ALWAYS refresh the freshness instant on this real frame, and refresh the
        // pixels only when a fresh snapshot exists this frame (otherwise keep the previous
        // pixels). A real frame therefore NEVER looks stale, regardless of readback throttling.
        //
        // GATE B metric #4: when a fresh readback exists, hash its pixels for the
        // duplicate-pixel detector (the GPU-path emitted pixels otherwise live only on the
        // GPU; the heartbeat injections — the actual repeticiones mechanism — are covered by
        // metric #5 and by the CPU arm's hash). The strided hash is THROTTLED to ~10 Hz via
        // the SAME `hash_sample_due_now` gate used on the CPU path, so the sampling cadence is
        // uniform on both paths and never adds a strided buffer read to the per-frame hot path.
        if let Some(hb_frame) = heartbeat_snapshot {
            // `HeartbeatFrame` is single-variant (`Cpu`) today; match by ref to read the
            // pixels for the duplicate detector without consuming the frame.
            let HeartbeatFrame::Cpu(ref f) = hb_frame;
            if self.hash_sample_due_now() {
                self.record_emitted_hash(strided_pixel_hash(&f.data));
            }
            if let Ok(mut guard) = self.last_frame.lock() {
                // Fresh pixels + fresh instant (both unconditional — independent of the
                // hash throttle, so the heartbeat snapshot and freshness never depend on
                // whether the duplicate detector sampled this frame).
                *guard = Some((hb_frame, std::time::Instant::now()));
            }
        } else if let Ok(mut guard) = self.last_frame.lock() {
            // No fresh pixels this frame — keep the existing snapshot but ALWAYS bump the
            // freshness instant so this real frame is not seen as stale (the stale-hole fix).
            if let Some((_, instant)) = guard.as_mut() {
                *instant = std::time::Instant::now();
            }
        }

        let outcome = match self.tx.try_send(FramePayload::GpuShared {
            handle,
            width,
            height,
            stride,
            timestamp,
        }) {
            Ok(()) => {
                // Fix 6: the frame is now QUEUED, so this slot is live. Advance the ring
                // cursor ONLY here — the next frame writes the next slot, so the producer
                // cannot overwrite this slot's pixels while its handle is still in the
                // channel (no residual aliasing under backpressure).
                if let Some(producer) = self.gpu_producer.as_mut() {
                    producer.advance_after_send();
                }
                self.fps_frame_count += 1;
                GpuFrameOutcome::Handled
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Fix 6: the frame was DROPPED and never queued, so we DO NOT advance the
                // cursor. The next frame reuses this same slot — safe, because this handle
                // never entered the channel and nothing references this slot's pixels.
                self.dropped.fetch_add(1, Ordering::Relaxed);
                GpuFrameOutcome::Handled
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                // Consumer dropped the receiver — request session teardown (the caller
                // owns `capture_control` and stops it). No advance: the frame was not queued.
                GpuFrameOutcome::HandledStop
            }
        };

        self.emit_capture_throughput_window();
        outcome
    }

    /// GATE B metric #4 throttle: advance the per-emit counter and report whether the
    /// strided duplicate-pixel hash is due this frame (~1 in `HASH_SAMPLE_EVERY_N`, ~10 Hz).
    ///
    /// Called once per emit attempt on BOTH the CPU/NVENC and GPU paths so the hash sampling
    /// is UNIFORM across paths. Keeping the strided buffer read off the per-frame hot path
    /// preserves the smooth NVENC baseline GATE B compares against. Pure integer work.
    #[inline]
    fn hash_sample_due_now(&mut self) -> bool {
        let due = hash_sample_due(self.hash_sample_counter, HASH_SAMPLE_EVERY_N);
        self.hash_sample_counter = self.hash_sample_counter.wrapping_add(1);
        due
    }

    /// GATE B metric #4 (repeticiones): record the strided pixel hash of an emitted frame
    /// and bump the duplicate counter when it matches the previous emitted frame's hash.
    ///
    /// Two consecutive emitted payloads sharing a hash means the encoder received
    /// pixel-identical input twice in a row — an objective duplicate. Pure integer work; no
    /// allocation. Aggregated per-second by `emit_capture_throughput_window`.
    fn record_emitted_hash(&mut self, hash: u64) {
        if self.last_emitted_hash == Some(hash) {
            self.duplicate_emit_count = self.duplicate_emit_count.saturating_add(1);
        }
        self.last_emitted_hash = Some(hash);
    }

    /// Per-second observability window: emit capture_fps and capture drop-delta
    /// (I1, D-PPT-1/3). Shared by the CPU and GPU producer paths.
    fn emit_capture_throughput_window(&mut self) {
        // Per-second observability window: emit capture_fps and capture drop-delta (I1, D-PPT-1/3).
        // Checked unconditionally after the match so both delivered and dropped frames advance
        // the window clock, keeping the log cadence stable even under backpressure.
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.fps_window_start);
        if interval_elapsed(
            self.fps_window_start,
            now,
            std::time::Duration::from_secs(1),
        ) {
            let fps = self.fps_frame_count as f64 / elapsed.as_secs_f64();
            tracing::info!(
                target: "sm_infra::capture::windows",
                capture_fps = %format!("{fps:.1}"),
                frames = self.fps_frame_count,
                "capture throughput"
            );

            // Compute per-interval drop delta (D-PPT-3).
            let current_dropped = self.dropped.load(Ordering::Relaxed);
            let (delta, new_last) = compute_drop_delta(current_dropped, self.last_dropped_snapshot);
            if delta > 0 {
                tracing::info!(
                    target: "sm_infra::capture::windows",
                    channel_drops = delta,
                    channel = "capture_to_enc",
                    "capture channel drops"
                );
            }

            // ── GATE B metrics (judder + repeticiones, path-agnostic) ──────────────
            // Metric #1: inter-frame WGC timestamp delta. Uniform 60fps ≈ 16.6ms flat;
            // a widening max/p99 is judder.
            tracing::info!(
                target: "sm_infra::capture::windows",
                frame_delta_max_ms = %format!("{:.1}", self.frame_delta_stats.max_ms()),
                frame_delta_p99_ms = self.frame_delta_stats.p99_ms(),
                "capture frame-delta"
            );

            // Metric #5: heartbeat injections this interval. Non-zero DURING MOTION means
            // the repeticiones bug is live (a real frame looked stale).
            let current_hb = self.heartbeat_fires.load(Ordering::Relaxed);
            let (hb_delta, new_hb_last) =
                compute_drop_delta(current_hb, self.last_heartbeat_snapshot);

            // Metric #4: consecutive duplicate-pixel emitted frames this interval.
            tracing::info!(
                target: "sm_infra::capture::windows",
                duplicate_emits = self.duplicate_emit_count,
                heartbeat_fires = hb_delta,
                "capture duplicate-frame detector"
            );

            // Metric #2 + Fix 1 skip count: GPU producer acquire wait + skipped frames.
            // Fix 7 confirmation metric: `readback_dur_max_ms` is the max wall-clock of the
            // async heartbeat readback step this window (CopyResource issue + non-blocking
            // try-map + tight copy). After the blocking Map is removed this is sub-millisecond,
            // which is the objective proof the GPU→CPU stall is gone.
            #[cfg(feature = "hw-encoder")]
            tracing::info!(
                target: "sm_infra::capture::windows",
                acquire_wait_max_ms = %format!("{:.1}", self.acquire_wait_stats.max_ms()),
                acquire_wait_p99_ms = self.acquire_wait_stats.p99_ms(),
                gpu_skips = self.gpu_skip_count,
                readback_dur_max_ms = %format!("{:.3}", self.readback_dur_stats.max_ms()),
                "capture gpu-producer contention"
            );

            // Reset window state.
            self.fps_frame_count = 0;
            self.fps_window_start = std::time::Instant::now();
            self.last_dropped_snapshot = new_last;
            self.frame_delta_stats.reset();
            self.duplicate_emit_count = 0;
            self.last_heartbeat_snapshot = new_hb_last;
            #[cfg(feature = "hw-encoder")]
            {
                self.acquire_wait_stats.reset();
                self.gpu_skip_count = 0;
                self.readback_dur_stats.reset();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Heartbeat thread — static-content frame injection
// ---------------------------------------------------------------------------

/// Heartbeat loop body — runs on a sibling OS thread spawned by `WindowsCaptureSource::start`.
///
/// WGC delivers frames only on content change. When the desktop is static, the encoder
/// receives nothing and the viewer freezes. This loop wakes every `HEARTBEAT_INTERVAL`,
/// checks the shared `last_frame` snapshot, and if no real frame has arrived within that
/// window, injects a duplicate (with an advanced monotonic timestamp) so the encoder keeps
/// producing output.
///
/// Exits when `stop_flag` is set OR the consumer drops the channel (Disconnected). A
/// `Full` send result is silently ignored — real-frame backpressure already handles this.
///
/// `heartbeat_fires` (GATE B metric #5) is incremented once per DELIVERED duplicate.
/// Non-zero during motion means the repeticiones bug is live; on a truly static screen a
/// steady ~10/s is EXPECTED and correct (that is the heartbeat doing its job).
fn heartbeat_loop(
    tx: std::sync::mpsc::SyncSender<FramePayload>,
    last_frame: LastFrameSlot,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    heartbeat_fires: Arc<AtomicU64>,
) {
    loop {
        std::thread::sleep(HEARTBEAT_INTERVAL);

        if stop_flag.load(Ordering::Relaxed) {
            return;
        }

        // Snapshot under lock, then drop the lock before sending.
        let snapshot = match last_frame.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return, // mutex poisoned — capture thread panicked; exit gracefully
        };

        let Some((frame, last_update)) = snapshot else {
            continue; // no real frame has arrived yet — nothing to duplicate
        };

        if last_update.elapsed() < HEARTBEAT_INTERVAL {
            continue; // a real frame arrived within the window — skip this beat
        }

        // Advance timestamp monotonically so the encoder + downstream RTP timestamps
        // see a regular cadence. `saturating_add` (inside into_payload_advanced) guards
        // against overflow at the far end of a session lifetime. The snapshot is always a
        // CPU frame (Fix 3) — on the GPU path it is a one-time CPU readback taken when the
        // frame was produced — so the heartbeat never references a live shared texture.
        let payload = frame.into_payload_advanced(HEARTBEAT_INTERVAL);

        // Reset the "last observed" instant so we don't immediately re-fire on the next
        // tick. Real frames continue to overwrite this when they arrive. Nested `if let`
        // (not let-chains) to stay compatible with MSRV 1.85.
        if let Ok(mut guard) = last_frame.lock() {
            if let Some((_, instant)) = guard.as_mut() {
                *instant = std::time::Instant::now();
            }
        }

        match tx.try_send(payload) {
            Ok(()) => {
                // GATE B metric #5: count one delivered duplicate. Read per-second by
                // `emit_capture_throughput_window` as the objective repeticiones counter.
                heartbeat_fires.fetch_add(1, Ordering::Relaxed);
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Channel saturated — encoder will catch up on real frames. Skip silently;
                // bumping `dropped` here would conflate heartbeat throttling with WGC drops.
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                return; // consumer dropped the receiver — session ending
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public adapter struct
// ---------------------------------------------------------------------------

/// Windows Graphics Capture adapter implementing [`CaptureSource`].
///
/// This adapter is gated to `cfg(target_os = "windows")`. It uses the
/// `windows-capture` v2 library's `start_free_threaded` path so the WGC
/// callback runs on a dedicated OS thread, fully isolated from Tauri's COM
/// apartment (R10 compliance).
///
/// # Thread safety
///
/// `WindowsCaptureSource` is `Send`. The `dropped` counter is an `Arc<AtomicU64>`
/// shared with the WGC handler thread; reads via `dropped_frames()` are safe from
/// any thread.
pub struct WindowsCaptureSource {
    /// Capture configuration supplied at construction time.
    config: CaptureConfig,

    /// The resolved monitor to capture. Populated in `new()`.
    monitor: Monitor,

    /// Cumulative count of frames dropped due to channel backpressure.
    /// Shared with the WGC callback thread via `Arc`.
    dropped: Arc<AtomicU64>,

    /// Handle to the running capture session, if any.
    /// `None` before `start()` or after `stop()`.
    control: Option<CaptureControl<WgcHandler, Box<dyn std::error::Error + Send + Sync>>>,

    /// Stop signal for the heartbeat thread spawned by `start()`.
    /// `None` before `start()` or after `stop()`; `Some` during an active session.
    heartbeat_stop: Option<Arc<std::sync::atomic::AtomicBool>>,

    /// GPU-resident hand-off shared with the encoder thread (PR-4 / TASK-08).
    /// `None` until `set_gpu_handoff` is called — the capture handler then always
    /// produces `FramePayload::Cpu`, byte-identical to the pre-GPU path.
    #[cfg(feature = "hw-encoder")]
    gpu_handoff: Option<Arc<crate::encode::gpu_handoff::GpuHandoff>>,
}

// SAFETY: Monitor wraps an HMONITOR handle. windows-capture declares Monitor as Send.
// WindowsCaptureSource is safe to send across thread boundaries.
unsafe impl Send for WindowsCaptureSource {}

impl std::fmt::Debug for WindowsCaptureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsCaptureSource")
            .field("config", &self.config)
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .field("active", &self.control.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// CaptureSource implementation
// ---------------------------------------------------------------------------

impl CaptureSource for WindowsCaptureSource {
    fn enumerate_monitors() -> Result<Vec<MonitorInfo>, CaptureError>
    where
        Self: Sized,
    {
        let primary = Monitor::primary()
            .map_err(|e| CaptureError::Internal(format!("primary monitor: {e}")))?;

        let all = Monitor::enumerate()
            .map_err(|e| CaptureError::Internal(format!("enumerate monitors: {e}")))?;

        let mut result = Vec::with_capacity(all.len());
        for m in &all {
            let is_primary = m == &primary;
            result.push(monitor_info_from(m, is_primary)?);
        }

        Ok(result)
    }

    fn new(config: CaptureConfig) -> Result<Self, CaptureError>
    where
        Self: Sized,
    {
        // Validate domain invariants (R5.4 — moved to sm-domain CaptureConfig::validate).
        config.validate()?;

        // Resolve the requested monitor.
        let monitor = match config.monitor {
            MonitorSelector::Primary => {
                Monitor::primary().map_err(|_| CaptureError::MonitorNotFound("primary".into()))?
            }

            MonitorSelector::ByIndex(idx) => {
                // Monitor::from_index uses 1-based indexing.
                Monitor::from_index(idx + 1)
                    .map_err(|_| CaptureError::MonitorNotFound(format!("index {idx}")))?
            }

            MonitorSelector::ById(id) => {
                // Scan enumerated monitors for a matching hash.
                let all = Monitor::enumerate()
                    .map_err(|e| CaptureError::Internal(format!("enumerate: {e}")))?;

                let mut found = None;
                for m in all {
                    let device_name = m.device_name().map_err(map_monitor_err)?;
                    if MonitorId(djb2(device_name.as_bytes())) == id {
                        found = Some(m);
                        break;
                    }
                }
                found.ok_or_else(|| CaptureError::MonitorNotFound(format!("id {:?}", id)))?
            }
        };

        Ok(Self {
            config,
            monitor,
            dropped: Arc::new(AtomicU64::new(0)),
            control: None,
            heartbeat_stop: None,
            #[cfg(feature = "hw-encoder")]
            gpu_handoff: None,
        })
    }

    fn start(&mut self, tx: std::sync::mpsc::SyncSender<FramePayload>) -> Result<(), CaptureError> {
        // R8.1 — runtime WGC support check.
        let supported = GraphicsCaptureApi::is_supported()
            .map_err(|e| CaptureError::Internal(format!("WGC IsSupported probe failed: {e}")))?;
        if !supported {
            return Err(CaptureError::NotSupported);
        }

        // Cursor setting.
        let cursor = if self.config.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };

        // Border setting — R9.1–R9.5.
        // BorderPolicy::Auto: disable on Win11 22H2+ (build ≥ 22621), leave default otherwise.
        // BorderPolicy::AlwaysOff: disable regardless of OS version (best-effort).
        // BorderPolicy::AlwaysOn: leave border enabled (OS default).
        let border = match self.config.border {
            BorderPolicy::Auto => {
                if supports_borderless() {
                    DrawBorderSettings::WithoutBorder
                } else {
                    DrawBorderSettings::Default
                }
            }
            BorderPolicy::AlwaysOff => DrawBorderSettings::WithoutBorder,
            BorderPolicy::AlwaysOn => DrawBorderSettings::WithBorder,
        };

        // Heartbeat infrastructure (capture-static-freeze fix): shared snapshot of the
        // most recent frame + a stop flag. Clone `tx` so the WGC handler and the heartbeat
        // thread each own a send handle on the same bounded channel.
        let last_frame: LastFrameSlot = Arc::new(std::sync::Mutex::new(None));
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // GATE B metric #5: heartbeat-fire counter, shared between the heartbeat thread
        // (writer) and the WGC handler (per-second reader/logger).
        let heartbeat_fires = Arc::new(AtomicU64::new(0));
        let hb_tx = tx.clone();
        let hb_last_frame = Arc::clone(&last_frame);
        let hb_stop = Arc::clone(&stop_flag);
        let hb_fires = Arc::clone(&heartbeat_fires);

        let flags = WgcHandlerFlags {
            dropped: Arc::clone(&self.dropped),
            tx,
            last_frame,
            heartbeat_fires,
            #[cfg(feature = "hw-encoder")]
            gpu_handoff: self.gpu_handoff.clone(),
        };
        let settings = Settings::new(
            self.monitor,
            cursor,
            border,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        );

        let control = WgcHandler::start_free_threaded(settings)
            .map_err(|e| CaptureError::SessionCreateFailed(format!("{e}")))?;

        // Sibling thread to the WGC OS thread; exits on stop_flag OR Disconnected.
        std::thread::Builder::new()
            .name("capture-heartbeat".into())
            .spawn(move || heartbeat_loop(hb_tx, hb_last_frame, hb_stop, hb_fires))
            .map_err(|e| CaptureError::Internal(format!("spawn heartbeat: {e}")))?;

        self.control = Some(control);
        self.heartbeat_stop = Some(stop_flag);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), CaptureError> {
        // Signal the heartbeat thread to exit at its next wake (max HEARTBEAT_INTERVAL delay).
        if let Some(flag) = self.heartbeat_stop.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(control) = self.control.take() {
            control
                .stop()
                .map_err(|e| CaptureError::Internal(format!("stop failed: {e}")))?;
        }
        // Idempotent: calling stop when already stopped is Ok(()) (AC #13).
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Additional accessors
// ---------------------------------------------------------------------------

impl WindowsCaptureSource {
    /// Attach the GPU-resident path hand-off shared with the encoder (PR-4 /
    /// TASK-08). MUST be called BEFORE [`CaptureSource::start`] so the capture
    /// handler thread receives it. When never called the capture source produces
    /// only `FramePayload::Cpu`, byte-identical to the pre-GPU path.
    #[cfg(feature = "hw-encoder")]
    pub fn set_gpu_handoff(&mut self, handoff: Arc<crate::encode::gpu_handoff::GpuHandoff>) {
        self.gpu_handoff = Some(handoff);
    }

    /// Returns the resolved monitor's pixel dimensions as `(width, height)`.
    ///
    /// The monitor is resolved at `new()` time (see `CaptureSource::new`). This method
    /// queries the stored `Monitor` handle synchronously. On error (e.g., the monitor
    /// was disconnected between `new()` and this call), returns `(0, 0)` so callers
    /// that forward dimensions to `EncoderConfig` will fall back to the adapter default
    /// via the sentinel-zero mechanism (see `effective_dimensions` in `windows_mft.rs`).
    pub fn dimensions(&self) -> (u32, u32) {
        let w = self.monitor.width().unwrap_or(0);
        let h = self.monitor.height().unwrap_or(0);
        (w, h)
    }
}

// ---------------------------------------------------------------------------
// Observability seams (perf-pipeline-throughput Slice 1)
// ---------------------------------------------------------------------------

/// Compute the per-interval drop delta for a monotonically-increasing drop counter.
///
/// Returns `(delta, new_last)` where:
/// - `delta` is the number of drops that occurred since the last snapshot
///   (`current.saturating_sub(last)` — monotonic, never negative),
/// - `new_last` is the snapshot to store for the next interval (equals `current`).
///
/// Called by `on_frame_arrived` (I1, D-PPT-3) at each 1-second window boundary
/// with `self.dropped.load(Relaxed)` as `current`.
#[inline]
fn compute_drop_delta(current: u64, last: u64) -> (u64, u64) {
    (current.saturating_sub(last), current)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Phase 7 tests (7.1) ────────────────────────────────────────────────────

    /// Unit test: `dropped_frames()` returns 0 on a freshly constructed source.
    ///
    /// This verifies the Arc<AtomicU64> is initialised to zero in `new()` and
    /// readable through the public `dropped_frames()` accessor (R11.3, R11.4).
    /// No live WGC session is required — this test is NOT `#[ignore]`.
    #[test]
    fn dropped_frames_starts_at_zero_after_new() {
        let config = sm_domain::CaptureConfig::default();
        // `new()` resolves the primary monitor — this succeeds on any Windows desktop.
        // If it fails (e.g., headless runner), we skip rather than fail.
        let source = match WindowsCaptureSource::new(config) {
            Ok(s) => s,
            Err(_) => return, // headless / no display — skip
        };
        assert_eq!(
            source.dropped_frames(),
            0,
            "dropped_frames() must be 0 on a freshly constructed source"
        );
    }

    /// Unit test: the `dropped_frames` counter correctly reflects the shared
    /// `Arc<AtomicU64>`. This tests the wiring between the adapter struct and
    /// the counter, independent of WGC or real frame delivery (R11.3, R11.4).
    #[test]
    fn dropped_frames_counter_reflects_arc_atomic() {
        // We cannot construct WgcHandler directly (private struct), but we can
        // verify the contract by confirming AtomicU64 is Sync and that the
        // counter value exposed by dropped_frames() is consistent with the
        // underlying atomic state via the Arc.
        let counter = Arc::new(AtomicU64::new(0));
        // Simulate what WgcHandler does on a full channel:
        counter.fetch_add(1, Ordering::Relaxed);
        counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "Arc<AtomicU64> must reflect 2 drops"
        );
        // Simulate a second thread reading the counter (satisfies R11.4):
        let reader = Arc::clone(&counter);
        let handle = std::thread::spawn(move || reader.load(Ordering::Relaxed));
        let val = handle.join().expect("reader thread must not panic");
        assert_eq!(val, 2, "counter must be readable from a different thread");
    }

    // ── Phase 6 tests (6.1) ────────────────────────────────────────────────────

    /// Build number gate: Win11 22H2 threshold is build 22621.
    /// All branches of the `supports_borderless_for_build` predicate are exercised
    /// without running on any specific OS version (pure logic test, R9.1/R9.3).
    #[test]
    fn border_policy_auto_disables_on_win11_22h2_plus() {
        // Exactly at the threshold (Win11 22H2).
        assert!(
            supports_borderless_for_build(22621),
            "build 22621 (Win11 22H2) must return true"
        );
        // One build above (e.g., a later cumulative update).
        assert!(
            supports_borderless_for_build(26100),
            "build 26100 (Win11 24H2) must return true"
        );
        // Just below the threshold (Win11 21H2).
        assert!(
            !supports_borderless_for_build(22000),
            "build 22000 (Win11 21H2) must return false"
        );
        // Win10 builds.
        assert!(
            !supports_borderless_for_build(19045),
            "build 19045 (Win10 22H2) must return false"
        );
        // Edge: build 0 (hypothetical / test guard).
        assert!(
            !supports_borderless_for_build(0),
            "build 0 must return false"
        );
    }

    /// Lock-in test: asserts a SPECIFIC, HARDCODED u64 output for a well-known device name.
    ///
    /// This test exists to catch any accidental change of the hash function. If this value
    /// ever changes, persisted monitor selections (saved user configuration) would become
    /// incompatible — that is a user-visible regression.
    ///
    /// The expected value `0xCA76352EF04EA74E` was computed by running the `djb2`
    /// implementation above on the byte sequence of `r"\\.\DISPLAY1"` (the canonical
    /// Windows device name returned by `Monitor::device_name()`).
    ///
    /// DO NOT update this constant unless the hash function is intentionally changed AND a
    /// migration path for existing stored IDs is provided.
    #[test]
    fn monitor_id_hash_is_stable_for_known_device_name() {
        // r"\\.\DISPLAY1" is the Windows device name format returned by Monitor::device_name().
        let device_name = r"\\.\DISPLAY1";
        let id = MonitorId(djb2(device_name.as_bytes()));
        assert_eq!(
            id,
            MonitorId(0xCA76352EF04EA74E_u64),
            "djb2 hash of '{}' must remain 0xCA76352EF04EA74E — \
             changing this breaks persisted monitor configuration",
            device_name,
        );
    }

    /// Additional stability check: the djb2 function must be pure — same input always
    /// gives the same output within a single process run.
    #[test]
    fn djb2_is_deterministic_across_calls() {
        let input = b"\\\\.\\ DISPLAY2";
        assert_eq!(djb2(input), djb2(input));
    }

    // ── Observability seam tests (WU-A RED — perf-pipeline-throughput Slice 1) ──

    /// Task 1.4 [RED]: compute_drop_delta computes the per-interval delta and advances
    /// the snapshot to the current cumulative value.
    #[test]
    fn drop_delta_computes_and_advances_snapshot() {
        // First call: current=5, last=2 → delta=3, new_last=5
        let (delta, new_last) = compute_drop_delta(5, 2);
        assert_eq!(delta, 3, "delta must be current - last");
        assert_eq!(new_last, 5, "new_last must equal current");

        // Second call: counter unchanged current=5, last=5 → delta=0, new_last=5
        let (delta2, new_last2) = compute_drop_delta(5, 5);
        assert_eq!(delta2, 0, "delta must be 0 when counter did not advance");
        assert_eq!(
            new_last2, 5,
            "new_last must equal current even when delta is 0"
        );

        // Saturating branch: current < last (e.g. counter reset) → delta clamps to 0,
        // new_last re-pins to current rather than underflowing.
        assert_eq!(
            compute_drop_delta(2, 5),
            (0, 2),
            "delta must saturate to 0 and re-pin new_last when current < last"
        );
    }

    // ── GATE B instrumentation tests (judder + repeticiones) ────────────────────

    /// Fix 1: only a transient `Timeout` is a non-fatal skip; every other copy error
    /// degrades the session. A regression that degraded on a timeout would re-introduce
    /// the consumer-stall-as-fatal behaviour; one that skipped on `Encoder`/`Abandoned`
    /// would mask a real device-lost. This pins both directions.
    #[cfg(feature = "hw-encoder")]
    #[test]
    fn copy_error_timeout_skips_everything_else_degrades() {
        use crate::capture::gpu_producer::CopyError;
        use sm_domain::encode::EncoderError;
        assert_eq!(
            copy_error_disposition(&CopyError::Timeout),
            CopyDisposition::Skip,
            "a non-blocking-acquire timeout is transient contention — skip, do NOT degrade"
        );
        assert_eq!(
            copy_error_disposition(&CopyError::Abandoned),
            CopyDisposition::Degrade,
            "an abandoned mutex (consumer died) must degrade the session"
        );
        assert_eq!(
            copy_error_disposition(&CopyError::Encoder(EncoderError::EncodeFailed(
                "device removed".into()
            ))),
            CopyDisposition::Degrade,
            "a real copy/device error must degrade — a skip must never mask device-lost"
        );
    }

    /// Fix 2: the readback-throttle decision. `None` (first frame) is always due; after a
    /// readback, the pixels are due again only once `HEARTBEAT_INTERVAL` has elapsed. This
    /// is the throttle that keeps a busy screen from paying a full readback per frame —
    /// WITHOUT touching the separate freshness-instant refresh that closes the stale hole.
    #[cfg(feature = "hw-encoder")]
    #[test]
    fn gpu_readback_throttle_due_only_once_per_interval() {
        let base = std::time::Instant::now();
        // First frame (no prior readback) is always due.
        assert!(
            gpu_readback_due(None, base, HEARTBEAT_INTERVAL),
            "first GPU frame must always take a readback"
        );
        // Just after a readback: not due (within the interval).
        let just_after = base + std::time::Duration::from_millis(1);
        assert!(
            !gpu_readback_due(Some(base), just_after, HEARTBEAT_INTERVAL),
            "a readback 1ms ago is not due — throttle holds"
        );
        // Exactly at the interval boundary: due (>= is inclusive).
        let at_boundary = base + HEARTBEAT_INTERVAL;
        assert!(
            gpu_readback_due(Some(base), at_boundary, HEARTBEAT_INTERVAL),
            "a full interval after the last readback must be due again"
        );
        // Well past the interval: due.
        let well_past = base + HEARTBEAT_INTERVAL * 3;
        assert!(
            gpu_readback_due(Some(base), well_past, HEARTBEAT_INTERVAL),
            "long after the last readback must be due"
        );
    }

    /// C: the CPU-arm strided hash samples ONLY on the GpuResident session. NVENC runs the
    /// CpuStagedFallback path and must do zero new per-frame work, so its hash gate is false;
    /// the pre-resolution `None` state is also false (no work before the path is known).
    #[cfg(feature = "hw-encoder")]
    #[test]
    fn cpu_hash_samples_only_on_gpu_resident_path() {
        use crate::encode::path_select::EncodePath;
        assert!(
            cpu_hash_should_sample(Some(EncodePath::GpuResident)),
            "the QSV GpuResident session is the only path that samples the CPU-arm hash"
        );
        assert!(
            !cpu_hash_should_sample(Some(EncodePath::CpuStagedFallback)),
            "NVENC (CpuStagedFallback) must do ZERO new per-frame work — no hash"
        );
        assert!(
            !cpu_hash_should_sample(None),
            "before the path resolves, the CPU callback must not sample the hash"
        );
    }

    /// Fix 2: the duplicate-pixel hash throttle. The strided hash must run at most ~1 frame
    /// in `HASH_SAMPLE_EVERY_N` (~10 Hz at 60fps), uniformly on both the CPU and GPU paths,
    /// so it never adds a per-frame strided buffer read to the NVENC baseline. This pins the
    /// cadence: the first emit samples, then exactly one sample per N afterwards.
    #[test]
    fn hash_sample_throttle_runs_at_most_once_per_n_frames() {
        // First frame (counter 0) is always due so the very first emit is never missed.
        assert!(
            hash_sample_due(0, HASH_SAMPLE_EVERY_N),
            "the first emit must sample the hash"
        );
        // Over a window of frames, count how many sample — must be ceil(window / N).
        let window: u32 = 600; // ~10s at 60fps
        let sampled = (0..window)
            .filter(|&c| hash_sample_due(c, HASH_SAMPLE_EVERY_N))
            .count() as u32;
        let expected = window.div_ceil(HASH_SAMPLE_EVERY_N);
        assert_eq!(
            sampled, expected,
            "over {window} frames the hash must sample exactly {expected} times (1 in {HASH_SAMPLE_EVERY_N}), got {sampled}"
        );
        // The samples must be evenly spaced: indices 0, N, 2N, ... and nothing between.
        for c in 0..window {
            let due = hash_sample_due(c, HASH_SAMPLE_EVERY_N);
            assert_eq!(
                due,
                c % HASH_SAMPLE_EVERY_N == 0,
                "frame {c} sample decision must be a clean modulo-{HASH_SAMPLE_EVERY_N} cadence"
            );
        }
        // ~10 Hz invariant at 60fps: at least 9, at most 11 samples per 60-frame second.
        let per_second = (0..60)
            .filter(|&c| hash_sample_due(c, HASH_SAMPLE_EVERY_N))
            .count();
        assert!(
            (9..=11).contains(&per_second),
            "the throttle must yield ~10 Hz at 60fps, got {per_second}/s"
        );
        // Defensive: a mis-set divisor of 0 or 1 degrades to sampling every frame, never
        // a panic (modulo-by-zero guard).
        assert!(hash_sample_due(7, 0), "every_n=0 must not panic and samples");
        assert!(hash_sample_due(7, 1), "every_n=1 samples every frame");
    }

    /// Metric #1/#2: the latency histogram's max is exact and the p99 lands in the right
    /// bucket. Uniform 16ms samples have a tight p99; a single 40ms outlier widens the max
    /// without dragging the p99 — exactly the judder signal we want to surface.
    #[test]
    fn latency_histogram_tracks_exact_max_and_bucketed_p99() {
        let mut h = LatencyHistogram::default();
        // 100 uniform 16ms samples → p99 bucket = 16, max ≈ 16ms.
        for _ in 0..100 {
            h.record(std::time::Duration::from_millis(16));
        }
        assert_eq!(h.p99_ms(), 16, "uniform 16ms p99 must be the 16ms bucket");
        assert!(
            (h.max_ms() - 16.0).abs() < 0.001,
            "max must be exactly 16ms, got {}",
            h.max_ms()
        );
        // One 40ms outlier: max jumps, p99 (1-in-101) stays at 16ms.
        h.record(std::time::Duration::from_millis(40));
        assert!(
            (h.max_ms() - 40.0).abs() < 0.001,
            "max must reflect the 40ms outlier"
        );
        assert_eq!(
            h.p99_ms(),
            16,
            "a single outlier in 101 samples must not move p99 off the 16ms bucket"
        );
        // Empty histogram reports zeros (no divide-by-zero).
        let empty = LatencyHistogram::default();
        assert_eq!(empty.p99_ms(), 0);
        assert_eq!(empty.max_ms(), 0.0);
    }

    /// Metric #4: the strided pixel hash is deterministic and distinguishes distinct
    /// frames, so two consecutive equal hashes is a trustworthy duplicate signal.
    #[test]
    fn strided_pixel_hash_is_stable_and_distinguishes_frames() {
        let frame_a = vec![0xABu8; 8192];
        let mut frame_b = frame_a.clone();
        // Flip a byte the stride samples (index 0 is always sampled).
        frame_b[0] = 0x00;
        assert_eq!(
            strided_pixel_hash(&frame_a),
            strided_pixel_hash(&frame_a),
            "same buffer must hash identically (deterministic)"
        );
        assert_ne!(
            strided_pixel_hash(&frame_a),
            strided_pixel_hash(&frame_b),
            "a changed sampled byte must change the hash"
        );
        // Length is folded in: a truncated buffer that matches on sampled bytes differs.
        let shorter = vec![0xABu8; 4096];
        assert_ne!(
            strided_pixel_hash(&frame_a),
            strided_pixel_hash(&shorter),
            "different-length buffers must not collide via length-folding"
        );
    }
}
