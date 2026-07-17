use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

use serde::Serialize;
use thiserror::Error;

/// Maximum serialized size of one JSON ledger record.
pub const RECORD_BYTE_LIMIT: usize = 512;
/// Maximum retained records per recorder.
pub const RECORD_COUNT_LIMIT: usize = 20_000;
/// Maximum local monotonic retention window in microseconds.
pub const RETENTION_WINDOW_US: u64 = 120_000_000;

const REASON_FIELD_JSON_OVERHEAD: usize = 12;
const MAX_JSON_BYTES_PER_REASON_BYTE: usize = 6;
const RTP_STREAM_LIMIT: usize = 256;
const RTP_TIMESTAMP_HISTORY_LIMIT: usize = 1_024;

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

    fn prepare_for_storage(&mut self) -> bool {
        let reason = self.reason.take();
        let identity_len = self.serialized_len();
        if identity_len > RECORD_BYTE_LIMIT {
            return false;
        }

        self.reason = reason.and_then(|reason| {
            let available = RECORD_BYTE_LIMIT
                .saturating_sub(identity_len)
                .saturating_sub(REASON_FIELD_JSON_OVERHEAD);
            let max_reason_bytes = available / MAX_JSON_BYTES_PER_REASON_BYTE;
            (max_reason_bytes > 0).then(|| Self::truncate_reason(&reason, max_reason_bytes))
        });
        true
    }

    fn truncate_reason(reason: &str, max_bytes: usize) -> String {
        if reason.len() <= max_bytes {
            return reason.to_owned();
        }

        let mut end = 0;
        for (offset, character) in reason.char_indices() {
            let next = offset + character.len_utf8();
            if next > max_bytes {
                break;
            }
            end = next;
        }
        reason[..end].to_owned()
    }
}

/// Result of submitting one record to a bounded recorder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOutcome {
    Recorded,
    RejectedOversizedIdentity,
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
    pub fn record(&mut self, mut record: LedgerRecord) -> RecordOutcome {
        self.evict_expired(record.mono_us);
        if !record.prepare_for_storage() {
            return RecordOutcome::RejectedOversizedIdentity;
        }

        if self.records.len() < RECORD_COUNT_LIMIT.saturating_sub(1) {
            self.records.push_back(record);
            return RecordOutcome::Recorded;
        }

        self.dropped_records = self.dropped_records.saturating_add(1);
        self.record_truncation(record.mono_us);
        RecordOutcome::Recorded
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
        let _ = truncation.prepare_for_storage();
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

/// Result of attempting to activate an observation-only transport ledger probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerActivation {
    /// The probe is attached to a validated canonical offer and epoch binding.
    Enabled,
    /// Observation is disabled without affecting media delivery.
    Disabled,
}

/// The RTP header fields used for payload-free correlation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpHeader {
    ssrc: u32,
    timestamp: u32,
    sequence: u16,
    observed_at_us: u64,
}

