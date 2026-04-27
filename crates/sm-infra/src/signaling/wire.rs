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
//! { "type": "Offer",     "sdp": "v=0 ..." }
//! { "type": "Answer",    "sdp": "v=0 ..." }
//! { "type": "Candidate", "sdp": "candidate:..." }
//! { "type": "Hello",     "proto": "v1" }
//! { "type": "Bye" }
//! ```
//!
//! Max frame body is 64 KiB. Frames larger than this return
//! [`io::ErrorKind::InvalidData`].

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Maximum allowed frame body size in bytes (64 KiB).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// A single message frame exchanged over the signaling TCP channel.
///
/// Serialised as a JSON tagged enum with `"type"` as the discriminant field.
/// The `sdp` fields carry **plain SDP text** — NOT JSON-wrapped strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingFrame {
    /// Initial handshake. `proto` MUST be `"v1"`.
    Hello { proto: String },
    /// SDP offer from the sender peer. `sdp` is plain SDP text.
    Offer { sdp: String },
    /// SDP answer from the receiver peer. `sdp` is plain SDP text.
    Answer { sdp: String },
    /// A trickled ICE candidate. `sdp` is the raw candidate attribute line.
    Candidate { sdp: String },
    /// Graceful close — both sides stop the TCP session after this frame.
    Bye,
}

/// Serialise `frame` and write it with a 4-byte big-endian length prefix.
///
/// Flushes the writer after writing so the bytes reach the peer in one batch.
pub fn write_frame<W: Write>(w: &mut W, frame: &SignalingFrame) -> io::Result<()> {
    let _ = (w, frame);
    unimplemented!("write_frame: not yet implemented")
}

/// Read a length-prefixed frame from `r`.
///
/// Returns `Err` with [`io::ErrorKind::InvalidData`] if the declared length
/// exceeds [`MAX_FRAME_BYTES`] or the body is not valid [`SignalingFrame`] JSON.
/// Returns `Err` with [`io::ErrorKind::UnexpectedEof`] on partial reads.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<SignalingFrame> {
    let _ = r;
    unimplemented!("read_frame: not yet implemented")
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
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must not fail for Offer");
        let decoded = read_frame(&mut Cursor::new(buf)).expect("read_frame must not fail");
        assert_eq!(decoded, frame, "Offer frame must survive a round-trip");
        // Verify inner SDP is plain text, not JSON-escaped.
        if let SignalingFrame::Offer { sdp: decoded_sdp } = decoded {
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
        let json = br#"{"type":"Offer","sdp":"v=0"}"#;
        let frame: SignalingFrame = serde_json::from_slice(json).expect("known JSON must decode");
        assert_eq!(
            frame,
            SignalingFrame::Offer {
                sdp: "v=0".to_string()
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
        };
        let json_str = serde_json::to_string(&frame).unwrap();
        assert!(json_str.contains("v=0"), "JSON must contain plain SDP text");
        assert!(
            !json_str.contains("\\\"v\\\""),
            "SDP must NOT be JSON-wrapped inside the frame"
        );
    }
}
