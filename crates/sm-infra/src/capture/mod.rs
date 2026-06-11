// Platform-gated capture adapter re-exports.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};

// Shared observability seam: exposed crate-internally so encode::windows_mft can
// call the same tested predicate (D-PPT-6, perf-pipeline-throughput Slice 1 W2 fix).
#[cfg(target_os = "windows")]
pub(crate) use windows::interval_elapsed;