impl RtpHeader {
    /// Creates a header-only observation without retaining RTP payload bytes.
    #[must_use]
    pub const fn new(ssrc: u32, timestamp: u32, sequence: u16, observed_at_us: u64) -> Self {
        Self {
            ssrc,
            timestamp,
            sequence,
            observed_at_us,
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
    last_observed_us: u64,
    seen_timestamps: VecDeque<(u32, u64)>,
}

impl RtpStreamState {
    fn expire_timestamp_history(&mut self, now_us: u64) {
        while self
            .seen_timestamps
            .front()
            .is_some_and(|(_, observed_at_us)| {
                observed_at_us.saturating_add(RETENTION_WINDOW_US) < now_us
            })
        {
            let _ = self.seen_timestamps.pop_front();
        }
    }

    fn remember_timestamp(&mut self, timestamp: u32, observed_at_us: u64) {
        if let Some(index) = self
            .seen_timestamps
            .iter()
            .position(|(seen_timestamp, _)| *seen_timestamp == timestamp)
        {
            let _ = self.seen_timestamps.remove(index);
        }
        if self.seen_timestamps.len() == RTP_TIMESTAMP_HISTORY_LIMIT {
            let _ = self.seen_timestamps.pop_front();
        }
        self.seen_timestamps.push_back((timestamp, observed_at_us));
    }
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
        self.evict_expired_streams(header.observed_at_us);
        if !self.streams.contains_key(&header.ssrc) {
            self.evict_oldest_stream_to_limit();
        }
        let state = self.streams.entry(header.ssrc).or_default();
        state.expire_timestamp_history(header.observed_at_us);
        let timestamp_changed = state
            .last_timestamp
            .is_some_and(|last| last != header.timestamp);
        if timestamp_changed {
            state.occurrence = state.occurrence.saturating_add(1);
        }
        let ambiguous = timestamp_changed
            && state
                .seen_timestamps
                .iter()
                .any(|(timestamp, _)| *timestamp == header.timestamp);
        state.last_timestamp = Some(header.timestamp);
        state.last_observed_us = header.observed_at_us;
        state.remember_timestamp(header.timestamp, header.observed_at_us);
        let _ = header.sequence;
        RtpObservation {
            occurrence: state.occurrence,
            ambiguous,
        }
    }

    fn evict_expired_streams(&mut self, now_us: u64) {
        self.streams.retain(|_, state| {
            state.last_observed_us.saturating_add(RETENTION_WINDOW_US) >= now_us
        });
    }

    fn evict_oldest_stream_to_limit(&mut self) {
        if self.streams.len() < RTP_STREAM_LIMIT {
            return;
        }
        let oldest_ssrc = self
            .streams
            .iter()
            .min_by_key(|(ssrc, state)| (state.last_observed_us, **ssrc))
            .map(|(ssrc, _)| *ssrc);
        if let Some(ssrc) = oldest_ssrc {
            let _ = self.streams.remove(&ssrc);
        }
    }
}

/// Source-owned identity for one encoded access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceAuKey {
    session: Arc<str>,
    epoch: u64,
    source_sequence: u64,
    media_time_90khz: u64,
}

impl SourceAuKey {
    /// Creates a source identity with its mandatory session and media fields.
    #[must_use]
    pub fn new(
        session: impl Into<Arc<str>>,
        epoch: u64,
        source_sequence: u64,
        media_time_90khz: u64,
    ) -> Self {
        Self {
            session: session.into(),
            epoch,
            source_sequence,
            media_time_90khz,
        }
    }

    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub const fn source_sequence(&self) -> u64 {
        self.source_sequence
    }

    #[must_use]
    pub const fn media_time_90khz(&self) -> u64 {
        self.media_time_90khz
    }
}

/// RTP fields bound to a source access unit at a transport stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpBinding {
    ssrc: u32,
    rtp_timestamp: u32,
    occurrence: u64,
}

impl RtpBinding {
    #[must_use]
    pub const fn new(ssrc: u32, rtp_timestamp: u32, occurrence: u64) -> Self {
        Self {
            ssrc,
            rtp_timestamp,
            occurrence,
        }
    }

    #[must_use]
    pub const fn ssrc(self) -> u32 {
        self.ssrc
    }

    #[must_use]
    pub const fn rtp_timestamp(self) -> u32 {
        self.rtp_timestamp
    }

    #[must_use]
    pub const fn occurrence(self) -> u64 {
        self.occurrence
    }
}

/// Source and RTP bindings composed for a transport-owned access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessUnitIdentity {
    source: SourceAuKey,
    rtp: RtpBinding,
}

impl AccessUnitIdentity {
    #[must_use]
    pub fn new(source: SourceAuKey, rtp: RtpBinding) -> Self {
        Self { source, rtp }
    }

    #[must_use]
    pub fn source(&self) -> SourceAuKey {
        self.source.clone()
    }

    #[must_use]
    pub const fn rtp(&self) -> RtpBinding {
        self.rtp
    }

    /// Returns the bound RTP SSRC for compatibility with existing callers.
    #[must_use]
    pub const fn ssrc(&self) -> u32 {
        self.rtp.ssrc()
    }

    /// Returns the bound RTP timestamp for compatibility with existing callers.
    #[must_use]
    pub const fn rtp_timestamp(&self) -> u32 {
        self.rtp.rtp_timestamp()
    }

    /// Returns the bound RTP occurrence for compatibility with existing callers.
    #[must_use]
    pub const fn occurrence(&self) -> u64 {
        self.rtp.occurrence()
    }
}

macro_rules! stage_witness {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name {
            identity: AccessUnitIdentity,
        }

        impl $name {
            #[must_use]
            pub fn new(identity: AccessUnitIdentity) -> Self {
                Self { identity }
            }

            #[must_use]
            pub fn identity(&self) -> AccessUnitIdentity {
                self.identity.clone()
            }
        }
    };
}

