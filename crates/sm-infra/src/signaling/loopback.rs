// Task 2.1 RED: LoopbackSignaling tests — types not yet implemented.
// These tests will compile-fail until task 2.2 adds the implementation.

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::signaling::{
        IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError,
        SignalingEvent, SignalingRole,
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
        let (mut a, mut b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (b_event_tx, b_event_rx) = sync_channel::<SignalingEvent>(8);
        b.start(b_event_tx).unwrap();

        // A does not need an event channel for this test; give it a throwaway one.
        let (a_event_tx, _a_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();

        let offer = SdpOffer("v=0\r\n".to_string());
        a.publish_local_offer(offer.clone()).unwrap();

        let ev = b_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected SignalingEvent::OfferReceived on side B within 1 s");

        match ev {
            SignalingEvent::OfferReceived(received) => {
                assert_eq!(received, offer, "received offer must equal the published offer");
            }
            other => panic!("expected OfferReceived, got {other:?}"),
        }
    }

    // ─── answer and candidate relay ──────────────────────────────────────────

    /// R8.3 — An answer sent on side B MUST appear as `AnswerReceived` on side A.
    #[test]
    fn answer_on_b_appears_as_answer_received_on_a() {
        let (mut a, mut b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

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
                assert_eq!(received, answer, "received answer must equal the published answer");
            }
            other => panic!("expected AnswerReceived, got {other:?}"),
        }
    }

    /// R8.3 — A candidate sent on side A MUST appear as `CandidateReceived` on side B.
    #[test]
    fn candidate_on_a_appears_as_candidate_received_on_b() {
        let (mut a, mut b) = LoopbackSignaling::pair(SignalingRole::Sender, SignalingRole::Receiver);

        let (b_event_tx, b_event_rx) = sync_channel::<SignalingEvent>(8);
        b.start(b_event_tx).unwrap();

        let (a_event_tx, _a_event_rx) = sync_channel::<SignalingEvent>(8);
        a.start(a_event_tx).unwrap();

        let cand = IceCandidate("candidate:1 1 udp 2130706431 192.168.1.1 7889 typ host".to_string());
        a.publish_local_candidate(cand.clone()).unwrap();

        let ev = b_event_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("expected CandidateReceived on side B within 1 s");

        match ev {
            SignalingEvent::CandidateReceived(received) => {
                assert_eq!(received, cand, "received candidate must equal the published candidate");
            }
            other => panic!("expected CandidateReceived, got {other:?}"),
        }
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

        let result = a.publish_local_offer(SdpOffer("v=0".to_string()));
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
        assert!(a.start(event_tx).is_ok(), "start() must succeed without network");
    }
}
