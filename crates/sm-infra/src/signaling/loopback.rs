//! In-memory loopback signaling fixture.
//!
//! [`LoopbackSignaling`] pairs two [`sm_domain::signaling::Signaling`] halves via
//! in-process channels. No network I/O, no mDNS, no OS threads — safe for unit tests
//! and CI environments where multicast DNS is unavailable.
//!
//! # Threading model
//!
//! `start()` is **synchronous**: it stores the `event_tx` channel and returns immediately
//! without spawning a thread. `publish_local_*` calls relay messages synchronously
//! through a `SyncSender<LoopbackFrame>` to the peer, which forwards them as
//! `SignalingEvent`s on the peer's `event_tx`. Because no thread is spawned, `stop()`
//! simply clears the channels — no `JoinHandle` to join.
//!
//! This choice was made in accordance with spec §3.4 ("MAY be synchronous") and is
//! documented here so future readers do not attempt to `join()` a non-existent thread.
//!
//! # Usage
//!
//! ```rust,no_run
//! use sm_infra::signaling::loopback::LoopbackSignaling;
//! use sm_domain::signaling::{Signaling, SignalingRole, SignalingEvent};
//! use std::sync::mpsc::sync_channel;
//!
//! let (mut sender_sig, mut receiver_sig) =
//!     LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
//!
//! let (tx_s, _rx_s) = sync_channel::<SignalingEvent>(8);
//! let (tx_r, _rx_r) = sync_channel::<SignalingEvent>(8);
//! sender_sig.start(tx_s).unwrap();
//! receiver_sig.start(tx_r).unwrap();
//! ```

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use sm_domain::signaling::{
    IceCandidate, QsvReceiverTelemetry, SdpAnswer, SdpOffer, Signaling, SignalingConfig,
    SignalingError, SignalingEvent, SignalingRole,
};

// ─── Internal wire frame ─────────────────────────────────────────────────────

/// Internal message type relayed between the two loopback halves.
///
/// Each variant corresponds to one outbound signaling artifact; the relay thread
/// (or direct-call relay in synchronous mode) converts it to the matching
/// [`SignalingEvent`] on the peer's `event_tx`.
#[derive(Debug, Clone)]
enum LoopbackFrame {
    /// Offer with associated supervisor attempt number (REQ-GE-1).
    Offer(SdpOffer, u8),
    Answer(SdpAnswer),
    Candidate(IceCandidate),
    QsvTelemetryRequest,
    QsvTelemetryResponse(QsvReceiverTelemetry),
}

// ─── Shared relay state ──────────────────────────────────────────────────────

/// State shared between the two halves of a `LoopbackSignaling` pair.
///
/// Each half holds an `Arc<LoopbackRelay>` pointing to the **peer's** relay state,
/// so when A calls `publish_local_offer`, it looks up B's `event_tx` and sends.
struct LoopbackRelay {
    /// The `event_tx` injected by the peer's `start()` call.
    ///
    /// `None` before `start()` is called and after `stop()`.
    event_tx: Mutex<Option<SyncSender<SignalingEvent>>>,
}

impl LoopbackRelay {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            event_tx: Mutex::new(None),
        })
    }

    /// Register the event channel. Called by `start()`.
    fn register(&self, tx: SyncSender<SignalingEvent>) {
        *self.event_tx.lock().unwrap() = Some(tx);
    }

    /// Deregister the event channel. Called by `stop()`.
    fn deregister(&self) {
        *self.event_tx.lock().unwrap() = None;
    }

    /// Relay a frame to this relay's registered `event_tx`.
    ///
    /// Returns `Err(SignalingError::NotRunning)` if `event_tx` is not set.
    fn relay(&self, frame: LoopbackFrame) -> Result<(), SignalingError> {
        let guard = self.event_tx.lock().unwrap();
        match &*guard {
            None => Err(SignalingError::NotRunning),
            Some(tx) => {
                let event = frame_to_event(frame);
                match tx.try_send(event) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => {
                        // Peer event channel is full — drop the frame (non-blocking contract).
                        Ok(())
                    }
                    Err(TrySendError::Disconnected(_)) => Err(SignalingError::NotRunning),
                }
            }
        }
    }
}

