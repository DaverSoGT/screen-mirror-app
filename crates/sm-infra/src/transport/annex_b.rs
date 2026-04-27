//! Annex-B / AVCC NAL unit helpers for the transport layer.
//!
//! The H.264 encoder outputs **Annex-B** byte streams: each NAL unit is prefixed with
//! a 3-byte (`00 00 01`) or 4-byte (`00 00 00 01`) start code. This module provides
//! helpers to:
//!
//! - Iterate over NAL units in an Annex-B stream (strips start codes, yields NAL payloads).
//! - Reconstruct an Annex-B stream from AVCC-framed data (receiver path, if needed).
//! - Detect whether a reconstructed stream contains an IDR NAL unit (keyframe detection).
//!
//! All functions are deterministic, allocation-free on the parsing side, and covered
//! by unit tests with golden H.264 bitstream fixtures.

// ─── Start-code detection ────────────────────────────────────────────────────

/// Location and length of an Annex-B start code in a byte slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartCode {
    /// Byte offset in the slice where the start code begins.
    pub offset: usize,
    /// Length of the start code in bytes (3 or 4).
    pub code_len: usize,
}

/// Find the next Annex-B start code (`00 00 00 01` or `00 00 01`) in `s`.
///
/// Returns `None` when no start code is present.
///
/// # RED stub — always returns None (implementation in task 4.2)
pub(crate) fn find_start_code(_s: &[u8]) -> Option<StartCode> {
    todo!("task 4.2: implement find_start_code")
}

// ─── NAL unit iterator ───────────────────────────────────────────────────────

/// Iterator over NAL units in an Annex-B byte stream.
pub(crate) struct NalIter<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let _ = (self.data, self.cursor);
        todo!("task 4.2: implement NalIter::next")
    }
}

/// Iterate over NAL units in an Annex-B byte stream.
///
/// # RED stub — unimplemented (task 4.2)
pub(crate) fn iter_nal_units(data: &[u8]) -> NalIter<'_> {
    NalIter { data, cursor: 0 }
}

// ─── AVCC reconstruction (receiver path) ────────────────────────────────────

/// Reconstruct an Annex-B stream from AVCC-framed data.
///
/// # RED stub — always returns empty (task 4.2)
pub(crate) fn reconstruct_annex_b(_data: &[u8]) -> Vec<u8> {
    todo!("task 4.2: implement reconstruct_annex_b")
}

// ─── IDR detection ───────────────────────────────────────────────────────────

/// Return `true` if the Annex-B stream contains at least one IDR NAL unit.
///
/// # RED stub — always returns false (task 4.2)
pub(crate) fn contains_idr_nal(_annex_b: &[u8]) -> bool {
    todo!("task 4.2: implement contains_idr_nal")
}

// ─── RTP timestamp conversion ────────────────────────────────────────────────

