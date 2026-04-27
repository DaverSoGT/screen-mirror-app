//! Annex-B / AVCC NAL unit helpers for the transport layer.
//!
//! The H.264 encoder outputs **Annex-B** byte streams: each NAL unit is prefixed with
//! a 3-byte (`00 00 01`) or 4-byte (`00 00 00 01`) start code. This module provides
//! helpers to:
//!
//! - Iterate over NAL units in an Annex-B stream (strips start codes, yields NAL payloads).
//! - Reconstruct an Annex-B stream from AVCC-framed data (receiver path, safety net).
//! - Detect whether a reconstructed stream contains an IDR NAL unit (keyframe detection).
//!
//! # str0m 0.18 behaviour (resolved at apply time — OQ3)
//!
//! str0m's `H264Depacketizer` with `is_avc = false` (the **default**) outputs
//! Annex-B framing in `MediaData.data` directly — 4-byte start codes (`00 00 00 01`)
//! prepended to each NAL. The AVCC branch in `reconstruct_annex_b` is kept as a
//! safety net for potential future API changes but is NOT triggered in normal operation.
//!
//! All functions are deterministic, pure (no I/O), and fully unit-tested.

// ─── Lint configuration ──────────────────────────────────────────────────────
// These functions are pub(crate) helpers consumed by str0m_sender and
// str0m_receiver (tasks 4.4 and 4.6). The allow is removed once those modules
// import the helpers and clippy can verify end-to-end usage.
#![allow(dead_code)]

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
/// Scans left-to-right and returns the first match. When both a 3-byte and a 4-byte
/// start code overlap (e.g. `00 00 00 01`), the 4-byte form is reported.
///
/// Returns `None` when no start code is present.
pub(crate) fn find_start_code(s: &[u8]) -> Option<StartCode> {
    let len = s.len();
    if len < 3 {
        return None;
    }
    let mut i = 0;
    // We need at least i+2 to be a valid index.
    while i + 2 < len {
        if s[i] == 0x00 && s[i + 1] == 0x00 {
            // Check 4-byte form first to avoid reporting 00 00 01 when 00 00 00 01 is present.
            if i + 3 < len && s[i + 2] == 0x00 && s[i + 3] == 0x01 {
                return Some(StartCode {
                    offset: i,
                    code_len: 4,
                });
            }
            if s[i + 2] == 0x01 {
                // 3-byte start code: 00 00 01
                return Some(StartCode {
                    offset: i,
                    code_len: 3,
                });
            }
        }
        i += 1;
    }
    None
}

// ─── NAL unit iterator ───────────────────────────────────────────────────────

/// Iterator over NAL units in an Annex-B byte stream.
///
/// Each item is a slice of the NAL payload — the bytes **after** the start code,
/// up to (but not including) the next start code or end of stream.
pub(crate) struct NalIter<'a> {
    data: &'a [u8],
    /// Current scan position (absolute, within `data`).
    cursor: usize,
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        // Find the next start code from the current cursor position.
        let sc = find_start_code(&self.data[self.cursor..])?;
        let nal_begin = self.cursor + sc.offset + sc.code_len;

        if nal_begin >= self.data.len() {
            // Start code at the very end — nothing to yield.
            self.cursor = self.data.len();
            return None;
        }

        // Find the next start code to know where this NAL ends.
        let next_sc = find_start_code(&self.data[nal_begin..]);
        let nal_end = match next_sc {
            Some(s) => nal_begin + s.offset,
            None => self.data.len(),
        };

        // Advance cursor to nal_end so the next call finds the NEXT start code.
        self.cursor = nal_end;

        Some(&self.data[nal_begin..nal_end])
    }
}

/// Iterate over NAL units in an Annex-B byte stream.
///
/// Each yielded slice starts at the NAL header byte (immediately after the start code)
/// and extends to just before the next start code or EOF. Empty payloads are never
/// yielded.
///
/// Handles mixed 3-byte and 4-byte start codes (real OpenH264 output uses both).
pub(crate) fn iter_nal_units(data: &[u8]) -> NalIter<'_> {
    NalIter { data, cursor: 0 }
}

// ─── AVCC reconstruction (receiver path) ────────────────────────────────────