fn frame_to_event(frame: LoopbackFrame) -> SignalingEvent {
    match frame {
        LoopbackFrame::Offer(o, attempt) => SignalingEvent::OfferReceived(o, attempt),
        LoopbackFrame::Answer(a) => SignalingEvent::AnswerReceived(a),
        LoopbackFrame::Candidate(c) => SignalingEvent::CandidateReceived(c),
        LoopbackFrame::QsvTelemetryRequest => SignalingEvent::QsvTelemetryRequest,
        LoopbackFrame::QsvTelemetryResponse(telemetry) => {
            SignalingEvent::QsvTelemetryResponse(telemetry)
        }
    }
}

// ─── LoopbackSignaling ───────────────────────────────────────────────────────

/// In-memory signaling fixture for tests and `examples/transport_smoke.rs`.
///
/// Implements [`Signaling`]. Constructed exclusively via [`LoopbackSignaling::pair`].
///
/// # No network I/O
///
/// This adapter has zero network dependencies (R8.5). Messages are relayed
/// synchronously through `Arc<Mutex<Option<SyncSender<SignalingEvent>>>>` — no
/// sockets, no threads, no mDNS. `SignalingConfig` is accepted at construction
/// (trait requirement) but not used at runtime — the loopback relay is
/// configuration-agnostic by design.
///
/// # Stop semantics
///
/// `stop()` clears `self_relay` so the peer's `publish_local_*` calls will
/// return `Err(NotRunning)` for this half. Calling `stop()` again is a no-op
/// (`stop()` is idempotent per R12.4 / S8.2).
pub struct LoopbackSignaling {
    /// This half's relay state — updated by `start()` / `stop()`.
    self_relay: Arc<LoopbackRelay>,
    /// The peer's relay state — used by `publish_local_*` to reach the peer's `event_tx`.
    peer_relay: Arc<LoopbackRelay>,
    /// Whether `start()` has been called and `stop()` has not yet been called.
    running: bool,
}

impl LoopbackSignaling {
    /// Construct a connected pair of `LoopbackSignaling` instances.
    ///
    /// The first returned half has `role_a`; the second has `role_b`.
    /// Pass each half to one test participant.
    ///
    /// ```rust,no_run
    /// use sm_infra::signaling::loopback::LoopbackSignaling;
    /// use sm_domain::signaling::SignalingRole;
    ///
    /// let (sender, receiver) =
    ///     LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
    /// ```
    pub fn pair(role_a: SignalingRole, role_b: SignalingRole) -> (Self, Self) {
        let relay_a = LoopbackRelay::new();
        let relay_b = LoopbackRelay::new();

        // role_a / role_b are accepted to match the trait's construction convention
        // but are not stored — LoopbackSignaling is role-agnostic at runtime.
        let _ = (role_a, role_b);

        let a = LoopbackSignaling {
            self_relay: Arc::clone(&relay_a),
            peer_relay: Arc::clone(&relay_b),
            running: false,
        };
        let b = LoopbackSignaling {
            self_relay: Arc::clone(&relay_b),
            peer_relay: Arc::clone(&relay_a),
            running: false,
        };
        (a, b)
    }
}

impl Signaling for LoopbackSignaling {
    /// Construct a standalone `LoopbackSignaling` instance.
    ///
    /// Note: for test usage, prefer [`LoopbackSignaling::pair`] which builds
    /// a wired pair. A standalone instance constructed here has no peer; all
    /// `publish_local_*` calls will return `Err(SignalingError::NotRunning)`
    /// until a peer relay is connected externally.
    fn new(_config: SignalingConfig) -> Result<Self, SignalingError> {
        let relay = LoopbackRelay::new();
        // Peer relay is orphaned — no peer will receive events. This is intentional
        // for standalone construction; use `pair()` for functional test setups.
        let peer_relay = LoopbackRelay::new();
        Ok(LoopbackSignaling {
            self_relay: relay,
            peer_relay,
            running: false,
        })
    }

    /// Begin signaling.
    ///
    /// Stores `event_tx` for this half so that when the peer calls `publish_local_*`,
    /// the event arrives on this channel. No thread is spawned.
    ///
    /// Returns `Err(AlreadyRunning)` if called twice without an intervening `stop()`.
    fn start(&mut self, event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
        if self.running {
            return Err(SignalingError::AlreadyRunning);
        }
        self.self_relay.register(event_tx);
        self.running = true;
        Ok(())
    }

