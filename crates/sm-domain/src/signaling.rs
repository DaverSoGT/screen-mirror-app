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

/// Compact QSV receiver telemetry sampled over the signaling/control channel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QsvReceiverTelemetry {
    /// Milliseconds since the last observed media fragment.
    pub media_gap_ms: u32,
    /// Fragment rate scaled by 100 to stay integer-only on the wire.
    pub fragments_per_s_x100: u32,
    /// Number of dropped media segments observed by the receiver.
    pub dropped_segments: u64,
    /// Number of receiver-side dropped frames observed by the transport.
    pub receiver_dropped_frames: u64,
    /// Number of emitted media fragments included in this receiver sample.
    pub fragments_emitted: u64,
    /// Receiver observation window in milliseconds.
    pub window_ms: u32,
}

// ─── SignalingConfig ─────────────────────────────────────────────────────────

/// Role for a signaling instance.
///
/// Sender publishes the mDNS service; Receiver discovers and connects.
///
/// `Serialize`/`Deserialize` are required because `SignalingRole` is embedded
/// in `SignalingFrame::ReconnectRequest` which travels over the TCP wire.
/// Plain enum representation: `"Sender"` / `"Receiver"` (PascalCase default).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingEvent {
    /// A peer was discovered (mDNS) or connected (loopback).
    PeerFound {
        /// Human-readable address of the discovered peer (e.g. `"192.168.1.5"`).
        host: String,
        /// TCP port the peer is listening on.
        port: u16,
    },
    /// Remote SDP offer arrived (receiver side).
    ///
    /// The second field is the sender's supervisor reconnect-attempt number at the
    /// time the Offer was published. The receiver's `run_signaling_drain` uses this
    /// to drop stale-generation Offers (REQ-GE-4, Decision 1).
    OfferReceived(SdpOffer, u8),
    /// Remote SDP answer arrived (sender side).
    AnswerReceived(SdpAnswer),
    /// Remote ICE candidate arrived (either side).
    CandidateReceived(IceCandidate),
    /// QSV telemetry request arrived on the control channel.
    QsvTelemetryRequest,
    /// QSV telemetry response arrived on the control channel.
    QsvTelemetryResponse(QsvReceiverTelemetry),
    /// Signaling closed.
    ///
    /// - `Some(n)` — a wire `Bye { attempt: n }` arrived from the peer (D-1).
    ///   The attempt value enables the receiver drain to filter stale-generation
    ///   Byes (REQ-BYE-4). `n` is the emitter's last published Offer attempt.
    /// - `None` — a transport-level close with no attempt context: TCP EOF (mdns.rs
    ///   EOF path) or any clean-close that is not a tagged Bye. EOF is always honored
    ///   (the socket is genuinely gone — never a stale replay).
    Closed { attempt: Option<u8> },
    /// Fatal error; signaling thread exits after this event.
    Error(SignalingError),
}

// ─── SignalingError ──────────────────────────────────────────────────────────

/// Errors produced by signaling operations.
///
/// Platform errors are converted to `String` at the adapter boundary so
/// `sm-domain` never names any platform-specific type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
    ///
    /// `attempt` is the supervisor reconnect-attempt number at the time of publishing
    /// (REQ-GE-1, REQ-GE-2). Carried through to the wire `Offer` frame.
    fn publish_local_offer(&self, offer: SdpOffer, attempt: u8) -> Result<(), SignalingError>;

    /// Publish the local SDP answer to the remote peer.
    fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError>;

    /// Publish a local ICE candidate to the remote peer.
    fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError>;

    /// Publish a QSV telemetry request to the remote peer.
    fn publish_qsv_telemetry_request(&self) -> Result<(), SignalingError> {
        Ok(())
    }

    /// Publish a QSV telemetry response to the remote peer.
    fn publish_qsv_telemetry_response(
        &self,
        telemetry: QsvReceiverTelemetry,
    ) -> Result<(), SignalingError> {
        let _ = telemetry;
        Ok(())
    }

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

        fn publish_local_offer(&self, offer: SdpOffer, attempt: u8) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::OfferReceived(offer, attempt))
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

        fn publish_qsv_telemetry_request(&self) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::QsvTelemetryRequest)
                    .map_err(|_| SignalingError::Io("channel full or closed".to_string()))?;
            }
            Ok(())
        }

        fn publish_qsv_telemetry_response(
            &self,
            telemetry: QsvReceiverTelemetry,
        ) -> Result<(), SignalingError> {
            if let Some(ref tx) = self.event_tx {
                tx.try_send(SignalingEvent::QsvTelemetryResponse(telemetry))
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
        sig.publish_local_offer(offer.clone(), 1).unwrap();
        let ev = event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected SignalingEvent within 1s");
        match ev {
            SignalingEvent::OfferReceived(received_offer, attempt) => {
                assert_eq!(
                    received_offer, offer,
                    "OfferReceived must contain the exact offer"
                );
                assert_eq!(attempt, 1, "OfferReceived must carry the attempt number");
            }
            other => panic!("expected OfferReceived, got {other:?}"),
        }
    }

    #[test]
    fn fake_signaling_qsv_telemetry_round_trip() {
        let mut sig = FakeSignaling::new_fake();
        let (event_tx, event_rx) = sync_channel::<SignalingEvent>(4);
        sig.start(event_tx).unwrap();
        let telemetry = QsvReceiverTelemetry {
            media_gap_ms: 120,
            fragments_per_s_x100: 750,
            dropped_segments: 3,
            receiver_dropped_frames: 4,
            fragments_emitted: 6,
            window_ms: 1_500,
        };

        sig.publish_qsv_telemetry_request().unwrap();
        sig.publish_qsv_telemetry_response(telemetry.clone())
            .unwrap();

        assert!(matches!(
            event_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            SignalingEvent::QsvTelemetryRequest
        ));
        assert_eq!(
            event_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            SignalingEvent::QsvTelemetryResponse(telemetry)
        );
    }
}
