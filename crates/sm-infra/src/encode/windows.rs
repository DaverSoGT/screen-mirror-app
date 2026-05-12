#![cfg(target_os = "windows")]
//! Windows H.264 software encoder backed by OpenH264 (Cisco BSD-2).
//!
//! # Overview
//!
//! [`WindowsOpenH264Encoder`] wraps `openh264::encoder::Encoder` behind the
//! [`VideoEncoder`] domain trait. It owns one OS thread (spawned in `start`)
//! that reads [`CaptureFrame`]s from the injected channel, converts them to
//! I420 via `bgra_to_i420::convert`, and encodes them with OpenH264.
//!
//! # OpenH264 knobs
//!
//! - **Rate control**: `RC_BITRATE_MODE` (CBR). Controlled by `config.bitrate_bps`.
//! - **Frame skip**: disabled — drop-newest backpressure handles congestion.
//! - **Usage type**: `ScreenContentRealTime` — optimises for sharp-edge desktop content.
//! - **Intra period**: `config.intra_period` frames between automatic IDR keyframes.
//! - **Max slice**: 1 slice/frame — RTP fragmentation is the transport's job.
//!
//! # Color space
//!
//! Input is BGRA8 (WGC output). `bgra_to_i420::convert` converts to I420 using
//! **BT.601 limited-range** coefficients, which matches OpenH264's default decode
//! color space. The conversion is stride-aware: WGC may pad rows to a GPU-aligned
//! stride > `width * 4`; using `width * 4` as the row pitch causes "diagonal tearing".
//!
//! # Thread model
//!
//! - `new()`: validates config, allocates shared atomics. No thread, no OpenH264 init.
//! - `start(rx, tx)`: spawns one OS thread that owns `openh264::Encoder`. OpenH264 is
//!   lazily initialised inside the thread on the first call to `encode()` — the encoder
//!   auto-reinitialises when frame dimensions change.
//! - `stop()`: idempotent. Sets the `stop` flag and joins the handle. NOTE: callers MUST
//!   drop their `frame_tx` (the `SyncSender` paired with `rx`) BEFORE calling `stop()` —
//!   otherwise the encoder thread can remain blocked on `rx.recv()` and `stop().join()`
//!   will deadlock. The `stop` flag is only consulted between recv calls.
//! - `Drop`: calls `stop()` if handle is still `Some` — no leaked thread on panic or
//!   forgotten call.
//!
//! # Warmup caveat
//!
//! OpenH264 may emit zero layers on the very first `encode()` call (codec warm-up).
//! The encoder thread skips packets with empty Annex-B output silently — the sequence
//! counter and timestamp are NOT consumed for empty frames.
//!
//! # Channel capacity
//!
//! [`ENCODE_CHANNEL_CAPACITY`] = 4 (mirrors `CAPTURE_CHANNEL_CAPACITY`). Drop-newest
//! policy: when the output channel is full, the encoded packet is silently dropped and
//! `dropped_frames()` is incremented.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;

use sm_domain::CaptureFrame;
use sm_domain::encode::{EncodedPacket, EncoderConfig, EncoderError, VideoEncoder};

use crate::encode::bgra_to_i420::{I420, convert};

/// Bounded output channel capacity. Mirrors `CAPTURE_CHANNEL_CAPACITY` from the capture adapter.
/// Four packets ≈ 67 ms of buffering at 30 fps, well under the 150 ms glass-to-glass budget.
pub const ENCODE_CHANNEL_CAPACITY: usize = 4;

/// Cross-thread state shared between the caller and the encoder OS thread.
struct EncoderShared {
    /// Set by `request_keyframe()`; cleared (swap → false) by the encoder thread before encode.
    keyframe_pending: AtomicBool,
    /// Non-zero means a new target bitrate is pending. 0 = no change.
    /// `set_bitrate(0)` is rejected at the public API so 0 is unambiguous as "no change".
    pending_bitrate: AtomicU32,
    /// Monotonically increasing count of encoded packets dropped due to output-channel backpressure.
    dropped: AtomicU64,
    /// Set by `stop()` / `Drop`. Checked at the top of the encoder loop.
    stop: AtomicBool,
}

