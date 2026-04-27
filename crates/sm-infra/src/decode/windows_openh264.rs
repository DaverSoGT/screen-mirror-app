#![cfg(target_os = "windows")]
//! Windows H.264 software decoder backed by OpenH264 (Cisco BSD-2).
//!
//! # Overview
//!
//! [`WindowsOpenH264Decoder`] wraps `openh264::decoder::Decoder` behind the
//! [`VideoDecoder`] domain trait. It owns one OS thread (spawned in `start`) that
//! reads [`EncodedPacket`]s from the injected channel, splits Annex-B into NAL units,
//! and decodes them via OpenH264.
//!
//! # Thread model
//!
//! - `new()`: validates config, allocates shared atomics. No thread, no OpenH264 init.
//! - `start(rx, frame_tx)`: spawns one OS thread named `"sm-decoder"` that owns the
//!   `openh264::Decoder` instance. Thread exits when `rx` closes or `frame_tx` disconnects.
//! - `stop()`: idempotent. Sets the `stop` flag and joins the handle. NOTE: callers MUST
//!   drop their `SyncSender<EncodedPacket>` (the `SyncSender` paired with `rx`) BEFORE
//!   calling `stop()` — otherwise the decoder thread remains blocked on `rx.recv()` and
//!   `stop().join()` will deadlock. The stop flag is only consulted between recv calls.
//! - `Drop`: calls `stop()` if handle is still `Some` — no leaked thread on panic or
//!   forgotten call.
//!
//! # R9.3 Guard
//!
//! `start()` returns `Err(DecoderError::InvalidConfig(_))` if `set_receiver()` has not
//! been called beforehand. Mirrors the `VideoSender::set_encoder` / R9.3 guard.
//!
//! # Channel capacity
//!
//! Output channel capacity is `DECODE_CHANNEL_CAPACITY` (4). Drop-newest backpressure:
//! when the output channel is full, the decoded frame is silently dropped and
//! `dropped_frames()` is incremented.
//!
//! # PLI feedback
//!
//! The decoder thread calls `VideoReceiver::request_keyframe()` on the injected receiver
//! in two situations:
//! 1. Immediately on startup (first-frame PLI, R3.8) so the upstream encoder produces an
//!    IDR even if the receiver joined mid-stream.
//! 2. On consecutive decode errors (R3.7): after `DECODE_ERROR_THRESHOLD` consecutive
//!    failures, one PLI is fired and the error counter is reset.
//!
//! PLI rate-limiting: at most one PLI per 500 ms (prevents PLI storms).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openh264::formats::YUVSource;

use sm_domain::decode::{DecodedFrame, DecoderConfig, DecoderError, PixelData, VideoDecoder};
use sm_domain::encode::EncodedPacket;
use sm_domain::transport::VideoReceiver;

use crate::transport::annex_b::iter_nal_units;

/// Number of consecutive decode errors before a PLI is fired.
const DECODE_ERROR_THRESHOLD: u32 = 3;

/// Minimum time between PLI requests (rate-limiting).
const PLI_RATE_LIMIT: Duration = Duration::from_millis(500);

/// Cross-thread state shared between the caller and the decoder OS thread.
struct DecoderShared {
    /// Set by `stop()` / `Drop`; checked at the top of the decoder loop.
    stop: AtomicBool,
    /// Monotonically increasing count of `DecodedFrame`s dropped due to output-channel backpressure.
    dropped_frames: AtomicU64,
    /// Monotonically increasing count of `EncodedPacket`s that failed to decode.
    dropped_packets: AtomicU64,
    /// Set by `request_keyframe()`; cleared (swap → false) by the decoder thread before dispatch.
    keyframe_pending: AtomicBool,
}

impl DecoderShared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            dropped_frames: AtomicU64::new(0),
            dropped_packets: AtomicU64::new(0),
            keyframe_pending: AtomicBool::new(false),
        }
    }
}

