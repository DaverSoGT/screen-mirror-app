use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;
use thiserror::Error;

/// Maximum serialized size of one JSON ledger record.
pub const RECORD_BYTE_LIMIT: usize = 512;
/// Maximum retained records per recorder.
pub const RECORD_COUNT_LIMIT: usize = 20_000;
/// Maximum local monotonic retention window in microseconds.
pub const RETENTION_WINDOW_US: u64 = 120_000_000;

/// Evidence coverage for one independently observed boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Observed,
    ConfirmedZero,
    Unobserved,
    NotInstrumented,
}

impl Evidence {
    /// Combines independent evidence without upgrading incomplete coverage.
    #[must_use]
    pub const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::NotInstrumented, _) | (_, Self::NotInstrumented) => Self::NotInstrumented,
            (Self::Observed, Self::Observed) => Self::Observed,
            (Self::ConfirmedZero, Self::ConfirmedZero) => Self::ConfirmedZero,
            _ => Self::Unobserved,
        }
    }
}

/// Local clock domains remain separate so callers cannot infer cross-machine latency.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockDomain {
    SenderMonotonic,
    ReceiverMonotonic,
    BrowserPerformance,
    Media90khz,
    MediaSeconds,
}

/// One payload-free, correlatable ledger observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LedgerRecord {
    v: u8,
    session_id: String,
    epoch: u64,
    role: String,
    stage: String,
    event: String,
    evidence: Evidence,
    clock_domain: ClockDomain,
    mono_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Event fields that belong to one local ledger observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerEvent<'a> {
    /// Producer role that emitted the observation.
    pub role: &'a str,
    /// Lifecycle boundary observed by the producer.
    pub stage: &'a str,
    /// Event name at that boundary.
    pub event: &'a str,
    /// Coverage state for this observation.
    pub evidence: Evidence,
    /// Time in the record's explicitly named local clock domain.
    pub mono_us: u64,
}

impl LedgerRecord {
    /// Creates a payload-free record using a named local clock domain.
    #[must_use]
    pub fn new(
        session_id: &str,
        epoch: u64,
        clock_domain: ClockDomain,
        event: LedgerEvent<'_>,
    ) -> Self {
        Self {
            v: 1,
            session_id: session_id.to_owned(),
            epoch,
            role: event.role.to_owned(),
            stage: event.stage.to_owned(),
            event: event.event.to_owned(),
            evidence: event.evidence,
            clock_domain,
            mono_us: event.mono_us,
            reason: None,
        }
    }

    /// Returns the JSON serialization length without emitting or storing payload data.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        serde_json::to_vec(self).map_or(RECORD_BYTE_LIMIT + 1, |json| json.len())
    }

    /// Returns whether this is the recorder's terminal truncation record.
    #[must_use]
    pub fn is_truncation(&self) -> bool {
        self.event == "ledger_truncated"
    }

    fn fit_within_limit(&mut self) {
        while self.serialized_len() > RECORD_BYTE_LIMIT {
            if !self.trim_longest_text_field() {
                break;
            }
        }
    }

    fn trim_longest_text_field(&mut self) -> bool {
        let mut index = 0;
        let mut length = self.session_id.len();
        for (candidate_index, candidate_length) in [
            self.role.len(),
            self.stage.len(),
            self.event.len(),
            self.reason.as_ref().map_or(0, String::len),
        ]
        .into_iter()
        .enumerate()
        {
            if candidate_length > length {
                index = candidate_index + 1;
                length = candidate_length;
            }
        }
        if length == 0 {
            return false;
        }
        match index {
            0 => self.session_id.pop().is_some(),
            1 => self.role.pop().is_some(),
            2 => self.stage.pop().is_some(),
            3 => self.event.pop().is_some(),
            4 => self
                .reason
                .as_mut()
                .is_some_and(|reason| reason.pop().is_some()),
            _ => false,
        }
    }
}

/// In-memory bounded recorder for local diagnostic observations only.
#[derive(Debug, Default)]
pub struct BoundedRecorder {
    records: VecDeque<LedgerRecord>,
    dropped_records: u64,
}

impl BoundedRecorder {
    /// Creates an empty recorder with the v1 bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one observation, retaining only the configured count and time window.
    pub fn record(&mut self, mut record: LedgerRecord) {
        self.evict_expired(record.mono_us);
        record.fit_within_limit();

        if self.records.len() < RECORD_COUNT_LIMIT.saturating_sub(1) {
            self.records.push_back(record);
            return;
        }

        self.dropped_records = self.dropped_records.saturating_add(1);
        self.record_truncation(record.mono_us);
    }

    /// Returns retained records in their local observation order.
    #[must_use]
    pub fn records(&self) -> &VecDeque<LedgerRecord> {
        &self.records
    }

    fn evict_expired(&mut self, now_us: u64) {
        while self
            .records
            .front()
            .is_some_and(|record| record.mono_us.saturating_add(RETENTION_WINDOW_US) < now_us)
        {
            let _ = self.records.pop_front();
        }
    }