impl EncoderShared {
    fn new() -> Self {
        Self {
            keyframe_pending: AtomicBool::new(false),
            pending_bitrate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }
}

/// Windows H.264 software encoder. Implements [`VideoEncoder`] via OpenH264.
///
/// Construction (`new`) is lightweight — no OS thread is created and no OpenH264
/// context is allocated until `start` is called. The encoder is `Send` so it
/// can be moved to any OS thread (e.g., a Tauri command handler) before starting.
pub struct WindowsOpenH264Encoder {
    config: EncoderConfig,
    state: Arc<EncoderShared>,
    /// `Some` while the encoder thread is running; `None` before `start` and after `stop`.
    handle: Option<JoinHandle<()>>,
    /// Reserved placeholder. `start()` moves `rx` directly into the thread closure;
    /// the `Receiver` is never owned by the struct. To unblock the thread on shutdown,
    /// callers MUST drop their `frame_tx` (the paired `SyncSender`) before calling
    /// `stop()` — the stop flag is only consulted between recv calls.
    #[allow(dead_code)]
    _rx_placeholder: (),
}

impl std::fmt::Debug for WindowsOpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsOpenH264Encoder")
            .field("config", &self.config)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

// SAFETY: All shared state crosses the thread boundary via `Arc<EncoderShared>` (all fields
// are atomics). `JoinHandle<()>` is Send. `EncoderConfig` is Clone + Send. No raw pointers.
// Verified by the static assert in tests below.
unsafe impl Send for WindowsOpenH264Encoder {}

impl VideoEncoder for WindowsOpenH264Encoder {
    /// Construct and validate an encoder configuration.
    ///
    /// Does NOT spawn the encoding thread or allocate the OpenH264 context.
    /// Returns `Err(InvalidConfig(_))` if `bitrate_bps == 0` or `framerate == 0`.
    fn new(config: EncoderConfig) -> Result<Self, EncoderError>
    where
        Self: Sized,
    {
        if config.bitrate_bps == 0 {
            return Err(EncoderError::InvalidConfig(
                "bitrate_bps must be > 0".into(),
            ));
        }
        if config.framerate == 0 {
            return Err(EncoderError::InvalidConfig("framerate must be > 0".into()));
        }
        Ok(Self {
            config,
            state: Arc::new(EncoderShared::new()),
            handle: None,
            _rx_placeholder: (),
        })
    }

