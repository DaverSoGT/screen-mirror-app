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
pub const CANDIDATE_RETRY_ATTEMPTS: u32 = 15;

/// Sleep between probe attempts in [`resolve_candidate_with_retry`].
/// 15 attempts × 100ms ≈ 1.5s total budget (no sleep after the final attempt).
pub const CANDIDATE_RETRY_INTERVAL: std::time::Duration =
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
pub fn resolve_candidate_with_retry<P, D>(
    mut probe: P,
    attempts: u32,
    mut delay: D,
) -> Option<std::net::SocketAddr>
where
    P: FnMut() -> Option<std::net::SocketAddr>,
    D: FnMut(std::time::Duration),
{
    for attempt in 0..attempts {
        if let Some(addr) = probe() {
            return Some(addr);
        }
        // Delay only BETWEEN attempts — never after the final probe.
        if attempt + 1 < attempts {
            delay(CANDIDATE_RETRY_INTERVAL);
        }
    }
    None
}

// ─── resolve_ipv4_with_retry helper ──────────────────────────────────────────

/// Number of times [`resolve_ipv4_with_retry`] probes for non-empty IPv4
/// addresses before giving up.
///
/// Budget: 40 × 500ms = 20s total (no sleep after the final attempt), which is
/// 10s under the receiver's 30s `DISCOVER_TIMEOUT`. A real Wi-Fi re-enable
/// (adapter up + DHCP) typically completes in 5-15s, so this window is
/// sufficient while remaining bounded.
pub const NIC_RETRY_ATTEMPTS: u32 = 40;

/// Sleep between probe attempts in [`resolve_ipv4_with_retry`].
/// 40 attempts × 500ms ≈ 20s total budget (no sleep after the final attempt).
pub const NIC_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Probe for at least one non-loopback IPv4 address, retrying up to `attempts`
/// times with a caller-supplied delay between tries.
///
/// Motivation: when the sender's NIC drops and returns (Wi-Fi flap), the mDNS
/// signaling bind in `run_sender_thread` calls `collect_ipv4_addrs()` which
/// returns an empty list while the NIC is still recovering. Without a retry the
/// thread emits `SignalingError::Io("no IPv4 network interfaces found")` and
/// returns, killing the signaling thread — and nothing re-enumerates when the
/// NIC comes back. This helper mirrors `resolve_candidate_with_retry` for the
/// bind enumeration path so the sender can wait out the NIC-down window.
///
/// Pure and side-effect-injected for testability:
/// - `probe` is called once per attempt; the first non-empty `Vec` short-circuits
///   and is returned immediately.
/// - `delay` is invoked BETWEEN attempts only (never after the last one), so a
///   test can pass a no-op closure and run instantly while production passes
///   `std::thread::sleep`.
/// - `should_stop` is checked at the TOP of each attempt iteration before the
///   probe runs. If it returns `true` the loop breaks immediately and an empty
///   `Vec` is returned. This keeps teardown latency bounded to at most one
///   `NIC_RETRY_INTERVAL` (500ms) rather than the full retry budget (~20s) —
///   fixing C1 where `MdnsSignaling::stop()` / `Drop` could block ~20s while
///   the thread slept through retries with the NIC down.
///   Tests pass `|| false`; production passes `|| stop.load(Ordering::Acquire)`.
///
/// Returns the first non-empty `Vec<Ipv4Addr>` found, or an empty `Vec` if
/// every attempt within the budget returns empty or `should_stop` fires.
/// Never exceeds `attempts` probe calls.
pub fn resolve_ipv4_with_retry<P, D, S>(
    mut probe: P,
    attempts: u32,
    mut delay: D,
    mut should_stop: S,
) -> Vec<std::net::Ipv4Addr>
where
    P: FnMut() -> Vec<std::net::Ipv4Addr>,
    D: FnMut(std::time::Duration),
    S: FnMut() -> bool,
{
    for attempt in 0..attempts {
        // Check the stop flag BEFORE the probe so teardown is immediately
        // responsive — at most one NIC_RETRY_INTERVAL of latency.
        if should_stop() {
            break;
        }
        let addrs = probe();
        if !addrs.is_empty() {
            return addrs;
        }
        // Delay only BETWEEN attempts — never after the final probe.
        if attempt + 1 < attempts {
            delay(NIC_RETRY_INTERVAL);
        }
    }
    vec![]
}

