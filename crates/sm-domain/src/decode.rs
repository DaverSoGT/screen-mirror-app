//! Port boundary for video decoding.
//!
//! This module will define the domain-level contract for decoding `EncodedPacket`s
//! (Annex-B H.264) into raw `DecodedFrame`s. No platform type, async runtime,
//! or codec-specific import is permitted here — all platform adaptation lives
//! in `sm-infra::decode`.
//!
//! # Key types (implemented in B1)
//!
//! | Type | Role |
//! |------|------|
//! | `VideoDecoder`           | Port trait implemented by each decoder adapter.       |
//! | `DecoderConfig`          | Configuration: hint width/height (decoder adapts).    |
//! | `DecodedFrame`           | A single decoded frame (raw pixel bytes + metadata).  |
//! | `DecodedFormat`          | Pixel layout: `I420` or `Bgra8`.                      |
//! | `DecoderError`           | Unified error enum for all decoder operations.        |
//! | `DECODE_CHANNEL_CAPACITY`| Bounded channel capacity constant (4).                |
//!
//! Stub populated by B0 scaffolding; full implementation in B1.