    /// Relay the local SDP offer to the peer.
    ///
    /// The peer receives `SignalingEvent::OfferReceived(offer, attempt)` on its `event_tx`.
    /// `attempt` is the supervisor reconnect-attempt number (REQ-GE-1).
    fn publish_local_offer(&self, offer: SdpOffer, attempt: u8) -> Result<(), SignalingError> {
        if !self.running {
            return Err(SignalingError::NotRunning);
        }
        self.peer_relay.relay(LoopbackFrame::Offer(offer, attempt))
    }

    /// Relay the local SDP answer to the peer.
    ///
    /// The peer receives `SignalingEvent::AnswerReceived` on its `event_tx`.
    fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError> {
        if !self.running {
            return Err(SignalingError::NotRunning);
        }
        self.peer_relay.relay(LoopbackFrame::Answer(answer))
    }

    /// Relay a local ICE candidate to the peer.
    ///
    /// The peer receives `SignalingEvent::CandidateReceived` on its `event_tx`.
    fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError> {
        if !self.running {
            return Err(SignalingError::NotRunning);
        }
        self.peer_relay.relay(LoopbackFrame::Candidate(cand))
    }

    fn publish_qsv_telemetry_request(&self) -> Result<(), SignalingError> {
        if !self.running {
            return Err(SignalingError::NotRunning);
        }
        self.peer_relay.relay(LoopbackFrame::QsvTelemetryRequest)
    }

    fn publish_qsv_telemetry_response(
        &self,
        telemetry: QsvReceiverTelemetry,
    ) -> Result<(), SignalingError> {
        if !self.running {
            return Err(SignalingError::NotRunning);
        }
        self.peer_relay
            .relay(LoopbackFrame::QsvTelemetryResponse(telemetry))
    }