    fn start(
        &mut self,
        rx: Receiver<CaptureFrame>,
        tx: SyncSender<EncodedPacket>,
    ) -> Result<(), EncoderError> {
        use openh264::encoder::{
            BitRate, EncoderConfig as OhCfg, FrameRate, IntraFramePeriod, RateControlMode,
            UsageType,
        };
        use openh264::formats::YUVSlices;
        use openh264::{OpenH264API, encoder::Encoder};

        let config = self.config.clone();
        let state = Arc::clone(&self.state);

        // Reset stop flag in case this encoder is restarted.
        state.stop.store(false, Ordering::Release);

        let handle = std::thread::spawn(move || {
            // ── Build OpenH264 config ──────────────────────────────────────────
            let oh_cfg = OhCfg::new()
                .bitrate(BitRate::from_bps(config.bitrate_bps))
                .max_frame_rate(FrameRate::from_hz(config.framerate as f32))
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .skip_frames(false)
                .intra_frame_period(IntraFramePeriod::from_num_frames(config.intra_period));

            let api = OpenH264API::from_source();
            let mut encoder = match Encoder::with_api_config(api, oh_cfg) {
                Ok(e) => e,
                Err(e) => {
                    // Construction failure — thread exits, tx is dropped, consumer sees RecvError.
                    let _ = e;
                    return;
                }
            };

            let mut scratch = I420::new(1, 1); // resized on first frame
            let mut seq: u64 = 0;

            loop {
                // ── Stop check ────────────────────────────────────────────────
                if state.stop.load(Ordering::Acquire) {
                    break;
                }

                // ── Receive frame ─────────────────────────────────────────────
                let frame = match rx.recv() {
                    Ok(f) => f,
                    Err(_) => break, // upstream sender dropped — normal shutdown
                };

                // ── Apply pending keyframe request ────────────────────────────
                if state.keyframe_pending.swap(false, Ordering::AcqRel) {
                    encoder.force_intra_frame();
                }

                // ── Apply pending bitrate change ──────────────────────────────
                let new_bps = state.pending_bitrate.swap(0, Ordering::AcqRel);
                if new_bps != 0 {
                    use openh264_sys2::{ENCODER_OPTION_BITRATE, SBitrateInfo, SPATIAL_LAYER_ALL};
                    let mut info = SBitrateInfo {
                        iLayer: SPATIAL_LAYER_ALL,
                        iBitrate: new_bps as std::os::raw::c_int,
                    };
                    // SAFETY: set_option is a raw FFI call; pOption points to a local SBitrateInfo
                    // that lives for the duration of the call. The encoder thread is the sole writer.
                    unsafe {
                        let raw = encoder.raw_api();
                        let _ = raw.set_option(
                            ENCODER_OPTION_BITRATE,
                            std::ptr::from_mut(&mut info).cast(),
                        );
                    }
                }

                // ── BGRA→I420 conversion (stride-aware) ───────────────────────
                convert(&frame, &mut scratch);

                // ── Encode ────────────────────────────────────────────────────
                // OpenH264 requires even dimensions. Round down.
                let w = (scratch.width as usize) & !1;
                let h = (scratch.height as usize) & !1;
                if w == 0 || h == 0 {
                    continue; // skip degenerate frames
                }

                let chroma_w = w / 2;
                let chroma_h = h / 2;
                let y_slice = &scratch.buf[scratch.y_offset()..scratch.y_offset() + w * h];
                let u_slice =
                    &scratch.buf[scratch.u_offset()..scratch.u_offset() + chroma_w * chroma_h];
                let v_slice =
                    &scratch.buf[scratch.v_offset()..scratch.v_offset() + chroma_w * chroma_h];

                let yuv =
                    YUVSlices::new((y_slice, u_slice, v_slice), (w, h), (w, chroma_w, chroma_w));

                let bs = match encoder.encode(&yuv) {
                    Ok(b) => b,
                    Err(_e) => {
                        // Encode error — skip frame but keep running.
                        continue;
                    }
                };

                // ── Assemble Annex-B ──────────────────────────────────────────
                // openh264 already prepends Annex-B start codes to each NAL unit.
                // `write_vec` just concatenates all NAL slices verbatim.
                let mut annex_b = Vec::new();
                bs.write_vec(&mut annex_b);

                if annex_b.is_empty() {
                    // OpenH264 can emit zero NALs on the very first frame (warm-up).
                    // Skip: do not consume a sequence number for an empty packet.
                    continue;
                }

                // ── Build EncodedPacket ───────────────────────────────────────
                use openh264::encoder::FrameType;
                let is_keyframe = bs.frame_type() == FrameType::IDR;

                let pkt = EncodedPacket {
                    data: Arc::from(annex_b.into_boxed_slice()),
                    is_keyframe,
                    timestamp: frame.timestamp,
                    sequence: seq,
                };
                seq += 1;

                // ── Try-send with drop-newest backpressure ────────────────────
                match tx.try_send(pkt) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        break; // consumer dropped rx — normal shutdown
                    }
                }
            }
        });

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), EncoderError> {
        // Signal the thread to stop.
        self.state.stop.store(true, Ordering::Release);
        // Join the thread. `Option::take` makes this idempotent.
        if let Some(h) = self.handle.take() {
            // Ignore panics in the encoder thread — they are reported via dropped tx.
            let _ = h.join();
        }
        Ok(())
    }

    fn request_keyframe(&self) {
        self.state.keyframe_pending.store(true, Ordering::Release);
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
        "sw_openh264"
    }
}

impl Drop for WindowsOpenH264Encoder {
    fn drop(&mut self) {
        // Ensure the encoder thread is always joined, even if `stop()` was never called.
        // This prevents a dangling thread after a panic or forgotten stop.
        let _ = self.stop();
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use sm_domain::capture::PixelFormat;
    use sm_domain::encode::{EncoderConfig, EncoderError, VideoEncoder};
    use std::time::Duration;

    // ─── Static assertion: WindowsOpenH264Encoder is Send ─────────────────────

    #[allow(dead_code)]
    fn _assert_send() {
        fn check<T: Send>() {}
        check::<WindowsOpenH264Encoder>();
    }

    // ─── Static assertion: WindowsOpenH264Encoder is Send + Sync ──────────────
    //
    // Guards R-DELTA-encoder-h264-windows-1 (VideoEncoder: Send + Sync).
    // WindowsOpenH264Encoder is already Sync: all shared state is via
    // Arc<EncoderShared> with AtomicBool/AtomicU32/AtomicU64 fields.
    // The openh264::Encoder lives exclusively inside the spawned thread stack
    // — it never escapes — so its non-Sync-ness is irrelevant. [R15.2, S15.2]

    #[allow(dead_code)]
    fn _assert_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<WindowsOpenH264Encoder>();
    }

