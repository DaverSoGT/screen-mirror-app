#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_observation_accepts_qsv_receiver_telemetry() {
        let telemetry = QsvReceiverTelemetry {
            media_gap_ms: 120,
            fragments_per_s_x100: 750,
            dropped_segments: 3,
            receiver_dropped_frames: 4,
        };

        let observation = summarize_sender_observation("hw_intel_qsv", telemetry.clone())
            .expect("QSV must consume receiver telemetry for observation");

        assert_eq!(observation.backend_name, "hw_intel_qsv");
        assert_eq!(observation.receiver, telemetry);
    }

    #[test]
    fn sender_observation_ignores_nvenc_receiver_telemetry() {
        let telemetry = QsvReceiverTelemetry {
            media_gap_ms: 120,
            fragments_per_s_x100: 750,
            dropped_segments: 3,
            receiver_dropped_frames: 4,
        };

        let observation = summarize_sender_observation("hw_nvenc", telemetry);

        assert!(observation.is_none());
    }
}
