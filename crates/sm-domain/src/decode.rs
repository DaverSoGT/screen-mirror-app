//! Port boundary for video decoding.
//!
//! This module defines the domain-level contract for decoding `EncodedPacket`s
//! (Annex-B H.264) into raw `DecodedFrame`s. No platform type, async runtime,
//! or codec-specific import is permitted here — all platform adaptation lives
//! in `sm-infra::decode`.
//!
//! # Key types
//!
//! | Type | Role |
//! |------|------|
//! | [`VideoDecoder`]            | Port trait implemented by each decoder adapter.        |
//! | [`DecoderConfig`]           | Configuration: hint width/height (decoder adapts).     |
//! | [`DecodedFrame`]            | A single decoded frame (raw pixel bytes + metadata).   |
//! | [`PixelData`]               | Pixel layout: `I420` (planar) or `Bgra8` (packed).     |
//! | [`DecoderError`]            | Unified error enum for all decoder operations.          |
//! | [`DECODE_CHANNEL_CAPACITY`] | Bounded channel capacity constant (4).                  |

use std::sync::Arc;
use std::time::Duration;

/// Bounded channel capacity for decoder input and output channels.
///
/// Mirrors `CAPTURE_CHANNEL_CAPACITY` (4), `ENCODE_CHANNEL_CAPACITY` (4), and
/// `TRANSPORT_CHANNEL_CAPACITY` (4). At 30 fps a 4-slot bounded channel buffers
/// ~133 ms — within the glass-to-glass budget while still backpressuring
/// egregiously slow consumers.
pub const DECODE_CHANNEL_CAPACITY: usize = 4;

/// Raw pixel data for a decoded frame.
///
/// V1 decoder adapters emit `I420` natively (openh264 output, BT.601 limited range).
/// `Bgra8` is reserved for the `i420_to_bgra` converter helper and the V2 native
/// render path.
///
/// Cloning is cheap — all `Arc<[u8]>` fields are reference-counted.
#[derive(Debug, Clone)]
pub enum PixelData {
    /// Planar YUV 4:2:0 (8-bit, BT.601 limited range).
    ///
    /// Plane sizes:
    /// - `y.len() == width * height`
    /// - `u.len() == (width / 2) * (height / 2)`
    /// - `v.len() == (width / 2) * (height / 2)`
    ///
    /// This invariant is enforced by the adapter when constructing `PixelData::I420`.
    I420 {
        /// Luma plane (`width × height` bytes).
        y: Arc<[u8]>,
        /// Cb (blue-difference chroma) plane (`(width/2) × (height/2)` bytes).
        u: Arc<[u8]>,
        /// Cr (red-difference chroma) plane (`(width/2) × (height/2)` bytes).
        v: Arc<[u8]>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
    },
    /// Packed BGRA 8-bit.
    ///
    /// `data.len() == width * height * 4` (4 bytes per pixel: Blue, Green, Red, Alpha=255).
    Bgra8 {
        /// Pixel data buffer (`width × height × 4` bytes).
        data: Arc<[u8]>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
    },
}

/// A single decoded frame emitted by the decoder thread.
///
/// Cloning is cheap — all buffers inside [`PixelData`] are `Arc<[u8]>`, so
/// clones share the underlying allocation (no byte copies).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Raw pixel data. Layout and dimensions depend on the variant.
    pub data: PixelData,
    /// Capture timestamp inherited from the source [`crate::encode::EncodedPacket`].
    pub timestamp: Duration,
    /// Monotonically increasing sequence number; resets to 0 after each `start()` call.
    pub sequence: u64,
}

/// Configuration for a decoder session.
///
/// # Defaults
///
/// - `width`:  1920 (PQ-D5; hint only — decoder adapts to actual SPS)
/// - `height`: 1080 (PQ-D5; hint only)
///
/// Fields are `pub` to allow `DecoderConfig { width: 1280, ..Default::default() }`
/// ergonomics without a builder.
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    /// Expected initial frame width in pixels (hint; decoder adapts to the actual SPS).
    pub width: u32,
    /// Expected initial frame height in pixels (hint; decoder adapts to the actual SPS).
    pub height: u32,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