    fn record_truncation(&mut self, mono_us: u64) {
        let reason = format!("dropped_records={}", self.dropped_records);
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.is_truncation())
        {
            record.reason = Some(reason);
            return;
        }

        let mut truncation = LedgerRecord::new(
            "local",
            0,
            ClockDomain::SenderMonotonic,
            LedgerEvent {
                role: "ledger",
                stage: "recorder",
                event: "ledger_truncated",
                evidence: Evidence::Unobserved,
                mono_us,
            },
        );
        truncation.reason = Some(reason);
        truncation.fit_within_limit();
        self.records.push_back(truncation);
    }
}

/// A validated v1 SDP ledger activation marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerMarker {
    session_id: String,
    epoch: u64,
}

impl LedgerMarker {
    /// Constructs a v1 marker from a normalized SDP session identity and epoch.
    #[must_use]
    pub fn new(session_id: &str, epoch: u64) -> Self {
        Self {
            session_id: session_id.to_owned(),
            epoch,
        }
    }

    /// Parses `a=x-sm-qsv-ledger:1:<percent-encoded-session-id>:<epoch>`.
    pub fn parse(attribute: &str) -> Result<Self, MarkerError> {
        let Some(value) = attribute.strip_prefix("a=x-sm-qsv-ledger:") else {
            return Err(MarkerError::Malformed);
        };
        let mut parts = value.split(':');
        let (Some(version), Some(session_id), Some(epoch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(MarkerError::Malformed);
        };
        let version = version.parse::<u8>().map_err(|_| MarkerError::Malformed)?;
        if version != 1 {
            return Err(MarkerError::UnsupportedVersion(version));
        }
        let session_id = session_id.replace("%20", " ");
        if session_id.is_empty() {
            return Err(MarkerError::Malformed);
        }
        let epoch = epoch.parse::<u64>().map_err(|_| MarkerError::Malformed)?;
        Ok(Self::new(&session_id, epoch))
    }

    /// Validates that the marker belongs to the normalized offer session identity.
    pub fn validate_for_session(&self, session_id: &str) -> Result<(), MarkerError> {
        if self.session_id == session_id {
            Ok(())
        } else {
            Err(MarkerError::SessionMismatch)
        }
    }
}

/// Marker parsing and binding errors.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MarkerError {
    /// The attribute has no supported v1 shape.
    #[error("malformed ledger marker")]
    Malformed,
    /// The marker version is not supported by this recorder.
    #[error("unsupported ledger marker version {0}")]
    UnsupportedVersion(u8),
    /// The marker identity differs from the normalized SDP offer identity.
    #[error("ledger marker session does not match offer session")]
    SessionMismatch,
}

/// The RTP header fields used for payload-free correlation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpHeader {
    ssrc: u32,
    timestamp: u32,
    sequence: u16,
}

impl RtpHeader {
    /// Creates a header-only observation without retaining RTP payload bytes.
    #[must_use]
    pub const fn new(ssrc: u32, timestamp: u32, sequence: u16) -> Self {
        Self {
            ssrc,
            timestamp,
            sequence,
        }
    }
}

/// Correlation result for one RTP header observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpObservation {
    /// Monotonic occurrence within an SSRC, disambiguating timestamp reuse.
    pub occurrence: u64,
    /// Whether a previously seen timestamp returned after a different timestamp.
    pub ambiguous: bool,
}

#[derive(Default)]
struct RtpStreamState {
    last_timestamp: Option<u32>,
    occurrence: u64,
    seen_timestamps: HashSet<u32>,
}

/// Aggregates RTP headers by SSRC without storing packet payloads.
#[derive(Default)]
pub struct RtpHeaderAggregator {
    streams: HashMap<u32, RtpStreamState>,
}