/// A witness recorded after a source writer accepts an access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterWitness {
    source: SourceAuKey,
}

impl WriterWitness {
    #[must_use]
    pub fn new(source: SourceAuKey) -> Self {
        Self { source }
    }

    #[must_use]
    pub fn source(&self) -> SourceAuKey {
        self.source.clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingWriter {
    witness: WriterWitness,
    accepted_at_us: u64,
}

/// The result of adding a writer witness to pending correlation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingPushOutcome {
    pub retained: bool,
    pub stale_evicted: usize,
    pub cap_evicted: usize,
}

/// Bounded, single-owner pending state for source writer witnesses.
#[derive(Debug)]
pub struct PendingWriterFifo {
    entries: VecDeque<PendingWriter>,
    capacity: usize,
    stale_after_us: u64,
}

impl PendingWriterFifo {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_limits(capacity, RETENTION_WINDOW_US)
    }

    #[must_use]
    pub fn with_limits(capacity: usize, stale_after_us: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity,
            stale_after_us,
        }
    }

    pub fn push(&mut self, witness: WriterWitness, accepted_at_us: u64) -> PendingPushOutcome {
        let stale_evicted = self.sweep_stale(accepted_at_us);
        if self.capacity == 0 {
            return PendingPushOutcome {
                retained: false,
                stale_evicted,
                cap_evicted: 0,
            };
        }

        self.entries.push_back(PendingWriter {
            witness,
            accepted_at_us,
        });
        let mut cap_evicted = 0;
        while self.entries.len() > self.capacity {
            let _ = self.entries.pop_front();
            cap_evicted += 1;
        }

        PendingPushOutcome {
            retained: true,
            stale_evicted,
            cap_evicted,
        }
    }

    #[must_use]
    pub fn oldest_matching_source(
        &mut self,
        session: &str,
        epoch: u64,
        rtp_timestamp: u32,
        now_us: u64,
    ) -> Option<SourceAuKey> {
        self.sweep_stale(now_us);
        self.entries
            .iter()
            .find(|entry| {
                entry.witness.source.session() == session
                    && entry.witness.source.epoch() == epoch
                    && entry.witness.source.media_time_90khz() as u32 == rtp_timestamp
            })
            .map(|entry| entry.witness.source())
    }

    #[must_use]
    pub fn pending_sources(&self) -> Vec<SourceAuKey> {
        self.entries
            .iter()
            .map(|entry| entry.witness.source())
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn sweep_stale(&mut self, now_us: u64) -> usize {
        let original_len = self.entries.len();
        self.entries
            .retain(|entry| entry.accepted_at_us.saturating_add(self.stale_after_us) >= now_us);
        original_len - self.entries.len()
    }
}

stage_witness!(
    UdpTransmitWitness,
    "A witness recorded when UDP transmit owns an access unit."
);
stage_witness!(
    UdpReceiveWitness,
    "A witness recorded when UDP receive observes an access unit."
);
stage_witness!(
    CompletedAuWitness,
    "A witness recorded when a completed access unit is delivered."
);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LedgerPositions {
    writer: usize,
    udp_transmit: usize,
    udp_receive: usize,
    completed_au: usize,
}

#[derive(Debug, Default)]
struct ProbeState {
    attempts: [u64; 4],
    writer: Vec<WriterWitness>,
    udp_transmit: Vec<UdpTransmitWitness>,
    udp_receive: Vec<UdpReceiveWitness>,
    completed_au: Vec<CompletedAuWitness>,
}

#[derive(Debug)]
pub struct TransportLedgerProbe {
    collecting: bool,
    state: Mutex<ProbeState>,
}

impl TransportLedgerProbe {
    #[must_use]
    pub fn collecting() -> Self {
        Self::new(true)
    }

    #[must_use]
    pub fn rejecting() -> Self {
        Self::new(false)
    }

    fn new(collecting: bool) -> Self {
        Self {
            collecting,
            state: Mutex::new(ProbeState::default()),
        }
    }

    pub fn record_writer(&self, witness: WriterWitness) {
        let mut state = self.lock_state();
        state.attempts[0] = state.attempts[0].saturating_add(1);
        if self.collecting {
            state.writer.push(witness);
        }
    }