/// Errors produced by decoder operations.
///
/// Platform errors are converted to `String` at the adapter boundary so
/// `sm-domain` never names any platform-specific type. No `#[from]` on any
/// variant — all conversions are explicit (parity with `CaptureError`,
/// `EncoderError`, `TransportError`).
#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    /// Configuration value rejected by the adapter, OR a required injection
    /// was missing (e.g., `start()` called before `set_receiver()` — R1.4 guard).
    #[error("invalid decoder config: {0}")]
    InvalidConfig(String),

    /// The decoder backend failed to initialise.
    #[error("decoder initialisation failed: {0}")]
    InitFailed(String),

    /// The decoder backend failed to decode a packet (corrupt NAL, missing IDR, etc.).
    #[error("decode failed: {0}")]
    DecodeFailed(String),

    /// The output channel has no receivers (consumer dropped `frame_rx`).
    #[error("output channel closed by consumer")]
    ChannelClosed,

    /// Generic wrapper for platform-level errors not worth a dedicated variant.
    #[error("internal decoder error: {0}")]
    Internal(String),
}

/// Port boundary for platform-specific decoder adapters.
///
/// # Channel discipline
///
/// `start(rx, frame_tx)` injects both channel ends. The decoder thread owns `rx`
/// and pulls [`crate::encode::EncodedPacket`]s, then pushes [`DecodedFrame`]s via
/// `frame_tx.try_send`. When the output channel is full the frame is dropped
/// (drop-newest) and [`dropped_frames()`](VideoDecoder::dropped_frames) is
/// incremented. The decoder thread exits when `rx` closes (`Err(RecvError)`) or
/// when `frame_tx` becomes disconnected (`Err(TrySendError::Disconnected)`).
///
/// # Thread model
///
/// `new()` does NOT spawn a thread. `start()` spawns exactly one OS thread.
/// `stop()` is idempotent and joins the thread.
///
/// CALLER MUST DROP the input `Sender<EncodedPacket>` BEFORE calling `stop()`
/// so the thread's `rx.recv()` unblocks naturally — same discipline as the encoder
/// and sender adapters.
///
/// # Receiver injection
///
/// Call [`set_receiver`](VideoDecoder::set_receiver) BEFORE [`start`](VideoDecoder::start).
/// `start()` returns `Err(InvalidConfig)` if no receiver is set (R1.4 guard,
/// mirroring `VideoSender::set_encoder` / R9.3).
///
/// The receiver is held inside the decoder thread for direct PLI response: when
/// decode errors accumulate or the very first IDR is awaited, the thread calls
/// `receiver.request_keyframe()` directly via the held `Arc`, without a channel hop.
pub trait VideoDecoder: Send + Sync {
    /// Construct a decoder with the given configuration.
    ///
    /// Validates `config` (e.g., `width > 0`, `height > 0`). Does NOT spawn a
    /// thread or initialise the codec backend — call
    /// [`start`](VideoDecoder::start) to begin.
    fn new(config: DecoderConfig) -> Result<Self, DecoderError>
    where
        Self: Sized;

    /// Inject the receiver reference for PLI feedback.
    ///
    /// MUST be called BEFORE [`start`](VideoDecoder::start). The receiver is held
    /// as `Arc<dyn VideoReceiver + Send + Sync>` so the decode thread can call
    /// `request_keyframe()` directly without a channel hop. Mirrors
    /// [`VideoSender::set_encoder`](crate::transport::VideoSender::set_encoder).
    fn set_receiver(&mut self, receiver: Arc<dyn crate::transport::VideoReceiver + Send + Sync>);

    /// Begin decoding. Spawns one OS thread that owns the codec backend.
    ///
    /// Returns `Err(DecoderError::InvalidConfig(_))` if `set_receiver()` was not
    /// called first (R1.4 guard). Returns `Ok(())` once the thread is spawned;
    /// decoding errors after `start()` surface by dropping `frame_tx`, causing the
    /// consumer's `frame_rx.recv()` to return `Err(RecvError)`.
    ///
    /// CALLER MUST DROP the input `Sender<EncodedPacket>` BEFORE calling `stop()`.
    fn start(
        &mut self,
        rx: std::sync::mpsc::Receiver<crate::encode::EncodedPacket>,
        frame_tx: std::sync::mpsc::SyncSender<DecodedFrame>,
    ) -> Result<(), DecoderError>;

    /// Stop the decoder. Idempotent. Joins the thread.
    ///
    /// A second call returns `Ok(())` without panicking (R1.6).
    fn stop(&mut self) -> Result<(), DecoderError>;

    /// Trigger an upstream PLI: signals the injected receiver to call
    /// `request_keyframe()`. No-op if no receiver was set or the decoder is
    /// already stopped. Thread-safe.
    fn request_keyframe(&self);