    // ─── Helper: build a minimal CaptureFrame ─────────────────────────────────

    /// Build a synthetic BGRA8 `CaptureFrame` at the given resolution.
    /// All pixels are black (0,0,0,255). Width and height are rounded to even
    /// values so the encoder can produce I420 without dimension issues.
    fn make_frame(width: u32, height: u32, ts_ms: u64) -> sm_domain::CaptureFrame {
        let stride = width * 4;
        let data = vec![0u8; (stride * height) as usize];
        sm_domain::CaptureFrame {
            data: Arc::from(data.as_slice()),
            width,
            height,
            stride,
            format: PixelFormat::Bgra8,
            timestamp: Duration::from_millis(ts_ms),
        }
    }

    // ─── A1: new with default config returns Ok ────────────────────────────────

    #[test]
    fn windows_openh264_new_default_config_ok() {
        let result = WindowsOpenH264Encoder::new(EncoderConfig::default());
        assert!(
            result.is_ok(),
            "expected Ok from new(default config), got: {result:?}"
        );
    }

    // ─── A2: new with bitrate=0 returns InvalidConfig ─────────────────────────

    #[test]
    fn windows_openh264_new_invalid_bitrate_rejected() {
        let cfg = EncoderConfig {
            bitrate_bps: 0,
            ..EncoderConfig::default()
        };
        let err = WindowsOpenH264Encoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}"
        );
    }

    // ─── A3: new with framerate=0 returns InvalidConfig ───────────────────────

    #[test]
    fn windows_openh264_new_invalid_framerate_rejected() {
        let cfg = EncoderConfig {
            framerate: 0,
            ..EncoderConfig::default()
        };
        let err = WindowsOpenH264Encoder::new(cfg).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig, got {err:?}"
        );
    }

    // ─── A4: Debug does not panic ──────────────────────────────────────────────

    #[test]
    fn windows_openh264_new_debug_does_not_panic() {
        let enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let s = format!("{enc:?}");
        assert!(!s.is_empty(), "Debug output should be non-empty");
    }

    // ─── A5: start then stop — no panic, Ok returned ──────────────────────────
    //
    // Verifies the encoder thread starts and can be joined cleanly.
    // We drop frame_tx before calling stop() so the thread's rx.recv() unblocks.

    #[test]
    fn windows_openh264_start_then_stop_ok() {
        let mut enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) =
            std::sync::mpsc::sync_channel::<sm_domain::encode::EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();

        // Drop the input sender so the encoder thread's rx.recv() returns Err and exits.
        drop(frame_tx);

        let result = enc.stop();
        assert!(result.is_ok(), "stop() should return Ok, got: {result:?}");
    }

    // ─── A6: stop is idempotent ────────────────────────────────────────────────
    //
    // Scenario S2.2: second stop() on an already-stopped encoder returns Ok without panic.

    #[test]
    fn windows_openh264_stop_is_idempotent() {
        let mut enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();

        // Stop on a never-started encoder is idempotent.
        enc.stop().unwrap();
        enc.stop().unwrap();

        // Start + stop + stop again.
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) =
            std::sync::mpsc::sync_channel::<sm_domain::encode::EncodedPacket>(4);
        enc.start(frame_rx, pkt_tx).unwrap();
        drop(frame_tx);
        enc.stop().unwrap();
        enc.stop().unwrap(); // second stop must not panic
    }

    // ─── A7: drop without stop — no thread leak ────────────────────────────────
    //
    // Construct + start + drop without calling stop().
    // The Drop impl calls stop() internally, so the thread must be joined.
    // If the thread is leaked this test hangs or valgrind would show an error.

    #[test]
    fn windows_openh264_drop_without_stop_joins_thread() {
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(4);
        let (pkt_tx, _pkt_rx) =
            std::sync::mpsc::sync_channel::<sm_domain::encode::EncodedPacket>(4);

        {
            let mut enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
            enc.start(frame_rx, pkt_tx).unwrap();

            // Drop frame_tx first so the encoder thread's recv() unblocks when enc drops.
            drop(frame_tx);
            // enc drops here — Drop calls stop() which sets stop=true and joins the handle.
        }
        // If we reach here without hanging, the thread was successfully joined.
    }

    // ─── A8: dropped_frames counter increments under backpressure ─────────────
    //
    // Scenario S8.1: output channel capacity = 1; consumer never reads;
    // after encoding several frames, dropped_frames() > 0.
    //
    // Note: this test sends real BGRA frames through the full encoder stack
    // (BGRA→I420 + OpenH264). It is a unit test (no #[ignore]) because OpenH264
    // is available on Windows and the encode is fast for small frames.

    #[test]
    fn windows_openh264_backpressure_increments_dropped_frames() {
        let mut enc = WindowsOpenH264Encoder::new(EncoderConfig {
            framerate: 60,
            ..EncoderConfig::default()
        })
        .unwrap();

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(32);
        // Capacity = 1 so the channel fills after the first packet.
        let (pkt_tx, _pkt_rx) =
            std::sync::mpsc::sync_channel::<sm_domain::encode::EncodedPacket>(1);

        enc.start(frame_rx, pkt_tx).unwrap();

        // Flood the encoder with 30 frames. At 60×60 pixels, each encode is fast.
        // With output capacity=1, most packets will be dropped.
        for i in 0..30u64 {
            // Use a 60×60 frame — even dimensions, small, fast to encode.
            let frame = make_frame(60, 60, i * 16);
            let _ = frame_tx.send(frame);
        }

        // Give the encoder thread time to process all frames.
        std::thread::sleep(Duration::from_millis(500));

        let dropped = enc.dropped_frames();

        // Drop frame_tx before stop so the thread's recv() unblocks.
        drop(frame_tx);
        enc.stop().unwrap();

        assert!(
            dropped > 0,
            "expected dropped_frames > 0 with capacity-1 channel, got {dropped}"
        );
    }

    // ─── A9: dropped_frames monotonically non-decreasing ─────────────────────
    //
    // Scenario S8.4: read dropped_frames at time T, read again at T+1, assert T+1 >= T.

    #[test]
    fn windows_openh264_dropped_frames_is_monotonic() {
        let enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let d0 = enc.dropped_frames();
        let d1 = enc.dropped_frames();
        assert!(
            d1 >= d0,
            "dropped_frames must be monotonically non-decreasing"
        );
    }

    // ─── A10: ENCODE_CHANNEL_CAPACITY is in range [4, 8] ─────────────────────

    #[test]
    fn windows_openh264_channel_capacity_in_valid_range() {
        const { assert!(ENCODE_CHANNEL_CAPACITY >= 4 && ENCODE_CHANNEL_CAPACITY <= 8) }
    }

    // ─── A11: request_keyframe — does not panic when called before start ───────
    //
    // Scenario S6.3: calling request_keyframe() on a stopped encoder must not panic.

    #[test]
    fn windows_openh264_request_keyframe_before_start_no_panic() {
        let enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        // Not started — just calling request_keyframe should not panic.
        enc.request_keyframe();
        // The flag is set but no thread reads it — that's fine.
        assert!(enc.state.keyframe_pending.load(Ordering::Relaxed));
    }

    // ─── A12: set_bitrate — valid bps is stored, zero is rejected ─────────────
    //
    // Scenario S7.2/S7.3: set_bitrate records new bps; set_bitrate(0) returns InvalidConfig.

    #[test]
    fn windows_openh264_set_bitrate_valid_stores_value() {
        let enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        enc.set_bitrate(8_000_000).unwrap();
        let stored = enc.state.pending_bitrate.load(Ordering::Relaxed);
        assert_eq!(stored, 8_000_000);
    }

    #[test]
    fn windows_openh264_set_bitrate_zero_rejected() {
        let enc = WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap();
        let err = enc.set_bitrate(0).unwrap_err();
        assert!(
            matches!(err, EncoderError::InvalidConfig(_)),
            "expected InvalidConfig for bitrate=0, got {err:?}"
        );
    }

    // ─── A13: set_bitrate and request_keyframe are callable from any thread ───

    #[test]
    fn windows_openh264_runtime_controls_callable_from_another_thread() {
        use std::sync::Arc as StdArc;

        let enc = StdArc::new(WindowsOpenH264Encoder::new(EncoderConfig::default()).unwrap());
        let enc_clone = StdArc::clone(&enc);

        let handle = std::thread::spawn(move || {
            enc_clone.request_keyframe();
            enc_clone.set_bitrate(2_000_000).unwrap();
            enc_clone.dropped_frames() // returns 0, confirms thread-safe read
        });

        let dropped = handle.join().expect("thread should not panic");
        assert_eq!(dropped, 0);

        // Verify the keyframe flag was set by the other thread.
        assert!(enc.state.keyframe_pending.load(Ordering::Relaxed));
    }
}