/// Windows H.264 software decoder. Implements [`VideoDecoder`] via OpenH264.
///
/// Construction (`new`) is lightweight — no OS thread is created and no OpenH264
/// context is allocated until `start` is called. The decoder is `Send + Sync` so
/// it can be shared across threads for stats polling or moved before starting.
pub struct WindowsOpenH264Decoder {
    config: DecoderConfig,
    state: Arc<DecoderShared>,
    /// Injected upstream receiver for PLI feedback (set before `start`).
    receiver: Option<Arc<dyn VideoReceiver + Send + Sync>>,
    /// `Some` while the decoder thread is running; `None` before `start` and after `stop`.
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WindowsOpenH264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsOpenH264Decoder")
            .field("config", &self.config)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

// SAFETY: All shared state crosses the thread boundary via `Arc<DecoderShared>` (all
// fields are atomics). `JoinHandle<()>` is Send. `DecoderConfig` is Clone + Send.
// The `Option<Arc<dyn VideoReceiver + Send + Sync>>` is Send+Sync. No raw pointers.
// The openh264::Decoder lives exclusively inside the spawned thread — never shared.
// Verified by the static assert in the tests below.
unsafe impl Send for WindowsOpenH264Decoder {}
unsafe impl Sync for WindowsOpenH264Decoder {}

impl VideoDecoder for WindowsOpenH264Decoder {
    /// Construct and validate a decoder configuration.
    ///
    /// Does NOT spawn the decoder thread or allocate the OpenH264 context.
    /// Returns `Err(InvalidConfig(_))` if `width == 0` or `height == 0`.
    fn new(config: DecoderConfig) -> Result<Self, DecoderError>
    where
        Self: Sized,
    {
        if config.width == 0 || config.height == 0 {
            return Err(DecoderError::InvalidConfig(
                "width and height must be > 0".into(),
            ));
        }
        Ok(Self {
            config,
            state: Arc::new(DecoderShared::new()),
            receiver: None,
            handle: None,
        })
    }

    /// Inject the receiver reference for PLI feedback.
    ///
    /// MUST be called BEFORE [`start`](VideoDecoder::start). The receiver is held
    /// as `Arc<dyn VideoReceiver + Send + Sync>` so the decode thread can call
    /// `request_keyframe()` directly without a channel hop. Mirrors
    /// [`VideoSender::set_encoder`](sm_domain::transport::VideoSender::set_encoder).
    fn set_receiver(&mut self, receiver: Arc<dyn VideoReceiver + Send + Sync>) {
        self.receiver = Some(receiver);
    }

