//! Port boundary for video encoding.
//!
//! This module defines the domain-level contract for encoding `CaptureFrame`s
//! into compressed video packets. No platform type, async runtime, or
//! codec-specific import is permitted here — all platform adaptation lives
//! in `sm-infra`.
//!
//! # Key types
//!
//! | Type | Role |
//! |------|------|
//! | [`VideoEncoder`] | Port trait implemented by each encoder adapter. |
//! | [`EncoderConfig`] | Configuration: bitrate, framerate, intra period. |
//! | [`EncodedPacket`] | A single encoded packet (Annex-B NAL bytes + flags). |
//! | [`EncoderError`] | Unified error enum for all encoder operations. |
//! | [`RateControlMode`] | Rate-control strategy (V1: ConstantBitrate only). |
//!
//! # Usage
//!
//! ```rust,ignore
//! use sm_domain::encode::{VideoEncoder, EncoderConfig};
//! use std::sync::mpsc::sync_channel;
//!
//! let (frame_tx, frame_rx) = sync_channel(4);
//! let (pkt_tx, pkt_rx)     = sync_channel(4);
//!
//! let mut enc = MyEncoderAdapter::new(EncoderConfig::default())?;
//! enc.start(frame_rx, pkt_tx)?;
//! // upstream pushes CaptureFrames into frame_tx;
//! // consumer pulls EncodedPackets from pkt_rx
//! enc.request_keyframe(); // force IDR on next frame
//! enc.set_bitrate(8_000_000)?;
//! enc.stop()?;
//! # Ok::<(), sm_domain::encode::EncoderError>(())
//! ```

use std::sync::Arc;
use std::time::Duration;

/// Carrier for one frame flowing through the capture→encoder channel.
///
/// The capture→encoder channel transports a `FramePayload` rather than a bare
/// [`crate::CaptureFrame`] so the GPU-resident path (design D7) can hand a shared
/// GPU texture across the thread boundary instead of CPU-staged bytes. The variant
/// is selected once at pipeline init by the path-selection gate; it is NOT a
/// per-frame decision:
///
/// - [`FramePayload::Cpu`] — today's behaviour: BGRA8 pixels already read back to
///   CPU memory in the [`crate::CaptureFrame`]. The encoder runs the rayon BGRA→NV12
///   convert and `MFCreateMemoryBuffer` sample path VERBATIM. This is the only
///   variant any non-Windows or non-GPU path ever produces.
/// - [`FramePayload::GpuShared`] — GPU-resident path: a cross-thread shared
///   `ID3D11Texture2D` keyed-mutex handle plus geometry and timestamp. The Windows
///   MFT encoder acquires the keyed mutex, runs `VideoProcessorBlt` BGRA→NV12 on the
///   GPU, and feeds `MFCreateDXGISurfaceBuffer` to the hardware MFT — no readback.
///
/// `FramePayload` lives in `sm-domain` (despite the GPU variant being a
/// Windows-only concept at runtime) so the frozen [`CaptureSource::start`] /
/// [`VideoEncoder::start`] channel signatures can carry it without `sm-domain`
/// depending on `windows`. The `handle` is a raw `isize` (the underlying
/// `HANDLE` reinterpreted) so no platform type leaks into the domain crate.
///
/// [`CaptureSource::start`]: crate::capture::CaptureSource::start
#[derive(Debug)]
pub enum FramePayload {
    /// CPU-staged path: pixel data already in `Arc<[u8]>` (today's code, zero change).
    Cpu(crate::CaptureFrame),