    /// Cumulative count of [`DecodedFrame`]s dropped due to output-channel backpressure.
    ///
    /// Monotonically non-decreasing. Thread-safe (backed by `AtomicU64`).
    fn dropped_frames(&self) -> u64;

    /// Cumulative count of [`crate::encode::EncodedPacket`]s rejected by the
    /// decoder backend (corrupt NALs, missing IDR before recovery, etc.).
    ///
    /// Monotonically non-decreasing. Thread-safe. Distinct from `dropped_frames` —
    /// this counts INPUT-side rejections; `dropped_frames` counts OUTPUT-side
    /// congestion.
    fn dropped_packets(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender};

    // ─── Compile-time trait bound assertion ──────────────────────────────────────
    //
    // Matches the const-fn pattern established in transport.rs.
    const _: () = {
        const fn _assert_send_sync<T: Send + Sync + ?Sized>() {}
        const fn _assert_video_decoder() {
            _assert_send_sync::<dyn VideoDecoder>();
        }
    };

    // ─── 1.1 RED: types + constant ───────────────────────────────────────────────

    // ── S2.1: DecoderConfig::default() is 1920×1080 ──────────────────────────────

    #[test]
    fn decoder_config_default_is_1920x1080() {
        let cfg = DecoderConfig::default();
        assert_eq!(cfg.width, 1920, "default width must be 1920");
        assert_eq!(cfg.height, 1080, "default height must be 1080");
    }

    // ── S2.6: DECODE_CHANNEL_CAPACITY == 4 ───────────────────────────────────────

    #[test]
    fn decode_channel_capacity_is_four() {
        assert_eq!(DECODE_CHANNEL_CAPACITY, 4);
    }

    // ── S2.5 (compile): PixelData has I420 and Bgra8 variants ───────────────────

    #[test]
    fn pixel_data_has_i420_and_bgra8_variants() {
        let y: Arc<[u8]> = Arc::from([0u8; 4].as_slice());
        let u: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let v: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let _i420 = PixelData::I420 {
            y: Arc::clone(&y),
            u: Arc::clone(&u),
            v: Arc::clone(&v),
            width: 2,
            height: 2,
        };
        let data: Arc<[u8]> = Arc::from([0u8; 16].as_slice());
        let _bgra8 = PixelData::Bgra8 {
            data,
            width: 2,
            height: 2,
        };
    }

    // ── S2.7: DecoderError Display strings are non-empty ─────────────────────────

    #[test]
    fn decoder_error_display_invalid_config() {
        let e = DecoderError::InvalidConfig("no receiver set".into());
        let s = format!("{e}");
        assert!(!s.is_empty(), "Display must be non-empty");
        assert!(
            s.contains("config") || s.contains("invalid"),
            "expected 'config' or 'invalid' in '{s}'"
        );
    }

    #[test]
    fn decoder_error_display_init_failed() {
        let e = DecoderError::InitFailed("codec init error".into());
        let s = format!("{e}");
        assert!(!s.is_empty());
        assert!(
            s.contains("init") || s.contains("initialisation") || s.contains("failed"),
            "expected init-related keyword in '{s}'"
        );
    }

    #[test]
    fn decoder_error_display_decode_failed() {
        let e = DecoderError::DecodeFailed("corrupt nal".into());
        let s = format!("{e}");
        assert!(!s.is_empty());
        assert!(
            s.contains("decode") || s.contains("failed"),
            "expected decode-related keyword in '{s}'"
        );
    }

    #[test]
    fn decoder_error_display_channel_closed() {
        let e = DecoderError::ChannelClosed;
        let s = format!("{e}");
        assert!(!s.is_empty());
        assert!(
            s.contains("closed") || s.contains("channel"),
            "expected 'closed' or 'channel' in '{s}'"
        );
    }

    #[test]
    fn decoder_error_display_internal() {
        let e = DecoderError::Internal("something broke".into());
        let s = format!("{e}");
        assert!(!s.is_empty());
        assert!(
            s.contains("internal") || s.contains("error"),
            "expected 'internal' or 'error' in '{s}'"
        );
    }

    // ─── 1.3 RED: DecodedFrame clone shares Arc buffer ───────────────────────────

    // ── S2.4: clone is cheap — Arc::ptr_eq returns true on the inner y/data arc ──

