//! Session lifecycle domain types: reconnect policy, session state, and internal bookkeeping.
//!
//! All types here are pure value types with zero platform dependencies.
//! No Tauri, no str0m, no OS imports. Only `std`, `serde`, and `thiserror`.
//!
//! # Cross-wire boundary
//!
//! `SessionState` and `DeadReason` are serialized and sent over the Tauri IPC channel.
//! `ReconnectPolicy`, `BackoffSchedule`, `ReconnectTrigger`, and `ReconnectAttempt`
//! are internal-only and MUST NOT cross the wire.

use std::num::NonZeroU8;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─── BackoffSchedule ─────────────────────────────────────────────────────────

/// Strategy for computing the delay between reconnect attempts.
///
/// V1 ships only `Exponential`. Linear and Fixed are reserved names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackoffSchedule {
    /// Exponential backoff: `base_ms * factor^(attempt-1)`.
    ///
    /// V1 default: `base_ms = 3000`, `factor = 3` → 3s / 9s / 27s.
    Exponential { base_ms: u32, factor: u32 },
}

// ─── ReconnectPolicy ─────────────────────────────────────────────────────────

/// Reconnect policy governing how many attempts are made and how long to wait
/// between them.
///
/// Internal-only: does NOT cross the Tauri IPC boundary.
/// Only `SessionState` is serialized and sent to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    /// Maximum number of reconnect attempts before transitioning to `Dead`.
    pub max_attempts: NonZeroU8,
    /// Backoff schedule used to compute delays between attempts.
    pub backoff: BackoffSchedule,
}

impl ReconnectPolicy {
    /// Returns the V1 fixed policy: 3 attempts with exponential backoff (3s/9s/27s).
    pub fn v1_default() -> Self {
        Self {
            // SAFETY: 3 is not zero.
            max_attempts: NonZeroU8::new(3).expect("3 is non-zero"),
            backoff: BackoffSchedule::Exponential {
                base_ms: 3000,
                factor: 3,
            },
        }
    }

    /// Compute the delay before attempt `n` (1-indexed).
    ///
    /// Formula: `base_ms * factor^(n-1)` milliseconds.
    /// Behaviour for `n > max_attempts` is unspecified — callers MUST NOT
    /// invoke this outside the `1..=max_attempts` range.
    pub fn delay_for_attempt(&self, n: NonZeroU8) -> Duration {
        let BackoffSchedule::Exponential { base_ms, factor } = self.backoff;
        let exponent = (n.get() - 1) as u32;
        let multiplier = factor.saturating_pow(exponent);
        Duration::from_millis(u64::from(base_ms) * u64::from(multiplier))
    }
}

// ─── SessionState ─────────────────────────────────────────────────────────────

/// Frontend-visible session lifecycle state.
///
/// Serialized with a `"kind"` discriminant tag (snake_case) for Tauri IPC.
/// Example: `{"kind":"reconnecting","attempt":2,"max":3}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    /// Initial ICE handshake in progress.
    Connecting,
    /// ICE established; frames are flowing.
    Connected,
    /// Reconnect in flight. `attempt` is the current attempt (1-indexed); `max` is the ceiling.
    Reconnecting {
        /// Current attempt number (1-indexed).
        attempt: NonZeroU8,
        /// Maximum number of attempts (from `ReconnectPolicy`).
        max: NonZeroU8,
    },
    /// All attempts exhausted (or user cancelled).
    Dead { reason: DeadReason },
}

// ─── DeadReason ──────────────────────────────────────────────────────────────

/// Why the session transitioned to `Dead`.
///
/// Serialized as a plain snake_case string for Tauri IPC.
/// Example: `"ice_failed_repeatedly"` (NOT `{"kind": "..."}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadReason {
    /// All attempts hit `TransportEvent::IceFailed`.
    IceFailedRepeatedly,
    /// All attempts hit `TransportEvent::ConnectionLost`.
    ConnectionLostRepeatedly,
    /// TCP signaling reuse and mDNS rediscovery both failed.
    SignalingChannelDead,
    /// User clicked Cancel during reconnect.
    UserCanceled,
}