    pub fn record_udp_transmit(&self, witness: UdpTransmitWitness) {
        let mut state = self.lock_state();
        state.attempts[1] = state.attempts[1].saturating_add(1);
        if self.collecting {
            state.udp_transmit.push(witness);
        }
    }

    pub fn record_udp_receive(&self, witness: UdpReceiveWitness) {
        let mut state = self.lock_state();
        state.attempts[2] = state.attempts[2].saturating_add(1);
        if self.collecting {
            state.udp_receive.push(witness);
        }
    }

    pub fn record_completed_au(&self, witness: CompletedAuWitness) {
        let mut state = self.lock_state();
        state.attempts[3] = state.attempts[3].saturating_add(1);
        if self.collecting {
            state.completed_au.push(witness);
        }
    }

    #[must_use]
    pub fn positions(&self) -> LedgerPositions {
        let state = self.lock_state();
        LedgerPositions {
            writer: state.writer.len(),
            udp_transmit: state.udp_transmit.len(),
            udp_receive: state.udp_receive.len(),
            completed_au: state.completed_au.len(),
        }
    }

    #[must_use]
    pub fn exact_delta_since(&self, position: LedgerPositions) -> [usize; 4] {
        let state = self.lock_state();
        [
            state.writer.len().saturating_sub(position.writer),
            state
                .udp_transmit
                .len()
                .saturating_sub(position.udp_transmit),
            state.udp_receive.len().saturating_sub(position.udp_receive),
            state
                .completed_au
                .len()
                .saturating_sub(position.completed_au),
        ]
    }

    #[must_use]
    pub fn attempted_delta(&self) -> [u64; 4] {
        self.lock_state().attempts
    }

    #[must_use]
    pub fn writer_witnesses(&self) -> Vec<WriterWitness> {
        self.lock_state().writer.clone()
    }

    #[must_use]
    pub fn writer_witnesses_since(&self, position: LedgerPositions) -> Vec<WriterWitness> {
        let state = self.lock_state();
        state.writer[position.writer.min(state.writer.len())..].to_vec()
    }

    #[must_use]
    pub fn udp_transmit_witnesses(&self) -> Vec<UdpTransmitWitness> {
        self.lock_state().udp_transmit.clone()
    }

    #[must_use]
    pub fn udp_transmit_witnesses_since(
        &self,
        position: LedgerPositions,
    ) -> Vec<UdpTransmitWitness> {
        let state = self.lock_state();
        state.udp_transmit[position.udp_transmit.min(state.udp_transmit.len())..].to_vec()
    }

    #[must_use]
    pub fn udp_receive_witnesses(&self) -> Vec<UdpReceiveWitness> {
        self.lock_state().udp_receive.clone()
    }

    #[must_use]
    pub fn udp_receive_witnesses_since(&self, position: LedgerPositions) -> Vec<UdpReceiveWitness> {
        let state = self.lock_state();
        state.udp_receive[position.udp_receive.min(state.udp_receive.len())..].to_vec()
    }

    #[must_use]
    pub fn completed_au_witnesses(&self) -> Vec<CompletedAuWitness> {
        self.lock_state().completed_au.clone()
    }

    #[must_use]
    pub fn completed_au_witnesses_since(
        &self,
        position: LedgerPositions,
    ) -> Vec<CompletedAuWitness> {
        let state = self.lock_state();
        state.completed_au[position.completed_au.min(state.completed_au.len())..].to_vec()
    }

    fn lock_state(&self) -> MutexGuard<'_, ProbeState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccessUnitIdentity, BoundedRecorder, ClockDomain, CompletedAuWitness, Evidence,
        LedgerEvent, LedgerMarker, LedgerRecord, MarkerError, PendingPushOutcome,
        PendingWriterFifo, RECORD_BYTE_LIMIT, RETENTION_WINDOW_US, RTP_STREAM_LIMIT,
        RTP_TIMESTAMP_HISTORY_LIMIT, RecordOutcome, RtpBinding, RtpHeader, RtpHeaderAggregator,
        SourceAuKey, TransportLedgerProbe, UdpReceiveWitness, UdpTransmitWitness, WriterWitness,
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
    fn recorder_rejects_oversized_identity_without_truncating_correlation_fields() {
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

        let outcome = recorder.record(record);

        assert_eq!(outcome, RecordOutcome::RejectedOversizedIdentity);
        assert!(recorder.records().is_empty());
    }

