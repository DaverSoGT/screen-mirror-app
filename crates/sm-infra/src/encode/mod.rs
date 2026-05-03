// Platform-gated encode adapter re-exports.

pub mod bgra_to_i420;
pub mod bgra_to_nv12; // NEW — always-on; Linux/macOS CI catches stride bugs

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "windows")]
pub use windows::{ENCODE_CHANNEL_CAPACITY, WindowsOpenH264Encoder};