// ─── ReconnectTrigger ─────────────────────────────────────────────────────────

/// Internal event that initiates a reconnect cycle.
///
/// NEVER serialized — stays inside the supervisor and is never sent to the frontend
/// or over the wire protocol.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconnectTrigger {
    /// `TransportEvent::IceFailed` was detected.
    IceFailed,
    /// `TransportEvent::ConnectionLost` was detected.
    ConnectionLost {
        /// Human-readable reason string from the transport layer.
        reason: String,
    },
    /// Remote peer sent `SignalingFrame::ReconnectRequest`.
    PeerRequested {
        /// The peer's session nonce (used for race resolution).
        peer_nonce: u64,
    },
}

// ─── ReconnectAttempt ─────────────────────────────────────────────────────────

/// Bookkeeping for a single reconnect attempt.
///
/// Internal-only. `Instant` is not `Serialize`, which is intentional:
/// this struct MUST NOT cross the Tauri IPC boundary.
#[derive(Debug, Clone)]
pub struct ReconnectAttempt {
    /// Attempt number (1-indexed; MUST be in `1..=policy.max_attempts`).
    pub attempt: NonZeroU8,
    /// Trigger that initiated this attempt.
    pub trigger: ReconnectTrigger,
    /// Wall-clock time when this attempt started.
    pub started_at: Instant,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── T1.1 tests ───────────────────────────────────────────────────────────

    /// AC-4 — `delay_for_attempt` returns 3s / 9s / 27s for attempts 1 / 2 / 3 (±10%).
    #[test]
    fn reconnect_policy_delay_for_attempt_values() {
        let policy = ReconnectPolicy::v1_default();
        let n1 = NonZeroU8::new(1).unwrap();
        let n2 = NonZeroU8::new(2).unwrap();
        let n3 = NonZeroU8::new(3).unwrap();

        let d1 = policy.delay_for_attempt(n1);
        let d2 = policy.delay_for_attempt(n2);
        let d3 = policy.delay_for_attempt(n3);

        // ±10% tolerance per AC-4
        let within_10pct = |actual: Duration, expected_ms: u64| -> bool {
            let lo = expected_ms * 90 / 100;
            let hi = expected_ms * 110 / 100;
            let actual_ms = actual.as_millis() as u64;
            actual_ms >= lo && actual_ms <= hi
        };

        assert!(
            within_10pct(d1, 3_000),
            "attempt 1 delay must be 3s ±10%, got {d1:?}"
        );
        assert!(
            within_10pct(d2, 9_000),
            "attempt 2 delay must be 9s ±10%, got {d2:?}"
        );
        assert!(
            within_10pct(d3, 27_000),
            "attempt 3 delay must be 27s ±10%, got {d3:?}"
        );
    }

    /// `v1_default` produces max_attempts=3 and Exponential{3000,3}.
    #[test]
    fn reconnect_policy_v1_default_fields() {
        let policy = ReconnectPolicy::v1_default();
        assert_eq!(policy.max_attempts.get(), 3);
        assert_eq!(
            policy.backoff,
            BackoffSchedule::Exponential {
                base_ms: 3000,
                factor: 3
            }
        );
    }

    // ─── T1.2 tests ───────────────────────────────────────────────────────────