    #[test]
    fn decoded_frame_i420_clone_shares_arc_buffer() {
        let y: Arc<[u8]> = Arc::from([0u8; 4].as_slice());
        let u: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let v: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let frame = DecodedFrame {
            data: PixelData::I420 {
                y: Arc::clone(&y),
                u: Arc::clone(&u),
                v: Arc::clone(&v),
                width: 2,
                height: 2,
            },
            timestamp: Duration::ZERO,
            sequence: 0,
        };
        let cloned = frame.clone();
        // Both frames must share the same y-plane allocation.
        if let (PixelData::I420 { y: y1, .. }, PixelData::I420 { y: y2, .. }) =
            (&frame.data, &cloned.data)
        {
            assert!(Arc::ptr_eq(y1, y2), "clone must not copy the y-plane bytes");
        } else {
            panic!("both frames must have I420 variant");
        }
    }

    #[test]
    fn decoded_frame_bgra8_clone_shares_arc_buffer() {
        let data: Arc<[u8]> = Arc::from([0u8; 16].as_slice());
        let frame = DecodedFrame {
            data: PixelData::Bgra8 {
                data: Arc::clone(&data),
                width: 2,
                height: 2,
            },
            timestamp: Duration::ZERO,
            sequence: 0,
        };
        let cloned = frame.clone();
        if let (PixelData::Bgra8 { data: d1, .. }, PixelData::Bgra8 { data: d2, .. }) =
            (&frame.data, &cloned.data)
        {
            assert!(Arc::ptr_eq(d1, d2), "clone must not copy bgra8 data bytes");
        } else {
            panic!("both frames must have Bgra8 variant");
        }
    }

    // ── S2.5: DecodedFrame is Send + Sync (compile-time check) ───────────────────

