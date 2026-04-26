// Platform-gated encode adapter re-exports.

pub mod bgra_to_i420;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub use windows::{WindowsOpenH264Encoder, ENCODE_CHANNEL_CAPACITY};