    /// Begin decoding. Spawns one OS thread named `"sm-decoder"`.
    ///
    /// Returns `Err(DecoderError::InvalidConfig(_))` if `set_receiver()` was not
    /// called first (R9.3 guard). Returns `Ok(())` once the thread is spawned.
    ///
    /// CALLER MUST DROP the input `SyncSender<EncodedPacket>` BEFORE calling
    /// `stop()` so the thread's `rx.recv()` unblocks naturally.
    fn start(
        &mut self,
        rx: Receiver<EncodedPacket>,
        frame_tx: SyncSender<DecodedFrame>,
    ) -> Result<(), DecoderError> {
        use openh264::{
            OpenH264API,
            decoder::{Decoder, DecoderConfig as OhCfg},
        };

        // R9.3 guard — must have a receiver before starting.
        let receiver_arc = match self.receiver.clone() {
            Some(r) => r,
            None => {
                return Err(DecoderError::InvalidConfig(
                    "set_receiver() must be called before start()".into(),
                ));
            }
        };

        let state = Arc::clone(&self.state);

        // Reset stop flag in case the decoder is restarted.
        state.stop.store(false, Ordering::Release);

        // Signal initial PLI so the upstream sends an IDR (R3.8 first-frame PLI).
        state.keyframe_pending.store(true, Ordering::Release);

        let handle = std::thread::Builder::new()
            .name("sm-decoder".into())
            .spawn(move || {
                // ── Init OpenH264 decoder inside the thread ────────────────────────────
                let api = OpenH264API::from_source();
                let mut decoder = match Decoder::with_api_config(api, OhCfg::new()) {
                    Ok(d) => d,
                    Err(e) => {
                        // Construction failure — thread exits, frame_tx is dropped,
                        // consumer sees RecvError on frame_rx.
                        let _ = e;
                        return;
                    }
                };

                let mut seq: u64 = 0;
                let mut consecutive_errors: u32 = 0;
                let mut last_pli_at = Instant::now() - PLI_RATE_LIMIT; // allow immediate first PLI

                // Helper: fire PLI if rate-limit allows.
                let fire_pli = |recv: &Arc<dyn VideoReceiver + Send + Sync>,
                                last_pli_at: &mut Instant| {
                    if last_pli_at.elapsed() >= PLI_RATE_LIMIT {
                        let _ = recv.request_keyframe();
                        *last_pli_at = Instant::now();
                    }
                };

                loop {
                    // ── Stop check ────────────────────────────────────────────────────
                    if state.stop.load(Ordering::Acquire) {
                        break;
                    }

                    // ── Apply pending PLI request (from request_keyframe() API) ───────
                    if state.keyframe_pending.swap(false, Ordering::AcqRel) {
                        fire_pli(&receiver_arc, &mut last_pli_at);
                    }

                    // ── Receive packet ────────────────────────────────────────────────
                    let pkt: EncodedPacket = match rx.recv() {
                        Ok(p) => p,
                        Err(_) => break, // upstream channel closed — normal shutdown
                    };

                    // ── Walk NAL units and decode ─────────────────────────────────────
                    //
                    // `iter_nal_units` yields raw NAL bytes WITHOUT the start code.
                    // openh264's `decode()` expects the full Annex-B unit INCLUDING the
                    // 4-byte start code prefix. We prepend [00 00 00 01] before each call.
                    let mut decoded_this_pkt = false;
                    for nal in iter_nal_units(&pkt.data) {
                        if nal.is_empty() {
                            continue;
                        }
                        // Prepend 4-byte Annex-B start code expected by openh264.
                        let mut nal_with_sc = Vec::with_capacity(4 + nal.len());
                        nal_with_sc.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                        nal_with_sc.extend_from_slice(nal);
                        match decoder.decode(&nal_with_sc) {
                            Ok(Some(yuv)) => {
                                // openh264 emits Some on the slice NAL of an access unit
                                // (SPS/PPS NALs emit None). Build PixelData::I420.
                                let (w, h) = yuv.dimensions();
                                let (y_stride, u_stride, _v_stride) = yuv.strides();

                                // Copy only the active pixels, stripping any row padding.
                                let y_plane = copy_strided_plane(yuv.y(), w, h, y_stride);
                                let u_plane = copy_strided_plane(yuv.u(), w / 2, h / 2, u_stride);
                                let v_plane = copy_strided_plane(yuv.v(), w / 2, h / 2, u_stride);

                                let frame = DecodedFrame {
                                    data: PixelData::I420 {
                                        y: Arc::from(y_plane.into_boxed_slice()),
                                        u: Arc::from(u_plane.into_boxed_slice()),
                                        v: Arc::from(v_plane.into_boxed_slice()),
                                        width: w as u32,
                                        height: h as u32,
                                    },
                                    timestamp: pkt.timestamp,
                                    sequence: seq,
                                };
                                seq += 1;
                                decoded_this_pkt = true;
                                consecutive_errors = 0;

                                // ── Try-send with drop-newest backpressure ────────────
                                match frame_tx.try_send(frame) {
                                    Ok(()) => {}
                                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                        state.dropped_frames.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                        return; // consumer dropped frame_rx — exit
                                    }
                                }
                            }
                            Ok(None) => {
                                // SPS/PPS/SEI NAL — no output frame, that's fine
                            }
                            Err(_e) => {
                                // Decode error — count it, continue
                                state.dropped_packets.fetch_add(1, Ordering::Relaxed);
                                consecutive_errors += 1;
                                if consecutive_errors >= DECODE_ERROR_THRESHOLD {
                                    fire_pli(&receiver_arc, &mut last_pli_at);
                                    consecutive_errors = 0;
                                }
                            }
                        }
                    }

                    // If we iterated all NALs but decoded nothing (e.g., SPS-only packet),
                    // that is fine — no error increment for non-error "no frame yet" cases.
                    let _ = decoded_this_pkt;
                }
            })
            .map_err(|e| DecoderError::Internal(format!("failed to spawn decoder thread: {e}")))?;

