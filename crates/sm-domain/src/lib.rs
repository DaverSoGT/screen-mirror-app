//! Domain core for the screen-mirror application.
//!
//! `sm-domain` is the innermost crate of the hexagonal architecture. It defines the
//! port boundaries (traits) and shared value types that cross-cut every platform
//! adapter and application layer. It MUST remain free of any platform-specific,
//! async-runtime, or GUI-framework dependency.
//!
//! # Crate policy
//!
//! - No `tokio`, `windows`, `tauri`, or OS-specific crate in `[dependencies]`.
//! - All platform adapters live in `sm-infra`, gated by `cfg(target_os = ...)`.
//! - Compile correctness on Ubuntu, macOS, and Windows is a CI requirement.
//!
//! # Modules
//!
//! - [`capture`] — port boundary for screen capture sources: trait, frame model,
//!   error taxonomy, configuration, and monitor enumeration types.
//! - [`encode`] — port boundary for video encoding: `VideoEncoder` trait,
//!   `EncoderConfig`, `EncodedPacket`, `EncoderError`, and `RateControlMode`.
//! - [`decode`] — port boundary for video decoding: `VideoDecoder` trait,
//!   `DecoderConfig`, `DecodedFrame`, `PixelData`, `DecoderError`, and
//!   `DECODE_CHANNEL_CAPACITY`.

pub mod capture;

pub use capture::BorderPolicy;
pub use capture::CaptureConfig;
pub use capture::CaptureError;
pub use capture::CaptureFrame;
pub use capture::CaptureSource;
pub use capture::MonitorId;
pub use capture::MonitorInfo;
pub use capture::MonitorSelector;
pub use capture::PixelFormat;
pub mod encode;
pub use encode::EncodedPacket;
pub use encode::EncoderConfig;
pub use encode::EncoderError;
pub use encode::RateControlMode;
pub use encode::VideoEncoder;
pub mod decode;
pub use decode::DecodedFrame;
pub use decode::DecoderConfig;
pub use decode::DecoderError;
pub use decode::PixelData;
pub use decode::VideoDecoder;
pub use decode::DECODE_CHANNEL_CAPACITY;
pub mod error;
pub mod session;
pub mod signaling;
pub mod transport;
