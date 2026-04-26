//! Port boundary for signaling (peer discovery, SDP/ICE exchange).
//!
//! SDP and ICE candidates are opaque to the domain — they cross the boundary
//! as `String` newtypes. No str0m, mdns-sd, or OS-specific type appears here.

// Types SdpOffer, SdpAnswer, IceCandidate, Signaling, SignalingConfig,
// SignalingEvent, SignalingError are not yet implemented.
// These tests are RED — they will fail to compile until the impl lands.

#[cfg(test)]
mod tests {
    use super::{
        IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError,
        SignalingEvent,
    };
    use std::sync::mpsc::{sync_channel, SyncSender};

    // ─── FakeSignaling ────────────────────────────────────────────────────────

    /// In-memory `Signaling` implementation for domain-level unit tests.
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
        assert!(!debug_str.is_empty(), "SdpOffer Debug must produce a non-empty string");
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
                assert_eq!(received_offer, offer, "OfferReceived must contain the exact offer");
            }
            other => panic!("expected OfferReceived, got {other:?}"),
        }
    }
}