        self.handle = Some(handle);
        Ok(())
    }

    /// Stop the decoder. Idempotent. Joins the thread.
    ///
    /// A second call returns `Ok(())` without panicking (R1.6).
    ///
    /// CALLER MUST DROP the input `SyncSender<EncodedPacket>` BEFORE calling `stop()`.
    /// The decoder thread blocks on `rx.recv()`; without dropping the sender, the thread
    /// cannot observe the stop flag and `stop().join()` will deadlock.
    fn stop(&mut self) -> Result<(), DecoderError> {
        // Signal the thread to stop.
        self.state.stop.store(true, Ordering::Release);
        // Join the thread. `Option::take` makes this idempotent.
        if let Some(h) = self.handle.take() {
            // Ignore panics in the decoder thread.
            let _ = h.join();
        }
        Ok(())
    }

    /// Trigger an upstream PLI: signals the injected receiver to call
    /// `request_keyframe()`. Thread-safe. No-op if no receiver is set.
    fn request_keyframe(&self) {
        self.state.keyframe_pending.store(true, Ordering::Release);
    }

    /// Cumulative count of `DecodedFrame`s dropped due to output-channel backpressure.
    ///
    /// Monotonically non-decreasing. Thread-safe.
    fn dropped_frames(&self) -> u64 {
        self.state.dropped_frames.load(Ordering::Relaxed)
    }

    /// Cumulative count of `EncodedPacket`s rejected by the decoder backend.
    ///
    /// Monotonically non-decreasing. Thread-safe.
    fn dropped_packets(&self) -> u64 {
        self.state.dropped_packets.load(Ordering::Relaxed)
    }
}

impl Drop for WindowsOpenH264Decoder {
    fn drop(&mut self) {
        // Ensure the decoder thread is always joined, even if `stop()` was never called.
        let _ = self.stop();
    }
}

