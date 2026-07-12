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
//! - Cross-platform adapters (transport, signaling) use unconditional top-level
//!   dependencies — no `cfg` gate required for pure-Rust crates.
//! - Integration tests requiring a live desktop session MUST be annotated `#[ignore]`
//!   and guarded by a runtime `IsSupported()` check. Run them with:
//!   `cargo nextest run -p sm-infra --run-ignored only`.
//!
//! # Modules
//!
//! - [`capture`] — Windows Graphics Capture adapter (`WindowsCaptureSource`) and
//!   the bounded frame channel constant (`CAPTURE_CHANNEL_CAPACITY`).
//!   On non-Windows targets this module compiles to an empty stub.
//! - [`encode`] — Windows OpenH264 software encoder (`WindowsOpenH264Encoder`)
//!   and the bounded packet channel constant (`ENCODE_CHANNEL_CAPACITY`).
//!   On non-Windows targets this module compiles to an empty stub. The
//!   adapter accepts `CaptureFrame`s on its input channel, performs BGRA→I420
//!   conversion internally, and emits Annex-B H.264 packets via OpenH264 (BSD-2).
//! - [`transport`] — Media transport adapters (`Str0mVideoSender`, `Str0mVideoReceiver`)
//!   backed by the str0m SansIO WebRTC stack. Cross-platform, no OS gate.
//! - [`signaling`] — Signaling adapters: `MdnsSignaling` (mDNS auto-discovery) and
//!   `LoopbackSignaling` (in-memory fixture for tests/CI). Cross-platform.
//! - [`decode`] — Decoder adapters (capability tier). V1: `WindowsOpenH264Decoder`
//!   (Windows-gated). Cross-platform module root for future adapters.
//! - [`render`] — Render-side helpers. V1: `fmp4_muxer` (fMP4 init + media segments for
//!   the MSE hot path) and `avcc` (SPS parser + avcC box builder). Cross-platform.

pub mod capture;
pub mod decode;
pub mod diagnostics;
pub mod encode;
pub mod render;
pub mod signaling;
pub mod transport;
