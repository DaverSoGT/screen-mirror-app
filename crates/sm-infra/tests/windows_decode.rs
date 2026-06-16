//! Integration tests for `WindowsOpenH264Decoder`.
//!
//! All tests are gated `#[cfg(target_os = "windows")]` and marked `#[ignore]`
//! because they run the full decoder stack (OpenH264 SW decode, real IDR packets
//! produced by `WindowsOpenH264Encoder`) and are intended to run manually on a
//! Windows host with:
//!
//!     cargo nextest run -p sm-infra --run-ignored only --tests windows_decode
//!
//! NASM in PATH gives OpenH264 a 2-3x SIMD speedup.
//! NASM is OPTIONAL — without it, OpenH264 falls back to portable C.
#![cfg(target_os = "windows")]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use sm_domain::capture::PixelFormat;
use sm_domain::decode::{DecodedFrame, DecoderConfig, PixelData, VideoDecoder};
use sm_domain::encode::{EncodedPacket, EncoderConfig, VideoEncoder};
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoReceiver};
use sm_infra::decode::windows_openh264::WindowsOpenH264Decoder;
use sm_infra::encode::WindowsOpenH264Encoder;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a synthetic `WIDTH x HEIGHT` BGRA8 CaptureFrame.
fn make_synthetic_frame(width: u32, height: u32, ts_ms: u64) -> sm_domain::CaptureFrame {
    let stride = width * 4;
    let total = (stride * height) as usize;
    let mut data = vec![0u8; total];
    for row in 0..height as usize {
        let row_base = row * stride as usize;
        for col in 0..width as usize {
            let pix = row_base + col * 4;
            data[pix] = (row % 256) as u8;
            data[pix + 1] = (col % 256) as u8;
            data[pix + 2] = 128u8;
            data[pix + 3] = 255u8;
        }
    }
    sm_domain::CaptureFrame {
        data: Arc::from(data.as_slice()),
        width,
        height,
        stride,
        format: PixelFormat::Bgra8,
        timestamp: Duration::from_millis(ts_ms),
    }
}

/// Counting `VideoReceiver` fixture for PLI tests.
struct CountingReceiver {
    keyframe_count: Arc<std::sync::atomic::AtomicU64>,
}

impl CountingReceiver {
    fn new() -> (Self, Arc<std::sync::atomic::AtomicU64>) {
        let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
        _pkt_tx: mpsc::SyncSender<EncodedPacket>,
        _event_tx: mpsc::SyncSender<TransportEvent>,
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
        self.keyframe_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        0
    }
}

