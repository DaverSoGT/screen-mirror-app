//! mDNS auto-discovery + TCP control channel signaling adapter.
//!
//! [`MdnsSignaling`] implements [`sm_domain::signaling::Signaling`]. It publishes
//! (sender role) or discovers (receiver role) the `_screen-mirror._tcp.local.` service
//! via mDNS and then exchanges SDP/ICE frames over a direct TCP connection using the
//! length-prefixed JSON protocol defined in [`crate::signaling::wire`].
//!
//! # Thread model
//!
//! `start()` spawns exactly one OS thread (`"sm-signaling-mdns"`). That thread drives
//! the mDNS daemon (via `mdns-sd`) and the TCP control socket. Once the TCP channel
//! is open, it loops reading frames and emitting [`SignalingEvent`]s. Outbound frames
//! are queued via an inbox and written on the same thread. `stop()` sets an
//! [`AtomicBool`] stop flag and joins the thread.
//!
//! # Usage
//!
//! ```rust,no_run
//! use sm_infra::signaling::mdns::MdnsSignaling;
//! use sm_domain::signaling::{Signaling, SignalingConfig, SignalingRole, SignalingEvent};
//! use std::sync::mpsc::sync_channel;
//!
//! let config = SignalingConfig {
//!     role: SignalingRole::Sender,
//!     ..Default::default()
//! };
//! let mut sig = MdnsSignaling::new(config).unwrap();
//! let (event_tx, event_rx) = sync_channel::<SignalingEvent>(8);
//! sig.start(event_tx).unwrap();
//! // ... exchange offer/answer/candidates ...
//! sig.stop().unwrap();
//! ```

use std::io::{self, BufReader, BufWriter, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use sm_domain::signaling::{
    IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent,
    SignalingRole,
};
use sm_domain::supervisor::SupervisorSignal;

use crate::signaling::wire::{MAX_FRAME_BYTES, SignalingFrame, write_frame};
use crate::transport::{NIC_RETRY_ATTEMPTS, NIC_RETRY_INTERVAL, resolve_ipv4_with_retry};

/// Write timeout for `publish_reconnect_request` / `publish_reconnect_ack`.
///
/// Per design §3 (TCP reuse heuristic): if the write does not complete within 2s,
/// the caller should fall back to the mDNS reset path.
const RECONNECT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

// ─── Constants ───────────────────────────────────────────────────────────────

/// mDNS service type for screen-mirror.
const SERVICE_TYPE: &str = "_screen-mirror._tcp.local.";

/// Instance name used when registering the sender service.
const INSTANCE_NAME: &str = "screen-mirror";

/// mDNS/TCP discovery timeout before `PeerNotFound` is reported.
///
/// D-7: extended from 10s to 30s to cover sender republish latency after S-1 supervisor
/// wakes on Bye. The sender republishes within ≤2s of receiving PeerBye via the supervisor;
/// 30s provides a 28s buffer for Windows mDNS jitter, anti-virus scan delays, and
/// USB-Ethernet hotplug events — all while satisfying the 12s reconnect budget.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Read timeout for the TCP frame loop — allows periodic stop-flag checks.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Capability advertised by peers that support the QSV delivery ledger.
const QSV_LEDGER_CAPABILITY: &str = "qsv-ledger-v1";

fn is_qsv_ledger_capability(capability: &str) -> bool {
    capability == QSV_LEDGER_CAPABILITY
}

fn replace_peer_qsv_ledger_capability(negotiated: &Arc<AtomicBool>, capabilities: &[String]) {
    let peer_supports_ledger = capabilities
        .iter()
        .take(16)
        .any(|capability| is_qsv_ledger_capability(capability));
    negotiated.store(peer_supports_ledger, Ordering::Release);
}

#[cfg(test)]
fn qsv_ledger_negotiated(negotiated: &Arc<AtomicBool>) -> bool {
    negotiated.load(Ordering::Acquire)
}

fn hello_capabilities(role: &SignalingRole, _negotiated: &Arc<AtomicBool>) -> Vec<String> {
    match role {
        SignalingRole::Sender | SignalingRole::Receiver => vec![QSV_LEDGER_CAPABILITY.to_string()],
    }
}

/// Process-global monotonic counter assigning a unique id to each signaling
/// connection (one per `run_frame_loop` invocation).
///
/// D6 instrumentation (design #963): during a dual-reconnect, the sender may have
/// two overlapping listeners on port 7889 (the reset listener and the rebuild
/// listener bound via SO_REUSEADDR). The receiver connects to exactly one of them.
/// Tagging each connection with a stable instance id lets the HW operator correlate
/// — across the `connection up`, `accept`, and `Bye` log lines — WHICH listener
/// served the connection that carried (or failed to carry) the fresh Offer, and
/// WHICH torn-down generation emitted the stale Bye. This settles the DEFERRED D3
/// dual-listener question at the next HW gate without changing any behavior.
static SIGNALING_INSTANCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Allocate the next signaling-instance id (monotonic, process-global).
fn next_signaling_instance_id() -> u64 {
    SIGNALING_INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed)
}

// ─── Internal control messages ────────────────────────────────────────────────

/// Outbound frames queued from the public API into the signaling thread.
#[derive(Debug)]
enum MdnsControl {
    /// Offer to be forwarded to the connected peer.
    /// Carries the supervisor reconnect-attempt number (REQ-GE-1).
    Offer(SdpOffer, u8),
    /// Answer to be forwarded to the connected peer.
    Answer(SdpAnswer),
    /// ICE candidate to be forwarded to the connected peer.
    Candidate(IceCandidate),
    /// Reconnect request to be forwarded to the connected peer.
    ///
    /// Published when local `IceFailed` or `ConnectionLost` is detected.
    ReconnectRequest {
        attempt: u8,
        requester_role: SignalingRole,
        session_nonce: u64,
    },
    /// Reconnect acknowledgment to be forwarded to the connected peer.
    ///
    /// Published by the losing side in a simultaneous-detect race, or the
    /// responding side in a one-sided detect.
    ReconnectAck { attempt: u8, session_nonce: u64 },
}

// ─── MdnsSignaling ────────────────────────────────────────────────────────────

/// mDNS auto-discovery + TCP control channel signaling adapter.
///
/// Implements [`Signaling`]. Role is fixed at construction:
/// - **Sender**: publishes `_screen-mirror._tcp.local.`, listens for TCP connections.
/// - **Receiver**: browses for `_screen-mirror._tcp.local.`, connects TCP to sender.
///
/// Once the TCP connection is established, both sides exchange [`SignalingFrame`]s
/// (length-prefixed JSON). The thread emits [`SignalingEvent`]s on the channel
/// injected via `start(event_tx)`.
///
/// # Network notes
///
/// Requires a working multicast interface. Tests that exercise mDNS discovery are
/// annotated `#[ignore]` per R7.5. Unit tests for framing and error mapping do NOT
/// require network access.
pub struct MdnsSignaling {
    /// Runtime configuration.
    config: SignalingConfig,
    /// Shared stop flag — raised by `stop()` and `Drop`.
    stop: Arc<AtomicBool>,
    /// Outbound control inbox (public API → signaling thread).
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    /// Thread handle (None before `start()` and after `stop()`).
    handle: Option<JoinHandle<()>>,
    /// Supervisor signal channel — used by the frame loop to route incoming
    /// `ReconnectRequest` and `ReconnectAck` frames to the reconnect supervisor.
    ///
    /// `None` until `set_supervisor_signal_tx` is called. When `None`, reconnect
    /// frames are silently consumed (backward-compatible — frame_to_event returns None).
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    /// When raised, the frame loop exits on the stop flag WITHOUT sending the
    /// teardown `Bye` frame (D3 stale-Bye fix, design #967).
    ///
    /// Default `false` — genuine shutdown still emits `Bye` so the receiver's
    /// `PeerBye` eager-wake fast-path is preserved. It is set to `true` ONLY on a
    /// generation that is being superseded by a fast rebuild, so the old
    /// generation's eventual teardown does not emit a spurious `Bye` on a
    /// connection the receiver may still be using (see `suppress_outbound_bye`).
    suppress_bye: Arc<AtomicBool>,
    /// When raised, the sender accept loop stops accepting NEW TCP connections
    /// (listener-handover accept-gate, design #971 §B option iii-a).
    ///
    /// Default `false`. Set to `true` ONLY on a generation that is being
    /// superseded by a fast rebuild, BEFORE the reset hook re-`start()`s it, so the
    /// re-started gen-G comes up already-superseded and never competes for the
    /// receiver's reconnect — only the offer-bearing gen-(G+1) accepts. This closes
    /// the dual-listener RST race (HW gate v4, #970): an offer-less gen-G socket
    /// must not steal and then RST the receiver's rebuilt connection.
    ///
    /// CRITICAL: this flag governs ONLY the pre-accept poll loop. It is NOT
    /// threaded into `run_frame_loop`, so raising it never closes an
    /// already-accepted live connection (SC-HO-1b). Sibling seam to `suppress_bye`.
    superseded: Arc<AtomicBool>,
    /// The attempt number from the LAST `MdnsControl::Offer` drained from the inbox.
    ///
    /// Stamped by `run_frame_loop` on every `MdnsControl::Offer(_, att)` drain
    /// (D-8, REQ-BYE-2). At teardown the frame loop loads this value (Acquire)
    /// and writes `Bye { attempt }` so the peer can filter stale-generation Byes
    /// (REQ-BYE-4). Seeded to 0 — an offer-less connection emits `Bye{attempt:0}`;
    /// any real receiver floor is ≥1, so `0 < floor` is always true → dropped.
    ///
    /// On non-Windows targets the frame loop body is dead code, but this field
    /// is read on all targets to stamp the teardown Bye. Clippy cross-target will
    /// confirm no dead_code warning here (if it fires, add cfg_attr as in half-1).
    last_offer_attempt: Arc<AtomicU8>,
    /// Shared peer capability state for the active signaling session.
    qsv_ledger_negotiated: Arc<AtomicBool>,
}

impl Signaling for MdnsSignaling {
    /// Construct an `MdnsSignaling` instance. No threads started, no network touched.
    fn new(config: SignalingConfig) -> Result<Self, SignalingError> {
        Ok(Self {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            handle: None,
            supervisor_signal_tx: Arc::new(Mutex::new(None)),
            suppress_bye: Arc::new(AtomicBool::new(false)),
            superseded: Arc::new(AtomicBool::new(false)),
            // D-8 (REQ-BYE-2): seeded 0; set to last drained Offer attempt in run_frame_loop.
            last_offer_attempt: Arc::new(AtomicU8::new(0)),
            qsv_ledger_negotiated: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Begin signaling. Spawns the `"sm-signaling-mdns"` OS thread.
    ///
    /// Returns `Err(AlreadyRunning)` if called twice without an intervening `stop()`.
    fn start(&mut self, event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
        if self.handle.is_some() {
            return Err(SignalingError::AlreadyRunning);
        }
        self.stop.store(false, Ordering::Release);
        self.clear_qsv_ledger_on_peer_disconnect();

        let config = self.config.clone();
        let stop = Arc::clone(&self.stop);
        let inbox = Arc::clone(&self.inbox);
        let supervisor_signal_tx = Arc::clone(&self.supervisor_signal_tx);
        let suppress_bye = Arc::clone(&self.suppress_bye);
        let superseded = Arc::clone(&self.superseded);
        let last_offer_attempt = Arc::clone(&self.last_offer_attempt);
        let qsv_ledger_negotiated = Arc::clone(&self.qsv_ledger_negotiated);

        let handle = thread::Builder::new()
            .name("sm-signaling-mdns".to_string())
            .spawn(move || {
                run_signaling_thread(
                    config,
                    stop,
                    inbox,
                    event_tx,
                    supervisor_signal_tx,
                    suppress_bye,
                    superseded,
                    last_offer_attempt,
                    qsv_ledger_negotiated,
                );
            })
            .map_err(|e| SignalingError::Io(e.to_string()))?;

        self.handle = Some(handle);
        Ok(())
    }

    /// Queue an SDP offer to be written on the TCP channel.
    ///
    /// `attempt` is the supervisor reconnect-attempt number at publish time (REQ-GE-1).
    /// Returns `Err(NotRunning)` if `start()` has not been called or `stop()` was called.
    fn publish_local_offer(&self, offer: SdpOffer, attempt: u8) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox
            .lock()
            .unwrap()
            .push(MdnsControl::Offer(offer, attempt));
        Ok(())
    }

    /// Queue an SDP answer to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox.lock().unwrap().push(MdnsControl::Answer(answer));
        Ok(())
    }

    /// Queue an ICE candidate to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox
            .lock()
            .unwrap()
            .push(MdnsControl::Candidate(cand));
        Ok(())
    }

    /// Stop signaling. Idempotent. Joins the thread.
    fn stop(&mut self) -> Result<(), SignalingError> {
        self.stop.store(true, Ordering::Release);
        self.clear_qsv_ledger_on_peer_disconnect();
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        Ok(())
    }
}

impl MdnsSignaling {
    /// Register a supervisor signal channel so incoming `ReconnectRequest` and
    /// `ReconnectAck` frames are forwarded to the reconnect supervisor instead of
    /// being silently consumed.
    ///
    /// Call this BEFORE `start()` to ensure no frames are missed. Calling after
    /// `start()` is safe but may miss frames that arrive before the channel is set.
    pub fn set_supervisor_signal_tx(&self, tx: SyncSender<SupervisorSignal>) {
        *self.supervisor_signal_tx.lock().unwrap() = Some(tx);
    }

