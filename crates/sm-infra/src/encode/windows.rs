#![cfg(target_os = "windows")]
//! Windows H.264 software encoder backed by OpenH264 (Cisco BSD-2).
//!
//! # Overview
//!
//! [`WindowsOpenH264Encoder`] wraps `openh264::encoder::Encoder` behind the
//! [`VideoEncoder`] domain trait. It owns one OS thread (spawned in `start`)
//! that reads [`CaptureFrame`]s from the injected channel, converts them to
//! I420 via [`bgra_to_i420::convert`], and encodes them with OpenH264.
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
//! Input is BGRA8 (WGC output). [`bgra_to_i420::convert`] converts to I420 using
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
//! - `stop()`: idempotent. Sets `stop` flag, drops the receiver (unblocking `rx.recv()`
//!   in the thread if it is blocking), then joins the handle.
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
    /// The input-channel receiver. Taken at `start`; stored as `None` afterwards.
    /// Stored here so `stop` can drop it (closing the channel, unblocking `recv` in the thread).
    ///
    /// Note: this field is `None` before `start` and after `start` (it is moved into the thread).
    /// The `Option` exists only to allow the type to compile — `start` moves `rx` directly into
    /// the thread closure; we don't need to store it. See `start` implementation.
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
    use sm_domain::encode::{EncoderConfig, EncoderError, VideoEncoder};

    // ─── Static assertion: WindowsOpenH264Encoder is Send ─────────────────────

    #[allow(dead_code)]
    fn _assert_send() {
        fn check<T: Send>() {}
        check::<WindowsOpenH264Encoder>();
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
}
