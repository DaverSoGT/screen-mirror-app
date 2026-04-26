//! Platform adapters for the screen-mirror application.
//!
//! `sm-infra` provides the concrete implementations of the domain ports defined in
//! `sm-domain`. Each adapter is gated by a `cfg(target_os = ...)` attribute so that
//! only the relevant platform code is compiled into the final binary.
//!
//! # Crate policy
//!
//! - Platform-specific dependencies (`windows-capture`, `windows-version`, etc.) MUST
//!   appear only under `[target.'cfg(target_os = "...")'.dependencies]` in `Cargo.toml`.
//! - The `sm-domain` crate is the only cross-platform dependency.
//! - Integration tests requiring a live desktop session MUST be annotated `#[ignore]`
//!   and guarded by a runtime `IsSupported()` check. Run them with:
//!   `cargo nextest run -p sm-infra --run-ignored only`.
//!
//! # Modules
//!
//! - [`capture`] — Windows Graphics Capture adapter (`WindowsCaptureSource`) and
//!   the bounded frame channel constant (`CAPTURE_CHANNEL_CAPACITY`).
//!   On non-Windows targets this module compiles to an empty stub.

pub mod capture;
pub mod encode;
mod signaling;
mod transport;