    /// Stop signaling.
    ///
    /// Clears `self_relay` so the peer can no longer deliver events to this half.
    /// Idempotent: a second call is a no-op and returns `Ok(())`.
    fn stop(&mut self) -> Result<(), SignalingError> {
        if !self.running {
            return Ok(());
        }
        self.self_relay.deregister();
        self.running = false;
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::signaling::{
        IceCandidate, QsvReceiverTelemetry, SdpAnswer, SdpOffer, Signaling, SignalingConfig,
        SignalingError, SignalingEvent, SignalingRole,
    };

    use crate::signaling::loopback::LoopbackSignaling;

    // ─── S8.0: pair() returns two instances (compile-level check) ────────────

    /// R8.2 — `LoopbackSignaling::pair()` MUST return two distinct halves.
    #[test]
    fn loopback_pair_returns_two_halves_r8_2() {
        let (_a, _b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
        // If this compiles the type exists and pair() is callable.
    }

    // ─── S8.1: offer on A → OfferReceived on B ───────────────────────────────

    /// R8.3, S8.1 — An offer sent on side A MUST appear as `OfferReceived` on side B.
    #[test]
    fn offer_on_a_appears_as_offer_received_on_b_s8_1() {
        let (mut a, mut b) =
            LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (b_event_tx, b_event_rx) = sync_channel::<SignalingEvent>(8);
        b.start(b_event_tx).unwrap();

        // A does not need an event channel for this test; give it a throwaway one.
        let (a_event_tx, _a_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();

        let offer = SdpOffer("v=0\r\n".to_string());
        a.publish_local_offer(offer.clone(), 1).unwrap();

        let ev = b_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected SignalingEvent::OfferReceived on side B within 1 s");

        match ev {
            SignalingEvent::OfferReceived(received, attempt) => {
                assert_eq!(
                    received, offer,
                    "received offer must equal the published offer"
                );
                assert_eq!(attempt, 1, "attempt must be relayed through loopback");
            }
            other => panic!("expected OfferReceived, got {other:?}"),
        }
    }

    // ─── answer and candidate relay ──────────────────────────────────────────

    /// R8.3 — An answer sent on side B MUST appear as `AnswerReceived` on side A.
    #[test]
    fn answer_on_b_appears_as_answer_received_on_a() {
        let (mut a, mut b) =
            LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (a_event_tx, a_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();

        let (b_event_tx, _b_event_rx) = sync_channel::<SignalingEvent>(8);
        b.start(b_event_tx).unwrap();

        let answer = SdpAnswer("v=0\r\nm=video\r\n".to_string());
        b.publish_local_answer(answer.clone()).unwrap();

        let ev = a_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected AnswerReceived on side A within 1 s");

        match ev {
            SignalingEvent::AnswerReceived(received) => {
                assert_eq!(
                    received, answer,
                    "received answer must equal the published answer"
                );
            }
            other => panic!("expected AnswerReceived, got {other:?}"),
        }
    }

    /// R8.3 — A candidate sent on side A MUST appear as `CandidateReceived` on side B.
    #[test]
    fn candidate_on_a_appears_as_candidate_received_on_b() {
        let (mut a, mut b) =
            LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (b_event_tx, b_event_rx) = sync_channel::<SignalingEvent>(8);
        b.start(b_event_tx).unwrap();

        let (a_event_tx, _a_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();

        let cand =
            IceCandidate("candidate:1 1 udp 2130706431 192.168.1.1 7889 typ host".to_string());
        a.publish_local_candidate(cand.clone()).unwrap();

        let ev = b_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected CandidateReceived on side B within 1 s");

        match ev {
            SignalingEvent::CandidateReceived(received) => {
                assert_eq!(
                    received, cand,
                    "received candidate must equal the published candidate"
                );
            }
            other => panic!("expected CandidateReceived, got {other:?}"),
        }
    }

    #[test]
    fn qsv_telemetry_round_trip_between_halves() {
        let (mut a, mut b) =
            LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (a_event_tx, a_event_rx) = sync_channel::<SignalingEvent>(8);
        let (b_event_tx, b_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();
        b.start(b_event_tx).unwrap();

        let telemetry = QsvReceiverTelemetry {
            media_gap_ms: 120,
            fragments_per_s_x100: 750,
            dropped_segments: 3,
            receiver_dropped_frames: 4,
        };

        a.publish_qsv_telemetry_request().unwrap();
        let request_event = b_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected QsvTelemetryRequest on side B within 1 s");
        assert_eq!(request_event, SignalingEvent::QsvTelemetryRequest);

        b.publish_qsv_telemetry_response(telemetry.clone()).unwrap();
        let response_event = a_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected QsvTelemetryResponse on side A within 1 s");
        assert_eq!(
            response_event,
            SignalingEvent::QsvTelemetryResponse(telemetry)
        );
    }

    // ─── S8.2: stop() is idempotent ──────────────────────────────────────────

    /// R8.3, S8.2 — `stop()` MUST be idempotent (second call returns Ok without panic).
    #[test]
    fn stop_is_idempotent_s8_2() {
        let (mut a, _b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
        let (event_tx, _event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(event_tx).unwrap();
        a.stop().unwrap();
        a.stop().unwrap(); // second stop MUST NOT panic
    }

    // ─── S8.2 (variant): send_offer after stop returns NotRunning ────────────

    /// S8.2 — After `stop()`, `publish_local_offer` MUST return `Err(SignalingError::NotRunning)`.
    #[test]
    fn publish_after_stop_returns_not_running_s8_2_variant() {
        let (mut a, _b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
        let (event_tx, _event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(event_tx).unwrap();
        a.stop().unwrap();

        let result = a.publish_local_offer(SdpOffer("v=0".to_string()), 1);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "expected Err(NotRunning) after stop(), got {result:?}"
        );
    }

    // ─── Signaling trait impl (R8.1) ─────────────────────────────────────────

    /// R8.1 — LoopbackSignaling MUST implement the Signaling trait.
    /// This is a compile-time check: if LoopbackSignaling does not implement Signaling,
    /// this function will not compile.
    fn _assert_implements_signaling_trait<T: Signaling>() {}
    fn _check_loopback_implements_signaling() {
        _assert_implements_signaling_trait::<LoopbackSignaling>();
    }

    // ─── new() via trait ─────────────────────────────────────────────────────

    /// R8.1 — LoopbackSignaling MUST be constructible via `Signaling::new`.
    #[test]
    fn loopback_new_via_trait_succeeds() {
        let result = LoopbackSignaling::new(SignalingConfig::default());
        assert!(result.is_ok(), "LoopbackSignaling::new must return Ok");
    }

    // ─── R8.5: no mdns/network dependency (structural) ───────────────────────

    /// R8.5 is verified statically by the no_platform_deps test. This test
    /// documents the invariant: LoopbackSignaling lives in sm-infra only and
    /// must not trigger any mdns-sd or network I/O when constructed or started.
    #[test]
    fn loopback_start_does_not_require_network() {
        let (mut a, _b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);
        let (event_tx, _event_rx) = sync_channel::<SignalingEvent>(8);
        // start() MUST return Ok without any network I/O.
        assert!(
            a.start(event_tx).is_ok(),
            "start() must succeed without network"
        );
    }
}
