//! QSV observation helpers for the observation-only runtime slice.

use sm_domain::signaling::QsvReceiverTelemetry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QsvSenderObservation {
    pub backend_name: String,
    pub receiver: QsvReceiverTelemetry,
}

pub fn summarize_sender_observation(
    backend_name: &str,
    receiver: QsvReceiverTelemetry,
) -> Option<QsvSenderObservation> {
    (backend_name == "hw_intel_qsv").then(|| QsvSenderObservation {
        backend_name: backend_name.to_string(),
        receiver,
    })
}

pub fn receiver_telemetry_from_counters(
    fragments_emitted: u64,
    dropped_segments: u64,
    receiver_dropped_frames: u64,
    first_fragment_at_ms: u64,
    last_fragment_at_ms: u64,
    now_ms: u64,
) -> QsvReceiverTelemetry {
    let media_gap_ms = last_fragment_at_ms
        .checked_sub(1)
        .map(|_| {
            now_ms
                .saturating_sub(last_fragment_at_ms)
                .min(u64::from(u32::MAX)) as u32
        })
        .unwrap_or(0);
    let fragments_per_s_x100 =
        if fragments_emitted > 1 && last_fragment_at_ms > first_fragment_at_ms {
            let elapsed_ms = last_fragment_at_ms - first_fragment_at_ms;
            (((fragments_emitted - 1) * 100_000) / elapsed_ms).min(u64::from(u32::MAX)) as u32
        } else {
            0
        };

    QsvReceiverTelemetry {
        media_gap_ms,
        fragments_per_s_x100,
        dropped_segments,
        receiver_dropped_frames,
    }
}

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
