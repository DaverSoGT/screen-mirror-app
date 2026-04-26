//! Media transport adapters.
//!
//! This module will contain `Str0mVideoSender` and `Str0mVideoReceiver` — the
//! concrete implementations of the `sm_domain::transport` ports backed by the
//! str0m SansIO WebRTC stack.
//!
//! Adapters are cross-platform per PQ-9 (pure-Rust, no OS-specific gate needed).
