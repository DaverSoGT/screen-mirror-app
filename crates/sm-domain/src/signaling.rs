//! Port boundary for signaling (peer discovery, SDP/ICE exchange).
//!
//! This module defines the domain-level contract for signaling adapters.
//! SDP and ICE candidates are opaque to the domain — they cross the boundary
//! as `String` newtypes. No str0m, mdns-sd, or OS-specific type appears here.
//!
//! # Key types
//!
//! | Type | Role |
//! |------|------|
//! | [`Signaling`] | Port trait implemented by each signaling adapter. |
//! | [`SdpOffer`] | Opaque newtype wrapping a raw SDP offer string. |
//! | [`SdpAnswer`] | Opaque newtype wrapping a raw SDP answer string. |
//! | [`IceCandidate`] | Opaque newtype wrapping a raw ICE candidate string. |
//! | [`SignalingConfig`] | Configuration for a signaling session. |
//! | [`SignalingEvent`] | Events emitted by signaling adapters. |
//! | [`SignalingError`] | Unified error enum for all signaling operations. |

use std::sync::mpsc::SyncSender;

// ─── SDP / ICE newtypes ──────────────────────────────────────────────────────

/// An opaque SDP offer.
///
/// The inner `String` is the raw SDP text from str0m or the signaling wire.
/// The domain treats it as an opaque value — no parsing occurs here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpOffer(pub String);

/// An opaque SDP answer.
///
/// The inner `String` is the raw SDP text. Returned by `VideoReceiver::apply_remote_offer`
/// and forwarded by the signaling adapter back to the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpAnswer(pub String);

/// An opaque ICE candidate.
///
/// The inner `String` is the raw candidate attribute line (e.g. `"candidate:..."`).
/// Both sender and receiver trickle candidates via the signaling adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCandidate(pub String);

// ─── SignalingConfig ─────────────────────────────────────────────────────────

/// Role for a signaling instance.
///
/// Sender publishes the mDNS service; Receiver discovers and connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingRole {
    /// This instance publishes the service and accepts the TCP control connection.
    Sender,
    /// This instance discovers the service and opens the TCP control connection.
    Receiver,
}

/// Configuration for a signaling session.
///
/// # Defaults
///
/// - `service_name`: `"_screen-mirror._tcp.local."`
/// - `control_port`: 7889
/// - `role`: `SignalingRole::Sender`
/// - `peer_hint`: `None`
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// mDNS service type to publish/discover.
    /// Default: `"_screen-mirror._tcp.local."`.
    pub service_name: String,
    /// TCP port for the SDP/ICE control channel. Default: 7889.
    pub control_port: u16,
    /// Role: sender publishes, receiver discovers.
    pub role: SignalingRole,
    /// Optional peer hint (`host:port`) — bypasses mDNS when `Some`.
    pub peer_hint: Option<String>,
}

impl Default for SignalingConfig {
    fn default() -> Self {
        Self {
            service_name: "_screen-mirror._tcp.local.".to_string(),
            control_port: 7889,
            role: SignalingRole::Sender,
            peer_hint: None,
        }
    }
}

// ─── SignalingEvent ──────────────────────────────────────────────────────────

/// Events emitted by a signaling adapter on the `event_tx` channel.
///
/// All variants are `Send + Sync` — the channel may be polled from any thread.
#[derive(Debug, Clone)]
pub enum SignalingEvent {
    /// A peer was discovered (mDNS) or connected (loopback).
    PeerFound {
        /// Human-readable address of the discovered peer (e.g. `"192.168.1.5"`).
        host: String,
        /// TCP port the peer is listening on.
        port: u16,
    },
    /// Remote SDP offer arrived (receiver side).
    OfferReceived(SdpOffer),
    /// Remote SDP answer arrived (sender side).
    AnswerReceived(SdpAnswer),
    /// Remote ICE candidate arrived (either side).
    CandidateReceived(IceCandidate),
    /// Signaling closed cleanly (both sides exchanged Bye).
    Closed,
    /// Fatal error; signaling thread exits after this event.
    Error(SignalingError),
}

// ─── SignalingError ──────────────────────────────────────────────────────────

/// Errors produced by signaling operations.
///
/// Platform errors are converted to `String` at the adapter boundary so
/// `sm-domain` never names any platform-specific type.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SignalingError {
    /// `start()` was called on an already-running signaling instance.
    #[error("signaling already running")]
    AlreadyRunning,

    /// The operation requires an active signaling session.
    #[error("signaling not running")]
    NotRunning,

    /// mDNS service registration or discovery failed.
    #[error("peer not found")]
    PeerNotFound,

    /// An I/O error from the TCP control socket.
    #[error("signaling I/O error: {0}")]
    Io(String),

    /// A wire-protocol error (e.g., malformed JSON, unknown frame type).
    #[error("signaling protocol error: {0}")]
    Protocol(String),
}

// ─── Signaling trait ─────────────────────────────────────────────────────────

