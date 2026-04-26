//! Port boundary for media transport.
//!
//! This module defines the domain-level contract for the WebRTC transport link.
//! No platform type, async runtime, str0m, or codec-specific import is permitted
//! here — all platform adaptation lives in `sm-infra`.

// Types VideoSender, VideoReceiver, TransportConfig, TransportEvent, TransportError,
// TransportRole, TRANSPORT_CHANNEL_CAPACITY are not yet implemented.
// These tests are RED — they will fail to compile until the impl lands.

#[cfg(test)]
mod tests {
    // These imports reference types that do not exist yet.
    // Compile error expected.
    use super::{
        TransportConfig, TransportError, TransportEvent, VideoReceiver, VideoSender,
        TRANSPORT_CHANNEL_CAPACITY,
    };
    use crate::signaling::{IceCandidate, SdpAnswer, SdpOffer};
    use crate::encode::{EncodedPacket, VideoEncoder};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{sync_channel, SyncSender, Receiver};

    // ─── FakeVideoSender ────────────────────────────────────────────────────────

    /// In-memory `VideoSender` for domain-level unit tests.
    struct FakeVideoSender {
        config: TransportConfig,
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        encoder: Option<Arc<dyn VideoEncoder + Send + Sync>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeVideoSender {
        fn new_fake() -> Self {
            Self {
                config: TransportConfig::default(),
                started: Arc::new(AtomicBool::new(false)),
                stopped: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
                encoder: None,
                handle: None,
            }
        }
    }

    impl VideoSender for FakeVideoSender {
        fn new(_config: TransportConfig) -> Result<Self, TransportError>
        where
            Self: Sized,
        {
            Ok(Self::new_fake())
        }

        fn set_encoder(&mut self, encoder: Arc<dyn VideoEncoder + Send + Sync>) {
            self.encoder = Some(encoder);
        }

        fn start(
            &mut self,
            rx: Receiver<EncodedPacket>,
            _event_tx: SyncSender<TransportEvent>,
        ) -> Result<(), TransportError> {
            if self.started.load(Ordering::Acquire) {
                return Err(TransportError::AlreadyRunning);
            }
            self.started.store(true, Ordering::Release);
            let stopped = Arc::clone(&self.stopped);
            let dropped = Arc::clone(&self.dropped);
            let handle = std::thread::spawn(move || {
                loop {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    match rx.recv() {
                        Ok(_pkt) => {}
                        Err(_) => break,
                    }
                    let _ = dropped.load(Ordering::Relaxed);
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), TransportError> {
            self.stopped.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn apply_remote_answer(&self, _answer: SdpAnswer) -> Result<(), TransportError> {
            Ok(())
        }

        fn add_remote_candidate(&self, _cand: IceCandidate) -> Result<(), TransportError> {
            Ok(())
        }

        fn create_local_offer(&self) -> Result<SdpOffer, TransportError> {
            Ok(SdpOffer("v=0\r\n".to_string()))
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── FakeVideoReceiver ──────────────────────────────────────────────────────

    /// In-memory `VideoReceiver` for domain-level unit tests.
    struct FakeVideoReceiver {
        stopped: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeVideoReceiver {
        fn new_fake() -> Self {
            Self {
                stopped: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
                handle: None,
            }
        }
    }

    impl VideoReceiver for FakeVideoReceiver {
        fn new(_config: TransportConfig) -> Result<Self, TransportError>
        where
            Self: Sized,
        {
            Ok(Self::new_fake())
        }

        fn start(
            &mut self,
            _pkt_tx: SyncSender<EncodedPacket>,
            _event_tx: SyncSender<TransportEvent>,
        ) -> Result<(), TransportError> {
            let stopped = Arc::clone(&self.stopped);
            let handle = std::thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), TransportError> {
            self.stopped.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn apply_remote_offer(&self, _offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
            Ok(SdpAnswer("v=0\r\n".to_string()))
        }

        fn add_remote_candidate(&self, _cand: IceCandidate) -> Result<(), TransportError> {
            Ok(())
        }

        fn request_keyframe(&self) -> Result<(), TransportError> {
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── S1.2: FakeVideoSender start/stop lifecycle ──────────────────────────────

    #[test]
    fn fake_video_sender_lifecycle_s1_2() {
        let mut sender = FakeVideoSender::new_fake();
        let (pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);
        sender.start(pkt_rx, event_tx).unwrap();
        assert_eq!(sender.dropped_frames(), 0);
        drop(pkt_tx);
        sender.stop().unwrap();
    }

    // ─── S1.3: FakeVideoSender idempotent stop ────────────────────────────────

    #[test]
    fn fake_video_sender_stop_idempotent_s1_3() {
        let mut sender = FakeVideoSender::new_fake();
        sender.stop().unwrap();
        let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);
        sender.started.store(false, Ordering::Release);
        sender.stopped.store(false, Ordering::Release);
        sender.start(pkt_rx, event_tx).unwrap();
        sender.stop().unwrap();
        sender.stop().unwrap();
    }

    // ─── S2.1: FakeVideoReceiver apply_remote_offer ──────────────────────────

    #[test]
    fn fake_video_receiver_apply_remote_offer_s2_1() {
        let receiver = FakeVideoReceiver::new_fake();
        let result = receiver.apply_remote_offer(SdpOffer("v=0\r\n".to_string()));
        assert!(result.is_ok(), "apply_remote_offer must return Ok");
        let _ = result.unwrap();
    }

    // ─── S2.2: FakeVideoReceiver idempotent stop ──────────────────────────────

    #[test]
    fn fake_video_receiver_stop_idempotent_s2_2() {
        let mut receiver = FakeVideoReceiver::new_fake();
        receiver.stop().unwrap();
        receiver.stop().unwrap();
    }

    // ─── S4.1: TransportConfig::default() values ──────────────────────────────

    #[test]
    fn transport_config_default_values_s4_1() {
        let cfg = TransportConfig::default();
        assert_eq!(cfg.udp_port, 7889, "default udp_port must be 7889 (PQ-1)");
        assert_eq!(cfg.h264_profile, "640032", "default h264_profile must be '640032' (PQ-3)");
    }

    // ─── S4.2: TransportEvent is Send + Sync ──────────────────────────────────

    #[test]
    fn transport_event_is_send_sync_s4_2() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransportEvent>();
    }

    // ─── S4.3: TransportError Display contains keyword ────────────────────────

    #[test]
    fn transport_error_display_contains_keyword_s4_3() {
        let err = TransportError::InvalidConfig("port 0 is not valid".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("port"), "TransportError::InvalidConfig display must contain 'port', got: {msg}");
    }

    // ─── S14.1: TRANSPORT_CHANNEL_CAPACITY == 4 ──────────────────────────────

    #[test]
    fn transport_channel_capacity_value_s14_1() {
        assert_eq!(TRANSPORT_CHANNEL_CAPACITY, 4, "TRANSPORT_CHANNEL_CAPACITY must be 4 (range [4,8])");
    }
}
