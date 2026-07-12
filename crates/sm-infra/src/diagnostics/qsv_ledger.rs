use super::{
    BoundedRecorder, ClockDomain, Evidence, LedgerMarker, LedgerRecord, MarkerError,
    RtpHeader, RtpHeaderAggregator,
};

const SESSION_ID: &str = "1000 2";
const EPOCH: u64 = 7;

fn observed_record(mono_us: u64) -> LedgerRecord {
    LedgerRecord::new(
        SESSION_ID,
        EPOCH,
        "sender",
        "output",
        "encoded",
        Evidence::Observed,
        ClockDomain::SenderMonotonic,
        mono_us,
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
