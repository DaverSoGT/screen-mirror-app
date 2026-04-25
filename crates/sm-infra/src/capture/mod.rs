// Platform-gated capture adapter re-exports.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};