impl RtpHeaderAggregator {
    /// Creates an empty header-only aggregator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observes one RTP header and marks timestamp reuse after a boundary as ambiguous.
    pub fn observe(&mut self, header: RtpHeader) -> RtpObservation {
        let state = self.streams.entry(header.ssrc).or_default();
        let timestamp_changed = state
            .last_timestamp
            .is_some_and(|last| last != header.timestamp);
        if timestamp_changed {
            state.occurrence = state.occurrence.saturating_add(1);
        }
        let ambiguous = timestamp_changed && state.seen_timestamps.contains(&header.timestamp);
        state.last_timestamp = Some(header.timestamp);
        let _ = state.seen_timestamps.insert(header.timestamp);
        let _ = header.sequence;
        RtpObservation {
            occurrence: state.occurrence,
            ambiguous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedRecorder, ClockDomain, Evidence, LedgerEvent, LedgerMarker, LedgerRecord,
        MarkerError, RtpHeader, RtpHeaderAggregator,
    };

    const SESSION_ID: &str = "1000 2";
    const EPOCH: u64 = 7;

    fn observed_record(mono_us: u64) -> LedgerRecord {
        LedgerRecord::new(
            SESSION_ID,
            EPOCH,
            ClockDomain::SenderMonotonic,
            LedgerEvent {
                role: "sender",
                stage: "output",
                event: "encoded",
                evidence: Evidence::Observed,
                mono_us,
            },
        )
    }

    #[test]
    fn marker_parser_accepts_one_supported_v1_marker() {
        let marker = LedgerMarker::parse("a=x-sm-qsv-ledger:1:1000%202:7").unwrap();

        assert_eq!(marker, LedgerMarker::new(SESSION_ID, EPOCH));
    }

    #[test]
    fn marker_parser_rejects_unsupported_versions() {
        let error = LedgerMarker::parse("a=x-sm-qsv-ledger:2:1000%202:7").unwrap_err();

        assert_eq!(error, MarkerError::UnsupportedVersion(2));
    }

    #[test]
    fn marker_validator_rejects_a_different_offer_session() {
        let marker = LedgerMarker::parse("a=x-sm-qsv-ledger:1:1000%202:7").unwrap();

        assert_eq!(
            marker.validate_for_session("1001 2"),
            Err(MarkerError::SessionMismatch)
        );
    }

    #[test]
    fn recorder_serializes_each_record_to_at_most_512_bytes() {
        let mut recorder = BoundedRecorder::new();
        let mut record = observed_record(1);
        record.reason = Some("x".repeat(4_000));

        recorder.record(record);

        assert!(recorder.records()[0].serialized_len() <= 512);
    }

    #[test]
    fn recorder_bounds_oversized_identity_fields_without_payload_capture() {
        let mut recorder = BoundedRecorder::new();
        let record = LedgerRecord::new(
            &"session".repeat(1_000),
            EPOCH,
            ClockDomain::SenderMonotonic,
            LedgerEvent {
                role: "sender",
                stage: "output",
                event: "encoded",
                evidence: Evidence::Observed,
                mono_us: 1,
            },
        );

        recorder.record(record);

        assert!(recorder.records()[0].serialized_len() <= 512);
    }

    #[test]
    fn recorder_keeps_at_most_twenty_thousand_records_and_reports_truncation() {
        let mut recorder = BoundedRecorder::new();
        for mono_us in 0..20_001 {
            recorder.record(observed_record(mono_us));
        }

        assert_eq!(recorder.records().len(), 20_000);
        assert!(recorder.records().iter().any(LedgerRecord::is_truncation));
    }

    #[test]
    fn recorder_evicts_records_older_than_the_one_hundred_twenty_second_retention_window() {
        let mut recorder = BoundedRecorder::new();
        recorder.record(observed_record(0));
        recorder.record(observed_record(120_000_001));

        assert_eq!(recorder.records().len(), 1);
    }

    #[test]
    fn evidence_algebra_preserves_confirmed_zero_when_all_sources_confirm_zero() {
        assert_eq!(
            Evidence::ConfirmedZero.combine(Evidence::ConfirmedZero),
            Evidence::ConfirmedZero
        );
    }

    #[test]
    fn evidence_algebra_marks_mixed_positive_and_zero_evidence_unobserved() {
        assert_eq!(
            Evidence::Observed.combine(Evidence::ConfirmedZero),
            Evidence::Unobserved
        );
    }

    #[test]
    fn evidence_algebra_propagates_not_instrumented_coverage() {
        assert_eq!(
            Evidence::Observed.combine(Evidence::NotInstrumented),
            Evidence::NotInstrumented
        );
    }

    #[test]
    fn clock_domains_serialize_as_distinct_local_labels() {
        let labels = [
            ClockDomain::SenderMonotonic,
            ClockDomain::ReceiverMonotonic,
            ClockDomain::BrowserPerformance,
            ClockDomain::Media90khz,
            ClockDomain::MediaSeconds,
        ]
        .map(|clock| serde_json::to_string(&clock).unwrap());

        assert_eq!(
            labels,
            [
                "\"sender_monotonic\"",
                "\"receiver_monotonic\"",
                "\"browser_performance\"",
                "\"media90khz\"",
                "\"media_seconds\""
            ]
        );
    }

    #[test]
    fn rtp_aggregator_keeps_one_occurrence_across_sequence_number_wrap() {
        let mut aggregator = RtpHeaderAggregator::new();
        aggregator.observe(RtpHeader::new(9, 44, u16::MAX));
        let observation = aggregator.observe(RtpHeader::new(9, 44, 0));

        assert_eq!(observation.occurrence, 0);
    }

    #[test]
    fn rtp_aggregator_marks_reused_timestamps_as_ambiguous_new_occurrences() {
        let mut aggregator = RtpHeaderAggregator::new();
        aggregator.observe(RtpHeader::new(9, 44, 10));
        aggregator.observe(RtpHeader::new(9, 45, 11));
        let observation = aggregator.observe(RtpHeader::new(9, 44, 12));

        assert_eq!((observation.occurrence, observation.ambiguous), (2, true));
    }
}