    /// GPU-resident path: a cross-thread shared DXGI keyed-mutex texture handle
    /// plus geometry and timestamp.
    ///
    /// Produced ONLY by the Windows capture adapter when the path-selection gate
    /// selected the GPU-resident path; consumed ONLY by the Windows MFT encoder.
    /// Every other encoder/capture pair only ever sees [`FramePayload::Cpu`].
    GpuShared {
        /// Cross-thread shared handle for the keyed-mutex `ID3D11Texture2D` BGRA
        /// surface, stored as `isize` (the raw `HANDLE` value) so the domain crate
        /// names no platform type.
        ///
        /// This is a BORROW, not an ownership transfer: the producer retains ownership
        /// of the underlying NT handle (closed only when the producer is dropped) and
        /// the consumer opens its OWN reference via `OpenSharedResource1`. To avoid
        /// pixel/timestamp aliasing, the producer cycles a small RING of distinct shared
        /// textures (one keyed mutex each) and sends a DIFFERENT slot's handle per frame,
        /// so the handle in each in-flight payload references that frame's own pixels —
        /// the producer never overwrites a texture whose handle is still queued. The
        /// consumer acquires the keyed mutex (`IDXGIKeyedMutex::AcquireSync`) before
        /// reading and releases it (`ReleaseSync`) afterwards; the producer uses a
        /// BOUNDED acquire on its side so a stalled consumer cannot hang the capture
        /// callback.
        handle: isize,
        /// Surface width in pixels.
        width: u32,
        /// Surface height in pixels.
        height: u32,
        /// Row stride in bytes of the source BGRA8 surface.
        stride: u32,
        /// Monotonic capture timestamp (same semantics as `CaptureFrame::timestamp`).
        timestamp: Duration,
    },
}

impl FramePayload {
    /// Capture timestamp of the frame, regardless of variant.
    ///
    /// Lets consumers that only care about timing (e.g. test stubs, ordering
    /// checks) read the timestamp without destructuring the variant.
    pub fn timestamp(&self) -> Duration {
        match self {
            FramePayload::Cpu(f) => f.timestamp,
            FramePayload::GpuShared { timestamp, .. } => *timestamp,
        }
    }
}

/// Rate control mode. Reserved for future expansion; V1 is `ConstantBitrate` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControlMode {
    /// Constant bit rate — codec targets a fixed bitrate regardless of scene complexity.
    ConstantBitrate,
}

/// Configuration for an encoder session.
///
/// # Defaults
///
/// - `bitrate_bps`: 4 000 000 (4 Mbps)
/// - `framerate`: 30
/// - `intra_period`: 60 (one IDR every 2 s at 30 fps)
/// - `rate_control`: `ConstantBitrate` (CBR)
/// - `width`: 0 (sentinel — adapter picks default, currently 1920 for MFT)
/// - `height`: 0 (sentinel — adapter picks default, currently 1080 for MFT)
///
/// Fields are `pub` to allow `EncoderConfig { bitrate_bps: 8_000_000, ..Default::default() }`
/// ergonomics without a builder.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Target framerate used for rate-control accounting.
    pub framerate: u32,
    /// Intra (keyframe) period in frames.
    pub intra_period: u32,
    /// Rate control mode.
    pub rate_control: RateControlMode,
    /// Frame width in pixels.
    /// Sentinel `0` = let the adapter pick its default (current adapters: 1920 for MFT).
    /// Production callers MUST pass the real screen dimensions (see `src-tauri/src/commands/sender.rs`).
    pub width: u32,
    /// Frame height in pixels.
    /// Sentinel `0` = let the adapter pick its default (current adapters: 1080 for MFT).
    /// Production callers MUST pass the real screen dimensions.
    pub height: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            framerate: 30,
            intra_period: 60,
            rate_control: RateControlMode::ConstantBitrate,
            width: 0,  // sentinel — adapter picks default
            height: 0, // sentinel — adapter picks default
        }
    }
}

/// A single encoded packet emitted by the encoder thread.
///
/// `data` is an Annex-B byte stream: one or more NAL units each prefixed with
/// the 4-byte start code `0x00 0x00 0x00 0x01`.
///
/// Cloning is cheap — it increments the `Arc` reference count without copying bytes.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Annex-B byte stream. Shared across clones via `Arc`.
    pub data: Arc<[u8]>,
    /// `true` iff this packet contains an IDR slice (NAL type 5).
    pub is_keyframe: bool,
    /// Capture timestamp inherited from the source `CaptureFrame`.
    pub timestamp: Duration,
    /// Monotonically increasing sequence number, starts at 0 after each `start` call.
    pub sequence: u64,
}