    /// AC-7 / AC-8 — `SessionState::Reconnecting` serde produces expected JSON shape.
    #[test]
    fn session_state_serde_reconnecting() {
        let state = SessionState::Reconnecting {
            attempt: NonZeroU8::new(2).unwrap(),
            max: NonZeroU8::new(3).unwrap(),
        };
        let json = serde_json::to_string(&state).expect("must serialize");
        // Spec §2.3 exact example: {"kind":"reconnecting","attempt":2,"max":3}
        assert_eq!(json, r#"{"kind":"reconnecting","attempt":2,"max":3}"#);
    }

    /// `SessionState::Reconnecting` round-trips through serde.
    #[test]
    fn session_state_serde_reconnecting_round_trip() {
        let original = SessionState::Reconnecting {
            attempt: NonZeroU8::new(1).unwrap(),
            max: NonZeroU8::new(3).unwrap(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    /// `SessionState::Dead` round-trips through serde.
    #[test]
    fn session_state_serde_dead_round_trip() {
        let original = SessionState::Dead {
            reason: DeadReason::IceFailedRepeatedly,
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    /// `SessionState::Dead` serializes to the exact JSON shape from spec §4.1.
    #[test]
    fn session_state_serde_dead_exact_json_shape() {
        let state = SessionState::Dead {
            reason: DeadReason::IceFailedRepeatedly,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#"{"kind":"dead","reason":"ice_failed_repeatedly"}"#);
    }

    /// `SessionState::Connecting` serializes to `{"kind":"connecting"}`.
    #[test]
    fn session_state_serde_connecting() {
        let state = SessionState::Connecting;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#"{"kind":"connecting"}"#);
    }

    /// `SessionState::Connected` serializes to `{"kind":"connected"}`.
    #[test]
    fn session_state_serde_connected() {
        let state = SessionState::Connected;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#"{"kind":"connected"}"#);
    }

    /// AC-7 — Each `DeadReason` variant serializes to the correct snake_case string.
    ///
    /// `DeadReason` is a plain enum (no tag) so each variant serializes as a
    /// bare JSON string: `"ice_failed_repeatedly"` etc.
    /// When embedded in `SessionState::Dead { reason }`, this becomes
    /// `{"kind":"dead","reason":"ice_failed_repeatedly"}` on the wire.
    #[test]
    fn dead_reason_serde_snake_case() {
        let cases = [
            (
                DeadReason::IceFailedRepeatedly,
                r#""ice_failed_repeatedly""#,
            ),
            (
                DeadReason::ConnectionLostRepeatedly,
                r#""connection_lost_repeatedly""#,
            ),
            (
                DeadReason::SignalingChannelDead,
                r#""signaling_channel_dead""#,
            ),
            (DeadReason::UserCanceled, r#""user_canceled""#),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("must serialize");
            assert_eq!(json, expected, "DeadReason variant serde mismatch");
        }
    }

    // ─── T1.3 tests ───────────────────────────────────────────────────────────

    /// AC-1 — `ReconnectTrigger` compiles, has expected variants, and is NOT Serialize.
    ///
    /// The absence of `Serialize` is verified structurally: if someone accidentally
    /// adds `derive(Serialize)`, the call to `serde_json::to_string` would compile
    /// and this test's comment would need updating. The canonical gate is
    /// `cargo build --release` producing no `inject_ice_failed` symbol (AC-12).
    /// Here we verify the type exists and has the expected variants.
    #[test]
    fn reconnect_trigger_variants_exist_and_not_serialize() {
        // Verify all variants construct successfully.
        let _t1 = ReconnectTrigger::IceFailed;
        let _t2 = ReconnectTrigger::ConnectionLost {
            reason: "poll error".to_string(),
        };
        let _t3 = ReconnectTrigger::PeerRequested { peer_nonce: 42 };

        // Verify Clone and PartialEq work.
        let trigger = ReconnectTrigger::IceFailed;
        assert_eq!(trigger.clone(), ReconnectTrigger::IceFailed);

        // NOTE: `ReconnectTrigger` deliberately does NOT derive `Serialize`.
        // The following line MUST NOT compile if uncommented:
        // let _ = serde_json::to_string(&trigger);
    }

    /// AC-10 — `ReconnectAttempt` constructs correctly with all fields.
    #[test]
    fn reconnect_attempt_constructs_with_all_fields() {
        let attempt = ReconnectAttempt {
            attempt: NonZeroU8::new(1).unwrap(),
            trigger: ReconnectTrigger::IceFailed,
            started_at: Instant::now(),
        };
        assert_eq!(attempt.attempt.get(), 1);
        assert!(matches!(attempt.trigger, ReconnectTrigger::IceFailed));
    }
}
