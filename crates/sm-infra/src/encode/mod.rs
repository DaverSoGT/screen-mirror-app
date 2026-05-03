// Platform-gated encode adapter re-exports.

pub mod bgra_to_i420;
pub mod bgra_to_nv12; // always-on; Linux/macOS CI catches stride bugs

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(all(target_os = "windows", feature = "hw-encoder"))]
pub mod windows_mft;

#[cfg(target_os = "windows")]
pub mod factory;

#[cfg(target_os = "windows")]
pub use windows::{ENCODE_CHANNEL_CAPACITY, WindowsOpenH264Encoder};

#[cfg(all(target_os = "windows", feature = "hw-encoder"))]
pub use windows_mft::WindowsMftH264Encoder;

#[cfg(target_os = "windows")]
pub use factory::build_video_encoder;
