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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use sm_domain::signaling::{
    IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent,
};

use crate::signaling::wire::SignalingFrame;

// ─── Internal control messages ────────────────────────────────────────────────

/// Outbound frames queued from the public API into the signaling thread.
#[allow(dead_code)]
#[derive(Debug)]
enum MdnsControl {
    /// Offer to be forwarded to the connected peer.
    Offer(SdpOffer),
    /// Answer to be forwarded to the connected peer.
    Answer(SdpAnswer),
    /// ICE candidate to be forwarded to the connected peer.
    Candidate(IceCandidate),
}

// ─── MdnsSignaling ────────────────────────────────────────────────────────────

/// mDNS auto-discovery + TCP control channel signaling adapter.
///
/// Implements [`Signaling`]. Role is fixed at construction:
/// - **Sender**: publishes `_screen-mirror._tcp.local.`, listens for TCP connections.
/// - **Receiver**: browses for `_screen-mirror._tcp.local.`, connects TCP to the sender.
pub struct MdnsSignaling {
    /// Runtime configuration.
    #[allow(dead_code)]
    config: SignalingConfig,
    /// Shared stop flag.
    stop: Arc<AtomicBool>,
    /// Outbound control inbox (public API → thread).
    #[allow(dead_code)]
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    /// Thread handle (None before `start()` and after `stop()`).
    handle: Option<JoinHandle<()>>,
}

impl Signaling for MdnsSignaling {
    /// Construct an `MdnsSignaling` instance from a [`SignalingConfig`].
    fn new(config: SignalingConfig) -> Result<Self, SignalingError> {
        Ok(Self {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            handle: None,
        })
    }

    fn start(&mut self, _event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
        if self.handle.is_some() {
            return Err(SignalingError::AlreadyRunning);
        }
        // Stub: full implementation in task 5.4.
        Ok(())
    }

    fn publish_local_offer(&self, _offer: SdpOffer) -> Result<(), SignalingError> {
        // Stub: does NOT correctly check running state yet.
        Ok(())
    }

    fn publish_local_answer(&self, _answer: SdpAnswer) -> Result<(), SignalingError> {
        Ok(())
    }

    fn publish_local_candidate(&self, _cand: IceCandidate) -> Result<(), SignalingError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SignalingError> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        Ok(())
    }
}

impl Drop for MdnsSignaling {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Convert an inbound [`SignalingFrame`] into the matching [`SignalingEvent`].
///
/// Returns `None` for `Hello` (consumed silently as a protocol-version check).
/// Not yet implemented — will be added in task 5.4.
#[allow(dead_code)]
pub(crate) fn frame_to_event(_frame: SignalingFrame) -> Option<SignalingEvent> {
    unimplemented!("frame_to_event: not yet implemented")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::signaling::{
        SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent, SignalingRole,
    };

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
    /// 5 seconds each emits SignalingEvent::PeerFound.
    ///
    /// This test is #[ignore] per R7.5 because it requires mDNS multicast.
    /// Run manually with: `cargo nextest run -- --run-ignored mdns_peer_discovery`
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

    /// S7.2 — frame_to_event maps Offer frame to OfferReceived.
    #[test]
    fn frame_to_event_offer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Offer {
            sdp: "v=0".to_string(),
        };
        let event = frame_to_event(frame).expect("Offer must produce an event");
        assert!(
            matches!(event, SignalingEvent::OfferReceived(SdpOffer(ref s)) if s == "v=0"),
            "Offer frame must map to OfferReceived with exact SDP"
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
        let event = frame_to_event(frame).expect("Answer must produce an event");
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
        let event = frame_to_event(frame).expect("Candidate must produce an event");
        assert!(matches!(event, SignalingEvent::CandidateReceived(_)));
    }

    /// S7.3 — Hello frame returns None (absorbed silently).
    #[test]
    fn frame_to_event_hello_returns_none() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Hello {
            proto: "v1".to_string(),
        });
        assert!(
            event.is_none(),
            "Hello frame must not produce a SignalingEvent"
        );
    }

    /// S7.3 — Bye frame produces Closed event.
    #[test]
    fn frame_to_event_bye_returns_closed() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Bye).expect("Bye must produce Closed");
        assert!(
            matches!(event, SignalingEvent::Closed),
            "Bye frame must map to SignalingEvent::Closed"
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

    /// R7.4 — stop() is idempotent.
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
        let result = sig.publish_local_offer(SdpOffer("v=0".to_string()));
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish before start must return NotRunning, got {result:?}"
        );
    }
}