    #[test]
    fn recorder_bounds_optional_reason_without_changing_identity_fields() {
        let mut recorder = BoundedRecorder::new();
        let mut record = observed_record(1);
        let identity = (
            record.session_id.clone(),
            record.epoch,
            record.role.clone(),
            record.stage.clone(),
            record.event.clone(),
        );
        record.reason = Some("\u{0000}".repeat(4_000));

        assert_eq!(recorder.record(record), RecordOutcome::Recorded);

        let recorded = &recorder.records()[0];
        assert_eq!(
            (
                recorded.session_id.as_str(),
                recorded.epoch,
                recorded.role.as_str(),
                recorded.stage.as_str(),
                recorded.event.as_str(),
            ),
            (
                identity.0.as_str(),
                identity.1,
                identity.2.as_str(),
                identity.3.as_str(),
                identity.4.as_str(),
            )
        );
        assert!(recorded.serialized_len() <= RECORD_BYTE_LIMIT);
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
        aggregator.observe(RtpHeader::new(9, 44, u16::MAX, 0));
        let observation = aggregator.observe(RtpHeader::new(9, 44, 0, 1));

        assert_eq!(observation.occurrence, 0);
    }

    #[test]
    fn rtp_aggregator_marks_reused_timestamps_as_ambiguous_new_occurrences() {
        let mut aggregator = RtpHeaderAggregator::new();
        aggregator.observe(RtpHeader::new(9, 44, 10, 0));
        aggregator.observe(RtpHeader::new(9, 45, 11, 1));
        let observation = aggregator.observe(RtpHeader::new(9, 44, 12, 2));

        assert_eq!((observation.occurrence, observation.ambiguous), (2, true));
    }

    #[test]
    fn rtp_aggregator_expires_streams_and_timestamp_history_at_retention_boundary() {
        let mut aggregator = RtpHeaderAggregator::new();
        aggregator.observe(RtpHeader::new(9, 44, 10, 0));
        aggregator.observe(RtpHeader::new(9, 45, 11, RETENTION_WINDOW_US + 1));
        let observation = aggregator.observe(RtpHeader::new(9, 44, 12, RETENTION_WINDOW_US + 2));

        assert_eq!(aggregator.streams.len(), 1);
        assert!(!observation.ambiguous);
    }

    #[test]
    fn rtp_aggregator_evicts_oldest_ssrc_and_bounds_timestamp_history() {
        let mut aggregator = RtpHeaderAggregator::new();
        for ssrc in 0..=RTP_STREAM_LIMIT as u32 {
            aggregator.observe(RtpHeader::new(ssrc, 0, 0, ssrc as u64));
        }
        for timestamp in 1..=RTP_TIMESTAMP_HISTORY_LIMIT as u32 {
            aggregator.observe(RtpHeader::new(
                99,
                timestamp,
                timestamp as u16,
                timestamp as u64,
            ));
        }

        assert_eq!(aggregator.streams.len(), RTP_STREAM_LIMIT);
        assert!(!aggregator.streams.contains_key(&0));
        assert!(aggregator.streams[&99].seen_timestamps.len() <= RTP_TIMESTAMP_HISTORY_LIMIT);
    }

    #[test]
    fn source_key_requires_session_epoch_sequence_and_media_time() {
        let source = SourceAuKey::new("session-a", 7, 41, 8_100);

        assert_eq!(source.session(), "session-a");
        assert_eq!(source.epoch(), 7);
        assert_eq!(source.source_sequence(), 41);
        assert_eq!(source.media_time_90khz(), 8_100);
    }

    fn writer(session: &str, epoch: u64, sequence: u64, media_time_90khz: u64) -> WriterWitness {
        WriterWitness::new(SourceAuKey::new(session, epoch, sequence, media_time_90khz))
    }

    #[test]
    fn pending_writer_fifo_returns_the_oldest_low_32_bit_timestamp_collision_without_consuming_it()
    {
        let first = SourceAuKey::new("session-a", 7, 41, 1);
        let second = SourceAuKey::new("session-a", 7, 42, (u32::MAX as u64) + 2);
        let mut pending = PendingWriterFifo::new(4);

        pending.push(WriterWitness::new(first.clone()), 10);
        pending.push(WriterWitness::new(second.clone()), 11);

        assert_eq!(
            pending.oldest_matching_source("session-a", 7, 1, 12),
            Some(first.clone())
        );
        assert_eq!(
            pending.oldest_matching_source("session-a", 7, 1, 12),
            Some(first)
        );
        assert_eq!(
            pending.pending_sources(),
            vec![SourceAuKey::new("session-a", 7, 41, 1), second]
        );
    }

