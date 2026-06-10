//! Length-prefixed JSON signaling wire frames.
//!
//! [`SignalingFrame`] is the tagged-union message type exchanged over the TCP
//! control channel between two [`crate::signaling::mdns::MdnsSignaling`] peers.
//!
//! # Wire format
//!
//! ```text
//! +----+----+----+----+--------+
//! | 32-bit BE length  | UTF-8  |
//! +----+----+----+----+ JSON   |
//!                     | bytes  |
//!                     +--------+
//! ```
//!
//! The JSON schema uses a tagged enum with `"type"` as the discriminant:
//!
//! ```json
//! { "type": "Offer",          "sdp": "v=0 ..." }
//! { "type": "Answer",         "sdp": "v=0 ..." }
//! { "type": "Candidate",      "sdp": "candidate:..." }
//! { "type": "Hello",          "proto": "v1" }
//! { "type": "Bye" }
//! { "type": "ReconnectRequest", "attempt": 1, "requester_role": "Sender", "session_nonce": 12345678 }
//! { "type": "ReconnectAck",     "attempt": 1, "session_nonce": 12345678 }
//! ```
//!
//! Unknown `"type"` values are REJECTED with `InvalidData` (no `serde(other)` catch-all).
//! Both peers in a session MUST run the same build (V1 same-binary LAN deployment).
//!
//! Max frame body is 64 KiB. Frames larger than this return
//! [`io::ErrorKind::InvalidData`].

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use sm_domain::signaling::SignalingRole;

/// Maximum allowed frame body size in bytes (64 KiB).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// A single message frame exchanged over the signaling TCP channel.
///
/// Serialised as a JSON tagged enum with `"type"` as the discriminant field.
/// The `sdp` fields carry **plain SDP text** — NOT JSON-wrapped strings.
///
/// # Strict reject policy
///
/// Unknown `"type"` values return `Err(InvalidData)`. There is no `serde(other)` catch-all.
/// Both peers MUST be the same build (V1 same-binary LAN deployment — spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingFrame {
    /// Initial handshake. `proto` MUST be `"v1"`.
    Hello { proto: String },
    /// SDP offer from the sender peer. `sdp` is plain SDP text.
    ///
    /// `attempt` is the supervisor reconnect-attempt number at the time this Offer was
    /// published. It is a REQUIRED field — both peers MUST run the same build.
    /// A JSON payload that omits `attempt` MUST fail deserialization (REQ-GE-1, SC-GE-2).
    /// Mixed-version deserialization failure is the correct and accepted behavior.
    Offer {
        sdp: String,
        /// Supervisor attempt number at Offer publish time (1-indexed, matches supervisor
        /// attempt counter). REQUIRED — no `#[serde(default)]` or `Option<u8>`.
        attempt: u8,
    },
    /// SDP answer from the receiver peer. `sdp` is plain SDP text.
    Answer { sdp: String },
    /// A trickled ICE candidate. `sdp` is the raw candidate attribute line.
    Candidate { sdp: String },
    /// Graceful close — both sides stop the TCP session after this frame.
    Bye,
    /// Reconnect request from the detecting side.
    ///
    /// Published when `IceFailed` or `ConnectionLost` is detected. The
    /// `session_nonce` is used for tie-breaking when both sides detect
    /// simultaneously (lower nonce wins — spec §3.2).
    ReconnectRequest {
        /// Current attempt number (1-indexed, matches supervisor attempt counter).
        attempt: u8,
        /// Role of the side publishing this request.
        requester_role: SignalingRole,
        /// Nonce generated once per session lifetime; used for race resolution.
        session_nonce: u64,
    },
    /// Acknowledgment from the losing side in a simultaneous-detect race, or
    /// from the responding side in a one-sided detect.
    ReconnectAck {
        /// Echo of the attempt number from the `ReconnectRequest` being acknowledged.
        attempt: u8,
        /// Echo of the winner's `session_nonce`.
        session_nonce: u64,
    },
}