    /// Suppress the teardown `Bye` frame for this signaling instance (D3 stale-Bye
    /// fix, design #967).
    ///
    /// Once set, the frame loop exits on the stop flag WITHOUT writing a `Bye`.
    /// Call this on a generation that is being superseded by a fast rebuild so its
    /// eventual teardown does not emit a spurious `Bye` on a connection the receiver
    /// may still be using. Genuine shutdown leaves this unset and still emits `Bye`,
    /// preserving the receiver's `PeerBye` eager-wake fast-path.
    ///
    /// Ordering: stores with `Release` to pair with the frame loop's `Acquire` load
    /// at the stop-flag Bye gate.
    pub fn suppress_outbound_bye(&self) {
        self.suppress_bye.store(true, Ordering::Release);
    }

    /// Read-only observer for the suppress-Bye flag (D3, design #967).
    ///
    /// Diagnostic accessor (sibling to the D6 instance-id instrumentation): lets the
    /// sender-coordinator reset-hook test (SC-D3-3) assert that the superseded
    /// generation had its teardown Bye muted. Loads with `Acquire`.
    pub fn is_bye_suppressed(&self) -> bool {
        self.suppress_bye.load(Ordering::Acquire)
    }

    /// Mark this signaling generation as superseded by a fast rebuild
    /// (listener-handover accept-gate, design #971 §B option iii-a).
    ///
    /// Once set, the sender accept loop stops accepting NEW TCP connections so only
    /// the offer-bearing gen-(G+1) answers the receiver's reconnect — closing the
    /// dual-listener RST race (#970). Call this in the `InitiateMdnsReset` hook
    /// right after `suppress_outbound_bye()` and BEFORE the reset's re-`start()`, so
    /// the re-started gen-G comes up already-superseded. The flag persists across
    /// the `stop()` + `start()` reuse cycle (it is cloned into the freshly-spawned
    /// accept thread), so the re-started listener never accepts.
    ///
    /// Does NOT close any already-accepted connection — the flag is never threaded
    /// into `run_frame_loop` (SC-HO-1b). Sibling seam to `suppress_outbound_bye`.
    ///
    /// Ordering: stores with `Release` to pair with the accept loop's `Acquire`
    /// load at the top-of-loop gate.
    pub fn mark_superseded(&self) {
        self.superseded.store(true, Ordering::Release);
    }

    /// Read-only observer for the superseded accept-gate flag (B, design #971).
    ///
    /// Diagnostic accessor (sibling to `is_bye_suppressed`): lets the
    /// sender-coordinator reset-hook test (SC-HO-2) assert that the superseded
    /// generation had its accept gate raised. Loads with `Acquire`.
    pub fn is_superseded(&self) -> bool {
        self.superseded.load(Ordering::Acquire)
    }

    /// Remove any queued `ReconnectRequest` frames from the outbound inbox,
    /// returning how many were dropped (D3 stale-Bye fix, design #967 §3).
    ///
    /// `InitiateMdnsReset` reuses the SAME inbox `Arc` across `stop()` + `start()`.
    /// A `ReconnectRequest` queued for the OLD connection but not yet drained would
    /// otherwise re-flush onto the NEW connection, keeping the superseded generation
    /// competing as an offer-less listener. This clear is TARGETED: only
    /// `ReconnectRequest` entries are removed; `Offer` / `Answer` / `Candidate` /
    /// `ReconnectAck` stay queued so no legitimately-needed frame is lost.
    ///
    /// MUST be called while no frame-loop thread is draining the inbox (i.e. between
    /// `stop()` — which joins the old thread — and the next `start()`), so the retain
    /// is race-free.
    pub fn drain_stale_reconnect_requests(&self) -> usize {
        let mut inbox = self.inbox.lock().unwrap();
        let before = inbox.len();
        inbox.retain(|msg| !matches!(msg, MdnsControl::ReconnectRequest { .. }));
        before - inbox.len()
    }

    /// Test-only accessor for the outbound inbox (D3, design #967).
    ///
    /// Lets `SC-D3-4` seed and inspect inbox contents to verify
    /// `drain_stale_reconnect_requests` is targeted. Kept module-private (matching
    /// `MdnsControl`'s visibility) and only reachable from the in-module test child.
    #[cfg(test)]
    fn inbox_for_test(&self) -> &Arc<Mutex<Vec<MdnsControl>>> {
        &self.inbox
    }

    /// Queue a `ReconnectRequest` frame to be written on the TCP channel.
    ///
    /// Uses the existing inbox mechanism — the frame loop writes it on the next
    /// inbox drain. Returns `Err(NotRunning)` if `start()` has not been called.
    ///
    /// Per design §3 TCP reuse heuristic: the TCP stream has a write timeout set to
    /// [`RECONNECT_WRITE_TIMEOUT`] (2s). If the write does not complete, the caller
    /// should invoke `reset()` for full mDNS rediscovery.
    pub fn publish_reconnect_request(
        &self,
        attempt: u8,
        requester_role: SignalingRole,
        session_nonce: u64,
    ) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox
            .lock()
            .unwrap()
            .push(MdnsControl::ReconnectRequest {
                attempt,
                requester_role,
                session_nonce,
            });
        Ok(())
    }

    /// Queue a `ReconnectAck` frame to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    pub fn publish_reconnect_ack(
        &self,
        attempt: u8,
        session_nonce: u64,
    ) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox.lock().unwrap().push(MdnsControl::ReconnectAck {
            attempt,
            session_nonce,
        });
        Ok(())
    }

    /// Perform a full mDNS reset: stop the current signaling instance and rebuild
    /// a new one with the same configuration.
    ///
    /// This is the TCP failure fallback path (design §3): when `publish_reconnect_request`
    /// fails or no `ReconnectAck` arrives within 2s, the supervisor calls `reset()` to
    /// tear down the stale TCP connection and rediscover the peer via mDNS.
    ///
    /// After `reset()`, callers MUST call `start(event_tx)` again with a fresh event channel.
    ///
    /// # Lifecycle
    ///
    /// The returned `MdnsSignaling` is in the same pre-start state as `new()`. The caller
    /// is responsible for re-registering the supervisor signal channel via
    /// `set_supervisor_signal_tx()` before calling `start()`.
    pub fn reset(self) -> Result<MdnsSignaling, SignalingError> {
        // `self` is consumed (moved), which calls Drop and stops the thread.
        // Construct a fresh instance with the same config.
        let config = self.config.clone();
        self.clear_qsv_ledger_on_peer_disconnect();
        // Drop `self` — this calls `Stop::drop` which calls `stop()`.
        drop(self);
        MdnsSignaling::new(config)
    }

    fn clear_qsv_ledger_on_peer_disconnect(&self) {
        self.qsv_ledger_negotiated.store(false, Ordering::Release);
    }

    #[cfg(test)]
    fn replace_peer_qsv_ledger_capability(&self, capabilities: &[String]) {
        replace_peer_qsv_ledger_capability(&self.qsv_ledger_negotiated, capabilities);
    }

    #[cfg(test)]
    fn qsv_ledger_negotiated_state_for_test(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.qsv_ledger_negotiated)
    }
}

impl Drop for MdnsSignaling {
    /// Ensures the signaling thread is stopped when `MdnsSignaling` is dropped.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ─── Frame → event mapping ────────────────────────────────────────────────────

/// Convert an inbound [`SignalingFrame`] into the matching [`SignalingEvent`].
///
/// Returns `None` for `Hello` — consumed silently as a protocol-version handshake.
/// Returns `None` for `ReconnectRequest`/`ReconnectAck` — these frames are forwarded
/// to the reconnect supervisor via `supervisor_signal_tx` (if set) instead of producing
/// a `SignalingEvent`. When `supervisor_signal_tx` is `None`, reconnect frames are
/// silently consumed (Phase 3 backward-compatible behavior).
/// All other variants map 1-to-1 to `SignalingEvent`.
pub(crate) fn frame_to_event(
    frame: SignalingFrame,
    supervisor_signal_tx: &Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) -> Option<SignalingEvent> {
    match frame {
        SignalingFrame::Hello { .. } => None,
        SignalingFrame::Offer { sdp, attempt } => {
            Some(SignalingEvent::OfferReceived(SdpOffer(sdp), attempt))
        }
        SignalingFrame::Answer { sdp } => Some(SignalingEvent::AnswerReceived(SdpAnswer(sdp))),
        SignalingFrame::Candidate { sdp } => {
            Some(SignalingEvent::CandidateReceived(IceCandidate(sdp)))
        }
        SignalingFrame::Bye { attempt } => {
            // D-3 (REQ-BYE-3): carry the attempt on Closed so the receiver drain can
            // apply the strict-less-than stale-Bye filter (REQ-BYE-4).
            // The EAGER LocalFailure{PeerBye} try_send that previously lived here is
            // REMOVED — it bypassed the drain filter entirely (R-2 bypass closed).
            // All Bye → supervisor escalation now flows exclusively through the drain
            // Closed arm (stream.rs), which has expected_attempt in scope.
            Some(SignalingEvent::Closed {
                attempt: Some(attempt),
            })
        }
        SignalingFrame::ReconnectRequest {
            attempt,
            requester_role,
            session_nonce,
        } => {
            // Route to supervisor channel; do NOT produce a SignalingEvent.
            // `session_nonce` from the peer acts as the peer's nonce for the
            // role-equal tie-break fallback. `requester_role` is forwarded as
            // `peer_role` so the supervisor's role-aware tie-break (design #963 D1)
            // can elect the offerer (Sender) as the active reconnector — previously
            // this field was discarded (#962), making the tie-break role-blind.
            if let Some(tx) = supervisor_signal_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(SupervisorSignal::PeerRequest {
                    peer_nonce: session_nonce,
                    peer_role: requester_role,
                    attempt,
                });
            }
            None
        }
        SignalingFrame::ReconnectAck {
            attempt,
            session_nonce,
        } => {
            // Route to supervisor channel; do NOT produce a SignalingEvent.
            if let Some(tx) = supervisor_signal_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(SupervisorSignal::PeerAck {
                    session_nonce,
                    attempt,
                });
            }
            None
        }
    }
}

// ─── Thread entry point ───────────────────────────────────────────────────────

struct FrameLoopContext {
    last_offer_attempt: Arc<AtomicU8>,
    role: SignalingRole,
    qsv_ledger_negotiated: Arc<AtomicBool>,
}

/// Dispatch to the sender or receiver thread based on role.
#[allow(clippy::too_many_arguments)]
fn run_signaling_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    suppress_bye: Arc<AtomicBool>,
    superseded: Arc<AtomicBool>,
    last_offer_attempt: Arc<AtomicU8>,
    qsv_ledger_negotiated: Arc<AtomicBool>,
) {
    match config.role {
        // `superseded` gates ONLY the sender accept loop (listener handover, B).
        // The receiver is a TCP client and never accepts, so it does not need it.
        SignalingRole::Sender => run_sender_thread(
            config,
            stop,
            inbox,
            event_tx,
            supervisor_signal_tx,
            suppress_bye,
            superseded,
            FrameLoopContext {
                last_offer_attempt,
                role: SignalingRole::Sender,
                qsv_ledger_negotiated,
            },
        ),
        SignalingRole::Receiver => run_receiver_thread(
            config,
            stop,
            inbox,
            event_tx,
            supervisor_signal_tx,
            suppress_bye,
            FrameLoopContext {
                last_offer_attempt,
                role: SignalingRole::Receiver,
                qsv_ledger_negotiated,
            },
        ),
    }
}

// ─── Socket helpers ───────────────────────────────────────────────────────────