    #[test]
    fn pending_writer_fifo_isolates_exact_session_and_epoch_while_matching_each_inserted_session() {
        let session_a = SourceAuKey::new("session-a", 7, 41, 9);
        let session_b = SourceAuKey::new("session-b", 7, 42, 9);
        let mut pending = PendingWriterFifo::new(4);

        pending.push(WriterWitness::new(session_a.clone()), 10);
        pending.push(WriterWitness::new(session_b.clone()), 11);

        assert_eq!(pending.oldest_matching_source("session-a", 8, 9, 12), None);
        assert_eq!(
            pending.oldest_matching_source("session-missing", 7, 9, 12),
            None
        );
        assert_eq!(
            pending.oldest_matching_source("session-b", 7, 9, 12),
            Some(session_b)
        );
        assert_eq!(
            pending.oldest_matching_source("session-a", 7, 9, 12),
            Some(session_a)
        );
    }

    #[test]
    fn pending_writer_fifo_preserves_equal_acceptance_time_insertion_order() {
        let first = SourceAuKey::new("session-a", 7, 41, 9);
        let second = SourceAuKey::new("session-a", 7, 42, 9);
        let mut pending = PendingWriterFifo::new(4);

        pending.push(WriterWitness::new(first.clone()), 10);
        pending.push(WriterWitness::new(second.clone()), 10);

        assert_eq!(pending.pending_sources(), vec![first, second]);
    }

    #[test]
    fn pending_writer_fifo_retains_the_stale_boundary() {
        let boundary = SourceAuKey::new("session-a", 7, 41, 1);
        let mut pending = PendingWriterFifo::with_limits(2, 10);

        pending.push(WriterWitness::new(boundary.clone()), 1);

        assert_eq!(
            pending.oldest_matching_source("session-a", 7, 1, 11),
            Some(boundary)
        );
    }

    #[test]
    fn pending_writer_fifo_stably_sweeps_a_stale_interior_entry() {
        let fresh_front = SourceAuKey::new("session-a", 7, 41, 1);
        let stale_middle = SourceAuKey::new("session-a", 7, 42, 2);
        let fresh_tail = SourceAuKey::new("session-a", 7, 43, 3);
        let new_tail = SourceAuKey::new("session-a", 7, 44, 4);
        let mut pending = PendingWriterFifo::with_limits(4, 10);

        pending.push(WriterWitness::new(fresh_front.clone()), 10);
        pending.push(WriterWitness::new(stale_middle), 0);
        pending.push(WriterWitness::new(fresh_tail.clone()), 5);
        let outcome = pending.push(WriterWitness::new(new_tail.clone()), 15);

        assert_eq!(outcome.stale_evicted, 1);
        assert_eq!(
            pending.pending_sources(),
            vec![fresh_front, fresh_tail, new_tail]
        );
    }

    #[test]
    fn pending_writer_fifo_evicts_the_global_oldest_entry_when_capacity_is_reached() {
        let first = SourceAuKey::new("session-a", 7, 41, 1);
        let second = SourceAuKey::new("session-a", 7, 42, 2);
        let mut pending = PendingWriterFifo::with_limits(2, u64::MAX);

        pending.push(WriterWitness::new(first), 10);
        pending.push(WriterWitness::new(second.clone()), 11);
        let outcome = pending.push(writer("session-a", 7, 43, 3), 12);

        assert_eq!(
            outcome,
            PendingPushOutcome {
                retained: true,
                stale_evicted: 0,
                cap_evicted: 1
            }
        );
        assert_eq!(
            pending.pending_sources(),
            vec![second, SourceAuKey::new("session-a", 7, 43, 3)]
        );
    }