#[cfg(test)]
mod resolve_ipv4_with_retry_tests {
    use super::{resolve_ipv4_with_retry, NIC_RETRY_ATTEMPTS};
    use std::cell::Cell;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    fn ip() -> Ipv4Addr {
        Ipv4Addr::new(192, 168, 1, 42)
    }

    /// SC-NIC-1: probe returns empty for the first K calls then non-empty →
    /// helper must keep probing and return the addresses once the NIC returns.
    /// A one-shot probe would fail; this asserts probe AND delay counts.
    #[test]
    fn returns_addrs_after_nic_returns_midway() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            let n = calls.get();
            calls.set(n + 1);
            // Empty for the first 3 calls (NIC down), non-empty afterwards.
            if n < 3 { vec![] } else { vec![ip()] }
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_ipv4_with_retry(probe, NIC_RETRY_ATTEMPTS, delay, || false);

        assert_eq!(resolved, vec![ip()], "must return addrs once NIC returns");
        assert_eq!(calls.get(), 4, "probe exactly until the first non-empty result");
        assert_eq!(
            delays.get(),
            3,
            "delay fires only BETWEEN attempts (once per empty probe)"
        );
    }

    /// SC-NIC-2: probe always returns empty → helper returns empty after EXACTLY
    /// `attempts` probes, never more. No real sleep (no-op delay closure).
    #[test]
    fn returns_empty_when_nic_never_returns_within_budget() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            vec![]
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_ipv4_with_retry(probe, NIC_RETRY_ATTEMPTS, delay, || false);

        assert!(resolved.is_empty(), "exhausted budget → empty (caller logs error)");
        assert_eq!(
            calls.get(),
            NIC_RETRY_ATTEMPTS,
            "must probe exactly the attempt budget, never more"
        );
        assert_eq!(
            delays.get(),
            NIC_RETRY_ATTEMPTS - 1,
            "no delay after the final attempt"
        );
    }

    /// SC-NIC-3: first call already returns non-empty → short-circuits immediately,
    /// zero delays fired.
    #[test]
    fn returns_immediately_on_first_success() {
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            vec![ip()]
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };

        let resolved = resolve_ipv4_with_retry(probe, NIC_RETRY_ATTEMPTS, delay, || false);

        assert_eq!(resolved, vec![ip()]);
        assert_eq!(calls.get(), 1, "first non-empty result short-circuits");
        assert_eq!(delays.get(), 0, "no delay when the first probe succeeds");
    }

    /// SC-NIC-4: `should_stop` returns true after K iterations → loop breaks early,
    /// returns empty, and does NOT run all `NIC_RETRY_ATTEMPTS` probes.
    ///
    /// Verifies the cancellable-retry fix for C1 (stop-flag not observed, blocking
    /// `MdnsSignaling::stop()` / `Drop` for up to ~20s). With this fix, teardown
    /// latency is bounded to one `NIC_RETRY_INTERVAL` (500ms) rather than the full
    /// retry budget (~20s).
    #[test]
    fn returns_early_when_should_stop_set() {
        const STOP_AFTER: u32 = 3; // stop flag trips after this many iterations
        let calls = Cell::new(0u32);
        let delays = Cell::new(0u32);

        let probe = || {
            calls.set(calls.get() + 1);
            vec![] // NIC never returns — without early-stop this runs 40 times
        };
        let delay = |_: Duration| {
            delays.set(delays.get() + 1);
        };
        // should_stop fires at the TOP of the (STOP_AFTER+1)-th iteration, so
        // exactly STOP_AFTER probes execute before the break.
        let should_stop = || calls.get() >= STOP_AFTER;

        let resolved =
            resolve_ipv4_with_retry(probe, NIC_RETRY_ATTEMPTS, delay, should_stop);

        assert!(
            resolved.is_empty(),
            "stopped early → result must be empty (no NIC found)"
        );
        assert_eq!(
            calls.get(),
            STOP_AFTER,
            "loop must break after exactly STOP_AFTER probes, not run all NIC_RETRY_ATTEMPTS"
        );
        assert!(
            calls.get() < NIC_RETRY_ATTEMPTS,
            "must not exhaust the full attempt budget when stop flag is set"
        );
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