/// Convert a `Duration` to a 90 kHz RTP timestamp value (wrapping `u64`).
///
/// # RED stub — always returns 0 (task 4.2)
pub(crate) fn duration_to_90khz(_dur: std::time::Duration) -> u64 {
    todo!("task 4.2: implement duration_to_90khz")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── find_start_code ───────────────────────────────────────────────────────

    #[test]
    fn find_start_code_4byte() {
        let data = &[0x00, 0x00, 0x00, 0x01, 0x67];
        let sc = find_start_code(data).unwrap();
        assert_eq!(sc.offset, 0);
        assert_eq!(sc.code_len, 4);
    }

    #[test]
    fn find_start_code_3byte() {
        let data = &[0x00, 0x00, 0x01, 0x41];
        let sc = find_start_code(data).unwrap();
        assert_eq!(sc.offset, 0);
        assert_eq!(sc.code_len, 3);
    }

    #[test]
    fn find_start_code_not_at_start() {
        let data = &[0xAB, 0xCD, 0x00, 0x00, 0x00, 0x01, 0x65];
        let sc = find_start_code(data).unwrap();
        assert_eq!(sc.offset, 2);
        assert_eq!(sc.code_len, 4);
    }

    #[test]
    fn find_start_code_none() {
        let data = &[0x67, 0x42, 0x00, 0x1f];
        assert!(find_start_code(data).is_none());
    }

    #[test]
    fn find_start_code_empty() {
        assert!(find_start_code(&[]).is_none());
    }

    #[test]
    fn find_start_code_too_short() {
        assert!(find_start_code(&[0x00, 0x00]).is_none());
    }

    // ── iter_nal_units ────────────────────────────────────────────────────────

    /// S13.1 equivalent: single 4-byte start code, single NAL.
    #[test]
    fn iter_nal_units_single_4byte_sc() {
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67];
        let nals: Vec<_> = iter_nal_units(data).collect();
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x67]);
    }

    /// Single 3-byte start code, single NAL.
    #[test]
    fn iter_nal_units_single_3byte_sc() {
        let data: &[u8] = &[0x00, 0x00, 0x01, 0x41];
        let nals: Vec<_> = iter_nal_units(data).collect();
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0], &[0x41]);
    }

    /// Mixed 4-byte and 3-byte start codes (real OpenH264 output pattern).
    #[test]
    fn iter_nal_units_mixed_start_codes() {
        let data: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, // PPS
            0x00, 0x00, 0x01, 0x65, // IDR
        ];
        let nals: Vec<_> = iter_nal_units(data).collect();
        assert_eq!(nals.len(), 3, "expected 3 NALs, got {}", nals.len());
        assert_eq!(nals[0], &[0x67], "first NAL must be SPS");
        assert_eq!(nals[1], &[0x68], "second NAL must be PPS");
        assert_eq!(nals[2], &[0x65], "third NAL must be IDR");
    }

    /// Multi-byte NAL bodies.
    #[test]
    fn iter_nal_units_multi_byte_bodies() {
        let data: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, // SPS (4 bytes)
            0x00, 0x00, 0x01, 0x41, 0xDE, 0xAD, // P-slice (3 bytes)
        ];
        let nals: Vec<_> = iter_nal_units(data).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 0x42, 0x00, 0x1f]);
        assert_eq!(nals[1], &[0x41, 0xDE, 0xAD]);
    }

    /// Empty input yields no NALs.
    #[test]
    fn iter_nal_units_empty_input() {
        let nals: Vec<_> = iter_nal_units(&[]).collect();
        assert!(nals.is_empty());
    }

    /// No start code yields no NALs.
    #[test]
    fn iter_nal_units_no_start_code() {
        let data: &[u8] = &[0x67, 0x42, 0x00, 0x1f];
        let nals: Vec<_> = iter_nal_units(data).collect();
        assert!(nals.is_empty());
    }

    // ── reconstruct_annex_b ───────────────────────────────────────────────────

    /// Already-Annex-B data is returned verbatim (4-byte SC).
    #[test]
    fn reconstruct_annex_b_already_annexb_4byte() {
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0x42];
        let result = reconstruct_annex_b(data);
        assert_eq!(result, data);
    }

    /// Already-Annex-B data is returned verbatim (3-byte SC).
    #[test]
    fn reconstruct_annex_b_already_annexb_3byte() {
        let data: &[u8] = &[0x00, 0x00, 0x01, 0x41, 0xDE];
        let result = reconstruct_annex_b(data);
        assert_eq!(result, data);
    }

    /// AVCC-framed data: 4-byte length + NAL bytes → Annex-B start codes.
    /// S13.4: given a single-NAL AVCC payload, the result starts with [00 00 00 01].
    #[test]
    fn reconstruct_annex_b_from_avcc_single_nal() {
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x02, 0x67, 0x42];
        let result = reconstruct_annex_b(data);
        assert_eq!(&result[0..4], &[0x00, 0x00, 0x00, 0x01], "must start with Annex-B start code");
        assert_eq!(&result[4..], &[0x67, 0x42]);
    }

    /// AVCC with SPS + PPS + IDR (3 NAL units).
    #[test]
    fn reconstruct_annex_b_from_avcc_three_nals() {
        let mut data = Vec::new();
        for &nal in &[0x67u8, 0x68, 0x65] {
            data.extend_from_slice(&(1u32).to_be_bytes());
            data.push(nal);
        }
        let result = reconstruct_annex_b(&data);
        assert_eq!(result.len(), 15);
        assert_eq!(&result[0..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(result[4], 0x67);
        assert_eq!(&result[5..9], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(result[9], 0x68);
        assert_eq!(&result[10..14], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(result[14], 0x65);
    }

    /// Empty data returns empty result.
    #[test]
    fn reconstruct_annex_b_empty() {
        assert!(reconstruct_annex_b(&[]).is_empty());
    }

    // ── contains_idr_nal ──────────────────────────────────────────────────────

    /// S13.5: IDR NAL (type 5) detected correctly.
    #[test]
    fn contains_idr_nal_true_on_idr() {
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD];
        assert!(contains_idr_nal(data), "must detect IDR (type 5)");
    }

    /// P-slice NAL (type 1) is NOT a keyframe.
    #[test]
    fn contains_idr_nal_false_on_p_slice() {
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x41, 0xDE, 0xAD];
        assert!(!contains_idr_nal(data), "P-slice must NOT be IDR");
    }

    /// SPS + PPS + IDR in a single frame.
    #[test]
    fn contains_idr_nal_sps_pps_idr() {
        let data: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x67, // SPS (type 7)
            0x00, 0x00, 0x00, 0x01, 0x68, // PPS (type 8)
            0x00, 0x00, 0x00, 0x01, 0x65, // IDR (type 5)
        ];
        assert!(contains_idr_nal(data));
    }

    /// Empty data is NOT a keyframe.
    #[test]
    fn contains_idr_nal_empty() {
        assert!(!contains_idr_nal(&[]));
    }

    // ── duration_to_90khz (RTP timestamp) ────────────────────────────────────

    /// S13.1: timestamp 0 → rtp_ts 0.
    #[test]
    fn duration_to_90khz_zero() {
        assert_eq!(duration_to_90khz(std::time::Duration::ZERO), 0);
    }

    /// S13.2: timestamp 1 second → rtp_ts 90_000.
    #[test]
    fn duration_to_90khz_one_second() {
        assert_eq!(
            duration_to_90khz(std::time::Duration::from_secs(1)),
            90_000
        );
    }

    /// S13.3 / R13.6: overflow does NOT panic.
    #[test]
    fn duration_to_90khz_overflow_no_panic() {
        let secs = (u32::MAX as f64 / 90_000.0) as u64 + 1;
        let dur = std::time::Duration::from_secs(secs);
        let _ts = duration_to_90khz(dur);
    }

    /// Large timestamp (100_000 seconds) does not panic.
    #[test]
    fn duration_to_90khz_large_no_panic() {
        let dur = std::time::Duration::from_secs(100_000);
        let _ts = duration_to_90khz(dur);
    }
}