/// Reconstruct an Annex-B stream from potentially AVCC-framed data.
///
/// **Detection logic** (applied in order):
/// 1. If `data` starts with `00 00 00 01` or `00 00 01` → already Annex-B; return verbatim.
/// 2. Otherwise assume AVCC framing: 4-byte big-endian NAL length + raw NAL bytes.
///    Strip each length prefix, prepend `00 00 00 01`.
///
/// Malformed AVCC (length field points past the end of `data`) is handled gracefully —
/// parsing stops at the first bad entry and returns whatever was accumulated.
///
/// # Note
///
/// str0m 0.18 with default settings outputs Annex-B (branch 1 always taken in practice).
/// Branch 2 exists as a safety net.
pub(crate) fn reconstruct_annex_b(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    // Detect whether the input already has Annex-B framing.
    let already_annexb = (data.len() >= 4
        && data[0] == 0x00
        && data[1] == 0x00
        && data[2] == 0x00
        && data[3] == 0x01)
        || (data.len() >= 3 && data[0] == 0x00 && data[1] == 0x00 && data[2] == 0x01);

    if already_annexb {
        return data.to_vec();
    }

    // AVCC branch: 4-byte big-endian length + raw NAL bytes.
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut cursor = 0usize;
    while cursor + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([
            data[cursor],
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
        ]) as usize;
        cursor += 4;
        if cursor + nal_len > data.len() {
            // Malformed — stop gracefully.
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[cursor..cursor + nal_len]);
        cursor += nal_len;
    }
    out
}

// ─── IDR detection ───────────────────────────────────────────────────────────

/// Return `true` if the Annex-B stream contains at least one IDR NAL unit
/// (NAL type 5 — Instantaneous Decoding Refresh = keyframe).
///
/// Scans all NAL units yielded by [`iter_nal_units`] and checks the lower 5 bits
/// of the first byte (NAL unit type field per H.264 spec §7.3.1).
pub(crate) fn contains_idr_nal(annex_b: &[u8]) -> bool {
    iter_nal_units(annex_b).any(|nal| nal.first().is_some_and(|b| (b & 0x1F) == 5))
}

// ─── RTP timestamp conversion ────────────────────────────────────────────────

/// Convert a [`std::time::Duration`] to a 90 kHz RTP timestamp value.
///
/// Returns a `u64` suitable for `MediaTime::from_90khz(v)`. The value naturally
/// overflows for very large durations — this is correct RTP behaviour (R13.1 wrap).
///
/// # Wrapping guarantee
///
/// If the caller narrows the result to `u32`, the truncation is well-defined in
/// Rust (`as u32` truncates the low 32 bits). For durations up to ~47721 seconds
/// the result fits in `u32` without truncation; beyond that it wraps as expected.
pub(crate) fn duration_to_90khz(dur: std::time::Duration) -> u64 {
    (dur.as_secs_f64() * 90_000.0) as u64
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
        // Start code appears after some garbage bytes.
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
        // 00 00 00 01 | 67 (SPS header byte)
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
        // SPS (4-byte SC) | PPS (4-byte SC) | IDR (3-byte SC)
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
        // AVCC: 4-byte length (2) + NAL (67 42)
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x02, 0x67, 0x42];
        let result = reconstruct_annex_b(data);
        assert_eq!(
            &result[0..4],
            &[0x00, 0x00, 0x00, 0x01],
            "must start with Annex-B start code"
        );
        assert_eq!(&result[4..], &[0x67, 0x42]);
    }

    /// AVCC with SPS + PPS + IDR (3 NAL units).
    #[test]
    fn reconstruct_annex_b_from_avcc_three_nals() {
        // SPS (1 byte: 0x67), PPS (1 byte: 0x68), IDR (1 byte: 0x65)
        let mut data = Vec::new();
        for &nal in &[0x67u8, 0x68, 0x65] {
            data.extend_from_slice(&(1u32).to_be_bytes());
            data.push(nal);
        }
        let result = reconstruct_annex_b(&data);
        // Should have 3 × (4-byte SC + 1-byte NAL) = 15 bytes
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
        // 4-byte SC + IDR byte (0x65 = 0110 0101; type = 0x1F & 0x65 = 5)
        let data: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x65, 0xDE, 0xAD];
        assert!(contains_idr_nal(data), "must detect IDR (type 5)");
    }

    /// P-slice NAL (type 1) is NOT a keyframe.
    #[test]
    fn contains_idr_nal_false_on_p_slice() {
        // 4-byte SC + P-slice byte (0x41 = 0100 0001; type = 1)
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
        assert_eq!(duration_to_90khz(std::time::Duration::from_secs(1)), 90_000);
    }

    /// S13.3 / R13.6: overflow does NOT panic; produces a defined (wrapped) value.
    #[test]
    fn duration_to_90khz_overflow_no_panic() {
        // u32::MAX / 90_000 ≈ 47721 seconds. Add 1 second to force overflow past u32::MAX.
        let secs = (u32::MAX as f64 / 90_000.0) as u64 + 1;
        let dur = std::time::Duration::from_secs(secs);
        // Must not panic. Value will exceed u32::MAX (but fits in u64).
        let ts = duration_to_90khz(dur);
        // Should be > u32::MAX
        assert!(
            ts > u32::MAX as u64,
            "expected overflow past u32::MAX, got {ts}"
        );
    }

    /// Large timestamp (100_000 seconds) does not panic.
    #[test]
    fn duration_to_90khz_large_no_panic() {
        let dur = std::time::Duration::from_secs(100_000);
        let _ts = duration_to_90khz(dur);
    }
}
