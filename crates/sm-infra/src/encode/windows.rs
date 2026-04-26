#![cfg(target_os = "windows")]
//! Windows OpenH264 encoder adapter (stub — full implementation in batches 3–5).

/// Bounded output channel capacity (mirrors `CAPTURE_CHANNEL_CAPACITY` from capture adapter).
pub const ENCODE_CHANNEL_CAPACITY: usize = 4;

/// Windows H.264 software encoder backed by OpenH264 (stub — full implementation in batch 3).
pub struct WindowsOpenH264Encoder;
