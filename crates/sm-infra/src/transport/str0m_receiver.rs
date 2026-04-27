// Placeholder — implementation will be added in task 3.4 (GREEN commit).

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::encode::EncodedPacket;
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoReceiver};

    use crate::transport::str0m_receiver::Str0mVideoReceiver;

    // ─── Static assertion: Str0mVideoReceiver is Send + Sync (task 3.5) ───────

    #[allow(dead_code)]
    fn _assert_send_sync_receiver() {
        fn check<T: Send + Sync>() {}
        check::<Str0mVideoReceiver>();
    }

    // ─── S6.1 (batch 3 variant): new() returns Ok with default config ─────────

    /// R6.2 (batch-3 variant): `Str0mVideoReceiver::new(config)` MUST return `Ok(_)`.
    #[test]
    fn str0m_receiver_new_default_config_returns_ok_s6_1() {
        let result = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        });
        assert!(
            result.is_ok(),
            "Str0mVideoReceiver::new(default) must return Ok, got: {result:?}"
        );
    }

    // ─── new() with port 0 still returns Ok ───────────────────────────────────

    #[test]
    fn str0m_receiver_new_port_zero_returns_ok() {
        let cfg = TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        };
        let result = Str0mVideoReceiver::new(cfg);
        assert!(result.is_ok(), "new() must not reject port 0");
    }

    // ─── S6.4 (part 1): start + stop — thread exits cleanly ──────────────────

    /// R6.4, S6.4 — `start()` spawns a thread; `stop()` joins it and returns Ok.
    #[test]
    fn str0m_receiver_start_then_stop_ok() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        receiver.start(pkt_tx, event_tx).unwrap();
        let result = receiver.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

    // ─── S6.4: stop() is idempotent ───────────────────────────────────────────

    /// R12.4, S6.4 — second `stop()` MUST return `Ok(())` without panic.
    #[test]
    fn str0m_receiver_stop_is_idempotent_s6_4() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();
        // Stop on never-started receiver — idempotent.
        receiver.stop().unwrap();
        receiver.stop().unwrap();

        // Start + stop + stop.
        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();
        receiver.stop().unwrap();
        receiver.stop().unwrap(); // second stop must not panic
    }

    // ─── S12.1 (receiver): Drop calls stop() — no thread leak ─────────────────

    /// R12.5 — Drop MUST call stop() if thread is still running.
    #[test]
    fn str0m_receiver_drop_without_stop_joins_thread() {
        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        {
            let mut receiver = Str0mVideoReceiver::new(TransportConfig {
                role: sm_domain::transport::TransportRole::Receiver,
                ..TransportConfig::default()
            })
            .unwrap();
            receiver.start(pkt_tx, event_tx).unwrap();
            // receiver drops here — Drop calls stop() which joins the thread.
        }
        // If we reach here without hanging the thread was joined.
    }

    // ─── dropped_frames() returns 0 before any drops ──────────────────────────

    #[test]
    fn str0m_receiver_dropped_frames_initially_zero() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();
        assert_eq!(
            receiver.dropped_frames(),
            0,
            "dropped_frames must be 0 before any activity"
        );
    }

    // ─── start() returns AlreadyRunning if called twice ───────────────────────

    #[test]
    fn str0m_receiver_start_twice_returns_already_running() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        let (pkt_tx2, _pkt_rx2) = sync_channel::<EncodedPacket>(4);
        let (event_tx2, _event_rx2) = sync_channel::<TransportEvent>(4);
        let result = receiver.start(pkt_tx2, event_tx2);
        assert!(
            matches!(result, Err(TransportError::AlreadyRunning)),
            "second start() must return Err(AlreadyRunning), got: {result:?}"
        );

        receiver.stop().unwrap();
    }
}
