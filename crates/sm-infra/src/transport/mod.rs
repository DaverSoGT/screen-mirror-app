//! Media transport adapters.
//!
//! This module contains [`Str0mVideoSender`] and [`Str0mVideoReceiver`] — the
//! concrete implementations of the `sm_domain::transport` ports backed by the
//! str0m SansIO WebRTC stack.
//!
//! Adapters are cross-platform per PQ-9 (pure-Rust, no OS-specific gate needed).

pub mod str0m_receiver;
pub mod str0m_sender;

pub use str0m_receiver::Str0mVideoReceiver;
pub use str0m_sender::Str0mVideoSender;