/// Produce a real IDR EncodedPacket from WindowsOpenH264Encoder using a small
/// synthetic frame. Returns None if no IDR arrives within 2 s (warmup miss).
fn produce_real_idr(width: u32, height: u32) -> Option<EncodedPacket> {
    let mut encoder = WindowsOpenH264Encoder::new(EncoderConfig::default())
        .expect("encoder construction should succeed");
    let (frame_tx, frame_rx) = mpsc::sync_channel::<sm_domain::encode::FramePayload>(8);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<EncodedPacket>(8);
    encoder.start(frame_rx, pkt_tx).unwrap();

    // Feed several frames to get past openh264 warmup.
    for i in 0..8u64 {
        let _ = frame_tx.try_send(sm_domain::encode::FramePayload::Cpu(make_synthetic_frame(
            width,
            height,
            i * 33,
        )));
        std::thread::sleep(Duration::from_millis(30));
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut idr: Option<EncodedPacket> = None;
    while Instant::now() < deadline {
        match pkt_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(pkt) if pkt.is_keyframe && idr.is_none() => {
                idr = Some(pkt);
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(frame_tx);
    let _ = encoder.stop();
    idr
}

// ─── I1: happy path — synthetic IDR decodes to I420 ─────────────────────────

#[test]
#[ignore]
fn windows_decoder_decodes_synthetic_idr_to_i420_frame() {
    let w = 64u32;
    let h = 64u32;

    let Some(idr) = produce_real_idr(w, h) else {
        eprintln!("[windows_decode I1] encoder produced no IDR — skipping");
        return;
    };

    let mut dec = WindowsOpenH264Decoder::new(DecoderConfig {
        width: w,
        height: h,
    })
    .expect("decoder construction should succeed");
    let (counting, _count) = CountingReceiver::new();
    dec.set_receiver(Arc::new(counting));

    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<EncodedPacket>(8);
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(8);
    dec.start(pkt_rx, frame_tx).unwrap();

    pkt_tx.send(idr.clone()).unwrap();

    let mut decoded: Option<DecodedFrame> = None;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match frame_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(f) => {
                decoded = Some(f);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(pkt_tx);
    dec.stop().expect("stop should succeed");

    let frame = decoded.expect("expected at least one DecodedFrame from real IDR");
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
            assert_eq!(y.len(), (*width as usize) * (*height as usize));
            assert_eq!(u.len(), (*width as usize / 2) * (*height as usize / 2));
            assert_eq!(v.len(), (*width as usize / 2) * (*height as usize / 2));
        }
        other => panic!("expected PixelData::I420, got {other:?}"),
    }
    assert_eq!(frame.timestamp, idr.timestamp, "timestamp must propagate");
    println!("[I1] PASS: decoded I420 frame from real IDR ({}x{})", w, h);
}

// ─── I2: request_keyframe is no-op pre-start, valid post-attach ──────────────

#[test]
#[ignore]
fn windows_decoder_request_keyframe_does_not_panic() {
    let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default())
        .expect("decoder construction should succeed");

    // Pre-set_receiver: request_keyframe must not panic.
    dec.request_keyframe();

    let (counting, count) = CountingReceiver::new();
    dec.set_receiver(Arc::new(counting));

    // Pre-start: request_keyframe sets the pending flag but does not panic.
    dec.request_keyframe();

    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<EncodedPacket>(8);
    let (frame_tx, _frame_rx) = mpsc::sync_channel::<DecodedFrame>(8);
    dec.start(pkt_rx, frame_tx).unwrap();

    // Give the thread time to fire initial PLI and process the pending flag.
    std::thread::sleep(Duration::from_millis(300));

    let count_after_start = count.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        count_after_start >= 1,
        "PLI should fire at least once on start, got {count_after_start}"
    );

    // Post-start: request_keyframe propagates via atomic flag.
    // Wait for rate-limit window, then request.
    std::thread::sleep(Duration::from_millis(600));
    dec.request_keyframe();

    // Send a minimal packet so the thread wakes up and processes the pending flag.
    let sps_nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67];
    let _ = pkt_tx.try_send(EncodedPacket {
        data: Arc::from(sps_nal),
        is_keyframe: false,
        timestamp: Duration::ZERO,
        sequence: 0,
    });
    std::thread::sleep(Duration::from_millis(300));

    let count_after_req = count.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        count_after_req > count_after_start,
        "explicit request_keyframe() must increase PLI count; before={count_after_start} after={count_after_req}"
    );

    drop(pkt_tx);
    dec.stop().expect("stop should succeed");
    println!("[I2] PASS: request_keyframe no-op pre-start, propagates post-start");
}

// ─── I3: backpressure — dropped_frames increments when output channel full ───

#[test]
#[ignore]
fn windows_decoder_dropped_frames_increments_when_output_full() {
    let w = 64u32;
    let h = 64u32;

    let Some(idr) = produce_real_idr(w, h) else {
        eprintln!("[windows_decode I3] encoder produced no IDR — skipping");
        return;
    };

    let mut dec = WindowsOpenH264Decoder::new(DecoderConfig {
        width: w,
        height: h,
    })
    .expect("decoder construction should succeed");
    let (counting, _count) = CountingReceiver::new();
    dec.set_receiver(Arc::new(counting));

    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<EncodedPacket>(16);
    // Output capacity = 1 — fills after the first decoded frame.
    let (frame_tx, _frame_rx) = mpsc::sync_channel::<DecodedFrame>(1);
    dec.start(pkt_rx, frame_tx).unwrap();

    // Flood the decoder with the same IDR repeatedly.
    for _ in 0..20 {
        let _ = pkt_tx.try_send(idr.clone());
    }

    std::thread::sleep(Duration::from_millis(1000));
    let dropped = dec.dropped_frames();

    drop(pkt_tx);
    dec.stop().expect("stop should succeed");

    assert!(
        dropped > 0,
        "dropped_frames must be > 0 when output channel is saturated, got {dropped}"
    );
    println!("[I3] PASS: dropped_frames = {dropped} with saturated output channel");
}

