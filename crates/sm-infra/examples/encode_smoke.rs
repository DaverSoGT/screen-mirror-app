//! End-to-end composability smoke: `WindowsCaptureSource` → `WindowsOpenH264Encoder` → `.h264` file.
//!
//! Captures the primary monitor for ~10 seconds, encodes the BGRA frames into an
//! Annex-B H.264 byte stream via OpenH264 (Cisco BSD-2), and writes every
//! `EncodedPacket::data` slice verbatim to disk. Halfway through the run the
//! example calls `request_keyframe()` to exercise the runtime-control seam.
//!
//! # Why this example exists
//!
//! Both `WindowsCaptureSource` and `WindowsOpenH264Encoder` are unit- and
//! integration-tested in isolation. This example is the live cross-stage smoke:
//! it proves the channel handoff (capture's `SyncSender<CaptureFrame>` ↔
//! encoder's `Receiver<CaptureFrame>`) composes without glue, and that the
//! resulting Annex-B file plays back in any standard H.264 demuxer.
//!
//! # Acceptance criteria
//!
//! - Process exits 0 within ~12 seconds.
//! - The output file is non-empty and starts with the Annex-B start code
//!   `0x00 0x00 0x00 0x01`.
//! - At least one IDR keyframe and several P-frames are reported in the summary.
//! - `ffplay <output>` plays back recognisable desktop content.
//!
//! # Usage
//!
//! ```text
//! cargo run -p sm-infra --example encode_smoke
//! cargo run -p sm-infra --example encode_smoke -- my_capture.h264
//! ```
//!
//! # Shutdown order
//!
//! `capture.stop()` is called BEFORE `encoder.stop()`. Capture owns the
//! `SyncSender<CaptureFrame>` inside its OS thread; joining capture drops that
//! sender, which makes the encoder thread's `rx.recv()` return `Err` and exit.
//! Calling `encoder.stop()` first while capture is still producing would leave
//! the encoder thread blocked on `recv()` and deadlock the join — the encoder
//! rustdoc spells this out.

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use sm_domain::encode::{EncoderConfig, VideoEncoder};
    use sm_domain::{CaptureConfig, CaptureSource};
    use sm_infra::capture::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};
    use sm_infra::encode::{ENCODE_CHANNEL_CAPACITY, WindowsOpenH264Encoder};

    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "encode_smoke.h264".to_string());

    let (frame_tx, frame_rx) = mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY);
    let (pkt_tx, pkt_rx) = mpsc::sync_channel(ENCODE_CHANNEL_CAPACITY);

    let mut capture = WindowsCaptureSource::new(CaptureConfig::default())?;
    let mut encoder = WindowsOpenH264Encoder::new(EncoderConfig::default())?;

    capture.start(frame_tx)?;
    encoder.start(frame_rx, pkt_tx)?;

    let mut file = BufWriter::new(File::create(&out_path)?);
    let mut packets = 0u32;
    let mut idr_packets = 0u32;
    let mut bytes_written = 0u64;
    let mut keyframe_requested = false;

    let start_at = Instant::now();
    let deadline = start_at + Duration::from_secs(10);
    let keyframe_at = start_at + Duration::from_secs(5);

    println!("encode_smoke — capturando 10 s del monitor primario → {out_path}");
    println!("(press Ctrl+C para abortar)");

    while Instant::now() < deadline {
        if !keyframe_requested && Instant::now() >= keyframe_at {
            encoder.request_keyframe();
            keyframe_requested = true;
            println!("  → request_keyframe() at +5 s");
        }

        match pkt_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(pkt) => {
                packets += 1;
                if pkt.is_keyframe {
                    idr_packets += 1;
                }
                file.write_all(&pkt.data)?;
                bytes_written += pkt.data.len() as u64;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    capture.stop()?;
    encoder.stop()?;
    file.flush()?;
    drop(file);

    let elapsed = start_at.elapsed();
    let file_size = std::fs::metadata(&out_path)?.len();
    let p_packets = packets.saturating_sub(idr_packets);

    println!();
    println!("smoke terminado en {:.2} s", elapsed.as_secs_f64());
    println!("  capture dropped_frames: {}", capture.dropped_frames());
    println!("  encoder dropped_frames: {}", encoder.dropped_frames());
    println!("  encoded packets: {packets} ({idr_packets} IDR, {p_packets} P)");
    println!("  bytes escritos: {bytes_written} (archivo en disco: {file_size} bytes)");
    println!("  archivo: {out_path}");
    println!();
    println!("verificá la salida con:");
    println!("  ffplay -i {out_path}");
    println!("  ffprobe -i {out_path}");

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("encode_smoke is Windows-only");
    std::process::exit(1);
}
