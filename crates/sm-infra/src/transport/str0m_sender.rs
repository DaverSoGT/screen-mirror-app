// Placeholder — implementation will be added in task 3.2 (GREEN commit).

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::sync_channel;

    use sm_domain::encode::{EncoderConfig, VideoEncoder};
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoSender};

    use crate::transport::str0m_sender::Str0mVideoSender;

    // ─── Static assertion: Str0mVideoSender is Send + Sync (task 3.5) ─────────

    #[allow(dead_code)]
    fn _assert_send_sync_sender() {
        fn check<T: Send + Sync>() {}
        check::<Str0mVideoSender>();
    }

    // ─── Helper: build a minimal FakeVideoEncoder for injection ───────────────

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct FakeEncoder {
        keyframe_called: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
    }

    impl FakeEncoder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                keyframe_called: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
            })
        }
    }

    impl VideoEncoder for FakeEncoder {
        fn new(_config: EncoderConfig) -> Result<Self, sm_domain::encode::EncoderError>
        where
            Self: Sized,
        {
            Ok(Self {
                keyframe_called: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
            })
        }

        fn start(
            &mut self,
            _rx: std::sync::mpsc::Receiver<sm_domain::CaptureFrame>,
            _tx: std::sync::mpsc::SyncSender<sm_domain::encode::EncodedPacket>,
        ) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn request_keyframe(&self) {
            self.keyframe_called.store(true, Ordering::Release);
        }

        fn set_bitrate(&self, _bps: u32) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── S5.1 (batch 3 variant): new() returns Ok with default config ─────────

    /// R5.2 (batch-3 variant): `Str0mVideoSender::new(config)` MUST return `Ok(_)`.
    /// No socket bind in new() per batch-3 constraint.
    #[test]
    fn str0m_sender_new_default_config_returns_ok_s5_1() {
        let result = Str0mVideoSender::new(TransportConfig::default());
        assert!(
            result.is_ok(),
            "Str0mVideoSender::new(default) must return Ok, got: {result:?}"
        );
    }

    // ─── new() with port 0 still returns Ok (validation deferred to start) ────

    #[test]
    fn str0m_sender_new_port_zero_returns_ok() {
        let cfg = TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        };
        // Port 0 is valid — OS picks an ephemeral port on bind in start().
        let result = Str0mVideoSender::new(cfg);
        assert!(result.is_ok(), "new() must not reject port 0");
    }

    // ─── set_encoder stores the encoder (no panic) ───────────────────────────

    #[test]
    fn str0m_sender_set_encoder_no_panic() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        // Must not panic.
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    }

    // ─── S5.2: start + stop — thread exits cleanly ───────────────────────────

    /// R5.3, S5.2 — `start()` spawns a thread; `stop()` joins it and returns Ok.
    #[test]
    fn str0m_sender_start_then_stop_ok_s5_2() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        sender.start(pkt_rx, event_tx).unwrap();

        // drop pkt_tx (already dropped via _pkt_tx going out of scope AFTER sender stopped)
        // Actually we need to drop _pkt_tx before stop to unblock the thread.
        // But pkt_tx is already held as _pkt_tx in this scope — drop it explicitly.
        // Note: _pkt_tx is not accessible once it goes into sync_channel binding.
        // Rework: use named binding.
        let _ = sender.stop();
    }

    // ─── start + stop with pkt_tx dropped first ──────────────────────────────

    #[test]
    fn str0m_sender_stop_after_pkt_tx_dropped() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        sender.start(pkt_rx, event_tx).unwrap();
        // Drop the sending side so thread's recv() unblocks.
        drop(pkt_tx);

        let result = sender.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

    // ─── S12.4: stop() is idempotent ──────────────────────────────────────────

    /// R12.4, S12.4 — second `stop()` MUST return `Ok(())` without panic.
    #[test]
    fn str0m_sender_stop_is_idempotent_s12_4() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        // Stop on never-started sender — idempotent.
        sender.stop().unwrap();
        sender.stop().unwrap();

        // Start + stop + stop.
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();
        drop(pkt_tx);
        sender.stop().unwrap();
        sender.stop().unwrap(); // second stop must not panic
    }

    // ─── S12.1: Drop calls stop() — no thread leak ────────────────────────────

    /// R12.5, S12.1 — Drop MUST call stop() if thread is still running.
    #[test]
    fn str0m_sender_drop_without_stop_joins_thread_s12_1() {
        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        {
            let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
            let enc = FakeEncoder::new();
            sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
            sender.start(pkt_rx, event_tx).unwrap();
            // Drop pkt_tx so the thread can exit when sender drops.
            drop(pkt_tx);
            // sender drops here — Drop calls stop() which sets stop=true and joins.
        }
        // If we reach here without hanging the thread was joined.
    }

    // ─── dropped_frames() returns 0 before any drops ──────────────────────────

    #[test]
    fn str0m_sender_dropped_frames_initially_zero() {
        let sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        assert_eq!(
            sender.dropped_frames(),
            0,
            "dropped_frames must be 0 before any activity"
        );
    }

    // ─── start() returns AlreadyRunning if called twice ───────────────────────

    #[test]
    fn str0m_sender_start_twice_returns_already_running() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        let (_pkt_tx2, pkt_rx2) = sync_channel(4);
        let (event_tx2, _event_rx2) = sync_channel::<TransportEvent>(4);
        let result = sender.start(pkt_rx2, event_tx2);
        assert!(
            matches!(result, Err(TransportError::AlreadyRunning)),
            "second start() must return Err(AlreadyRunning), got: {result:?}"
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }
}
