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

// ─── resolve_candidate_with_retry helper ─────────────────────────────────────

/// Number of times [`resolve_candidate_with_retry`] probes for a usable host
/// candidate before giving up. With [`CANDIDATE_RETRY_INTERVAL`] this bounds the
/// total wait to ~1.5s — comfortably less than the supervisor's 15s
/// `rebuild_timeout` (sender.rs / stream.rs), so a NIC flap during the
/// `InitiateMdnsReset → InitiateRebuild` window cannot starve the rebuild.
pub(crate) const CANDIDATE_RETRY_ATTEMPTS: u32 = 15;

/// Sleep between probe attempts in [`resolve_candidate_with_retry`].
/// 15 attempts × 100ms ≈ 1.5s total budget (no sleep after the final attempt).
pub(crate) const CANDIDATE_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// Probe for a usable host candidate, retrying up to `attempts` times with a
/// caller-supplied delay between tries.
///
/// Motivation: on a real reconnect the supervisor fires `InitiateMdnsReset`
/// then immediately `InitiateRebuild`. The mDNS reset transiently drops the NIC
/// ("no IPv4 network interfaces found"), so a ONE-SHOT `candidate_addr()` probe
/// during the rebuild can observe no non-loopback NIC and skip the host-candidate
/// publish for that whole WebRTC generation — leaving str0m with no local
/// candidate to nominate, so media never flows. Retrying across the NIC-down
/// window lets the publish succeed once the interface returns.
///
/// Pure and side-effect-injected for testability:
/// - `probe` is called once per attempt; the first `Some(addr)` short-circuits
///   and is returned immediately.
/// - `delay` is invoked BETWEEN attempts only (never after the last one), so a
///   test can pass a no-op closure and run instantly while production passes
///   `std::thread::sleep`.
///
/// Returns `Some(addr)` as soon as a probe yields one, or `None` if every
/// attempt within the budget returns `None`. Never exceeds `attempts` probe
/// calls.
pub(crate) fn resolve_candidate_with_retry<P, D>(
    mut probe: P,
    attempts: u32,
    mut delay: D,
) -> Option<std::net::SocketAddr>
where
    P: FnMut() -> Option<std::net::SocketAddr>,
    D: FnMut(std::time::Duration),
{
    // RED stub: one-shot probe (current production behaviour). Retries not yet
    // implemented — the retry test must fail against this.
    let _ = &mut delay;
    let _ = attempts;
    probe()
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

#[cfg(test)]
mod resolve_candidate_with_retry_tests {
    use super::{resolve_candidate_with_retry, CANDIDATE_RETRY_ATTEMPTS};
    use std::cell::Cell;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;

    fn addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 7889)
    }

    /// SC-MR-1 (RED): probe returns `None` for the first K calls then `Some(addr)`
    /// → helper must keep probing and return the address. A one-shot probe fails.
    #[test]
    fn returns_addr_after_nic_returns_midway() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            let n = calls.get();
            calls.set(n + 1);
            // None for the first 3 calls (NIC down), Some afterwards.
            if n < 3 { None } else { Some(addr()) }
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_candidate_with_retry(probe, CANDIDATE_RETRY_ATTEMPTS, delay);

        assert_eq!(resolved, Some(addr()), "must resolve once the NIC returns");
        assert_eq!(calls.get(), 4, "should probe exactly until the first Some");
        assert_eq!(
            delays.get(),
            3,
            "delay fires only BETWEEN attempts (once per failed probe)"
        );
    }

    /// SC-MR-2 (RED): probe returns `None` every call → helper returns `None`,
    /// and MUST NOT exceed the attempt budget. A no-delay run proves no real sleep.
    #[test]
    fn returns_none_when_nic_never_returns_within_budget() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            None
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_candidate_with_retry(probe, CANDIDATE_RETRY_ATTEMPTS, delay);

        assert_eq!(resolved, None, "exhausted budget → None (caller logs skip)");
        assert_eq!(
            calls.get(),
            CANDIDATE_RETRY_ATTEMPTS,
            "must probe exactly the attempt budget, never more"
        );
        assert_eq!(
            delays.get(),
            CANDIDATE_RETRY_ATTEMPTS - 1,
            "no delay after the final attempt"
        );
    }

    /// SC-MR-3 (RED): immediate success on the first probe → no delay at all.
    #[test]
    fn returns_immediately_on_first_success() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            Some(addr())
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_candidate_with_retry(probe, CANDIDATE_RETRY_ATTEMPTS, delay);

        assert_eq!(resolved, Some(addr()));
        assert_eq!(calls.get(), 1, "first Some short-circuits");
        assert_eq!(delays.get(), 0, "no delay when the first probe succeeds");
    }
}