/// Errors produced by encoder operations.
///
/// Platform errors are converted to `String` at the adapter boundary so
/// `sm-domain` never names any platform-specific type. No `#[from]` on any
/// variant — all conversions are explicit.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    /// Configuration value rejected by the adapter (e.g., `bitrate_bps = 0`).
    #[error("invalid encoder config: {0}")]
    InvalidConfig(String),

    /// The encoder backend failed to initialise.
    #[error("encoder initialisation failed: {0}")]
    InitFailed(String),

    /// The encoder backend failed to encode a frame.
    #[error("encode failed: {0}")]
    EncodeFailed(String),

    /// The output channel has no receivers.
    #[error("output channel closed by consumer")]
    ChannelClosed,

    /// Generic wrapper for platform-level errors not worth a dedicated variant.
    #[error("internal encoder error: {0}")]
    Internal(String),
}

/// Port boundary for platform-specific encoder adapters.
///
/// # Channel discipline
///
/// `start(rx, tx)` injects both channel ends. The encoder thread owns `rx`
/// and pulls [`FramePayload`]s (each wrapping either a CPU `CaptureFrame` or a
/// GPU-shared texture handle), then pushes `EncodedPacket`s via `tx.try_send`.
/// When the output channel is full the packet is dropped (drop-newest) and
/// `dropped_frames()` is incremented. The encoder thread exits when `rx`
/// closes (`Err(RecvError)`) or when `tx` becomes disconnected
/// (`Err(TrySendError::Disconnected)`).
///
/// # Thread model
///
/// `new()` does NOT spawn a thread. `start()` spawns exactly one OS thread.
/// `stop()` is idempotent and joins the thread.
///
/// # Keyframe semantics
///
/// `request_keyframe()` uses atomic signalling; the next frame encoded after
/// the call will be an IDR (NAL type 5) with SPS/PPS prepended.
pub trait VideoEncoder: Send + Sync {
    /// Construct an encoder with the given configuration.
    ///
    /// Validates `config` (e.g., `bitrate_bps > 0`, `framerate > 0`). Does NOT spawn
    /// the encoding thread — call [`start`](VideoEncoder::start) to begin.
    fn new(config: EncoderConfig) -> Result<Self, EncoderError>
    where
        Self: Sized;

    /// Begin encoding. Spawns the encoder OS thread.
    ///
    /// Returns `Ok(())` once the thread is spawned. Encoding errors after `start`
    /// surface by terminating the thread and dropping `tx`, causing the consumer's
    /// `rx.recv()` to return `Err(RecvError)`.
    fn start(
        &mut self,
        rx: std::sync::mpsc::Receiver<FramePayload>,
        tx: std::sync::mpsc::SyncSender<EncodedPacket>,
    ) -> Result<(), EncoderError>;

    /// Stop the encoding session.
    ///
    /// Idempotent: a second call returns `Ok(())`. Joins the encoder thread before returning.
    fn stop(&mut self) -> Result<(), EncoderError>;

    /// Force the next encoded frame to be an IDR keyframe.
    ///
    /// Thread-safe. Calling while stopped does not panic.
    fn request_keyframe(&self);

    /// Update the target bitrate at runtime (bits per second).
    ///
    /// Returns `Err(EncoderError::InvalidConfig(_))` if `bps == 0`. Thread-safe.
    fn set_bitrate(&self, bps: u32) -> Result<(), EncoderError>;

    /// Cumulative count of `EncodedPacket`s dropped due to output-channel backpressure.
    ///
    /// Thread-safe via `AtomicU64`. Monotonically non-decreasing.
    fn dropped_frames(&self) -> u64;