    #[test]
    fn pending_writer_fifo_fails_open_for_zero_capacity_missing_and_stale_matches_without_payloads()
    {
        let mut disabled = PendingWriterFifo::new(0);

        assert_eq!(
            disabled.push(writer("session-a", 7, 41, 9), 10),
            PendingPushOutcome {
                retained: false,
                stale_evicted: 0,
                cap_evicted: 0
            }
        );
        assert!(disabled.is_empty());
        assert_eq!(disabled.oldest_matching_source("session-a", 7, 9, 10), None);

        let mut pending = PendingWriterFifo::with_limits(1, 10);
        pending.push(writer("session-a", 7, 41, 9), 0);

        assert_eq!(pending.oldest_matching_source("session-a", 7, 9, 11), None);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn access_unit_identity_composes_source_and_rtp_bindings() {
        let source = SourceAuKey::new("session-a", 7, 41, 8_100);
        let rtp = RtpBinding::new(99, 8_100, 0);
        let identity = AccessUnitIdentity::new(source.clone(), rtp);

        assert_eq!(identity.source(), source);
        assert_eq!(identity.rtp(), rtp);
        assert_eq!(identity.ssrc(), 99);
        assert_eq!(identity.rtp_timestamp(), 8_100);
        assert_eq!(identity.occurrence(), 0);
    }

    #[test]
    fn collecting_probe_keeps_stage_owned_witnesses_and_immutable_exact_deltas() {
        let probe = TransportLedgerProbe::collecting();
        let source = SourceAuKey::new("session-a", 7, 41, 8_100);
        let identity = AccessUnitIdentity::new(source.clone(), RtpBinding::new(99, 8_100, 0));
        let second_source = SourceAuKey::new("session-a", 7, 42, 8_200);
        let second_identity =
            AccessUnitIdentity::new(second_source.clone(), RtpBinding::new(99, 8_200, 1));
        let start = probe.positions();

        probe.record_writer(WriterWitness::new(source));
        let writer_snapshot = probe.writer_witnesses();
        let checkpoint = probe.positions();
        probe.record_writer(WriterWitness::new(second_source));
        probe.record_udp_transmit(UdpTransmitWitness::new(identity.clone()));
        probe.record_udp_receive(UdpReceiveWitness::new(identity.clone()));
        probe.record_completed_au(CompletedAuWitness::new(identity.clone()));

        assert_eq!(
            WriterWitness::new(identity.source()).source(),
            identity.source()
        );
        assert_eq!(
            UdpTransmitWitness::new(identity.clone()).identity(),
            identity
        );
        assert_eq!(
            UdpReceiveWitness::new(identity.clone()).identity(),
            identity
        );
        assert_eq!(
            CompletedAuWitness::new(identity.clone()).identity(),
            identity
        );
        assert_eq!(writer_snapshot, vec![WriterWitness::new(identity.source())]);
        assert_eq!(probe.exact_delta_since(start), [2, 1, 1, 1]);
        assert_eq!(probe.exact_delta_since(checkpoint), [1, 1, 1, 1]);
        assert_eq!(
            probe.writer_witnesses_since(checkpoint),
            vec![WriterWitness::new(second_identity.source())]
        );
        assert_eq!(
            probe.udp_transmit_witnesses_since(start),
            vec![UdpTransmitWitness::new(identity.clone())]
        );
        assert_eq!(
            probe.udp_receive_witnesses_since(start),
            vec![UdpReceiveWitness::new(identity.clone())]
        );
        assert_eq!(
            probe.completed_au_witnesses_since(start),
            vec![CompletedAuWitness::new(identity)]
        );
    }

    #[test]
    fn rejecting_probe_counts_two_attempts_per_stage_without_retaining_witnesses() {
        let probe = TransportLedgerProbe::rejecting();
        let source = SourceAuKey::new("session-a", 7, 41, 8_100);
        let identity = AccessUnitIdentity::new(source.clone(), RtpBinding::new(99, 8_100, 0));

        for _ in 0..2 {
            probe.record_writer(WriterWitness::new(source.clone()));
            probe.record_udp_transmit(UdpTransmitWitness::new(identity.clone()));
            probe.record_udp_receive(UdpReceiveWitness::new(identity.clone()));
            probe.record_completed_au(CompletedAuWitness::new(identity.clone()));
        }

        assert_eq!(probe.attempted_delta(), [2, 2, 2, 2]);
        assert!(probe.writer_witnesses().is_empty());
        assert!(probe.udp_transmit_witnesses().is_empty());
        assert!(probe.udp_receive_witnesses().is_empty());
        assert!(probe.completed_au_witnesses().is_empty());
    }
}
