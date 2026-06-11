// Platform-gated capture adapter re-exports.

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};

// Shared observability seam: exposed crate-internally so encode::windows_mft can
// call the same tested predicate (D-PPT-6, perf-pipeline-throughput Slice 1 W2 fix).
// Gated on `hw-encoder` to match the sole consumer (encode/windows_mft.rs); otherwise
// the re-export is unused under `--no-default-features` and emits a dead-import warning.
// The capture side calls `interval_elapsed` directly inside windows.rs, not via this
// re-export, so its usage is unaffected by this gate.
#[cfg(all(target_os = "windows", feature = "hw-encoder"))]
pub(crate) use windows::interval_elapsed;
