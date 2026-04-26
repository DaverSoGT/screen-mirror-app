//! Signaling adapters.
//!
//! This module will contain `MdnsSignaling` (mDNS auto-discovery + TCP control
//! channel) and `LoopbackSignaling` (in-memory fixture for tests and CI).
//!
//! Both implement the `sm_domain::signaling::Signaling` port.