/// Serialise `frame` and write it with a 4-byte big-endian length prefix.
///
/// Flushes the writer after writing so the bytes reach the peer in one batch.
pub fn write_frame<W: Write>(w: &mut W, frame: &SignalingFrame) -> io::Result<()> {
    let body =
        serde_json::to_vec(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = body.len();
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_BYTES})"),
        ));
    }
    w.write_all(&(len as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read a length-prefixed frame from `r`.
///
/// Returns `Err` with [`io::ErrorKind::InvalidData`] if the declared length
/// exceeds [`MAX_FRAME_BYTES`] or the body is not valid [`SignalingFrame`] JSON.
/// Returns `Err` with [`io::ErrorKind::UnexpectedEof`] on partial reads.
///
/// On oversize length, the error message includes the raw 4-byte prefix in
/// hex and an ASCII rendering, so operators can identify spurious peers
/// (port scanners, mDNS auto-probers) that connect but don't speak our
/// protocol.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<SignalingFrame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        let ascii: String = len_buf
            .iter()
            .map(|b| {
                if (0x20..0x7f).contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame too large: declared {len} bytes (max {MAX_FRAME_BYTES}); raw prefix bytes: {:02x} {:02x} {:02x} {:02x} (\"{ascii}\")",
                len_buf[0], len_buf[1], len_buf[2], len_buf[3]
            ),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    // ─── Round-trip: Hello ────────────────────────────────────────────────────

    /// R7.3, S7.2 — SignalingFrame::Hello survives write_frame/read_frame.
    #[test]
    fn hello_frame_round_trip() {
        let frame = SignalingFrame::Hello {
            proto: "v1".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for Hello");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        assert_eq!(decoded, frame);
    }

    // ─── Round-trip: Offer ────────────────────────────────────────────────────

    /// R7.3, S7.2 — SignalingFrame::Offer with plain SDP text survives round-trip.
    #[test]
    fn offer_frame_round_trip_plain_sdp() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\nm=video 7889 RTP/AVP 96\r\n";
        let frame = SignalingFrame::Offer {
            sdp: sdp.to_string(),
            attempt: 1,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for Offer");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        assert_eq!(decoded, frame, "Offer frame must survive a round-trip");
        // Verify inner SDP is plain text, not JSON-escaped.
        if let SignalingFrame::Offer {
            sdp: decoded_sdp, ..
        } = decoded
        {
            assert!(
                !decoded_sdp.contains("\\\""),
                "inner SDP must be plain text, not JSON-escaped"
            );
        }
    }

    // ─── Round-trip: Answer ───────────────────────────────────────────────────

    /// R7.3 — SignalingFrame::Answer survives round-trip.
    #[test]
    fn answer_frame_round_trip() {
        let frame = SignalingFrame::Answer {
            sdp: "v=0\r\nm=video 9 RTP/SAVPF 96\r\n".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let decoded = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(decoded, frame);
    }

    // ─── Round-trip: Candidate ────────────────────────────────────────────────

    /// R7.3 — SignalingFrame::Candidate survives round-trip.
    #[test]
    fn candidate_frame_round_trip() {
        let frame = SignalingFrame::Candidate {
            sdp: "candidate:1 1 udp 2130706431 192.168.1.1 7889 typ host".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let decoded = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(decoded, frame);
    }

    // ─── Round-trip: Bye ──────────────────────────────────────────────────────

    /// R7.3 — SignalingFrame::Bye survives round-trip.
    #[test]
    fn bye_frame_round_trip() {
        let frame = SignalingFrame::Bye;
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let decoded = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(decoded, frame);
    }

    // ─── Serde: known JSON → frame (S7.2) ────────────────────────────────────

    /// S7.2 — Known JSON `{"type":"Offer","sdp":"v=0"}` decodes to Offer variant.
    #[test]
    fn known_json_decodes_to_offer_s7_2() {
        let json = br#"{"type":"Offer","sdp":"v=0","attempt":1}"#;
        let frame: SignalingFrame = serde_json::from_slice(json).expect("known JSON must decode");
        assert_eq!(
            frame,
            SignalingFrame::Offer {
                sdp: "v=0".to_string(),
                attempt: 1,
            }
        );
    }

    /// S7.2 — Known JSON Answer decodes correctly.
    #[test]
    fn known_json_decodes_to_answer_s7_2() {
        let json = br#"{"type":"Answer","sdp":"v=0\r\nm=video"}"#;
        let frame: SignalingFrame = serde_json::from_slice(json).expect("known JSON must decode");
        assert!(matches!(frame, SignalingFrame::Answer { .. }));
    }

    // ─── Length-prefix structural check ──────────────────────────────────────

    /// The first 4 bytes of a written frame MUST equal the body length (big-endian).
    #[test]
    fn write_frame_length_prefix_equals_body_length() {
        let frame = SignalingFrame::Bye;
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let declared_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(
            declared_len,
            buf.len() - 4,
            "length prefix must equal body length"
        );
    }

    // ─── Error: oversize declared length (read) ───────────────────────────────

    /// R7.3 — read_frame with declared length > MAX_FRAME_BYTES → InvalidData.
    #[test]
    fn read_frame_rejects_oversize_declared_length() {
        let oversized_len = (MAX_FRAME_BYTES + 1) as u32;
        let bytes = oversized_len.to_be_bytes().to_vec();
        let mut cursor = Cursor::new(bytes);
        let err = read_frame(&mut cursor).expect_err("must fail for oversize length");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Diagnostic: oversize-length error message must include the raw 4 bytes
    /// in hex and ASCII so operators can identify spurious peers (port scanners,
    /// mDNS auto-probers) that connect but don't speak our protocol.
    #[test]
    fn read_frame_oversize_error_includes_raw_bytes() {
        // 0x2D 0x66 0x02 0x3A — the bytes observed in B11 smoke (B11-S2).
        let prefix = [0x2D, 0x66, 0x02, 0x3A];
        let mut cursor = Cursor::new(prefix.to_vec());
        let err = read_frame(&mut cursor).expect_err("must fail for oversize length");
        let msg = err.to_string();
        assert!(
            msg.contains("2d 66 02 3a"),
            "error message must include hex bytes; got: {msg}"
        );
        assert!(
            msg.contains("\"-f.:\""),
            "error message must include ASCII rendering with non-printable as '.'; got: {msg}"
        );
    }

    // ─── Error: malformed JSON body (S7.3) ────────────────────────────────────

    /// S7.3 — read_frame on malformed JSON → InvalidData.
    #[test]
    fn read_frame_rejects_malformed_json() {
        let body = b"not valid json!!!";
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let err = read_frame(&mut Cursor::new(bytes)).expect_err("must fail for malformed JSON");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // ─── Error: unknown JSON frame type (S7.3) ────────────────────────────────

    /// S7.3 — read_frame on valid JSON with unknown `"type"` → InvalidData.
    #[test]
    fn read_frame_rejects_unknown_frame_type() {
        let body = br#"{"type":"UnknownType","value":"x"}"#;
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let err =
            read_frame(&mut Cursor::new(bytes)).expect_err("must fail for unknown frame type");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // ─── Error: truncated body ────────────────────────────────────────────────

    /// read_frame on body shorter than declared length → UnexpectedEof.
    #[test]
    fn read_frame_rejects_truncated_body() {
        let mut bytes: Vec<u8> = 100u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"xxxx"); // only 4 of 100 bytes
        let err = read_frame(&mut Cursor::new(bytes)).expect_err("must fail for truncated body");
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    // ─── SDP is plain text, not JSON-wrapped (B4 carry-over) ─────────────────

    /// Verifies that SDP text inside Offer frames is NOT JSON-wrapped.
    /// This is a B4 carry-over invariant: SDP is always plain text.
    #[test]
    fn offer_sdp_is_plain_text_not_json_wrapped() {
        let plain_sdp = "v=0\r\no=- 123 456 IN IP4 127.0.0.1\r\n";
        let frame = SignalingFrame::Offer {
            sdp: plain_sdp.to_string(),
            attempt: 1,
        };
        let json_str = serde_json::to_string(&frame).unwrap();
        assert!(json_str.contains("v=0"), "JSON must contain plain SDP text");
        assert!(
            !json_str.contains("\\\"v\\\""),
            "SDP must NOT be JSON-wrapped inside the frame"
        );
    }

    // ─── T3.1: ReconnectRequest and ReconnectAck round-trips ─────────────────

    /// AC-5 / AC-10 / T3.1 — `ReconnectRequest` survives write_frame/read_frame round-trip.
    #[test]
    fn reconnect_request_frame_round_trip() {
        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Sender,
            session_nonce: 12_345_678,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for ReconnectRequest");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        assert_eq!(decoded, frame);
    }

    /// Verify `ReconnectRequest` JSON discriminant is `"ReconnectRequest"` (spec §3.1).
    #[test]
    fn reconnect_request_frame_json_discriminant() {
        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Sender,
            session_nonce: 12_345_678,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains(r#""type":"ReconnectRequest""#),
            "ReconnectRequest must have type discriminant; got: {json}"
        );
        assert!(
            json.contains(r#""session_nonce":12345678"#),
            "ReconnectRequest must include session_nonce; got: {json}"
        );
    }

    /// AC-6 / T3.1 — `ReconnectAck` survives write_frame/read_frame round-trip.
    #[test]
    fn reconnect_ack_frame_round_trip() {
        let frame = SignalingFrame::ReconnectAck {
            attempt: 1,
            session_nonce: 12_345_678,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for ReconnectAck");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        assert_eq!(decoded, frame);
    }

    /// Verify `ReconnectAck` JSON discriminant is `"ReconnectAck"` (spec §3.1).
    #[test]
    fn reconnect_ack_frame_json_discriminant() {
        let frame = SignalingFrame::ReconnectAck {
            attempt: 2,
            session_nonce: 99,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            json.contains(r#""type":"ReconnectAck""#),
            "ReconnectAck must have type discriminant; got: {json}"
        );
        assert!(
            json.contains(r#""session_nonce":99"#),
            "ReconnectAck must include session_nonce; got: {json}"
        );
    }

    /// T3.1 — `ReconnectRequest` with `Receiver` role round-trips.
    #[test]
    fn reconnect_request_receiver_role_round_trip() {
        let frame = SignalingFrame::ReconnectRequest {
            attempt: 2,
            requester_role: SignalingRole::Receiver,
            session_nonce: 42,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        let decoded = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(decoded, frame);
    }

    // ─── SC-GE-1: Offer round-trip with attempt field ────────────────────────

    /// SC-GE-1 — `SignalingFrame::Offer` with `attempt` field survives write_frame/read_frame.
    ///
    /// REQ-GE-1: the `attempt` field is REQUIRED on Offer (not Option, not serde(default)).
    /// Both peers MUST run the same build.
    #[test]
    fn test_offer_round_trip_with_attempt() {
        let frame = SignalingFrame::Offer {
            sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".to_string(),
            attempt: 2,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for Offer with attempt");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        match decoded {
            SignalingFrame::Offer { sdp: _, attempt } => {
                assert_eq!(attempt, 2, "attempt must survive round-trip");
            }
            other => panic!("expected Offer, got {other:?}"),
        }
    }

    // ─── SC-GE-2: Missing attempt field fails deserialization ────────────────

    /// SC-GE-2 — Deserializing an Offer JSON without `attempt` MUST return Err.
    ///
    /// REQ-GE-1: the field is REQUIRED — omitting it is a wire-protocol error.
    /// Mixed-version deserialization failure is correct and expected.
    #[test]
    fn test_offer_missing_attempt_fails_deserialize() {
        let json = br#"{"type":"Offer","sdp":"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n"}"#;
        let result: Result<SignalingFrame, _> = serde_json::from_slice(json);
        assert!(
            result.is_err(),
            "Offer JSON without 'attempt' field MUST fail deserialization (REQ-GE-1)"
        );
    }

    /// T3.1 — Existing strict-reject behavior is preserved for `ReconnectRequest` and `ReconnectAck`
    /// being new known types. Unknown types still return `InvalidData`.
    /// Verifies spec §3.3 (no catch-all).
    #[test]
    fn read_frame_still_rejects_unknown_type_after_adding_reconnect_variants() {
        let body = br#"{"type":"UnknownType","value":"x"}"#;
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(body);
        let err = read_frame(&mut Cursor::new(bytes))
            .expect_err("must still reject unknown frame types after adding reconnect variants");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
