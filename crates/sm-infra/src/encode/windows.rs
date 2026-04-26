#![cfg(target_os = "windows")]
//! Windows OpenH264 encoder adapter (stub — full implementation in batches 3–5).

use crate::encode::bgra_to_i420::{I420, convert};

/// Bounded output channel capacity (mirrors `CAPTURE_CHANNEL_CAPACITY` from capture adapter).
pub const ENCODE_CHANNEL_CAPACITY: usize = 4;

/// Windows H.264 software encoder backed by OpenH264 (stub — full implementation in batch 3).
pub struct WindowsOpenH264Encoder;

// Silence unused-import warnings until Batch 3 wires the real usage.
// These imports are consumed by the encoder thread in the next batch.
const _: () = {
    let _ = convert as fn(&sm_domain::CaptureFrame, &mut I420);
};
