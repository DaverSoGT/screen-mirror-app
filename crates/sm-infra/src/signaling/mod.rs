//! Signaling adapters.
//!
//! - [`loopback::LoopbackSignaling`] — In-memory fixture for tests and CI.
//!   No network, no mDNS. Implements [`sm_domain::signaling::Signaling`].
//! - [`mdns::MdnsSignaling`] — mDNS auto-discovery + TCP control channel.
//!   Implements [`sm_domain::signaling::Signaling`].
//! - [`wire`] — Length-prefixed JSON wire framing shared by `MdnsSignaling`.

pub mod loopback;
pub mod mdns;
pub mod wire;
