//! Media transport adapters.
//!
//! This module contains [`Str0mVideoSender`] and [`Str0mVideoReceiver`] — the
//! concrete implementations of the `sm_domain::transport` ports backed by the
//! str0m SansIO WebRTC stack.
//!
//! Adapters are cross-platform per PQ-9 (pure-Rust, no OS-specific gate needed).

pub mod annex_b;
pub mod str0m_receiver;
pub mod str0m_sender;

pub use str0m_receiver::Str0mVideoReceiver;
pub use str0m_sender::Str0mVideoSender;

// ─── NIC enumeration ─────────────────────────────────────────────────────────

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Test-only override for NIC enumeration. When `Some`, `enumerate_local_ipv4()`
    /// returns the contained list instead of querying the OS. Reset to `None` between
    /// tests to avoid bleed. Per-thread isolation prevents cross-test contamination
    /// when `cargo nextest` runs tests in parallel.
    static NIC_OVERRIDE: std::cell::RefCell<Option<Vec<std::net::Ipv4Addr>>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Return the list of non-loopback IPv4 addresses bound to local network interfaces.
///
/// In test builds (or when the `test-support` feature is enabled) this function
/// first checks the per-thread `NIC_OVERRIDE`. If a `Some` value is set (by
/// [`NicOverrideGuard`]), that list is returned verbatim — useful for injecting
/// a known address or simulating NIC absence.
///
/// In production builds the function queries `if_addrs::get_if_addrs()` and
/// filters to non-loopback IPv4 addresses only.
pub(crate) fn enumerate_local_ipv4() -> Vec<std::net::Ipv4Addr> {
    #[cfg(any(test, feature = "test-support"))]
    {
        let override_val = NIC_OVERRIDE.with(|cell| cell.borrow().clone());
        if let Some(ips) = override_val {
            return ips;
        }
    }
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|iface| match iface.addr {
                if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => Some(v4.ip),
                _ => None,
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// RAII guard that overrides NIC enumeration for the current thread during tests.
///
/// Constructing a `NicOverrideGuard` sets the per-thread NIC list to `ips`.
/// Dropping the guard restores the default (OS-queried) behaviour.
///
/// Only available when the `test-support` feature is enabled or in test builds.
/// Production binaries do not include this type.
///
/// # Example
///
/// ```rust,no_run
/// # #[cfg(any(test, feature = "test-support"))]
/// # {
/// use std::net::Ipv4Addr;
/// use sm_infra::transport::NicOverrideGuard;
/// let _guard = NicOverrideGuard::new(vec![Ipv4Addr::new(192, 168, 1, 42)]);
/// // enumerate_local_ipv4() now returns [192.168.1.42] until _guard drops.
/// # }
/// ```
#[cfg(any(test, feature = "test-support"))]
pub struct NicOverrideGuard;

#[cfg(any(test, feature = "test-support"))]
impl NicOverrideGuard {
    /// Set the per-thread NIC override to `ips` and return the guard.
    ///
    /// When `ips` is empty this simulates a machine with no usable NIC
    /// (e.g. a CI environment with only loopback adapters).
    pub fn new(ips: Vec<std::net::Ipv4Addr>) -> Self {
        NIC_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(ips));
        Self
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for NicOverrideGuard {
    fn drop(&mut self) {
        NIC_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
    }
}

// ─── publish_host_candidate helper ───────────────────────────────────────────

/// Publish a trickle-ICE host candidate for `addr` on the given signaling channel.
///
/// This is the single source of truth for the publish-side wire encoding:
/// `str0m::Candidate::host(addr, "udp")` serialised as JSON via `serde_json::to_string`
/// (matching the consume-side `serde_json::from_str::<Candidate>` in both adapters).
///
/// Called from `build_production_sender_bundle` (sender.rs) and
/// `build_production_bundle` (stream.rs) after the respective `start()` /
/// `start_with_socket()` calls, AFTER the SDP offer is published (so the peer
/// processes Offer → Candidate in FIFO order).
///
/// Returns `Err` only if `Candidate::host` construction or JSON serialisation
/// fails (neither is expected to fail for a valid `SocketAddr`).
pub fn publish_host_candidate(
    signaling: &dyn sm_domain::signaling::Signaling,
    addr: std::net::SocketAddr,
) -> Result<(), sm_domain::signaling::SignalingError> {
    let cand = str0m::Candidate::host(addr, "udp").map_err(|e| {
        sm_domain::signaling::SignalingError::Protocol(format!("Candidate::host failed: {e}"))
    })?;
    let json = serde_json::to_string(&cand).map_err(|e| {
        sm_domain::signaling::SignalingError::Protocol(format!("Candidate JSON encode failed: {e}"))
    })?;
    signaling.publish_local_candidate(sm_domain::signaling::IceCandidate(json))
}