fn bind_tcp_listener_reusable(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    // SO_REUSEADDR on Windows allows binding while a previous socket on the
    // same address is still LIVE (LISTEN state) — exactly what we need for
    // the supervisor rebuild race where the old MdnsSignaling thread still
    // holds 0.0.0.0:7889 via Arc clones in coordinator_hooks (engram #1417).
    // On Unix this only covers TIME_WAIT rebinds — also useful, never harmful.
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

// ─── Sender thread ────────────────────────────────────────────────────────────

/// Outcome of the gated accept poll loop.
///
/// Distinguishes a real accepted connection from a clean gate-driven exit (stop or
/// superseded) and from an I/O error, so the caller can run the right cleanup
/// (mDNS shutdown) on each path.
enum AcceptOutcome {
    /// A peer connected and was accepted; carries the stream.
    Accepted(std::net::TcpStream),
    /// The loop exited cleanly because `stop` or `superseded` was raised — no
    /// connection was accepted.
    Gated,
    /// `accept()` returned a hard I/O error (already emitted to `event_tx`).
    Errored,
}

/// Poll `listener.accept()` non-blocking, gated by `stop` and `superseded`.
///
/// Returns:
/// - [`AcceptOutcome::Accepted`] with the stream + emits `PeerFound`, on a real connect.
/// - [`AcceptOutcome::Gated`] when `stop` OR `superseded` is raised before a connect —
///   the superseded gate (design #971 §B) is what makes a re-started, offer-less
///   gen-G NOT compete for the receiver's reconnect.
/// - [`AcceptOutcome::Errored`] on a hard `accept()` error (already emitted).
///
/// The listener MUST already be in non-blocking mode. Both flags are loaded with
/// `Acquire` to pair with the `Release` stores in `stop()` / `mark_superseded()`.
fn accept_one_with_gate(
    listener: &std::net::TcpListener,
    stop: &Arc<AtomicBool>,
    superseded: &Arc<AtomicBool>,
    event_tx: &SyncSender<SignalingEvent>,
) -> AcceptOutcome {
    loop {
        // Gate at the TOP of the loop, alongside the stop check. A superseded
        // generation stops accepting NEW connections so only the offer-bearing
        // gen-(G+1) answers (listener handover, B). It does NOT touch any
        // already-accepted connection — that lives in `run_frame_loop`.
        if stop.load(Ordering::Acquire) || superseded.load(Ordering::Acquire) {
            return AcceptOutcome::Gated;
        }
        match listener.accept() {
            Ok((stream, addr)) => {
                let _ = emit(
                    event_tx,
                    SignalingEvent::PeerFound {
                        host: addr.ip().to_string(),
                        port: addr.port(),
                    },
                );
                return AcceptOutcome::Accepted(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                emit_error(event_tx, SignalingError::Io(e.to_string()));
                return AcceptOutcome::Errored;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sender_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    suppress_bye: Arc<AtomicBool>,
    superseded: Arc<AtomicBool>,
    context: FrameLoopContext,
) {
    context
        .qsv_ledger_negotiated
        .store(false, Ordering::Release);
    let port = config.control_port;

    // Bind TCP listener BEFORE mDNS registration so the receiver can connect
    // immediately after discovery.
    let bind_addr: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("static 0.0.0.0:{port} format is always a valid SocketAddr");
    let listener = match bind_tcp_listener_reusable(bind_addr) {
        Ok(l) => l,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };
    // Non-blocking accept so we can poll the stop flag while waiting.
    if let Err(e) = listener.set_nonblocking(true) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    // Enumerate IPv4 addresses for mDNS registration, retrying across a NIC-down
    // window (e.g. Wi-Fi flap). `resolve_ipv4_with_retry` polls up to
    // NIC_RETRY_ATTEMPTS times with NIC_RETRY_INTERVAL between probes — a budget
    // of ~20s, comfortably under DISCOVER_TIMEOUT (30s). If the NIC returns
    // within that window, the bind proceeds; if not, we still terminate cleanly.
    //
    // C1 fix: pass the thread's stop flag as `should_stop` so that when
    // `MdnsSignaling::stop()` sets the flag, the retry loop breaks at the top of
    // the next iteration rather than sleeping through the full ~20s budget.
    // Teardown latency is now bounded to at most one NIC_RETRY_INTERVAL (500ms).
    let attempts_before_success = std::cell::Cell::new(0u32);
    let ip_list = resolve_ipv4_with_retry(
        || {
            attempts_before_success.set(attempts_before_success.get() + 1);
            collect_ipv4_addrs()
        },
        NIC_RETRY_ATTEMPTS,
        std::thread::sleep,
        || stop.load(Ordering::Acquire),
    );
    if ip_list.is_empty() {
        // All NIC_RETRY_ATTEMPTS probes exhausted and NIC did not return — the
        // sender is genuinely offline. Log loudly so HW-gate logs show the budget
        // was honoured, then terminate with the standard error.
        eprintln!(
            "[sm-signaling] ERROR: NIC enumeration exhausted after {} attempts \
             ({} × {}ms ≈ {}s budget) — no IPv4 interfaces found; \
             sender signaling thread terminating",
            NIC_RETRY_ATTEMPTS,
            NIC_RETRY_ATTEMPTS,
            NIC_RETRY_INTERVAL.as_millis(),
            NIC_RETRY_INTERVAL.as_millis() * u128::from(NIC_RETRY_ATTEMPTS - 1) / 1000,
        );
        emit_error(
            &event_tx,
            SignalingError::Io("no IPv4 network interfaces found".to_string()),
        );
        return;
    }
    // NIC returned — log recovery if it took more than one probe.
    let probes = attempts_before_success.get();
    if probes > 1 {
        eprintln!(
            "[sm-signaling] NIC recovered after {} probe(s) \
             (~{}ms wait) — proceeding with mDNS registration on {}",
            probes,
            NIC_RETRY_INTERVAL.as_millis() * u128::from(probes - 1),
            ip_list[0],
        );
    }

    // Register mDNS service.
    let mdns = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    let host_name = format!("{}.local.", ip_list[0]);
    let ip_str = ip_list[0].to_string();
    let props = [("role", "sender"), ("proto", "v1")];
    let service_info = match ServiceInfo::new(
        SERVICE_TYPE,
        INSTANCE_NAME,
        &host_name,
        ip_str.as_str(),
        port,
        &props[..],
    ) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    if let Err(e) = mdns.register(service_info) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    // Accept one TCP connection (non-blocking with stop + superseded polling).
    // The `superseded` accept-gate (listener handover, design #971 §B option iii-a)
    // makes a re-started, offer-less gen-G stop accepting NEW connections so only
    // the offer-bearing gen-(G+1) answers the receiver's reconnect.
    let stream = match accept_one_with_gate(&listener, &stop, &superseded, &event_tx) {
        AcceptOutcome::Accepted(stream) => stream,
        AcceptOutcome::Gated | AcceptOutcome::Errored => {
            let _ = mdns.shutdown();
            return;
        }
    };

    // D-8: mdns.shutdown() is called AFTER run_frame_loop returns so the mDNS service
    // stays published throughout the entire TCP session. A reconnecting receiver can
    // still discover the sender while streaming. When run_frame_loop exits (Bye, error,
    // or stop_flag), shutdown() sends the goodbye packet to clean up the mDNS entry.
    run_frame_loop(
        stream,
        stop,
        inbox,
        event_tx,
        supervisor_signal_tx,
        suppress_bye,
        context,
    );
    let _ = mdns.shutdown();
}

// ─── Receiver thread ──────────────────────────────────────────────────────────

fn run_receiver_thread(
    _config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    suppress_bye: Arc<AtomicBool>,
    context: FrameLoopContext,
) {
    context
        .qsv_ledger_negotiated
        .store(false, Ordering::Release);
    let mdns = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    let browse_rx = match mdns.browse(SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    // Wait for a resolved service within the discovery timeout.
    let deadline = std::time::Instant::now() + DISCOVER_TIMEOUT;
    let resolved = loop {
        if stop.load(Ordering::Acquire) {
            let _ = mdns.shutdown();
            return;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            emit_error(&event_tx, SignalingError::PeerNotFound);
            let _ = mdns.shutdown();
            return;
        }
        match browse_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ServiceEvent::ServiceResolved(info)) => break info,
            Ok(_) => continue,
            Err(_) => continue,
        }
    };

    // Pick first IPv4 address from the resolved service.
    let peer_addr = resolved
        .get_addresses()
        .iter()
        .filter_map(|a| match a.to_ip_addr() {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .next();

    let peer_ip: Ipv4Addr = match peer_addr {
        Some(ip) => ip,
        None => {
            emit_error(
                &event_tx,
                SignalingError::Io("resolved service has no IPv4 address".to_string()),
            );
            let _ = mdns.shutdown();
            return;
        }
    };

    let peer_port = resolved.get_port();
    let _ = emit(
        &event_tx,
        SignalingEvent::PeerFound {
            host: peer_ip.to_string(),
            port: peer_port,
        },
    );
    let stream = match TcpStream::connect((peer_ip, peer_port)) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            let _ = mdns.shutdown();
            return;
        }
    };

    // D-8: mdns.shutdown() is called AFTER run_frame_loop returns so the mDNS service
    // stays published throughout the entire TCP session. The mDNS entry remains active
    // until the TCP session ends, ensuring a reconnecting sender can still be discovered
    // by other receivers. Shutdown sends the goodbye packet to clean up the mDNS entry.
    run_frame_loop(
        stream,
        stop,
        inbox,
        event_tx,
        supervisor_signal_tx,
        suppress_bye,
        context,
    );
    let _ = mdns.shutdown();
}

// ─── Shared TCP frame loop ────────────────────────────────────────────────────

/// Drive the TCP control channel: write outbound frames from inbox, read inbound
/// frames and emit `SignalingEvent`s. Runs until the stop flag is set or the
/// connection closes.
fn run_frame_loop(
    stream: TcpStream,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
    suppress_bye: Arc<AtomicBool>,
    context: FrameLoopContext,
) {
    let FrameLoopContext {
        last_offer_attempt,
        role,
        qsv_ledger_negotiated,
    } = context;
    qsv_ledger_negotiated.store(false, Ordering::Release);
    // D6 instrumentation: assign a unique signaling-instance id to this connection
    // so the HW operator can correlate which listener/connection served it (and,
    // on teardown, which instance emitted a stale Bye) across overlapping
    // dual-listener generations during a dual-reconnect.
    let instance_id = next_signaling_instance_id();

    // Diagnostic: log the actual TCP endpoints so loopback/dup-connect can be
    // distinguished from cross-host. Should always be peer != local; equal
    // would indicate the writer is feeding the reader on the same machine. The
    // `local` endpoint identifies WHICH listener (e.g. reset vs rebuild on 7889)
    // accepted/served this connection.
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|e| format!("<peer_addr err: {e}>"));
    let local = stream
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|e| format!("<local_addr err: {e}>"));
    eprintln!(
        "[sm-signaling-frame-loop] connection up: instance={instance_id} local={local} peer={peer}"
    );

    // Set read timeout so the loop can check the stop flag and drain the inbox.
    if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        qsv_ledger_negotiated.store(false, Ordering::Release);
        return;
    }

    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            qsv_ledger_negotiated.store(false, Ordering::Release);
            return;
        }
    };

    // Set write timeout on the cloned stream so reconnect frame writes do not
    // block indefinitely on a half-open TCP connection (design §3, AC-6).
    // Note: on Windows (Winsock), write timeout via SO_SNDTIMEO may not always
    // fire reliably on half-open sockets; the 2s ack-timeout in the supervisor
    // is the primary guard against hanging.
    if let Err(e) = write_stream.set_write_timeout(Some(RECONNECT_WRITE_TIMEOUT)) {
        eprintln!("[sm-signaling-frame-loop] WARN: set_write_timeout failed: {e}");
        // Non-fatal: continue without write timeout.
    }

    let mut writer = BufWriter::new(write_stream);
    let mut reader = BufReader::new(stream);

    // Send Hello frame first.
    if let Err(e) = write_frame(
        &mut writer,
        &SignalingFrame::Hello {
            proto: "v1".to_string(),
            capabilities: hello_capabilities(&role, &qsv_ledger_negotiated),
        },
    ) {
        eprintln!("[sm-signaling-frame-loop] EXIT: hello write failed: {e}");
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        qsv_ledger_negotiated.store(false, Ordering::Release);
        return;
    }

    loop {
        if stop.load(Ordering::Acquire) {
            // D3 stale-Bye fix (design #967): a superseded generation muting its
            // teardown Bye exits here WITHOUT writing Bye. Acquire pairs with the
            // Release store in `suppress_outbound_bye`. Genuine shutdown leaves the
            // flag false and still emits Bye, preserving the receiver's PeerBye
            // eager-wake fast-path.
            if suppress_bye.load(Ordering::Acquire) {
                // D6: the instance id lets the HW operator confirm the suppressed
                // teardown came from the stale (offer-less) generation.
                eprintln!(
                    "[sm-signaling-frame-loop] EXIT: instance={instance_id} stop flag set, Bye SUPPRESSED (D3)"
                );
                break;
            }
            // D6: tag the stop-flag Bye with the instance id. Per #962 this is the
            // stale-Bye source during a dual-reconnect — a torn-down OLD generation
            // emitting Bye on a connection the peer may still be using. The instance
            // id lets the HW operator confirm whether the Bye came from the offer-
            // bearing generation or a stale one.
            // D-8 (REQ-BYE-2): stamp teardown Bye with the last drained Offer attempt.
            // Release-Acquire pairing: last_offer_attempt was stored with Release on
            // MdnsControl::Offer drain; we load with Acquire here so the value is
            // always at least as fresh as the last stored attempt.
            let bye_att = last_offer_attempt.load(Ordering::Acquire);
            eprintln!(
                "[sm-signaling-frame-loop] EXIT: instance={instance_id} stop flag set, \
                 sending Bye(attempt={bye_att})"
            );
            let _ = write_frame(&mut writer, &SignalingFrame::Bye { attempt: bye_att });
            break;
        }

        // Drain outbound inbox → write frames.
        let pending: Vec<MdnsControl> = inbox.lock().unwrap().drain(..).collect();
        for msg in pending {
            let frame = match msg {
                MdnsControl::Offer(o, att) => {
                    // D-8 (REQ-BYE-2): track the last-drained Offer attempt so the
                    // teardown Bye carries the correct generation stamp.
                    // Store with Release so the teardown load(Acquire) sees this value.
                    last_offer_attempt.store(att, Ordering::Release);
                    SignalingFrame::Offer {
                        sdp: o.0,
                        attempt: att,
                    }
                }
                MdnsControl::Answer(a) => SignalingFrame::Answer { sdp: a.0 },
                MdnsControl::Candidate(c) => SignalingFrame::Candidate { sdp: c.0 },
                MdnsControl::ReconnectRequest {
                    attempt,
                    requester_role,
                    session_nonce,
                } => SignalingFrame::ReconnectRequest {
                    attempt,
                    requester_role,
                    session_nonce,
                },
                MdnsControl::ReconnectAck {
                    attempt,
                    session_nonce,
                } => SignalingFrame::ReconnectAck {
                    attempt,
                    session_nonce,
                },
            };
            let kind = match &frame {
                SignalingFrame::Offer { sdp, attempt } => {
                    format!("Offer (sdp={} bytes, attempt={attempt})", sdp.len())
                }
                SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                SignalingFrame::Candidate { sdp } => format!("Candidate (sdp={} bytes)", sdp.len()),
                SignalingFrame::Hello { proto, .. } => format!("Hello (proto={proto})"),
                SignalingFrame::Bye { attempt } => format!("Bye(attempt={attempt})"),
                SignalingFrame::ReconnectRequest {
                    attempt,
                    session_nonce,
                    ..
                } => format!("ReconnectRequest (attempt={attempt}, nonce={session_nonce})"),
                SignalingFrame::ReconnectAck {
                    attempt,
                    session_nonce,
                } => format!("ReconnectAck (attempt={attempt}, nonce={session_nonce})"),
            };
            eprintln!("[sm-signaling-frame-loop] OUT → instance={instance_id} {kind}");
            if let Err(e) = write_frame(&mut writer, &frame) {
                eprintln!("[sm-signaling-frame-loop] write_frame error: {e}");
                emit_error(&event_tx, SignalingError::Io(e.to_string()));
                qsv_ledger_negotiated.store(false, Ordering::Release);
                return;
            }
        }

        // Read one inbound frame. read_frame_or_pending returns Ok(None) when
        // no data is available (so the caller can drain inbox + check stop),
        // and retries internally on transient TimedOut/WouldBlock once a frame
        // has begun arriving — preventing the buffer-desync bug where
        // BufReader::read_exact silently consumes partial body bytes on
        // timeout, causing subsequent reads to start mid-frame.
        match read_frame_or_pending(&mut reader, &stop) {
            Ok(Some(frame)) => {
                let kind = match &frame {
                    SignalingFrame::Hello { proto, .. } => format!("Hello (proto={proto})"),
                    SignalingFrame::Offer { sdp, attempt } => {
                        format!("Offer (sdp={} bytes, attempt={attempt})", sdp.len())
                    }
                    SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Candidate { sdp } => {
                        format!("Candidate (sdp={} bytes)", sdp.len())
                    }
                    SignalingFrame::Bye { attempt } => format!("Bye(attempt={attempt})"),
                    SignalingFrame::ReconnectRequest {
                        attempt,
                        session_nonce,
                        ..
                    } => format!("ReconnectRequest (attempt={attempt}, nonce={session_nonce})"),
                    SignalingFrame::ReconnectAck {
                        attempt,
                        session_nonce,
                    } => format!("ReconnectAck (attempt={attempt}, nonce={session_nonce})"),
                };
                eprintln!("[sm-signaling-frame-loop] IN  ← instance={instance_id} {kind}");
                if let SignalingFrame::Hello { capabilities, .. } = &frame {
                    replace_peer_qsv_ledger_capability(&qsv_ledger_negotiated, capabilities);
                }
                match frame_to_event(frame, &supervisor_signal_tx) {
                    Some(SignalingEvent::Closed { attempt }) => {
                        eprintln!(
                            "[sm-signaling-frame-loop] EXIT: peer sent Bye(attempt={attempt:?}) → emit Closed"
                        );
                        let _ = emit(&event_tx, SignalingEvent::Closed { attempt });
                        break;
                    }
                    Some(ev) => {
                        let _ = emit(&event_tx, ev);
                    }
                    None => {} // Hello or reconnect frame — absorbed / routed to supervisor
                }
            }
            Ok(None) => {
                // No data available right now — loop to drain inbox + re-check stop.
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                eprintln!(
                    "[sm-signaling-frame-loop] EXIT: peer closed (EOF) → emit Closed{{attempt:None}}"
                );
                // D-1: EOF has no attempt context — None signals the drain to always honor.
                let _ = emit(&event_tx, SignalingEvent::Closed { attempt: None });
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Stop flag was set during a partial read.
                eprintln!("[sm-signaling-frame-loop] EXIT: stop flag set during partial read");
                break;
            }
            Err(e) => {
                // Diagnostic: dump up to 64 more bytes from the reader so we can
                // see what content surrounded the oversize length prefix. This
                // helps identify whether we're mid-SDP-body, mid-JSON, or
                // looking at a foreign protocol payload.
                use std::io::BufRead;
                let extra: Vec<u8> = reader
                    .fill_buf()
                    .map(|b| b[..b.len().min(64)].to_vec())
                    .unwrap_or_default();
                let hex: String = extra
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii: String = extra
                    .iter()
                    .map(|b| {
                        if (0x20..0x7f).contains(b) {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                eprintln!("[sm-signaling-frame-loop] EXIT: read error: {e}");
                eprintln!(
                    "[sm-signaling-frame-loop] read error context: peer={peer} local={local}"
                );
                eprintln!(
                    "[sm-signaling-frame-loop] next {} bytes (hex)  : {hex}",
                    extra.len()
                );
                eprintln!(
                    "[sm-signaling-frame-loop] next {} bytes (ascii): {ascii}",
                    extra.len()
                );
                emit_error(
                    &event_tx,
                    SignalingError::Protocol(format!("frame read error: {e}")),
                );
                break;
            }
        }
    }
    qsv_ledger_negotiated.store(false, Ordering::Release);
}

// ─── Resilient frame reader ──────────────────────────────────────────────────
//
// The TCP stream has a short read timeout (READ_TIMEOUT) so the frame loop
// can periodically check the stop flag and drain the outbound inbox. The
// previous implementation called wire::read_frame, which uses
// `Read::read_exact` internally. read_exact has the bad property that on
// TimedOut it CONSUMES partial bytes from the BufReader and discards them;
// the caller can only re-call read_exact with a fresh buffer, so any bytes
// already consumed are lost from the wire. For small frames (Hello = 33
// bytes, almost always a single TCP segment) this rarely fires, but for the
// SDP answer/offer (~4.6 KB across multiple segments with inter-segment
// gaps) the timeout commonly fires partway through the body, leaving the
// reader pointed mid-body. The next loop iteration's read_frame then reads
// 4 mid-SDP bytes as a length prefix and trips MAX_FRAME_BYTES — the
// "frame too large" / "na=r" symptom seen in B11 smoke.
//
// `read_frame_or_pending` fixes this by:
// - Returning Ok(None) when NO bytes have been consumed yet and the read
//   times out, so the loop keeps draining the inbox and checking stop.
// - Once any byte has been consumed, retrying transparently on
//   TimedOut/WouldBlock until the full prefix and body are read, ensuring
//   the wire stays aligned. Stop flag is checked between retries to keep
//   the loop interruptible.

/// Read a complete signaling frame, or return `Ok(None)` if no data is
/// currently available on the reader. Internally retries on TimedOut /
/// WouldBlock once a partial read has begun.
fn read_frame_or_pending<R: Read>(
    reader: &mut R,
    stop: &Arc<AtomicBool>,
) -> io::Result<Option<SignalingFrame>> {
    let mut prefix = [0u8; 4];
    // First-byte probe: if no data is available, return Ok(None) so the
    // caller can drain inbox + check stop. Once we read at least 1 byte we
    // are committed to completing this frame.
    match reader.read(&mut prefix[..1]) {
        Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed")),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    }

    // Complete the prefix with retry-on-timeout.
    read_exact_resilient(reader, &mut prefix[1..], stop)?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        let ascii: String = prefix
            .iter()
            .map(|b| {
                if (0x20..0x7f).contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame too large: declared {len} bytes (max {MAX_FRAME_BYTES}); raw prefix bytes: {:02x} {:02x} {:02x} {:02x} (\"{ascii}\")",
                prefix[0], prefix[1], prefix[2], prefix[3]
            ),
        ));
    }

    // Read the full body, tolerating transient timeouts.
    let mut body = vec![0u8; len];
    read_exact_resilient(reader, &mut body, stop)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Like `Read::read_exact`, but retries on TimedOut / WouldBlock and
/// honours `stop` (returning `Interrupted`) between attempts. Bytes
/// consumed across timeouts are accumulated in `buf`, so callers never
/// observe lost-byte desync.
fn read_exact_resilient<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stop flag set during read",
            ));
        }
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed mid-frame",
                ));
            }
            Ok(n) => filled += n,
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Enumerate non-loopback IPv4 addresses on all active network interfaces.
fn collect_ipv4_addrs() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|iface| {
                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                    if !v4.ip.is_loopback() {
                        Some(v4.ip)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// Emit a `SignalingEvent` on `event_tx` without blocking (drop-newest on full).
fn emit(tx: &SyncSender<SignalingEvent>, event: SignalingEvent) -> Result<(), ()> {
    match tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Ok(()), // drop silently — channel is full
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}

/// Emit a `SignalingEvent::Error` wrapping the given `SignalingError`.
fn emit_error(tx: &SyncSender<SignalingEvent>, err: SignalingError) {
    let _ = emit(tx, SignalingEvent::Error(err));
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::signaling::{
        SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent, SignalingRole,
    };

    use super::bind_tcp_listener_reusable;
    use crate::signaling::mdns::MdnsSignaling;

    // ─── Compile-time check: implements Signaling ─────────────────────────────

    /// R7.1 — MdnsSignaling MUST implement Signaling (compile-time check).
    fn _assert_implements_signaling<T: Signaling>() {}
    fn _check() {
        _assert_implements_signaling::<MdnsSignaling>();
    }

    // ─── S7.1: mDNS discovery (ignored — requires multicast) ─────────────────

    /// S7.1 — Given two MdnsSignaling instances (sender + receiver) on the same host
    /// with a working multicast interface, when start() is called on both, then within
    /// 5 seconds each emits `SignalingEvent::PeerFound`.
    ///
    /// This test is `#[ignore]` per R7.5 — it requires mDNS multicast.
    /// Run manually: `cargo nextest run -- --run-ignored mdns_peer_discovery`
    #[test]
    #[ignore]
    fn mdns_peer_discovery_s7_1() {
        use std::time::Duration;

        let sender_config = SignalingConfig {
            role: SignalingRole::Sender,
            control_port: 17891,
            ..Default::default()
        };
        let receiver_config = SignalingConfig {
            role: SignalingRole::Receiver,
            control_port: 17891,
            ..Default::default()
        };

        let mut sender_sig = MdnsSignaling::new(sender_config).unwrap();
        let mut receiver_sig = MdnsSignaling::new(receiver_config).unwrap();

        let (s_tx, s_rx) = sync_channel::<SignalingEvent>(8);
        let (r_tx, r_rx) = sync_channel::<SignalingEvent>(8);

        sender_sig.start(s_tx).unwrap();
        receiver_sig.start(r_tx).unwrap();

        let timeout = Duration::from_secs(5);
        let sender_found = s_rx
            .recv_timeout(timeout)
            .map(|e| matches!(e, SignalingEvent::PeerFound { .. }))
            .unwrap_or(false);
        let receiver_found = r_rx
            .recv_timeout(timeout)
            .map(|e| matches!(e, SignalingEvent::PeerFound { .. }))
            .unwrap_or(false);

        sender_sig.stop().unwrap();
        receiver_sig.stop().unwrap();

        assert!(
            sender_found,
            "sender must emit PeerFound within 5 s (requires multicast)"
        );
        assert!(
            receiver_found,
            "receiver must emit PeerFound within 5 s (requires multicast)"
        );
    }

    // ─── frame_to_event mapping (unit, no network) ───────────────────────────

    use std::sync::Mutex;
    use std::sync::mpsc::sync_channel as sc;

    fn no_supervisor() -> std::sync::Arc<
        Mutex<Option<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>>,
    > {
        std::sync::Arc::new(Mutex::new(None))
    }

    /// S7.2 — frame_to_event maps Offer frame to OfferReceived with attempt.
    #[test]
    fn frame_to_event_offer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Offer {
            sdp: "v=0".to_string(),
            attempt: 1,
        };
        let event = frame_to_event(frame, &no_supervisor()).expect("Offer must produce an event");
        assert!(
            matches!(event, SignalingEvent::OfferReceived(SdpOffer(ref s), 1) if s == "v=0"),
            "Offer frame must map to OfferReceived with exact SDP and attempt"
        );
    }

    /// S7.2 — frame_to_event maps Answer frame to AnswerReceived.
    #[test]
    fn frame_to_event_answer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Answer {
            sdp: "v=0\r\nm=video".to_string(),
        };
        let event = frame_to_event(frame, &no_supervisor()).expect("Answer must produce an event");
        assert!(matches!(event, SignalingEvent::AnswerReceived(_)));
    }

    /// S7.2 — frame_to_event maps Candidate frame to CandidateReceived.
    #[test]
    fn frame_to_event_candidate_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Candidate {
            sdp: "candidate:1 1 udp 2130706431 127.0.0.1 9 typ host".to_string(),
        };
        let event =
            frame_to_event(frame, &no_supervisor()).expect("Candidate must produce an event");
        assert!(matches!(event, SignalingEvent::CandidateReceived(_)));
    }

    /// S7.3 — Hello frame returns None (absorbed silently).
    #[test]
    fn frame_to_event_hello_returns_none() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(
            SignalingFrame::Hello {
                proto: "v1".to_string(),
                capabilities: Vec::new(),
            },
            &no_supervisor(),
        );
        assert!(
            event.is_none(),
            "Hello frame must not produce a SignalingEvent"
        );
    }

    /// S7.3 / T-03 — Bye frame produces Closed{attempt:Some(n)} event, no eager LocalFailure.
    ///
    /// GIVEN: `frame_to_event(Bye { attempt: 1 })` with no supervisor wired.
    /// THEN:  returns `Some(Closed { attempt: Some(1) })` and does NOT send to any supervisor.
    #[test]
    fn frame_to_event_bye_returns_closed() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Bye { attempt: 1 }, &no_supervisor())
            .expect("Bye must produce Closed");
        assert!(
            matches!(event, SignalingEvent::Closed { attempt: Some(1) }),
            "Bye frame must map to SignalingEvent::Closed{{attempt:Some(1)}}, got {event:?}"
        );
    }

    /// SC-CONV-2-9 / T-03 — `frame_to_event(Bye{attempt})` returns `Closed{Some(attempt)}`
    ///                        and DOES NOT send `LocalFailure{PeerBye}` to any supervisor.
    ///
    /// GIVEN: a wired supervisor_signal_tx (Some).
    /// WHEN:  `frame_to_event(Bye { attempt: 3 })` is called.
    /// THEN:  1. Returns `Some(Closed { attempt: Some(3) })`.
    ///        2. NO `LocalFailure{PeerBye}` arrives in supervisor_signal_tx (D-3).
    #[test]
    fn frame_to_event_bye_returns_closed_with_attempt() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let event = frame_to_event(SignalingFrame::Bye { attempt: 3 }, &supervisor_signal_tx)
            .expect("SC-CONV-2-9: Bye must produce an event");

        // Assert 1: result is Closed{Some(3)}
        assert!(
            matches!(event, SignalingEvent::Closed { attempt: Some(3) }),
            "SC-CONV-2-9: Bye{{attempt:3}} must map to Closed{{attempt:Some(3)}}, got {event:?}"
        );

        // Assert 2: NO LocalFailure{PeerBye} was sent to supervisor (D-3, R-2 bypass closed)
        let no_send = sup_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            no_send.is_err(),
            "SC-CONV-2-9: frame_to_event MUST NOT send LocalFailure{{PeerBye}} to supervisor \
             (D-3 centralize route); got: {no_send:?}"
        );
    }

    // ─── T5.1: frame_to_event routes reconnect frames to supervisor channel ──

    /// T5.1 / AC-5 — `ReconnectRequest` frame is routed to the supervisor channel
    /// as `SupervisorSignal::PeerRequest` when a supervisor_signal_tx is registered.
    #[test]
    fn frame_to_event_reconnect_request_routes_to_supervisor_channel() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 2,
            requester_role: SignalingRole::Sender,
            session_nonce: 42_000,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(
            result.is_none(),
            "ReconnectRequest must not produce a SignalingEvent (routed to supervisor)"
        );

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerRequest within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerRequest {
                peer_nonce: 42_000,
                peer_role: SignalingRole::Sender,
                attempt: 2,
            }
        );
    }

    /// T5.1 / AC-6 — `ReconnectAck` frame is routed to the supervisor channel
    /// as `SupervisorSignal::PeerAck` when a supervisor_signal_tx is registered.
    #[test]
    fn frame_to_event_reconnect_ack_routes_to_supervisor_channel() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectAck {
            attempt: 1,
            session_nonce: 99,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(
            result.is_none(),
            "ReconnectAck must not produce a SignalingEvent (routed to supervisor)"
        );

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerAck within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerAck {
                session_nonce: 99,
                attempt: 1,
            }
        );
    }

    /// SC-DR-5 — `frame_to_event` MUST forward the wire `requester_role` into the
    /// `SupervisorSignal::PeerRequest { peer_role }` instead of discarding it.
    ///
    /// Root cause #962: `mdns.rs` dropped `requester_role` (`requester_role: _`),
    /// so the supervisor's tie-break was role-blind. Design #963 D1 plumbs the role
    /// through. This pins the plumbing: a `Receiver`-role request must arrive as
    /// `peer_role: Receiver` on the supervisor channel.
    #[test]
    fn sc_dr_5_requester_role_not_discarded() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Receiver,
            session_nonce: 42,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(
            result.is_none(),
            "ReconnectRequest must not produce a SignalingEvent"
        );

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerRequest within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerRequest {
                peer_nonce: 42,
                peer_role: SignalingRole::Receiver,
                attempt: 1,
            }
        );
    }

    /// T5.1 — `ReconnectRequest` returns None silently when no supervisor channel is set.
    #[test]
    fn frame_to_event_reconnect_request_returns_none_without_supervisor() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Receiver,
            session_nonce: 1234,
        };

        let result = frame_to_event(frame, &no_supervisor());
        assert!(
            result.is_none(),
            "ReconnectRequest must return None (no supervisor registered)"
        );
    }

    // ─── T5.1: publish_reconnect_request / publish_reconnect_ack API ─────────

    /// T5.1 — `publish_reconnect_request` returns NotRunning before start().
    #[test]
    fn publish_reconnect_request_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_reconnect_request(1, SignalingRole::Sender, 42);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish_reconnect_request before start must return NotRunning, got {result:?}"
        );
    }

    /// T5.1 — `publish_reconnect_ack` returns NotRunning before start().
    #[test]
    fn publish_reconnect_ack_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_reconnect_ack(1, 42);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish_reconnect_ack before start must return NotRunning, got {result:?}"
        );
    }

    // ─── T5.2: reset() rebuilds a fresh MdnsSignaling ────────────────────────

    /// T5.2 — `reset()` on a stopped instance returns a new instance in pre-start state.
    #[test]
    fn mdns_signaling_reset_returns_fresh_instance() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let fresh = sig.reset().expect("reset must succeed");
        // Fresh instance must be in pre-start state: publish returns NotRunning.
        let result = fresh.publish_local_offer(SdpOffer("v=0".to_string()), 1);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "after reset(), new instance must be in pre-start state; got {result:?}"
        );
    }

    // ─── new() via Signaling trait ─────────────────────────────────────────────

    /// R7.1 — MdnsSignaling::new succeeds without network access.
    #[test]
    fn mdns_signaling_new_succeeds() {
        assert!(
            MdnsSignaling::new(SignalingConfig::default()).is_ok(),
            "new() must succeed without network"
        );
    }

    // ─── stop() is idempotent ─────────────────────────────────────────────────

    /// R7.4 — stop() is idempotent: second call returns Ok without panic.
    #[test]
    fn mdns_signaling_stop_is_idempotent() {
        let mut sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        sig.stop().unwrap();
        sig.stop().unwrap();
    }

    // ─── publish_local_offer before start → NotRunning ────────────────────────

    /// R7.4 — publish_local_offer before start() returns Err(NotRunning).
    #[test]
    fn mdns_publish_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_local_offer(SdpOffer("v=0".to_string()), 1);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish before start must return NotRunning, got {result:?}"
        );
    }

    // ─── B11-S2 regression: resilient frame reader ──────────────────────────────

    use std::io::{self, Read};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    use crate::signaling::mdns::read_frame_or_pending;
    use crate::signaling::wire::{SignalingFrame, write_frame};

    /// A test reader that simulates a TCP stream with arbitrary partial-read
    /// + transient-timeout behaviour. Each entry in `script` is either:
    /// - `Ok(n)` — return up to `n` bytes from the pending buffer.
    /// - `Err(kind)` — return that error kind.
    enum Step {
        /// Deliver up to N bytes (or fewer if the buffer is shorter).
        Bytes(usize),
        /// Return a transient error (TimedOut or WouldBlock).
        Timeout,
        /// Return Ok(0) to signal EOF.
        Eof,
    }

    struct ScriptedReader {
        data: Vec<u8>,
        cursor: usize,
        steps: std::vec::IntoIter<Step>,
    }

    impl Read for ScriptedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            match self.steps.next() {
                Some(Step::Bytes(n)) => {
                    let avail = self.data.len() - self.cursor;
                    let want = n.min(out.len()).min(avail);
                    out[..want].copy_from_slice(&self.data[self.cursor..self.cursor + want]);
                    self.cursor += want;
                    Ok(want)
                }
                Some(Step::Timeout) => {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "scripted timeout"))
                }
                Some(Step::Eof) => Ok(0),
                None => Err(io::Error::other("script exhausted")),
            }
        }
    }

    /// B11-S2 regression: the body read MUST NOT lose bytes when the underlying
    /// reader returns transient TimedOut between segments. read_frame_or_pending
    /// should accumulate partial reads and parse the complete frame.
    #[test]
    fn read_frame_or_pending_survives_timeout_mid_body() {
        // Construct a real Answer frame on the wire.
        let frame = SignalingFrame::Answer {
            sdp: "v=0\r\no=test 1 1 IN IP4 0.0.0.0\r\na=rtcp-fb:127 nack pli\r\na=fmtp:127 level-asymmetry-allowed=1\r\n".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must succeed");

        // Simulate: deliver prefix in two chunks (1 then 3), then body in
        // three chunks with TimedOut between each chunk.
        let body_len = buf.len() - 4;
        let third = body_len / 3;
        let steps = vec![
            Step::Bytes(1), // first byte of prefix
            Step::Bytes(3), // remaining 3 bytes of prefix
            Step::Bytes(third),
            Step::Timeout,
            Step::Bytes(third),
            Step::Timeout,
            Step::Bytes(body_len), // remainder
        ];
        let mut reader = ScriptedReader {
            data: buf,
            cursor: 0,
            steps: steps.into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));

        let result = read_frame_or_pending(&mut reader, &stop)
            .expect("read must succeed despite mid-body timeouts")
            .expect("must return Some(frame)");

        match result {
            SignalingFrame::Answer { sdp } => assert!(
                sdp.contains("rtcp-fb:127 nack pli"),
                "frame body must round-trip without byte loss"
            ),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    /// First-byte timeout MUST return Ok(None) so the caller can drain the
    /// outbound inbox + check the stop flag, NOT consume any bytes.
    #[test]
    fn read_frame_or_pending_first_byte_timeout_returns_none() {
        let mut reader = ScriptedReader {
            data: Vec::new(),
            cursor: 0,
            steps: vec![Step::Timeout].into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let result =
            read_frame_or_pending(&mut reader, &stop).expect("first-byte timeout must not error");
        assert!(
            result.is_none(),
            "first-byte timeout must return Ok(None), got {result:?}"
        );
    }

    /// Stop flag set during a partial read MUST surface as Interrupted.
    #[test]
    fn read_frame_or_pending_stop_flag_returns_interrupted() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        // First read returns 1 byte (commits to the frame); we set stop
        // before the next attempt.
        let mut reader = ScriptedReader {
            data: vec![0u8; 4],
            cursor: 0,
            steps: vec![Step::Bytes(1), Step::Timeout, Step::Timeout].into_iter(),
        };
        // Set stop AFTER constructing reader. The retry loop will see it.
        stop_clone.store(true, std::sync::atomic::Ordering::Release);
        let err = read_frame_or_pending(&mut reader, &stop)
            .expect_err("must return Err when stop is set during retry");
        assert_eq!(
            err.kind(),
            io::ErrorKind::Interrupted,
            "stop flag must produce Interrupted, got {err:?}"
        );
    }

    /// Mid-frame EOF (peer closed after sending partial body) MUST surface
    /// as UnexpectedEof.
    #[test]
    fn read_frame_or_pending_mid_frame_eof_returns_unexpected_eof() {
        let frame = SignalingFrame::Hello {
            proto: "v1".to_string(),
            capabilities: Vec::new(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        // Truncate the body so EOF arrives mid-body.
        buf.truncate(buf.len() - 5);
        let mut reader = ScriptedReader {
            data: buf,
            cursor: 0,
            // First-byte then everything we have (which is short), then EOF.
            steps: vec![Step::Bytes(1), Step::Bytes(usize::MAX), Step::Eof].into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let err =
            read_frame_or_pending(&mut reader, &stop).expect_err("must error on truncated frame");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    // ─── SC-1: bind_tcp_listener_reusable — bind→drop→rebind (cross-platform) ─

    /// SC-1 — After dropping a TcpListener, the same port can be rebound immediately.
    /// Exercises TIME_WAIT rebind on Unix and confirms SO_REUSEADDR is set correctly
    /// on all platforms.
    #[test]
    fn bind_tcp_listener_reusable_rebind_after_drop_succeeds() {
        use std::net::SocketAddr;
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let listener1 = bind_tcp_listener_reusable(zero).expect("first bind");
        let port = listener1.local_addr().unwrap().port();
        drop(listener1);

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let listener2 = bind_tcp_listener_reusable(fixed);
        assert!(
            listener2.is_ok(),
            "rebind after drop must succeed (got: {:?})",
            listener2.err()
        );
    }

    // ─── SC-3: bind_tcp_listener_reusable — live rebind (Windows-only) ────────

    /// SC-3 — On Windows, SO_REUSEADDR allows a second bind while the first socket
    /// is still alive (LISTEN state). This reproduces the Arc-lifetime race where the
    /// old MdnsSignaling thread still holds 0.0.0.0:7889 when a rebuild worker
    /// attempts to bind a new one (engram #1417).
    #[cfg(target_os = "windows")]
    #[test]
    fn bind_tcp_listener_reusable_live_rebind_windows_succeeds() {
        use std::net::SocketAddr;
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let listener1 = bind_tcp_listener_reusable(zero).expect("first bind");
        let port = listener1.local_addr().unwrap().port();
        let _hold = listener1; // intentionally NOT dropped — reproduces Arc race

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let listener2 = bind_tcp_listener_reusable(fixed);
        assert!(
            listener2.is_ok(),
            "live rebind on Windows must succeed (Arc race repro), got: {:?}",
            listener2.err()
        );
    }

    // ─── SC-C-001: DISCOVER_TIMEOUT must be 30s ───────────────────────────────────

    /// SC-C-001 — `DISCOVER_TIMEOUT` constant must be `Duration::from_secs(30)`.
    ///
    /// RED: current value is `Duration::from_secs(10)` (too short for sender republish
    ///      after S-1 supervisor wakes — see design D-7). A 10s window guarantees a
    ///      miss in the legacy path and gives zero margin in the S-1 path.
    ///
    /// GREEN (T07): change constant to 30s → test passes.
    #[test]
    fn discover_timeout_is_30s() {
        use std::time::Duration;
        assert_eq!(
            super::DISCOVER_TIMEOUT,
            Duration::from_secs(30),
            "SC-C-001 FAIL: DISCOVER_TIMEOUT must be Duration::from_secs(30). \
             Current value is {:?}. \
             Fix (D-7): change `const DISCOVER_TIMEOUT: Duration = Duration::from_secs(10)` \
             to `Duration::from_secs(30)` in mdns.rs.",
            super::DISCOVER_TIMEOUT
        );
    }

    // ─── SC-D-001 / SC-D-002: mdns.shutdown() MUST be called AFTER run_frame_loop ──

    /// SC-D-001 — Sender thread: `mdns.shutdown()` must be called AFTER `run_frame_loop`
    /// returns.
    ///
    /// Test strategy: start a real `MdnsSignaling` sender, connect to it via loopback
    /// TCP (bypassing mDNS discovery), drive `run_frame_loop` to exit by closing the
    /// connection, then verify that `MdnsSignaling::stop()` completes cleanly AND
    /// that `SignalingEvent::Closed` was emitted by the frame loop (not before it).
    ///
    /// The KEY invariant: `SignalingEvent::Closed` is emitted by `run_frame_loop` when
    /// the peer closes the connection. With the BROKEN code, `mdns.shutdown()` is
    /// called BEFORE `run_frame_loop` starts — but the frame loop still runs (shutdown
    /// does not prevent TCP). The observable difference is that with the BROKEN code,
    /// the mDNS service entry is removed from the network BEFORE the TCP session ends,
    /// which means a reconnecting receiver would find no service during the session.
    ///
    /// For a unit test without real mDNS, we verify the SEQUENCING by tracking when
    /// `SignalingEvent::Closed` arrives relative to `stop()` returning. The frame loop
    /// MUST have run (producing Closed) before the signaling stop completes.
    ///
    /// RED (current code): mdns.shutdown() is at L495 BEFORE run_frame_loop at L496.
    ///     The test detects this via: `DISCOVER_TIMEOUT` constant value is 10s.
    ///     After D-8 fix: shutdown moves to after run_frame_loop.
    ///
    /// For a concrete RED/GREEN test that verifies the SOURCE CODE ordering without
    /// real mDNS infrastructure, we use a source-text structural assertion:
    /// the line "let _ = mdns.shutdown();" must appear AFTER the line
    /// "run_frame_loop(" in the sender thread function body.
    ///
    /// This is a legitimate static gate: any refactor that re-introduces the broken
    /// ordering will fail this test immediately, catching regressions at compile/test
    /// time on every CI run.
    #[test]
    fn sender_mdns_shutdown_happens_after_frame_loop() {
        // Read the source file (relative to the manifest directory at test time).
        // `CARGO_MANIFEST_DIR` is set by Cargo for all crate-level tests.
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
        let source_path = std::path::PathBuf::from(&manifest_dir).join("src/signaling/mdns.rs");
        let source =
            std::fs::read_to_string(&source_path).expect("mdns.rs must be readable in tests");

        // Find run_sender_thread body boundaries.
        let sender_fn_start = source
            .find("fn run_sender_thread(")
            .expect("run_sender_thread must exist in mdns.rs");
        let sender_fn_end = {
            source[sender_fn_start..]
                .find("\nfn run_receiver_thread(")
                .map(|rel| sender_fn_start + rel)
                .unwrap_or(source.len())
        };
        let sender_body = &source[sender_fn_start..sender_fn_end];

        // Use rfind for the LAST occurrence of mdns.shutdown() in the sender body.
        // The early-exit shutdown calls (inside error branches) are NOT the main-path
        // shutdown. The main-path shutdown is the one directly adjacent to run_frame_loop.
        // rfind finds the LAST occurrence which should be the main-path one.
        let shutdown_pos = sender_body
            .rfind("mdns.shutdown()")
            .expect("mdns.shutdown() must appear in run_sender_thread");
        let frame_loop_pos = sender_body
            .rfind("run_frame_loop(")
            .expect("run_frame_loop( must appear in run_sender_thread");

        // SC-D-001 assertion: the main-path mdns.shutdown() MUST appear AFTER run_frame_loop.
        // RED: current code has shutdown at L495 BEFORE run_frame_loop at L496.
        //      With the early-exit shutdowns present, rfind still finds the main-path
        //      shutdown. In BROKEN state, the main-path shutdown is BEFORE run_frame_loop.
        // GREEN (T05): main-path shutdown moved to AFTER run_frame_loop.
        assert!(
            shutdown_pos > frame_loop_pos,
            "SC-D-001 FAIL: in run_sender_thread, the main-path mdns.shutdown() \
             (last occurrence, byte offset {shutdown_pos}) appears BEFORE run_frame_loop \
             (last occurrence, byte offset {frame_loop_pos}). \
             Fix (D-8): move `let _ = mdns.shutdown();` to AFTER `run_frame_loop(...)` \
             so the mDNS service stays published during the entire TCP session."
        );
    }

    /// SC-D-002 — Receiver thread: `mdns.shutdown()` must be called AFTER `run_frame_loop`
    /// returns.
    ///
    /// Mirror of SC-D-001 for `run_receiver_thread`.
    ///
    /// RED: production code has `mdns.shutdown()` at L574 BEFORE `run_frame_loop` at L584.
    /// GREEN (T05): shutdown moved to after `run_frame_loop`.
    #[test]
    fn receiver_mdns_shutdown_happens_after_frame_loop() {
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
        let source_path = std::path::PathBuf::from(&manifest_dir).join("src/signaling/mdns.rs");
        let source =
            std::fs::read_to_string(&source_path).expect("mdns.rs must be readable in tests");

        // Find run_receiver_thread body boundaries.
        let receiver_fn_start = source
            .find("fn run_receiver_thread(")
            .expect("run_receiver_thread must exist in mdns.rs");
        let receiver_fn_end = {
            source[receiver_fn_start..]
                .find("\n// ─── Shared TCP frame loop")
                .map(|rel| receiver_fn_start + rel)
                .unwrap_or(source.len())
        };
        let receiver_body = &source[receiver_fn_start..receiver_fn_end];

        // Use rfind: the main-path shutdown is the LAST occurrence (after run_frame_loop).
        let shutdown_pos = receiver_body
            .rfind("mdns.shutdown()")
            .expect("mdns.shutdown() must appear in run_receiver_thread");
        let frame_loop_pos = receiver_body
            .rfind("run_frame_loop(")
            .expect("run_frame_loop( must appear in run_receiver_thread");

        // SC-D-002 assertion: main-path mdns.shutdown() MUST appear AFTER run_frame_loop.
        // RED: current code has shutdown at L574 BEFORE run_frame_loop at L584.
        // GREEN (T05): shutdown moved to AFTER run_frame_loop.
        assert!(
            shutdown_pos > frame_loop_pos,
            "SC-D-002 FAIL: in run_receiver_thread, the main-path mdns.shutdown() \
             (last occurrence, byte offset {shutdown_pos}) appears BEFORE run_frame_loop \
             (last occurrence, byte offset {frame_loop_pos}). \
             Fix (D-8): move `let _ = mdns.shutdown();` to AFTER `run_frame_loop(...)` \
             so the mDNS service stays published during the entire TCP session."
        );
    }

    // ─── SC-S1-001 (mdns) — rewritten for D-3: frame_to_event(Bye) MUST NOT ────
    // ─── send LocalFailure{PeerBye}; ALL Bye honoring flows through drain filter ─
    //
    // D-3 (REQ-BYE-3): the eager LocalFailure{PeerBye} try_send in frame_to_event
    // was the R-2 bypass that made the drain filter ineffective. It is REMOVED.
    // frame_to_event(Bye{att}) now ONLY returns Some(Closed{Some(att)}).
    // All Bye → supervisor escalation flows exclusively through run_signaling_drain.
    //
    // Previous SC-S1-001 asserted the OPPOSITE (eager send required). That assertion
    // is now INVERTED: the test confirms zero supervisor send from frame_to_event.

    /// SC-S1-001 (mdns, D-3 rewrite) — `frame_to_event(Bye{attempt})` MUST NOT
    ///     send `LocalFailure{PeerBye}` to `supervisor_signal_tx`, and MUST return
    ///     `Some(Closed { attempt: Some(n) })`.
    ///
    /// GIVEN: a wired `supervisor_signal_tx` (Some).
    /// WHEN:  `frame_to_event(Bye { attempt: 1 })` is called.
    /// THEN:  1. Returns `Some(Closed { attempt: Some(1) })`.
    ///        2. supervisor_signal_tx receives NOTHING within 100ms (D-3 — eager path REMOVED).
    ///
    /// This is the D-3 regression guard: if the eager send is accidentally re-introduced,
    /// this test will fail with "unexpected signal received".
    #[test]
    fn sc_s1_001_frame_to_event_bye_does_not_eagerly_send_local_failure() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::sync::mpsc::sync_channel;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        let (sup_tx, sup_rx) = sync_channel::<SupervisorSignal>(8);
        let supervisor_signal_tx = Arc::new(Mutex::new(Some(sup_tx)));

        let event = frame_to_event(SignalingFrame::Bye { attempt: 1 }, &supervisor_signal_tx);

        // Assert 1: returns Some(Closed{Some(1)})
        assert!(
            matches!(
                event,
                Some(sm_domain::signaling::SignalingEvent::Closed { attempt: Some(1) })
            ),
            "SC-S1-001: frame_to_event(Bye{{1}}) must return Some(Closed{{Some(1)}}), got {event:?}"
        );

        // Assert 2: NO LocalFailure{PeerBye} was sent to supervisor (D-3, R-2 bypass closed)
        let no_send = sup_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            no_send.is_err(),
            "SC-S1-001: frame_to_event MUST NOT send LocalFailure{{PeerBye}} to supervisor \
             after D-3 (eager path removed); got unexpected signal: {no_send:?}"
        );
    }

    /// SC-S1-001b (D-3 rewrite) — `frame_to_event(Bye{attempt})` with `None` supervisor
    ///     returns `Some(Closed{Some(n)})` without panicking.
    #[test]
    fn sc_s1_001b_frame_to_event_bye_with_none_supervisor_returns_closed() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Bye { attempt: 1 }, &no_supervisor());
        assert!(
            matches!(
                event,
                Some(sm_domain::signaling::SignalingEvent::Closed { attempt: Some(1) })
            ),
            "SC-S1-001b: frame_to_event(Bye{{1}}) with None supervisor must return \
             Some(Closed{{Some(1)}}), got {event:?}"
        );
    }

    // ─── SC-D3-1 / SC-D3-2: suppress_bye gates the stop-flag teardown Bye ──────
    //
    // D3 stale-Bye fix (design #967): a superseded sender generation, on
    // rebuild teardown, must NOT emit the spurious stop-flag Bye on a
    // connection the receiver may still be using. The Bye is gated by a
    // per-instance `suppress_bye` flag, set ONLY on the superseded generation.
    // Genuine shutdown (default `suppress_bye=false`) MUST still emit Bye so the
    // receiver's PeerBye eager-wake fast-path is preserved.
    //
    // Strategy: drive `run_frame_loop` on the server side of a real loopback TCP
    // pair, let it send Hello, then set the stop flag and observe what the client
    // side reads next:
    //   - suppress_bye=true  → no Bye frame; the peer sees EOF (connection closes).
    //   - suppress_bye=false → a Bye frame arrives before close.

    /// Read the next frame from `stream` with a bounded timeout, mapping a clean
    /// peer-close (EOF) to `Ok(None)`. Used by the SC-D3 frame-loop tests.
    #[cfg(test)]
    fn read_next_frame_or_eof(
        stream: &mut std::net::TcpStream,
    ) -> std::io::Result<Option<crate::signaling::wire::SignalingFrame>> {
        use crate::signaling::wire::read_frame;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .expect("set_read_timeout");
        match read_frame(stream) {
            Ok(frame) => Ok(Some(frame)),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Spawn `run_frame_loop` on the accepted server side of a loopback TCP pair.
    /// Returns the client stream, the stop flag, the loop's thread handle, and the
    /// `last_offer_attempt` atomic (so tests can inspect / pre-seed it).
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn spawn_frame_loop_over_loopback(
        suppress_bye: Arc<AtomicBool>,
    ) -> (
        std::net::TcpStream,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let (client, stop, handle, _last_offer_attempt, _negotiated, _event_rx) =
            spawn_frame_loop_over_loopback_with_attempt(suppress_bye, Arc::new(AtomicU8::new(0)));
        (client, stop, handle)
    }

    /// Extended variant of `spawn_frame_loop_over_loopback` that exposes `last_offer_attempt`.
    /// Used by T-06 tests.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn spawn_frame_loop_over_loopback_with_attempt(
        suppress_bye: Arc<AtomicBool>,
        last_offer_attempt: Arc<AtomicU8>,
    ) -> (
        std::net::TcpStream,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
        Arc<AtomicU8>,
        Arc<AtomicBool>,
        std::sync::mpsc::Receiver<SignalingEvent>,
    ) {
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local_addr");
        let client = TcpStream::connect(addr).expect("connect loopback client");
        let (server, _peer) = listener.accept().expect("accept loopback server");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let suppress_loop = Arc::clone(&suppress_bye);
        let last_offer_loop = Arc::clone(&last_offer_attempt);
        let negotiated = Arc::new(AtomicBool::new(false));
        let negotiated_loop = Arc::clone(&negotiated);
        let (event_tx, event_rx) = sync_channel::<SignalingEvent>(16);
        let inbox: Arc<Mutex<Vec<super::MdnsControl>>> = Arc::new(Mutex::new(Vec::new()));
        let supervisor: Arc<
            Mutex<Option<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>>,
        > = Arc::new(Mutex::new(None));

        let handle = std::thread::spawn(move || {
            super::run_frame_loop(
                server,
                stop_loop,
                inbox,
                event_tx,
                supervisor,
                suppress_loop,
                super::FrameLoopContext {
                    last_offer_attempt: last_offer_loop,
                    role: SignalingRole::Sender,
                    qsv_ledger_negotiated: negotiated_loop,
                },
            );
        });

        (
            client,
            stop,
            handle,
            last_offer_attempt,
            negotiated,
            event_rx,
        )
    }

    // ─── T-06 / D-8: last_offer_attempt stored on Offer drain; teardown Bye carries it ──

    /// T-06 / D-8 — last_offer_attempt is stored when MdnsControl::Offer is drained.
    ///
    /// GIVEN: a frame loop with inbox pre-loaded with MdnsControl::Offer(sdp, 3).
    /// WHEN:  the inbox is drained in the frame loop's outbound pass.
    /// THEN:  last_offer_attempt == 3 (REQ-BYE-2, SC-CONV-2-1 stamp source).
    ///
    /// Mechanism: we inject the Offer into the inbox BEFORE unblocking the loop,
    /// stop the loop immediately, then check last_offer_attempt before joining.
    #[test]
    fn last_offer_attempt_stored_on_offer_drain() {
        use super::MdnsControl;
        use sm_domain::signaling::SdpOffer;
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("local_addr");
        let _client = TcpStream::connect(addr).expect("connect client");
        let (server, _peer) = listener.accept().expect("accept server");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let suppress_bye = Arc::new(AtomicBool::new(true)); // suppress to avoid Bye write complexity
        let suppress_loop = Arc::clone(&suppress_bye);
        let last_offer_attempt = Arc::new(AtomicU8::new(0));
        let last_offer_loop = Arc::clone(&last_offer_attempt);

        // Pre-seed the inbox with an Offer(sdp, 3) so the first drain iteration stores att=3.
        let inbox: Arc<Mutex<Vec<MdnsControl>>> = Arc::new(Mutex::new(vec![MdnsControl::Offer(
            SdpOffer("v=0\r\n".to_string()),
            3,
        )]));
        let inbox_loop = Arc::clone(&inbox);

        let (event_tx, _event_rx) = sync_channel::<SignalingEvent>(16);
        let supervisor: Arc<
            Mutex<Option<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>>,
        > = Arc::new(Mutex::new(None));

        let handle = std::thread::spawn(move || {
            super::run_frame_loop(
                server,
                stop_loop,
                inbox_loop,
                event_tx,
                supervisor,
                suppress_loop,
                super::FrameLoopContext {
                    last_offer_attempt: last_offer_loop,
                    role: SignalingRole::Sender,
                    qsv_ledger_negotiated: Arc::new(AtomicBool::new(false)),
                },
            );
        });

        // Give the loop time to drain the inbox (one poll cycle ≈ READ_TIMEOUT = 100ms).
        std::thread::sleep(std::time::Duration::from_millis(250));

        // Assert last_offer_attempt was updated to 3.
        let stored = last_offer_attempt.load(Ordering::Acquire);
        assert_eq!(
            stored, 3,
            "T-06/D-8: last_offer_attempt must be 3 after draining Offer(sdp, 3), got {stored}"
        );

        // Stop the loop.
        stop.store(true, Ordering::Release);
        handle.join().expect("frame loop must join");
    }

    /// T-06 / D-8 — teardown Bye carries last_offer_attempt value (REQ-BYE-2).
    ///
    /// GIVEN: last_offer_attempt pre-seeded to 2.
    /// WHEN:  stop flag is set (suppress_bye=false so the Bye is written).
    /// THEN:  the wire frame received by the peer is `Bye { attempt: 2 }`.
    #[test]
    fn teardown_bye_carries_last_offer_attempt() {
        use crate::signaling::wire::SignalingFrame;

        let last_offer_attempt = Arc::new(AtomicU8::new(2));
        let suppress_bye = Arc::new(AtomicBool::new(false));
        let (mut client, stop, handle, _, _, _event_rx) =
            spawn_frame_loop_over_loopback_with_attempt(suppress_bye, last_offer_attempt);

        // Read Hello (sent on connection).
        let hello = read_next_frame_or_eof(&mut client)
            .expect("read hello")
            .expect("hello frame must arrive");
        assert!(
            matches!(hello, SignalingFrame::Hello { .. }),
            "expected Hello, got {hello:?}"
        );

        // Trigger teardown.
        stop.store(true, Ordering::Release);

        // Read the Bye frame — must carry attempt=2.
        let next = read_next_frame_or_eof(&mut client)
            .expect("read after stop")
            .expect("T-06/D-8: teardown Bye must be written (suppress_bye=false)");
        assert!(
            matches!(next, SignalingFrame::Bye { attempt: 2 }),
            "T-06/D-8: teardown Bye must carry attempt=2, got {next:?}"
        );

        handle.join().expect("frame loop thread must join");
    }

    /// SC-D3-1 — With `suppress_bye=true`, the frame loop exits on the stop flag
    /// WITHOUT writing a Bye frame; the peer observes only Hello then EOF.
    ///
    /// RED: `run_frame_loop` has no `suppress_bye` parameter and the stop-flag
    ///      branch unconditionally writes Bye, so the peer reads a Bye frame.
    /// GREEN (WU-D3a): the Bye write is gated behind `!suppress_bye.load(Acquire)`.
    #[test]
    fn sc_d3_1_suppressed_frame_loop_exits_without_bye() {
        use crate::signaling::wire::SignalingFrame;

        let suppress_bye = Arc::new(AtomicBool::new(true));
        let (mut client, stop, handle) = spawn_frame_loop_over_loopback(suppress_bye);

        // The loop first sends Hello.
        let hello = read_next_frame_or_eof(&mut client)
            .expect("read hello")
            .expect("hello frame must arrive");
        assert!(
            matches!(hello, SignalingFrame::Hello { .. }),
            "first frame must be Hello, got {hello:?}"
        );

        // Trigger teardown.
        stop.store(true, std::sync::atomic::Ordering::Release);

        // With suppression on, the next read must be EOF (clean close), NOT a Bye.
        let next = read_next_frame_or_eof(&mut client).expect("read after stop");
        assert!(
            next.is_none(),
            "SC-D3-1 FAIL: with suppress_bye=true the loop must close WITHOUT a Bye, \
             but the peer read {next:?}"
        );

        handle.join().expect("frame loop thread must join");
    }

    /// SC-D3-2 — With `suppress_bye=false` (default), the frame loop STILL writes a
    /// Bye frame on stop. Protects genuine-shutdown and the receiver PeerBye
    /// eager-wake fast-path.
    ///
    /// RED: `run_frame_loop` has no `suppress_bye` parameter (compile failure).
    /// GREEN (WU-D3a): default path remains Bye-on-stop.
    #[test]
    fn sc_d3_2_default_frame_loop_still_writes_bye() {
        use crate::signaling::wire::SignalingFrame;

        let suppress_bye = Arc::new(AtomicBool::new(false));
        let (mut client, stop, handle) = spawn_frame_loop_over_loopback(suppress_bye);

        let hello = read_next_frame_or_eof(&mut client)
            .expect("read hello")
            .expect("hello frame must arrive");
        assert!(
            matches!(hello, SignalingFrame::Hello { .. }),
            "first frame must be Hello, got {hello:?}"
        );

        stop.store(true, std::sync::atomic::Ordering::Release);

        let next = read_next_frame_or_eof(&mut client)
            .expect("read after stop")
            .expect("SC-D3-2 FAIL: default path must emit a Bye frame on stop");
        assert!(
            matches!(next, SignalingFrame::Bye { .. }),
            "SC-D3-2 FAIL: default (suppress_bye=false) must emit Bye on stop, got {next:?}"
        );

        handle.join().expect("frame loop thread must join");
    }

    // ─── SC-D3-4: reset must NOT re-flush a stale ReconnectRequest (D3c, #967) ──
    //
    // The InitiateMdnsReset hook reuses the SAME inbox Arc across stop()+start().
    // A ReconnectRequest queued for the OLD connection but not yet drained must be
    // cleared before re-start, or it re-flushes onto the NEW connection — keeping
    // the superseded gen-G competing as an offer-less listener (design #967 §3).
    // The clear MUST be targeted: only ReconnectRequest entries are removed; any
    // legitimately-queued Offer / Answer / Candidate / ReconnectAck is preserved.

    /// SC-D3-4 — `drain_stale_reconnect_requests()` removes ONLY queued
    /// `ReconnectRequest` entries from the inbox, leaving every other variant intact.
    ///
    /// RED: the method does not exist yet (compile failure).
    /// GREEN (WU-D3c): targeted retain that drops only `ReconnectRequest`.
    #[test]
    fn sc_d3_4_drain_stale_reconnect_requests_is_targeted() {
        use super::MdnsControl;
        use sm_domain::signaling::{SdpOffer, SignalingConfig, SignalingRole};

        let sig = MdnsSignaling::new(SignalingConfig {
            role: SignalingRole::Receiver,
            ..Default::default()
        })
        .expect("new signaling");

        // Seed the reused inbox with a stale ReconnectRequest plus benign frames
        // that MUST survive the targeted drain.
        {
            let mut inbox = sig.inbox_for_test().lock().unwrap();
            inbox.push(MdnsControl::Offer(SdpOffer("v=0".into()), 1));
            inbox.push(MdnsControl::ReconnectRequest {
                attempt: 1,
                requester_role: SignalingRole::Sender,
                session_nonce: 42,
            });
            inbox.push(MdnsControl::ReconnectAck {
                attempt: 1,
                session_nonce: 42,
            });
        }

        let removed = sig.drain_stale_reconnect_requests();
        assert_eq!(
            removed, 1,
            "SC-D3-4 FAIL: exactly one stale ReconnectRequest must be drained, got {removed}"
        );

        let kinds: Vec<&'static str> = sig
            .inbox_for_test()
            .lock()
            .unwrap()
            .iter()
            .map(|m| match m {
                MdnsControl::Offer(_, _) => "Offer",
                MdnsControl::Answer(_) => "Answer",
                MdnsControl::Candidate(_) => "Candidate",
                MdnsControl::ReconnectRequest { .. } => "ReconnectRequest",
                MdnsControl::ReconnectAck { .. } => "ReconnectAck",
            })
            .collect();

        assert!(
            !kinds.contains(&"ReconnectRequest"),
            "SC-D3-4 FAIL: stale ReconnectRequest must be removed; inbox still has {kinds:?}"
        );
        assert!(
            kinds.contains(&"Offer") && kinds.contains(&"ReconnectAck"),
            "SC-D3-4 FAIL: non-ReconnectRequest frames must be preserved; inbox has {kinds:?}"
        );
    }

    // ─── SC-HO-1 / SC-HO-1b: superseded accept-gate (listener handover, B) ────
    //
    // Listener handover (design #971 §B, option iii-a): on the dual-reconnect /
    // severance path the reset hook re-`start()`s gen-G, which re-binds :7889 and
    // accepts AGAIN — but gen-G has no Offer (its inbox was drained, D3c). The
    // receiver's rebuilt connection can then land on the offer-less gen-G socket
    // and RST (HW gate v4, #970). The fix: a per-instance `superseded` accept-gate
    // that, once raised, stops gen-G from accepting NEW connections so only the
    // offer-bearing gen-(G+1) answers. CRITICAL: the flag must NOT close an
    // already-accepted live connection — it gates ONLY the pre-accept loop, never
    // `run_frame_loop`.

    /// Spawn just the accept-gate poll loop (`accept_one_with_gate`) over a
    /// loopback `TcpListener`. Returns the bound port, the stop + superseded flags,
    /// the event receiver, and the loop's join handle. This exercises the accept
    /// gate in isolation WITHOUT mDNS registration (no multicast needed).
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn spawn_accept_gate_over_loopback(
        superseded: Arc<AtomicBool>,
    ) -> (
        u16,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        std::sync::mpsc::Receiver<SignalingEvent>,
        std::thread::JoinHandle<()>,
    ) {
        use std::net::TcpListener;

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind loopback accept-gate listener");
        let port = listener.local_addr().expect("local_addr").port();
        listener
            .set_nonblocking(true)
            .expect("set_nonblocking on accept-gate listener");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let superseded_loop = Arc::clone(&superseded);
        let (event_tx, event_rx) = sync_channel::<SignalingEvent>(16);

        let handle = std::thread::spawn(move || {
            // The gate returns Some(stream) on accept, None on stop/superseded exit.
            let _ = super::accept_one_with_gate(&listener, &stop_loop, &superseded_loop, &event_tx);
        });

        (port, stop, superseded, event_rx, handle)
    }

    /// SC-HO-1 — With `superseded=false` the accept gate accepts a new TCP
    /// connection and emits `PeerFound`; with `superseded=true` it does NOT accept
    /// (no `PeerFound` within 200 ms) and exits cleanly on stop.
    ///
    /// RED: `accept_one_with_gate` does not exist yet (compile failure).
    /// GREEN (WU-B1): the gate checks `superseded` at the TOP of the poll loop,
    ///      alongside `stop`, and returns without accepting when raised.
    #[test]
    fn sc_ho_1_superseded_flag_stops_accept() {
        use std::net::TcpStream;
        use std::time::Duration;

        // ── Case A: not superseded → connection IS accepted (PeerFound). ──
        let superseded_off = Arc::new(AtomicBool::new(false));
        let (port, stop, _sup, event_rx, handle) = spawn_accept_gate_over_loopback(superseded_off);

        let _client = TcpStream::connect(("127.0.0.1", port)).expect("connect to accept gate");
        let ev = event_rx.recv_timeout(Duration::from_millis(500));
        assert!(
            matches!(ev, Ok(SignalingEvent::PeerFound { .. })),
            "SC-HO-1 FAIL: with superseded=false the gate MUST accept and emit PeerFound, got {ev:?}"
        );
        stop.store(true, std::sync::atomic::Ordering::Release);
        handle
            .join()
            .expect("accept-gate thread (case A) must join");

        // ── Case B: superseded → connection is NOT accepted (no PeerFound). ──
        let superseded_on = Arc::new(AtomicBool::new(true));
        let (port_b, stop_b, _sup_b, event_rx_b, handle_b) =
            spawn_accept_gate_over_loopback(superseded_on);

        // The kernel SYN backlog still completes the TCP handshake, but the gate
        // must NOT call accept() → no PeerFound is emitted.
        let _client_b = TcpStream::connect(("127.0.0.1", port_b));
        let ev_b = event_rx_b.recv_timeout(Duration::from_millis(200));
        assert!(
            ev_b.is_err(),
            "SC-HO-1 FAIL: with superseded=true the gate MUST NOT accept (no PeerFound), got {ev_b:?}"
        );
        stop_b.store(true, std::sync::atomic::Ordering::Release);
        handle_b
            .join()
            .expect("accept-gate thread (case B) must join");
    }

    /// SC-HO-1b — Raising `superseded` does NOT close an already-accepted
    /// connection: the live `run_frame_loop` is structurally independent of the
    /// accept gate (the flag is NOT threaded into `run_frame_loop`). With
    /// `suppress_bye=false`, the live frame loop STILL emits its Bye only on the
    /// stop flag — proving `superseded` neither tears down the connection nor
    /// suppresses its Bye.
    ///
    /// RED: shares the SC-HO-1 compile failure (`accept_one_with_gate` missing);
    ///      this test compiles only once the gate exists and the frame loop is
    ///      left untouched by the gate.
    /// GREEN (WU-B1): `superseded` governs ONLY the pre-accept loop.
    #[test]
    fn sc_ho_1b_superseded_does_not_kill_existing_frame_loop() {
        use crate::signaling::wire::SignalingFrame;

        // suppress_bye=false so a genuine stop still emits Bye; superseded must
        // have NO bearing on the already-accepted frame loop.
        let suppress_bye = Arc::new(AtomicBool::new(false));
        let (mut client, stop, handle) = spawn_frame_loop_over_loopback(suppress_bye);

        // The frame loop first sends Hello on the already-accepted connection.
        let hello = read_next_frame_or_eof(&mut client)
            .expect("read hello")
            .expect("hello frame must arrive on the live connection");
        assert!(
            matches!(hello, SignalingFrame::Hello { .. }),
            "first frame must be Hello, got {hello:?}"
        );

        // Raise a SEPARATE superseded flag. Because `run_frame_loop` does NOT take
        // `superseded`, this must NOT close the connection nor inject a Bye. The
        // live frame loop keeps running; the connection stays open.
        let superseded = Arc::new(AtomicBool::new(true));
        superseded.store(true, std::sync::atomic::Ordering::Release);

        // The connection must STILL be alive: no spurious Bye, no EOF yet. A short
        // read must time out (WouldBlock) rather than return a frame or EOF.
        client
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .expect("set_read_timeout");
        let mut buf = [0u8; 1];
        let read_res = std::io::Read::read(&mut client, &mut buf);
        match read_res {
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            other => panic!(
                "SC-HO-1b FAIL: raising superseded must NOT disturb the live frame loop \
                 (expected the connection to stay open with no Bye/EOF), got {other:?}"
            ),
        }

        // Now a genuine stop with suppress_bye=false MUST still emit a Bye —
        // proving the live connection was untouched by superseded.
        stop.store(true, std::sync::atomic::Ordering::Release);
        let next = read_next_frame_or_eof(&mut client)
            .expect("read after stop")
            .expect("SC-HO-1b FAIL: default path must still emit a Bye on stop");
        assert!(
            matches!(next, SignalingFrame::Bye { .. }),
            "SC-HO-1b FAIL: live frame loop must emit Bye on stop (superseded irrelevant), got {next:?}"
        );

        handle.join().expect("frame loop thread must join");
    }

    #[test]
    fn pr5b_a1_pr2a_frame_loop_replaces_and_clears_shared_capability_state() {
        use crate::signaling::wire::{SignalingFrame, write_frame};

        let (mut peer, stop, handle, _, negotiated, event_rx) =
            spawn_frame_loop_over_loopback_with_attempt(
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicU8::new(0)),
            );
        let hello = read_next_frame_or_eof(&mut peer).expect("read outbound Hello");
        assert!(
            matches!(hello, Some(SignalingFrame::Hello { capabilities, .. }) if capabilities == vec!["qsv-ledger-v1"])
        );
        for (capabilities, expected) in [
            (vec!["qsv-ledger-v1".to_string()], true),
            (Vec::new(), false),
            (vec!["qsv-ledger-v1".to_string()], true),
        ] {
            write_frame(
                &mut peer,
                &SignalingFrame::Hello {
                    proto: "v1".to_string(),
                    capabilities,
                },
            )
            .expect("send peer Hello");
            write_frame(
                &mut peer,
                &SignalingFrame::Offer {
                    sdp: "v=0".to_string(),
                    attempt: 1,
                },
            )
            .expect("send barrier Offer");
            assert!(matches!(
                event_rx.recv().expect("receive barrier Offer"),
                SignalingEvent::OfferReceived(_, 1)
            ));
            assert_eq!(negotiated.load(Ordering::Acquire), expected);
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("frame loop joins");
        assert!(!negotiated.load(Ordering::Acquire));
    }

    #[test]
    fn pr5b_a1_pr2a_qsv_ledger_token_is_exact_and_case_sensitive() {
        assert!(super::is_qsv_ledger_capability("qsv-ledger-v1"));
        assert!(!super::is_qsv_ledger_capability("QSV-ledger-v1"));
        assert!(!super::is_qsv_ledger_capability("qsv-ledger-v01"));
        assert!(!super::is_qsv_ledger_capability("qsv-ledger-v1-extra"));
    }

    #[test]
    fn pr5b_a1_pr2a_peer_capabilities_are_bounded_replaced_and_deduplicated() {
        let negotiated = Arc::new(AtomicBool::new(false));

        super::replace_peer_qsv_ledger_capability(
            &negotiated,
            &[
                "unknown-v1".to_string(),
                "qsv--ledger-v1".to_string(),
                "QSV-ledger-v1".to_string(),
                "qsv-ledger-v1".to_string(),
                "qsv-ledger-v1".to_string(),
            ],
        );
        assert!(
            super::qsv_ledger_negotiated(&negotiated),
            "the first valid exact token must negotiate the capability"
        );

        let mut sixteenth_token = (0..15)
            .map(|index| format!("unknown-{index}"))
            .collect::<Vec<_>>();
        sixteenth_token.push("qsv-ledger-v1".to_string());
        super::replace_peer_qsv_ledger_capability(&negotiated, &sixteenth_token);
        assert!(super::qsv_ledger_negotiated(&negotiated));

        let mut later_token = (0..16)
            .map(|index| format!("unknown-{index}"))
            .collect::<Vec<_>>();
        later_token.push("qsv-ledger-v1".to_string());
        super::replace_peer_qsv_ledger_capability(&negotiated, &later_token);
        assert!(
            !super::qsv_ledger_negotiated(&negotiated),
            "each Hello must replace, not accumulate, and tokens after the first 16 are ignored"
        );
    }

    #[test]
    fn pr5b_a1_pr2a_hello_advertises_the_capability_for_sender_and_receiver() {
        let negotiated = Arc::new(AtomicBool::new(false));

        for role in [SignalingRole::Sender, SignalingRole::Receiver] {
            assert_eq!(
                super::hello_capabilities(&role, &negotiated),
                vec!["qsv-ledger-v1".to_string()],
                "{role:?} must advertise the QSV ledger capability"
            );
        }
    }

    #[test]
    fn pr5b_a1_pr2a_negotiated_state_clears_for_peer_disconnect_reset_and_local_stop() {
        let signaling = MdnsSignaling::new(SignalingConfig {
            role: SignalingRole::Sender,
            ..Default::default()
        })
        .expect("new signaling");
        let negotiated = signaling.qsv_ledger_negotiated_state_for_test();

        signaling.replace_peer_qsv_ledger_capability(&["qsv-ledger-v1".to_string()]);
        assert!(super::qsv_ledger_negotiated(&negotiated));
        signaling.clear_qsv_ledger_on_peer_disconnect();
        assert!(!super::qsv_ledger_negotiated(&negotiated));

        signaling.replace_peer_qsv_ledger_capability(&["qsv-ledger-v1".to_string()]);
        let reset = signaling.reset().expect("reset signaling");
        assert!(
            !super::qsv_ledger_negotiated(&negotiated),
            "reset must Release-clear the shared state before dropping the old instance"
        );
        assert!(
            !super::qsv_ledger_negotiated(&reset.qsv_ledger_negotiated_state_for_test()),
            "a reset instance must begin with no negotiated peer capability"
        );

        let mut stopped = reset;
        stopped.replace_peer_qsv_ledger_capability(&["qsv-ledger-v1".to_string()]);
        let stopped_state = stopped.qsv_ledger_negotiated_state_for_test();
        stopped.stop().expect("stop signaling");
        assert!(
            !super::qsv_ledger_negotiated(&stopped_state),
            "local stop must Release-clear the negotiated state"
        );
    }

    #[test]
    fn a1_pr2b_same_object_restart_keeps_the_shared_arc_and_resets_it_before_each_worker() {
        let mut signaling = MdnsSignaling::new(SignalingConfig {
            role: SignalingRole::Sender,
            ..Default::default()
        })
        .expect("new signaling");
        let negotiated = signaling.qsv_ledger_negotiated_state_for_test();
        let first_receipts = signaling.a1_pr2b_queue_receipts();
        let second_receipts = signaling.a1_pr2b_queue_receipts();
        signaling.a1_pr2b_install_runner();
        let (event_tx, _event_rx) = sync_channel(1);

        signaling.start(event_tx.clone()).expect("first start");
        let first_entered = first_receipts.recv().expect("first entered receipt");
        assert!(first_entered.same_negotiated_arc(&negotiated));
        assert!(!first_entered.negotiated());

        negotiated.store(true, Ordering::Release);
        signaling.stop().expect("first stop");
        assert_eq!(
            first_receipts.drain(),
            vec!["StopObserved", "Exited"],
            "receipts must preserve the deterministic first-worker stop order"
        );

        signaling.start(event_tx).expect("second start");
        let second_entered = second_receipts.recv().expect("second entered receipt");
        assert!(second_entered.same_negotiated_arc(&negotiated));
        assert!(
            !second_entered.negotiated(),
            "the restarted worker must enter with peer negotiation reset"
        );
        signaling.stop().expect("second stop");
        assert_eq!(
            second_receipts.drain(),
            vec!["StopObserved", "Exited"],
            "the second worker must retain FIFO receipt ordering"
        );
    }

    #[test]
    fn a1_pr2b_preflight_and_failed_spawn_do_not_consume_receipts_or_invocations() {
        let mut signaling = MdnsSignaling::new(SignalingConfig {
            role: SignalingRole::Sender,
            ..Default::default()
        })
        .expect("new signaling");
        let negotiated = signaling.qsv_ledger_negotiated_state_for_test();
        let (event_tx, _event_rx) = sync_channel(1);

        assert!(matches!(signaling.start(event_tx.clone()), Err(SignalingError::Io(_))));
        assert!(!signaling.stop.load(Ordering::Acquire));
        assert!(!negotiated.load(Ordering::Acquire));
        assert!(signaling.handle.is_none());

        let receipts = signaling.a1_pr2b_queue_receipts();
        signaling.a1_pr2b_install_runner();
        signaling.a1_pr2b_fail_next_spawn();
        assert!(matches!(signaling.start(event_tx.clone()), Err(SignalingError::Io(_))));
        assert!(signaling.stop.load(Ordering::Acquire));
        assert!(!negotiated.load(Ordering::Acquire));
        assert!(signaling.handle.is_none());
        assert!(receipts.is_empty(), "failed spawn must not send or consume receipts");

        signaling.start(event_tx.clone()).expect("retry start");
        let entered = receipts.recv().expect("retried invocation receipt");
        assert_eq!(entered.invocation(), 0, "failed spawn must not advance invocation");
        assert!(matches!(signaling.start(event_tx), Err(SignalingError::AlreadyRunning)));
        signaling.stop().expect("retry stop");
        assert_eq!(receipts.drain(), vec!["StopObserved", "Exited"]);
    }
}