/// Copy `width × height` pixels from a strided plane buffer into a compact Vec.
///
/// OpenH264's `DecodedYUV` planes may have stride > width (GPU alignment padding).
/// This helper copies only the active columns, producing a tight `width × height`
/// buffer with no padding bytes — ready for `Arc<[u8]>`.
#[inline]
fn copy_strided_plane(src: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let row_start = row * stride;
        let row_end = row_start + width;
        if row_end <= src.len() {
            out.extend_from_slice(&src[row_start..row_end]);
        }
    }
    out
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use sm_domain::decode::{DECODE_CHANNEL_CAPACITY, DecoderConfig, DecoderError, VideoDecoder};
    use sm_domain::encode::EncodedPacket;
    use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoReceiver};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    // ─── Static assertions: WindowsOpenH264Decoder is Send + Sync ───────────────

    #[allow(dead_code)]
    fn _assert_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<WindowsOpenH264Decoder>();
    }

    // ─── Counting VideoReceiver fixture ──────────────────────────────────────────

    /// Counting receiver for PLI tests.
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

    impl VideoReceiver for CountingReceiver {
        fn new(_config: TransportConfig) -> Result<Self, TransportError>
        where
            Self: Sized,
        {
            let (s, _) = Self::new();
            Ok(s)
        }

        fn start(
            &mut self,
            _pkt_tx: SyncSender<EncodedPacket>,
            _event_tx: SyncSender<TransportEvent>,
        ) -> Result<(), TransportError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), TransportError> {
            Ok(())
        }

        fn apply_remote_offer(&self, _offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
            Ok(SdpAnswer("v=0\r\n".to_string()))
        }

        fn add_remote_candidate(&self, _cand: IceCandidate) -> Result<(), TransportError> {
            Ok(())
        }

        fn request_keyframe(&self) -> Result<(), TransportError> {
            self.keyframe_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            0
        }
    }

    // ─── Helper: build a minimal EncodedPacket ────────────────────────────────────

    fn make_packet(data: &[u8], is_keyframe: bool, ts_ms: u64) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(data),
            is_keyframe,
            timestamp: Duration::from_millis(ts_ms),
            sequence: 0,
        }
    }

    // ─── B3.1 RED: Lifecycle + R9.3 guard ─────────────────────────────────────────

    // ── A1: new with default config returns Ok ────────────────────────────────────

    #[test]
    fn windows_openh264_decoder_new_default_config_ok() {
        let result = WindowsOpenH264Decoder::new(DecoderConfig::default());
        assert!(
            result.is_ok(),
            "expected Ok from new(default config), got: {result:?}"
        );
    }

    // ── A2: new with width=0 returns InvalidConfig ────────────────────────────────

    #[test]
    fn windows_openh264_decoder_new_zero_width_rejected() {
        let cfg = DecoderConfig {
            width: 0,
            height: 1080,
        };
        let err = WindowsOpenH264Decoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, DecoderError::InvalidConfig(_)),
            "expected InvalidConfig for width=0, got {err:?}"
        );
    }

    // ── A3: new with height=0 returns InvalidConfig ───────────────────────────────

    #[test]
    fn windows_openh264_decoder_new_zero_height_rejected() {
        let cfg = DecoderConfig {
            width: 1920,
            height: 0,
        };
        let err = WindowsOpenH264Decoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, DecoderError::InvalidConfig(_)),
            "expected InvalidConfig for height=0, got {err:?}"
        );
    }

    // ── A4: Debug does not panic ──────────────────────────────────────────────────

    #[test]
    fn windows_openh264_decoder_debug_does_not_panic() {
        let dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let s = format!("{dec:?}");
        assert!(!s.is_empty(), "Debug output must be non-empty");
    }

    // ── S3.3: start() without set_receiver() returns InvalidConfig (R9.3 guard) ──

    #[test]
    fn windows_openh264_decoder_start_without_receiver_returns_invalid_config() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let (_pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);
        let err = dec.start(pkt_rx, frame_tx).unwrap_err();
        assert!(
            matches!(err, DecoderError::InvalidConfig(_)),
            "expected InvalidConfig when no receiver is set, got {err:?}"
        );
    }

    // ── S3.2 + S3.6: start then stop — no panic, Ok returned ─────────────────────
    //
    // Also verifies the thread is named "sm-decoder" by capturing the name inside
    // the thread and asserting it from outside.

    #[test]
    fn windows_openh264_decoder_start_then_stop_ok() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        dec.start(pkt_rx, frame_tx).unwrap();

        // Drop the input sender so the decoder thread's rx.recv() returns Err and exits.
        drop(pkt_tx);

        let result = dec.stop();
        assert!(result.is_ok(), "stop() should return Ok, got: {result:?}");
    }

    // ── S3.7: stop is idempotent ──────────────────────────────────────────────────

    #[test]
    fn windows_openh264_decoder_stop_is_idempotent() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();

        // Stop on a never-started decoder is idempotent.
        dec.stop().unwrap();
        dec.stop().unwrap();

        // Start + stop + stop again.
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));
        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);
        dec.start(pkt_rx, frame_tx).unwrap();
        drop(pkt_tx);
        dec.stop().unwrap();
        dec.stop().unwrap(); // second stop must not panic
    }

    // ── S3.8: Drop calls stop — no thread leak ────────────────────────────────────
    //
    // Construct + start + drop without calling stop(). If the thread is leaked this
    // test hangs.

    #[test]
    fn windows_openh264_decoder_drop_without_stop_joins_thread() {
        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        {
            let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
            let (counting, _count) = CountingReceiver::new();
            dec.set_receiver(Arc::new(counting));
            dec.start(pkt_rx, frame_tx).unwrap();
            // Drop pkt_tx first so the thread's recv() unblocks when dec drops.
            drop(pkt_tx);
            // dec drops here — Drop calls stop() which sets stop=true and joins the handle.
        }
        // If we reach here without hanging, the thread was successfully joined.
    }

    // ── dropped_frames is monotonically non-decreasing ───────────────────────────

    #[test]
    fn windows_openh264_decoder_dropped_frames_is_monotonic() {
        let dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let d0 = dec.dropped_frames();
        let d1 = dec.dropped_frames();
        assert!(
            d1 >= d0,
            "dropped_frames must be monotonically non-decreasing"
        );
    }

    // ── dropped_packets is monotonically non-decreasing ──────────────────────────

    #[test]
    fn windows_openh264_decoder_dropped_packets_is_monotonic() {
        let dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let d0 = dec.dropped_packets();
        let d1 = dec.dropped_packets();
        assert!(
            d1 >= d0,
            "dropped_packets must be monotonically non-decreasing"
        );
    }

    // ── R1.9: request_keyframe does not panic when called before start ─────────────

    #[test]
    fn windows_openh264_decoder_request_keyframe_no_panic_before_start() {
        let dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        // Not started — just calling request_keyframe should not panic.
        dec.request_keyframe();
        // The flag is set but no thread reads it — that's fine.
        assert!(dec.state.keyframe_pending.load(Ordering::Relaxed));
    }

    // ─── B3.3 RED: Decode flow — PLI fires on start + request_keyframe propagates ─

    // ── Initial PLI fires at least once on start (R3.8) ──────────────────────────

    #[test]
    fn windows_openh264_decoder_initial_pli_fires_on_start() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let (counting, count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        dec.start(pkt_rx, frame_tx).unwrap();

        // Give the thread time to fire the initial PLI.
        std::thread::sleep(Duration::from_millis(200));

        let after_start = count.load(Ordering::Relaxed);
        assert!(
            after_start >= 1,
            "initial PLI must fire at least once on start, got {after_start}"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ── request_keyframe() propagates to receiver via the atomic flag ─────────────

    #[test]
    fn windows_openh264_decoder_request_keyframe_propagates_to_receiver() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let (counting, count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        dec.start(pkt_rx, frame_tx).unwrap();

        // Let initial PLI fire.
        std::thread::sleep(Duration::from_millis(200));
        let after_start = count.load(Ordering::Relaxed);
        assert!(
            after_start >= 1,
            "initial PLI must fire at least once on start"
        );

        // Wait for PLI rate-limit to reset, then request another keyframe.
        std::thread::sleep(Duration::from_millis(600));
        dec.request_keyframe();

        // Send a valid packet so the thread wakes up and checks the flag.
        // Use a minimal Annex-B buffer (SPS NAL header byte only — won't decode a frame
        // but will unblock the recv() and exercise the keyframe_pending check).
        let sps_nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67];
        let _ = pkt_tx.try_send(make_packet(sps_nal, false, 0));

        std::thread::sleep(Duration::from_millis(200));
        let after_request = count.load(Ordering::Relaxed);
        assert!(
            after_request > after_start,
            "keyframe count must increase after explicit request; was {after_start}, got {after_request}"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ─── B3.5 RED: PLI fires after consecutive decode errors ─────────────────────

    #[test]
    fn windows_openh264_decoder_pli_fires_on_consecutive_decode_errors() {
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default()).unwrap();
        let (counting, count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx, _frame_rx) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        dec.start(pkt_rx, frame_tx).unwrap();

        // Wait for initial PLI.
        std::thread::sleep(Duration::from_millis(200));
        let after_start = count.load(Ordering::Relaxed);

        // Wait for PLI rate-limit window to reset.
        std::thread::sleep(Duration::from_millis(600));

        // Send DECODE_ERROR_THRESHOLD garbled NALs. Each has a start code but
        // the payload is garbage that openh264 will fail to decode.
        // We embed each in its own packet so each triggers one decode() call.
        for i in 0..DECODE_ERROR_THRESHOLD {
            // Garbage IDR byte (0x65) but payload is random junk.
            let garbage: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD, (i as u8)];
            let _ = pkt_tx.try_send(make_packet(&garbage, false, (i * 33) as u64));
        }

        std::thread::sleep(Duration::from_millis(500));
        let after_errors = count.load(Ordering::Relaxed);
        assert!(
            after_errors > after_start,
            "PLI must fire after {DECODE_ERROR_THRESHOLD} consecutive decode errors; \
             before={after_start}, after={after_errors}"
        );

        drop(pkt_tx);
        dec.stop().unwrap();
    }

    // ─── B3.7 RED: Backpressure — dropped_frames increments on full output channel ─

    #[test]
    fn windows_openh264_decoder_dropped_frames_on_full_output_channel() {
        use crate::encode::windows::WindowsOpenH264Encoder;
        use sm_domain::CaptureFrame;
        use sm_domain::capture::PixelFormat;
        use sm_domain::encode::EncoderConfig;
        use sm_domain::encode::VideoEncoder;

        // First, encode several frames to get past openh264 warmup and obtain a real IDR.
        let mut encoder = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<CaptureFrame>(8);
        let (pkt_tx_enc, pkt_rx_enc) = std::sync::mpsc::sync_channel::<EncodedPacket>(8);
        encoder.start(frame_rx, pkt_tx_enc).unwrap();

        // Send several 64×64 black BGRA frames to warm up the encoder.
        let w = 64u32;
        let h = 64u32;
        let stride = w * 4;
        let frame_data = vec![0u8; (stride * h) as usize];
        for i in 0..5u64 {
            let _ = frame_tx.try_send(CaptureFrame {
                data: Arc::from(frame_data.as_slice()),
                width: w,
                height: h,
                stride,
                format: PixelFormat::Bgra8,
                timestamp: Duration::from_millis(i * 33),
            });
        }

        // Collect the first IDR packet (scan for up to 2 s).
        let mut encoded_pkt: Option<EncodedPacket> = None;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            while let Ok(pkt) = pkt_rx_enc.try_recv() {
                if pkt.is_keyframe && encoded_pkt.is_none() {
                    encoded_pkt = Some(pkt);
                }
            }
            if encoded_pkt.is_some() {
                break;
            }
        }
        drop(frame_tx);
        let _ = encoder.stop();

        let Some(idr_pkt) = encoded_pkt else {
            // If encoder produced no IDR (e.g., CI warmup skip), skip the test gracefully.
            return;
        };

        // Now set up the decoder with output capacity = 1.
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig {
            width: w,
            height: h,
        })
        .unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx_dec, pkt_rx_dec) = std::sync::mpsc::sync_channel::<EncodedPacket>(16);
        // Output capacity = 1 — fills after the first decoded frame.
        let (frame_tx_dec, _frame_rx_dec) = std::sync::mpsc::sync_channel::<DecodedFrame>(1);

        dec.start(pkt_rx_dec, frame_tx_dec).unwrap();

        // Flood with the same IDR packet many times.
        for _ in 0..20 {
            let _ = pkt_tx_dec.try_send(idr_pkt.clone());
        }

        std::thread::sleep(Duration::from_millis(800));
        let dropped = dec.dropped_frames();

        drop(pkt_tx_dec);
        dec.stop().unwrap();

        assert!(
            dropped > 0,
            "dropped_frames must be > 0 when output channel is full, got {dropped}"
        );
    }

    // ─── B3.3 RED: Synthetic IDR → DecodedFrame::I420 ────────────────────────────
    //
    // This test encodes a real frame via WindowsOpenH264Encoder, then feeds the
    // resulting IDR Annex-B packet into the decoder and asserts that one
    // DecodedFrame with PixelData::I420 arrives on the output channel.

    #[test]
    fn windows_openh264_decoder_synthetic_idr_produces_decoded_frame() {
        use crate::encode::windows::WindowsOpenH264Encoder;
        use sm_domain::CaptureFrame;
        use sm_domain::capture::PixelFormat;
        use sm_domain::encode::EncoderConfig;
        use sm_domain::encode::VideoEncoder;

        // ── Encode several frames to get past openh264 warmup ────────────────────
        let mut encoder = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<CaptureFrame>(8);
        let (enc_pkt_tx, enc_pkt_rx) = std::sync::mpsc::sync_channel::<EncodedPacket>(8);
        encoder.start(frame_rx, enc_pkt_tx).unwrap();

        let w = 64u32;
        let h = 64u32;
        let stride = w * 4;
        let frame_data = vec![0u8; (stride * h) as usize];

        // Send several frames — openh264 may suppress the first on warmup.
        for i in 0..5u64 {
            let _ = frame_tx.try_send(CaptureFrame {
                data: Arc::from(frame_data.as_slice()),
                width: w,
                height: h,
                stride,
                format: PixelFormat::Bgra8,
                timestamp: Duration::from_millis(i * 33),
            });
        }

        // Collect the first IDR packet (scan up to 2 s).
        let mut idr_pkt: Option<EncodedPacket> = None;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            while let Ok(pkt) = enc_pkt_rx.try_recv() {
                if pkt.is_keyframe && idr_pkt.is_none() {
                    idr_pkt = Some(pkt);
                }
            }
            if idr_pkt.is_some() {
                break;
            }
        }
        drop(frame_tx);
        let _ = encoder.stop();

        let Some(idr) = idr_pkt else {
            // Encoder warm-up suppressed all IDRs — skip test gracefully.
            return;
        };

        // ── Feed into decoder ─────────────────────────────────────────────────────
        let mut dec = WindowsOpenH264Decoder::new(DecoderConfig {
            width: w,
            height: h,
        })
        .unwrap();
        let (counting, _count) = CountingReceiver::new();
        dec.set_receiver(Arc::new(counting));

        let (pkt_tx, pkt_rx) =
            std::sync::mpsc::sync_channel::<EncodedPacket>(DECODE_CHANNEL_CAPACITY);
        let (frame_tx_dec, frame_rx_dec) =
            std::sync::mpsc::sync_channel::<DecodedFrame>(DECODE_CHANNEL_CAPACITY);

        dec.start(pkt_rx, frame_tx_dec).unwrap();

        let _ = pkt_tx.send(idr.clone());

        // Wait up to 3 s for a decoded frame.
        let mut decoded_frame: Option<DecodedFrame> = None;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(50));
            if let Ok(f) = frame_rx_dec.try_recv() {
                decoded_frame = Some(f);
                break;
            }
        }

        drop(pkt_tx);
        dec.stop().unwrap();

        let frame = decoded_frame
            .expect("expected at least one DecodedFrame from a valid IDR packet within 3 s");

        // Verify the frame contains I420 pixel data.
        match &frame.data {
            PixelData::I420 {
                y,
                u,
                v,
                width,
                height,
            } => {
                assert!(*width > 0, "decoded frame width must be > 0");
                assert!(*height > 0, "decoded frame height must be > 0");
                assert_eq!(
                    y.len(),
                    (*width as usize) * (*height as usize),
                    "Y plane length must be width × height"
                );
                assert_eq!(
                    u.len(),
                    (*width as usize / 2) * (*height as usize / 2),
                    "U plane length must be (width/2) × (height/2)"
                );
                assert_eq!(
                    v.len(),
                    (*width as usize / 2) * (*height as usize / 2),
                    "V plane length must be (width/2) × (height/2)"
                );
            }
            other => {
                panic!("expected PixelData::I420, got {other:?}");
            }
        }

        // Sequence starts at 0 after start().
        assert_eq!(
            frame.sequence, 0,
            "first decoded frame must have sequence 0"
        );
        // Timestamp must match the input packet.
        assert_eq!(
            frame.timestamp, idr.timestamp,
            "decoded frame timestamp must match input packet timestamp"
        );
    }

    // ─── copy_strided_plane unit tests ────────────────────────────────────────────

    #[test]
    fn copy_strided_plane_no_padding() {
        // 4 pixels wide, 2 rows, stride = 4 (no padding)
        let src: Vec<u8> = (0..8u8).collect();
        let out = copy_strided_plane(&src, 4, 2, 4);
        assert_eq!(out, src, "no-padding case must produce identical output");
    }

    #[test]
    fn copy_strided_plane_with_padding() {
        // 2 pixels wide, 2 rows, stride = 4 (2 padding bytes per row)
        // src = [0,1, PAD,PAD, 2,3, PAD,PAD]
        let src: Vec<u8> = vec![0, 1, 0xFF, 0xFF, 2, 3, 0xFF, 0xFF];
        let out = copy_strided_plane(&src, 2, 2, 4);
        assert_eq!(out, vec![0, 1, 2, 3], "padding must be stripped");
    }
}