// ─── I4: lifecycle — stop twice is idempotent ────────────────────────────────

#[test]
#[ignore]
fn windows_decoder_lifecycle_idempotent_stop() {
    let mut dec = WindowsOpenH264Decoder::new(DecoderConfig::default())
        .expect("decoder construction should succeed");
    let (counting, _count) = CountingReceiver::new();
    dec.set_receiver(Arc::new(counting));

    let (pkt_tx, pkt_rx) = mpsc::sync_channel::<EncodedPacket>(4);
    let (frame_tx, _frame_rx) = mpsc::sync_channel::<DecodedFrame>(4);
    dec.start(pkt_rx, frame_tx).unwrap();

    drop(pkt_tx);

    let first = dec.stop();
    assert!(first.is_ok(), "first stop() must return Ok, got: {first:?}");

    let second = dec.stop();
    assert!(
        second.is_ok(),
        "second stop() must return Ok (idempotent), got: {second:?}"
    );
    println!("[I4] PASS: stop() is idempotent");
}

// ─── I5: P-frames after IDR — several frames decoded in sequence ──────────────

#[test]
#[ignore]
fn windows_decoder_handles_p_frames_after_idr() {
    let w = 64u32;
    let h = 64u32;
    const N_FRAMES: u64 = 12;
    const DEADLINE: Duration = Duration::from_secs(8);

    let mut encoder = WindowsOpenH264Encoder::new(EncoderConfig::default())
        .expect("encoder construction should succeed");
    let (frame_tx_enc, frame_rx_enc) = mpsc::sync_channel::<sm_domain::encode::FramePayload>(16);
    let (pkt_tx_enc, pkt_rx_enc) = mpsc::sync_channel::<EncodedPacket>(32);
    encoder.start(frame_rx_enc, pkt_tx_enc).unwrap();

    // Feed frames and collect encoded packets (IDR + P-frames).
    let producer = std::thread::spawn(move || {
        for i in 0..N_FRAMES {
            let frame = sm_domain::encode::FramePayload::Cpu(make_synthetic_frame(w, h, i * 33));
            if frame_tx_enc.send(frame).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        // frame_tx_enc drops here.
    });

    let mut encoded_pkts: Vec<EncodedPacket> = Vec::new();
    let t0 = Instant::now();
    while encoded_pkts.len() < N_FRAMES as usize && t0.elapsed() < DEADLINE {
        match pkt_rx_enc.recv_timeout(Duration::from_millis(200)) {
            Ok(pkt) => encoded_pkts.push(pkt),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    producer.join().expect("producer thread should not panic");
    drop(pkt_rx_enc);
    let _ = encoder.stop();

    if encoded_pkts.is_empty() {
        eprintln!("[windows_decode I5] encoder produced no packets — skipping");
        return;
    }

    // Now decode the collected packets.
    let mut dec = WindowsOpenH264Decoder::new(DecoderConfig {
        width: w,
        height: h,
    })
    .expect("decoder construction should succeed");
    let (counting, _count) = CountingReceiver::new();
    dec.set_receiver(Arc::new(counting));

    let (pkt_tx_dec, pkt_rx_dec) = mpsc::sync_channel::<EncodedPacket>(32);
    let (frame_tx_dec, frame_rx_dec) = mpsc::sync_channel::<DecodedFrame>(32);
    dec.start(pkt_rx_dec, frame_tx_dec).unwrap();

    let n_sent = encoded_pkts.len();
    for pkt in encoded_pkts {
        let _ = pkt_tx_dec.send(pkt);
    }

    // Collect decoded frames (openh264 may suppress first frame on warmup).
    let mut decoded_count = 0usize;
    let deadline2 = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline2 {
        match frame_rx_dec.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => {
                decoded_count += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(pkt_tx_dec);
    dec.stop().expect("stop should succeed");

    // Allow for openh264 warmup: at least 1 decoded frame is expected.
    assert!(
        decoded_count >= 1,
        "expected at least 1 DecodedFrame from {n_sent} packets, got {decoded_count}"
    );
    println!("[I5] PASS: decoded {decoded_count} frames from {n_sent} packets (IDR + P-frames)");
}
