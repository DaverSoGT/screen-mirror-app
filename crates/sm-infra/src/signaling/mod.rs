//! Signaling adapters.
//!
//! - [`loopback::LoopbackSignaling`] — In-memory fixture for tests and CI.
//!   No network, no mDNS. Implements [`sm_domain::signaling::Signaling`].
//! - `MdnsSignaling` (future batch) — mDNS auto-discovery + TCP control channel.

pub mod loopback;