    #[test]
    fn decoded_frame_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<DecodedFrame>();
    }

    // ── S2.2 / S2.3: plane-size discipline (compile only — enforced by adapter) ──

    #[test]
    fn pixel_data_i420_plane_sizes_2x2() {
        // Y = 2*2 = 4, U = 1*1 = 1, V = 1*1 = 1
        let y: Arc<[u8]> = Arc::from([0u8; 4].as_slice());
        let u: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let v: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
        let pd = PixelData::I420 {
            y: Arc::clone(&y),
            u: Arc::clone(&u),
            v: Arc::clone(&v),
            width: 2,
            height: 2,
        };
        if let PixelData::I420 {
            y,
            u,
            v,
            width,
            height,
        } = pd
        {
            assert_eq!(y.len(), (width * height) as usize);
            assert_eq!(u.len(), ((width / 2) * (height / 2)) as usize);
            assert_eq!(v.len(), ((width / 2) * (height / 2)) as usize);
        }
    }

    #[test]
    fn pixel_data_bgra8_plane_size_2x2() {
        // 2*2*4 = 16
        let data: Arc<[u8]> = Arc::from([0u8; 16].as_slice());
        let pd = PixelData::Bgra8 {
            data: Arc::clone(&data),
            width: 2,
            height: 2,
        };
        if let PixelData::Bgra8 {
            data,
            width,
            height,
        } = pd
        {
            assert_eq!(data.len(), (width * height * 4) as usize);
        }
    }

    // ─── 1.5 RED: VideoDecoder trait + FakeVideoDecoder ──────────────────────────

    // ── S1.5: dyn VideoDecoder is Send + Sync (runtime check mirrors compile-time) ─

    #[test]
    fn video_decoder_trait_is_dyn_send_sync() {
        fn check<T: Send + Sync + ?Sized>() {}
        check::<dyn VideoDecoder>();
    }

    // ─── FakeVideoDecoder ─────────────────────────────────────────────────────────

    /// Counting VideoReceiver fixture for PLI tests.
    struct CountingReceiver {
        keyframe_count: Arc<AtomicU64>,
    }

    impl CountingReceiver {
        fn new() -> (Self, Arc<AtomicU64>) {
            let count = Arc::new(AtomicU64::new(0));
            (
                Self {
                    keyframe_count: Arc::clone(&count),
                },
                count,
            )
        }
    }

    impl crate::transport::VideoReceiver for CountingReceiver {
        fn new(
            _config: crate::transport::TransportConfig,
        ) -> Result<Self, crate::transport::TransportError>
        where
            Self: Sized,
        {
            let (s, _) = Self::new();
            Ok(s)
        }

        fn start(
            &mut self,
            _pkt_tx: SyncSender<crate::encode::EncodedPacket>,
            _event_tx: SyncSender<crate::transport::TransportEvent>,
        ) -> Result<(), crate::transport::TransportError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), crate::transport::TransportError> {
            Ok(())
        }

        fn apply_remote_offer(
            &self,
            _offer: crate::signaling::SdpOffer,
        ) -> Result<crate::signaling::SdpAnswer, crate::transport::TransportError> {
            Ok(crate::signaling::SdpAnswer("v=0\r\n".to_string()))
        }

        fn add_remote_candidate(
            &self,
            _cand: crate::signaling::IceCandidate,
        ) -> Result<(), crate::transport::TransportError> {
            Ok(())
        }

        fn request_keyframe(&self) -> Result<(), crate::transport::TransportError> {
            self.keyframe_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            0
        }
    }

    /// In-memory `VideoDecoder` for domain-level unit tests.
    /// Drains rx, emits a dummy `DecodedFrame` per packet, tracks counters.
    struct FakeVideoDecoder {
        #[allow(dead_code)]
        config: DecoderConfig,
        receiver: Option<Arc<dyn crate::transport::VideoReceiver + Send + Sync>>,
        keyframe_pending: Arc<AtomicBool>,
        dropped_frames: Arc<AtomicU64>,
        dropped_packets: Arc<AtomicU64>,
        stop_flag: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl VideoDecoder for FakeVideoDecoder {
        fn new(config: DecoderConfig) -> Result<Self, DecoderError> {
            if config.width == 0 || config.height == 0 {
                return Err(DecoderError::InvalidConfig(
                    "width and height must be > 0".into(),
                ));
            }
            Ok(Self {
                config,
                receiver: None,
                keyframe_pending: Arc::new(AtomicBool::new(false)),
                dropped_frames: Arc::new(AtomicU64::new(0)),
                dropped_packets: Arc::new(AtomicU64::new(0)),
                stop_flag: Arc::new(AtomicBool::new(false)),
                handle: None,
            })
        }

        fn set_receiver(
            &mut self,
            receiver: Arc<dyn crate::transport::VideoReceiver + Send + Sync>,
        ) {
            self.receiver = Some(receiver);
        }

        fn start(
            &mut self,
            rx: Receiver<crate::encode::EncodedPacket>,
            frame_tx: SyncSender<DecodedFrame>,
        ) -> Result<(), DecoderError> {
            if self.receiver.is_none() {
                return Err(DecoderError::InvalidConfig(
                    "set_receiver() must be called before start()".into(),
                ));
            }
            self.stop_flag.store(false, Ordering::Release);
            let stop = Arc::clone(&self.stop_flag);
            let dropped_frames = Arc::clone(&self.dropped_frames);
            let dropped_packets = Arc::clone(&self.dropped_packets);
            let keyframe_pending = Arc::clone(&self.keyframe_pending);
            let receiver = self.receiver.clone();

            let handle = std::thread::spawn(move || {
                // Fire initial PLI so the remote sends an IDR (R1.4 / R3.8 pattern).
                if let Some(ref recv) = receiver {
                    let _ = recv.request_keyframe();
                }
                let mut seq: u64 = 0;
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    // Apply any pending PLI request before blocking on recv.
                    if keyframe_pending.swap(false, Ordering::AcqRel) {
                        if let Some(ref recv) = receiver {
                            let _ = recv.request_keyframe();
                        }
                    }
                    let pkt = match rx.recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    };
                    // Simulate "corrupt" packet: data shorter than 5 bytes.
                    if pkt.data.len() < 5 {
                        dropped_packets.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // Emit a dummy I420 2×2 frame.
                    let y: Arc<[u8]> = Arc::from([0u8; 4].as_slice());
                    let u: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
                    let v: Arc<[u8]> = Arc::from([128u8; 1].as_slice());
                    let frame = DecodedFrame {
                        data: PixelData::I420 {
                            y,
                            u,
                            v,
                            width: 2,
                            height: 2,
                        },
                        timestamp: pkt.timestamp,
                        sequence: seq,
                    };
                    seq += 1;
                    match frame_tx.try_send(frame) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            dropped_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), DecoderError> {
            self.stop_flag.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn request_keyframe(&self) {
            self.keyframe_pending.store(true, Ordering::Release);
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped_frames.load(Ordering::Relaxed)
        }

        fn dropped_packets(&self) -> u64 {
            self.dropped_packets.load(Ordering::Relaxed)
        }
    }

    // ── S1.2: FakeVideoDecoder::new with default config returns Ok ────────────────

    #[test]
    fn fake_decoder_config_default_accepted() {
        let result = FakeVideoDecoder::new(DecoderConfig::default());
        assert!(
            result.is_ok(),
            "FakeVideoDecoder::new(default config) must return Ok"
        );
    }

    // ── S1.4: start() without set_receiver() returns InvalidConfig ───────────────

    #[test]
    fn fake_decoder_start_without_receiver_returns_invalid_config() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        let (_pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(4);
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        let err = dec.start(pkt_rx, frame_tx).unwrap_err();
        assert!(
            matches!(err, DecoderError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}"
        );
    }

    // ── S1.2: full start → stop lifecycle ────────────────────────────────────────

    #[test]
    fn fake_decoder_start_then_stop_ok() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(4);
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        dec.start(pkt_rx, frame_tx).unwrap();
        // Drop input sender so thread unblocks on recv().
        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ── S1.3: stop is idempotent ──────────────────────────────────────────────────

    #[test]
    fn fake_decoder_stop_is_idempotent() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        // stop on never-started — must not panic
        dec.stop().unwrap();
        dec.stop().unwrap();

        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(4);
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        dec.start(pkt_rx, frame_tx).unwrap();
        drop(pkt_tx);
        dec.stop().unwrap();
        dec.stop().unwrap(); // second stop — no panic
    }

    // ── S1.6: request_keyframe propagates to the counting receiver ────────────────

    #[test]
    fn fake_decoder_request_keyframe_propagates() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        let (counting, count) = CountingReceiver::new();
        let receiver_arc: Arc<dyn crate::transport::VideoReceiver + Send + Sync> =
            Arc::new(counting);
        dec.set_receiver(Arc::clone(&receiver_arc));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(4);
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        dec.start(pkt_rx, frame_tx).unwrap();

        // Give the thread a moment to fire the initial PLI (first-frame PLI).
        std::thread::sleep(std::time::Duration::from_millis(50));
        let after_start = count.load(Ordering::Relaxed);
        assert!(
            after_start >= 1,
            "initial PLI must fire at least once on start"
        );

        // Request another keyframe via the public API.
        dec.request_keyframe();
        // Send a valid packet so the thread wakes up from recv() and checks the flag.
        let valid_nal: Arc<[u8]> = Arc::from([0x00u8, 0x00, 0x00, 0x01, 0x65].as_slice());
        let _ = pkt_tx.try_send(crate::encode::EncodedPacket {
            data: valid_nal,
            is_keyframe: true,
            timestamp: Duration::ZERO,
            sequence: 0,
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        let after_request = count.load(Ordering::Relaxed);
        assert!(
            after_request >= 2,
            "keyframe count must be >= 2 after explicit request, got {after_request}"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ── R1.7: dropped_frames increments when frame_tx channel is full ─────────────

    #[test]
    fn fake_decoder_dropped_frames_increments_on_full_channel() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(16);
        // Output channel capacity = 1 so it fills quickly.
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(1);
        dec.start(pkt_rx, frame_tx).unwrap();

        // Flood with valid packets (≥5 bytes so they are not "corrupt").
        let valid_nal: Arc<[u8]> = Arc::from([0x00u8, 0x00, 0x00, 0x01, 0x65].as_slice());
        for i in 0..20u64 {
            let _ = pkt_tx.try_send(crate::encode::EncodedPacket {
                data: Arc::clone(&valid_nal),
                is_keyframe: i == 0,
                timestamp: Duration::from_millis(i * 33),
                sequence: i,
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        let dropped = dec.dropped_frames();
        assert!(
            dropped > 0,
            "dropped_frames must be > 0 after flooding a capacity-1 channel, got {dropped}"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ── R1.8: dropped_packets increments for "corrupt" (short) packets ─────────────

    #[test]
    fn fake_decoder_dropped_packets_increments_on_corrupt_packet() {
        let mut dec = FakeVideoDecoder::new(DecoderConfig::default()).unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));
        let (pkt_tx, pkt_rx) = std::sync::mpsc::sync_channel::<crate::encode::EncodedPacket>(4);
        let (frame_tx, _frame_rx) = std::sync::mpsc::sync_channel::<DecodedFrame>(4);
        dec.start(pkt_rx, frame_tx).unwrap();

        // Send a "corrupt" packet (< 5 bytes) — FakeVideoDecoder increments dropped_packets.
        let corrupt: Arc<[u8]> = Arc::from([0x00u8, 0x01].as_slice());
        pkt_tx
            .send(crate::encode::EncodedPacket {
                data: corrupt,
                is_keyframe: false,
                timestamp: Duration::ZERO,
                sequence: 0,
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            dec.dropped_packets(),
            1,
            "dropped_packets must be 1 after one corrupt packet"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }
}