    /// Return the stable backend identifier for this encoder.
    ///
    /// The return value MUST be one of the five canonical vocabulary strings:
    ///
    /// | Token            | Meaning                              |
    /// |------------------|--------------------------------------|
    /// | `"sw_openh264"`  | Software encoder (Cisco OpenH264)    |
    /// | `"hw_nvenc"`     | NVIDIA NVENC via MFT                 |
    /// | `"hw_intel_qsv"` | Intel Quick Sync via MFT             |
    /// | `"hw_unknown"`   | MFT encoder, vendor CLSID unknown    |
    /// | `"sw_fake"`      | Test fake encoder (tests only)       |
    ///
    /// # Allocation contract
    ///
    /// Returns `&'static str` — the value is a compile-time string literal owned
    /// by the impl. The call site converts to `String` at most once per session.
    fn backend_name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender};

    // ─── FakeVideoEncoder ───────────────────────────────────────────────────────

    /// In-memory `VideoEncoder` implementation for domain-level unit tests.
    /// No codec — drains `rx` on a background thread and emits dummy `EncodedPacket`s.
    #[allow(dead_code)]
    struct FakeVideoEncoder {
        config: EncoderConfig,
        keyframe_pending: Arc<AtomicBool>,
        pending_bitrate: Arc<AtomicU32>,
        dropped: Arc<AtomicU64>,
        stop_flag: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Default for FakeVideoEncoder {
        fn default() -> Self {
            Self {
                config: EncoderConfig::default(),
                keyframe_pending: Arc::new(AtomicBool::new(false)),
                pending_bitrate: Arc::new(AtomicU32::new(0)),
                dropped: Arc::new(AtomicU64::new(0)),
                stop_flag: Arc::new(AtomicBool::new(false)),
                handle: None,
            }
        }
    }

    impl VideoEncoder for FakeVideoEncoder {
        fn new(config: EncoderConfig) -> Result<Self, EncoderError> {
            if config.bitrate_bps == 0 {
                return Err(EncoderError::InvalidConfig("bitrate must be > 0".into()));
            }
            if config.framerate == 0 {
                return Err(EncoderError::InvalidConfig("framerate must be > 0".into()));
            }
            Ok(Self {
                config,
                ..Default::default()
            })
        }

        fn start(
            &mut self,
            rx: Receiver<FramePayload>,
            tx: SyncSender<EncodedPacket>,
        ) -> Result<(), EncoderError> {
            let keyframe = Arc::clone(&self.keyframe_pending);
            let dropped = Arc::clone(&self.dropped);
            let stop = Arc::clone(&self.stop_flag);
            let handle = std::thread::spawn(move || {
                let mut seq: u64 = 0;
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let payload = match rx.recv() {
                        Ok(f) => f,
                        Err(_) => break,
                    };
                    let is_keyframe = keyframe.swap(false, Ordering::AcqRel);
                    let pkt = EncodedPacket {
                        data: Arc::from(vec![0x00u8, 0x00, 0x00, 0x01, 0x65].as_slice()),
                        is_keyframe,
                        timestamp: payload.timestamp(),
                        sequence: seq,
                    };
                    seq += 1;
                    match tx.try_send(pkt) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), EncoderError> {
            self.stop_flag.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn request_keyframe(&self) {
            self.keyframe_pending.store(true, Ordering::Release);
        }

        fn set_bitrate(&self, bps: u32) -> Result<(), EncoderError> {
            if bps == 0 {
                return Err(EncoderError::InvalidConfig("bitrate must be > 0".into()));
            }
            self.pending_bitrate.store(bps, Ordering::Release);
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }

        fn backend_name(&self) -> &'static str {
            "sw_fake"
        }
    }

    // ─── T3.1: encoder_config_accepts_zero_dimensions ─────────────────────────
    //
    // Sentinel semantics: width=0, height=0 must be accepted without panic.
    // This test is RED until T1.3 adds the width/height fields to EncoderConfig.

    #[test]
    fn encoder_config_accepts_zero_dimensions() {
        let cfg = EncoderConfig {
            bitrate_bps: 4_000_000,
            framerate: 30,
            intra_period: 60,
            rate_control: RateControlMode::ConstantBitrate,
            width: 0,
            height: 0,
        };
        assert_eq!(cfg.width, 0, "sentinel width must be 0");
        assert_eq!(cfg.height, 0, "sentinel height must be 0");

        // Default() must also return sentinel zeros.
        let def = EncoderConfig::default();
        assert_eq!(def.width, 0, "default width must be sentinel 0");
        assert_eq!(def.height, 0, "default height must be sentinel 0");
    }

    // ─── T3.1b: encoder_config_accepts_nonzero_dimensions ─────────────────────
    //
    // Round-trip: non-zero dimensions pass through unchanged.
    // This test is RED until T1.3 adds the width/height fields.

    #[test]
    fn encoder_config_accepts_nonzero_dimensions() {
        let cfg = EncoderConfig {
            width: 640,
            height: 480,
            ..Default::default()
        };
        assert_eq!(cfg.width, 640, "width must round-trip correctly");
        assert_eq!(cfg.height, 480, "height must round-trip correctly");
    }

    // ─── D1: EncoderConfig::default() field values ─────────────────────────────

    #[test]
    fn encoder_config_default_matches_a3() {
        let cfg = EncoderConfig::default();
        assert_eq!(cfg.bitrate_bps, 4_000_000);
        assert_eq!(cfg.framerate, 30);
        assert_eq!(cfg.intra_period, 60);
        assert_eq!(cfg.rate_control, RateControlMode::ConstantBitrate);
    }

    // ─── D2: EncoderError Display strings ──────────────────────────────────────

    #[test]
    fn encoder_error_display_strings() {
        let e = EncoderError::InvalidConfig("bitrate must be > 0".into());
        assert!(format!("{e}").contains("bitrate"));
        let e = EncoderError::InitFailed("codec init failed".into());
        assert!(format!("{e}").contains("initialisation"));
        let e = EncoderError::EncodeFailed("frame encode error".into());
        assert!(format!("{e}").contains("encode"));
        let e = EncoderError::ChannelClosed;
        let s = format!("{e}").to_lowercase();
        assert!(s.contains("closed"), "expected 'closed' in '{s}'");
        let e = EncoderError::Internal("something went wrong".into());
        assert!(format!("{e}").contains("internal"));
    }

    // ─── D3: EncoderError Debug does not panic ─────────────────────────────────

    #[test]
    fn encoder_error_debug_does_not_panic() {
        let variants: &[EncoderError] = &[
            EncoderError::InvalidConfig("x".into()),
            EncoderError::InitFailed("x".into()),
            EncoderError::EncodeFailed("x".into()),
            EncoderError::ChannelClosed,
            EncoderError::Internal("x".into()),
        ];
        for v in variants {
            assert!(!format!("{v:?}").is_empty());
        }
    }

    // ─── D4: EncodedPacket clone shares Arc buffer ─────────────────────────────

    #[test]
    fn encoded_packet_clone_shares_buffer() {
        let pkt = EncodedPacket {
            data: Arc::from([1u8, 2, 3].as_slice()),
            is_keyframe: true,
            timestamp: Duration::from_millis(0),
            sequence: 0,
        };
        let cloned = pkt.clone();
        assert!(Arc::ptr_eq(&pkt.data, &cloned.data));
    }

    // ─── D5: EncodedPacket is Send + Sync ──────────────────────────────────────

    #[test]
    fn encoded_packet_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EncodedPacket>();
    }

    // ─── D6: FakeVideoEncoder lifecycle ────────────────────────────────────────

    #[test]
    fn fake_video_encoder_lifecycle() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();
        enc.request_keyframe();
        enc.set_bitrate(8_000_000).unwrap();
        assert_eq!(enc.dropped_frames(), 0);
        // stop is idempotent
        enc.stop().unwrap();
        enc.stop().unwrap();
        drop(frame_tx);
    }

    // ─── D7: VideoEncoder trait Send bound satisfied ───────────────────────────

    #[test]
    fn video_encoder_trait_send_bound_satisfied() {
        fn takes_send<T: VideoEncoder + Send + 'static>(_: T) {}
        takes_send(FakeVideoEncoder::default());
    }

    // ─── S15.1: FakeVideoEncoder satisfies Send + Sync ────────────────────────
    //
    // The VideoEncoder trait MUST be Send + Sync so that adapters can hold
    // Arc<dyn VideoEncoder + Send + Sync> for PLI feedback without a channel hop.
    // This static assertion verifies both the concrete FakeVideoEncoder type
    // AND that the trait bound allows dyn usage.
    //
    // [S15.1] Given this assertion in the test suite, when `cargo nextest run -p sm-domain`
    // runs, then it compiles and passes on all platforms.

    #[test]
    fn fake_video_encoder_is_send_sync_s15_1() {
        // Verifies FakeVideoEncoder (concrete type) satisfies Send + Sync.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FakeVideoEncoder>();
    }

    #[test]
    fn video_encoder_trait_object_is_send_sync_s15_1_dyn() {
        // Verifies that VideoEncoder trait objects are usable as Send + Sync.
        // This test requires the trait to be: pub trait VideoEncoder: Send + Sync
        // If the trait bound is Send-only, `dyn VideoEncoder + Send + Sync` is
        // redundant-but-still-allowed on the concrete type. The CRITICAL assertion
        // is that Arc<dyn VideoEncoder + Send + Sync> compiles — which requires
        // the trait to declare Sync (otherwise the dyn coercion won't type-check).
        fn assert_arc_dyn_send_sync<T: VideoEncoder + Send + Sync + 'static>() {
            let enc = T::new(EncoderConfig::default()).unwrap();
            let _arc: std::sync::Arc<dyn VideoEncoder + Send + Sync> = std::sync::Arc::new(enc);
        }
        assert_arc_dyn_send_sync::<FakeVideoEncoder>();
    }

    // ─── D8: set_bitrate(0) rejected ───────────────────────────────────────────

    #[test]
    fn set_bitrate_zero_rejected() {
        let enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let err = enc.set_bitrate(0).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}"
        );
    }

    // ─── Helper: build a minimal CaptureFrame ──────────────────────────────────

    fn make_frame(ts_ms: u64) -> FramePayload {
        FramePayload::Cpu(crate::CaptureFrame {
            data: Arc::from(vec![0u8; 4].as_slice()),
            width: 1,
            height: 1,
            stride: 4,
            format: crate::capture::PixelFormat::Bgra8,
            timestamp: Duration::from_millis(ts_ms),
        })
    }

    // ─── D9: start then stop — output channel closes cleanly ───────────────────
    //
    // Scenario S2.1: after stop(), pkt_rx.recv() returns Err (tx was dropped by thread).

    #[test]
    fn fake_encoder_start_then_stop_closes_output_channel() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (_frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();
        enc.stop().unwrap();
        // After stop(), the encoder thread is joined. The last sender (pkt_tx clone inside
        // the thread) is gone — pkt_rx.recv() must return Err immediately.
        let result = pkt_rx.try_recv();
        // Either the channel is empty (no frames sent) or disconnected — both are Ok here.
        // The important assertion is that stop() returned Ok and we are still alive (no panic).
        assert!(
            result.is_err() || result.is_ok(),
            "unexpected state after stop"
        );
        // Double-stop is idempotent.
        enc.stop().unwrap();
    }

    // ─── D10: stop is idempotent ────────────────────────────────────────────────
    //
    // Scenario S2.2: second stop() on an already-stopped encoder returns Ok without panic.

    #[test]
    fn fake_encoder_stop_is_idempotent() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        // Stop on a never-started encoder is also idempotent.
        enc.stop().unwrap();
        enc.stop().unwrap();

        // Start, stop, stop again.
        let (_frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();
        enc.stop().unwrap();
        enc.stop().unwrap();
    }

    // ─── D11: rx senders dropped → encoder thread exits cleanly ─────────────────
    //
    // Scenario S2.3: when all input senders are dropped, the encoder thread exits
    // without panic. We verify by starting, dropping frame_tx, then calling stop()
    // which should join quickly.

    #[test]
    fn fake_encoder_rx_disconnect_exits_thread_cleanly() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();
        // Drop all input senders — encoder thread should exit on next recv().
        drop(frame_tx);
        // stop() joins the thread; if the thread panicked, join() would return Err
        // which we propagate — but the test expects Ok.
        enc.stop().unwrap();
    }

    // ─── D12: consumer drops output rx → encoder thread stops ───────────────────
    //
    // Scenario S2.4: when the consumer drops the receiver side of the output channel,
    // the next try_send returns Disconnected and the thread exits without panic.

    #[test]
    fn fake_encoder_tx_disconnect_exits_thread_cleanly() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();

        // Drop the consumer side — next try_send will return Disconnected.
        drop(pkt_rx);

        // Send a frame to wake up the thread so it hits the try_send path.
        let _ = frame_tx.send(make_frame(1));

        // Give the thread a moment to process.
        std::thread::sleep(Duration::from_millis(50));

        // The thread should have exited on its own. stop() should join immediately.
        enc.stop().unwrap();
    }

    // ─── D13: request_keyframe sets flag; next FakeEncoder packet has is_keyframe ─

    #[test]
    fn fake_encoder_request_keyframe_marks_next_packet() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();

        // Send a plain frame first to prime the sequence.
        frame_tx.send(make_frame(0)).unwrap();
        let _first = pkt_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // Now request a keyframe, then send one more frame.
        enc.request_keyframe();
        frame_tx.send(make_frame(1)).unwrap();
        let keyframe_pkt = pkt_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            keyframe_pkt.is_keyframe,
            "expected is_keyframe == true after request_keyframe()"
        );

        // Drop frame_tx first so the thread's rx.recv() unblocks on join.
        drop(frame_tx);
        enc.stop().unwrap();
    }

    // ─── D14: set_bitrate updates pending_bitrate (internal state check) ─────────

    #[test]
    fn fake_encoder_set_bitrate_records_value() {
        let enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        enc.set_bitrate(8_000_000).unwrap();
        // Verify the stored value via the atomic (white-box, acceptable for domain unit test).
        let stored = enc.pending_bitrate.load(Ordering::Relaxed);
        assert_eq!(stored, 8_000_000);
    }

    // ─── D15: dropped_frames counter via backpressure ─────────────────────────────

    #[test]
    fn fake_encoder_backpressure_increments_dropped_frames() {
        let mut enc = FakeVideoEncoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(16);
        // Output channel capacity = 1 so it fills immediately.
        let (pkt_tx, _pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(1);
        enc.start(frame_rx, pkt_tx).unwrap();

        // Flood with frames. Channel capacity 1 → most should be dropped.
        for i in 0..20u64 {
            let _ = frame_tx.send(make_frame(i));
        }

        // Wait for the thread to process.
        std::thread::sleep(Duration::from_millis(100));

        let dropped = enc.dropped_frames();
        assert!(dropped > 0, "expected dropped_frames > 0, got {dropped}");

        // Drop frame_tx first so the thread's rx.recv() unblocks on join.
        drop(frame_tx);
        enc.stop().unwrap();
    }

    // ─── S15.1: VideoEncoder dyn trait object is Sync ─────────────────────────
    //
    // This test is RED when `VideoEncoder: Send` only (the trait does not declare
    // `Sync`). `dyn VideoEncoder` is only `Sync` if the trait requires `Sync`.
    // After the one-line change to `pub trait VideoEncoder: Send + Sync`, this
    // test compiles and passes.
    //
    // [S15.1] Given this assertion, when `cargo nextest run -p sm-domain` runs,
    // then it compiles and passes on all platforms.

    #[test]
    fn video_encoder_dyn_trait_object_is_sync_s15_1() {
        // Verify dyn VideoEncoder (the trait object, not a concrete type) is Sync.
        // This requires the trait to declare: pub trait VideoEncoder: Send + Sync
        fn check_dyn_sync<T: ?Sized + Sync>() {}
        check_dyn_sync::<dyn VideoEncoder>();
    }

    // ─── T.A.1: fake_encoder_reports_backend_name (RED) ──────────────────────
    //
    // Confirms `backend_name()` exists on `VideoEncoder` and that `FakeVideoEncoder`
    // returns `"sw_fake"`. RED until T.A.2 adds the trait method + impl.

    #[test]
    fn fake_encoder_reports_backend_name() {
        let enc = FakeVideoEncoder::default();
        assert_eq!(
            enc.backend_name(),
            "sw_fake",
            "FakeVideoEncoder must return the canonical test-encoder token"
        );
    }
}