/// Port boundary for platform-specific signaling adapters.
///
/// # Channel discipline
///
/// `start(event_tx)` injects the outbound event channel. The signaling thread
/// emits `SignalingEvent`s asynchronously via `event_tx.try_send`.
///
/// # Thread model
///
/// `new()` does NOT spawn a thread. `start()` spawns exactly one OS thread.
/// `stop()` is idempotent and joins the thread.
///
/// # Adapter implementations
///
/// - `sm_infra::signaling::MdnsSignaling` — mDNS auto-discovery + TCP control channel.
/// - `sm_infra::signaling::LoopbackSignaling` — In-memory fixture, no networking.
pub trait Signaling: Send + Sync {
    /// Construct a signaling instance with the given configuration.
    ///
    /// Does NOT connect to the network. Does NOT spawn a thread.
    fn new(config: SignalingConfig) -> Result<Self, SignalingError>
    where
        Self: Sized;

    /// Begin signaling. Spawns one OS thread.
    ///
    /// The thread drives mDNS publish/discover and the TCP control channel,
    /// emitting `SignalingEvent`s on `event_tx`.
    fn start(&mut self, event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError>;

    /// Publish the local SDP offer to the remote peer.
    fn publish_local_offer(&self, offer: SdpOffer) -> Result<(), SignalingError>;

    /// Publish the local SDP answer to the remote peer.
    fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError>;

    /// Publish a local ICE candidate to the remote peer.
    fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError>;

    /// Stop signaling. Idempotent. Closes the TCP socket and joins the thread.
    fn stop(&mut self) -> Result<(), SignalingError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    // ─── Trait bound assertion: Signaling is Send + Sync ──────────────────────
    //
    // Concrete adapters (`MdnsSignaling`, `LoopbackSignaling`) are already
    // `Send + Sync` by composition. This compile-time assertion locks the
    // contract so callers can hold `Arc<dyn Signaling>` cross-thread. Closes
    // verify-report SUGGESTION 3 from `transport-webrtc-str0m`.
    const _: () = {
        const fn _assert_send_sync<T: Send + Sync + ?Sized>() {}
        const fn _assert_signaling() {
            _assert_send_sync::<dyn Signaling>();
        }
    };

    // ─── FakeSignaling ────────────────────────────────────────────────────────

    /// In-memory `Signaling` implementation for domain-level unit tests.
    /// Relays `publish_local_*` calls immediately as `SignalingEvent`s on the event channel.
    struct FakeSignaling {
        event_tx: Option<SyncSender<SignalingEvent>>,
    }

    impl FakeSignaling {
        fn new_fake() -> Self {
            Self { event_tx: None }
        }
    }

    impl Signaling for FakeSignaling {
        fn new(_config: SignalingConfig) -> Result<Self, SignalingError>
        where
            Self: Sized,
        {
            Ok(Self::new_fake())
        }

        fn start(&mut self, event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
            self.event_tx = Some(event_tx);
            Ok(())
        }

        fn publish_local_offer(&self, offer: SdpOffer) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::OfferReceived(offer))
                    .map_err(|_| SignalingError::Io("channel full or closed".to_string()))?;
            }
            Ok(())
        }

        fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::AnswerReceived(answer))
                    .map_err(|_| SignalingError::Io("channel full or closed".to_string()))?;
            }
            Ok(())
        }

        fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::CandidateReceived(cand))
                    .map_err(|_| SignalingError::Io("channel full or closed".to_string()))?;
            }
            Ok(())
        }

        fn stop(&mut self) -> Result<(), SignalingError> {
            self.event_tx = None;
            Ok(())
        }
    }

    // ─── S3.1: SdpOffer Debug non-empty ───────────────────────────────────────

    #[test]
    fn sdp_offer_debug_non_empty_s3_1() {
        let offer = SdpOffer("test".to_string());
        let debug_str = format!("{offer:?}");
        assert!(
            !debug_str.is_empty(),
            "SdpOffer Debug must produce a non-empty string"
        );
    }

    // ─── S3.2: SdpOffer("a") != SdpOffer("b") ────────────────────────────────

    #[test]
    fn sdp_offer_inequality_s3_2() {
        let a = SdpOffer("a".to_string());
        let b = SdpOffer("b".to_string());
        assert_ne!(a, b, "SdpOffer('a') must not equal SdpOffer('b')");
    }

    // ─── S3.3: FakeSignaling::send_offer emits OfferReceived ─────────────────

    #[test]
    fn fake_signaling_publish_local_offer_emits_offer_received_s3_3() {
        let mut sig = FakeSignaling::new_fake();
        let (event_tx, event_rx) = sync_channel::<SignalingEvent>(4);
        sig.start(event_tx).unwrap();
        let offer = SdpOffer("v=0\r\n".to_string());
        sig.publish_local_offer(offer.clone()).unwrap();
        let ev = event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected SignalingEvent within 1s");
        match ev {
            SignalingEvent::OfferReceived(received_offer) => {
                assert_eq!(
                    received_offer, offer,
                    "OfferReceived must contain the exact offer"
                );
            }
            other => panic!("expected OfferReceived, got {other:?}"),
        }
    }
}
