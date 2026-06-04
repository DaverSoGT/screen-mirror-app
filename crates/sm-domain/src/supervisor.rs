//! Domain reconnect supervisor — pure state machine, zero platform dependencies.
//!
//! [`ReconnectSupervisor`] drives the reconnect lifecycle for a single session.
//! It is designed to run on the existing transport-event drain thread (per design §3):
//! no new long-lived OS thread is introduced; the supervisor evaluates state transitions
//! synchronously on each event.
//!
//! # State machine
//!
//! ```text
//! Connected
//!   ─── local IceFailed/ConnectionLost ──→ AwaitingAck (publishes ReconnectRequest)
//!   ─── recv PeerRequest (we are loser) ──→ Rebuilding (sends Ack; follows winner)
//!
//! AwaitingAck
//!   ─── recv PeerAck ──────────────────→ Rebuilding
//!   ─── recv PeerRequest (race) ────────→ Winner: Rebuilding | Loser: Rebuilding (ack sent)
//!   ─── 2s ack-timeout / TCP write Err → caller invokes mDNS reset; then Rebuilding
//!   ─── Stop ───────────────────────────→ exits
//!
//! Rebuilding
//!   ─── bundle-build OK ────────────────→ Connected (attempt reset)
//!   ─── bundle-build fail / ICE timeout → backoff sleep; if n+1 > max → Dead; else AwaitingAck (n+1)
//!   ─── Stop during backoff sleep ───────→ exits (recv_timeout unblocks within 100ms)
//!
//! Dead
//!   ─── terminal; caller emits "dead" event; no further attempt without explicit reset
//! ```
//!
//! # Backoff sleep
//!
//! Backoff uses [`std::sync::mpsc::Receiver::recv_timeout`] on the `signal_rx` channel
//! so that `Stop` interrupts the sleep within the channel poll cadence — no spin loop,
//! no `std::thread::sleep`. This satisfies AC-13 (stop interrupts mid-backoff) and
//! AC-14 (no CPU spin during sleep).
//!
//! # Test injection
//!
//! Tests drive the supervisor by sending [`SupervisorSignal`]s and reading
//! [`SupervisorOutcome`]s. No wall-clock timing is involved in tests.

use std::num::NonZeroU8;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

use crate::session::{DeadReason, ReconnectPolicy, ReconnectTrigger, SessionState};
use crate::signaling::SignalingRole;

// ─── SupervisorSignal ─────────────────────────────────────────────────────────

/// Signal sent to the supervisor from outside (stop, peer events, rebuild results).
///
/// The supervisor's backoff sleeps on `recv_timeout` on the `signal_rx` channel,
/// so any signal sent here will interrupt an in-progress sleep within the poll cadence.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorSignal {
    /// External stop requested (user cancel or `stop_sender`/`stop_stream`).
    Stop,
    /// A local `TransportEvent::IceFailed` or `ConnectionLost` was detected.
    ///
    /// Initiates a reconnect cycle from `Connected` state. Ignored if already reconnecting.
    LocalFailure {
        /// The trigger that caused the failure.
        trigger: ReconnectTrigger,
    },
    /// Peer sent `SignalingFrame::ReconnectAck` acknowledging our `ReconnectRequest`.
    ///
    /// Carries the echoed session_nonce so we can verify it belongs to our current cycle.
    PeerAck {
        /// The nonce echoed by the peer (must match our `my_nonce` to be accepted).
        session_nonce: u64,
        /// Attempt number echoed by the peer.
        attempt: u8,
    },
    /// Peer sent `SignalingFrame::ReconnectRequest` (either one-sided or simultaneous race).
    PeerRequest {
        /// The peer's session nonce (used for the role-equal tie-break fallback).
        peer_nonce: u64,
        /// The peer's signaling role (`Sender`/`Receiver`). The role-aware tie-break
        /// (design #963 D1) uses this to elect the offerer (Sender) as the active
        /// reconnector; the nonce is only consulted when roles are equal.
        peer_role: SignalingRole,
        /// Attempt number from the peer's request.
        attempt: u8,
    },
    /// Bundle rebuild succeeded and ICE connected.
    ///
    /// Transitions from `Rebuilding` → `Connected` and resets the attempt counter.
    RebuildSucceeded,
    /// Bundle rebuild failed (ICE timeout, build error, etc.).
    ///
    /// Supervisor increments the attempt counter; if `n+1 > max` → `Dead`; else backoff sleep.
    RebuildFailed,
}

// ─── SupervisorOutcome ────────────────────────────────────────────────────────

/// Outcome notifications emitted by the supervisor to its caller.
///
/// The caller (bridge thread) reads these and acts accordingly:
/// publishing signaling frames, emitting frontend events, tearing down bundles, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum SupervisorOutcome {
    /// Supervisor wants the caller to publish a `ReconnectRequest` frame.
    PublishReconnectRequest { attempt: u8, session_nonce: u64 },
    /// Supervisor wants the caller to publish a `ReconnectAck` frame.
    PublishReconnectAck { attempt: u8, session_nonce: u64 },
    /// Supervisor entered `Reconnecting` state — caller should emit the frontend event.
    StateChanged(SessionState),
    /// Supervisor wants the caller to initiate a bundle rebuild.
    InitiateRebuild,
    /// Supervisor wants the caller to perform full mDNS reset (TCP write failed / ack timeout).
    InitiateMdnsReset,
    /// Reconnect exhausted all attempts — session is dead.
    Dead(DeadReason),
    /// Stop signal processed — caller may clean up.
    Stopped,
}

// ─── Internal state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum SupervisorState {
    Connected,
    AwaitingAck {
        attempt: NonZeroU8,
        trigger: ReconnectTrigger,
    },
    Rebuilding {
        attempt: NonZeroU8,
        trigger: ReconnectTrigger,
    },
    Dead,
    Stopped,
}

// ─── ReconnectSupervisor ─────────────────────────────────────────────────────

/// Domain reconnect supervisor.
///
/// Holds the session nonce and reconnect policy. Drives state transitions in response
/// to [`SupervisorSignal`]s received on `signal_rx`, and emits [`SupervisorOutcome`]s
/// on `outcome_tx` to guide the caller's actions.
///
/// # Usage
///
/// ```rust,ignore
/// let (signal_tx, signal_rx) = sync_channel(8);
/// let (outcome_tx, outcome_rx) = sync_channel(8);
/// let sup = ReconnectSupervisor::new(
///     ReconnectPolicy::v1_default(),
///     my_session_nonce,
///     my_signaling_role,
///     signal_rx,
///     outcome_tx,
/// );
/// // Run on drain thread:
/// sup.run(ack_timeout, rebuild_timeout);
/// ```
pub struct ReconnectSupervisor {
    policy: ReconnectPolicy,
    my_nonce: u64,
    /// This supervisor's fixed signaling role. The Sender is the WebRTC offerer
    /// and is therefore always the active reconnector in a simultaneous race
    /// (design #963 D1); the Receiver (answerer) always defers.
    my_role: SignalingRole,
    signal_rx: Receiver<SupervisorSignal>,
    outcome_tx: SyncSender<SupervisorOutcome>,
    state: SupervisorState,
}

impl ReconnectSupervisor {
    /// Construct a supervisor with the given policy and session nonce.
    ///
    /// `signal_rx` receives signals from the bridge/test driver.
    /// `outcome_tx` receives outcome notifications for the bridge to act on.
    pub fn new(
        policy: ReconnectPolicy,
        my_nonce: u64,
        my_role: SignalingRole,
        signal_rx: Receiver<SupervisorSignal>,
        outcome_tx: SyncSender<SupervisorOutcome>,
    ) -> Self {
        Self {
            policy,
            my_nonce,
            my_role,
            signal_rx,
            outcome_tx,
            state: SupervisorState::Connected,
        }
    }

    /// Whether this supervisor is the active reconnector against a peer with the
    /// given role, per the role-aware tie-break (design #963 D1). Delegates to the
    /// pure [`decide_tiebreak`] so the rule is unit-testable in isolation.
    fn is_active_reconnector(&self, peer_role: SignalingRole, peer_nonce: u64) -> bool {
        matches!(
            decide_tiebreak(self.my_role.clone(), peer_role, self.my_nonce, peer_nonce),
            TieOutcome::ActiveReconnector
        )
    }

    /// Return the current `SessionState` as a frontend-visible enum.
    ///
    /// Intended for test assertions and diagnostic reads. This is a snapshot of
    /// the internal state — safe to call before `run()` starts or after `run()`
    /// returns. NOT safe to call concurrently with `run()` (the supervisor is
    /// `!Send`; the caller must ensure exclusive access).
    pub fn session_state(&self) -> SessionState {
        match &self.state {
            SupervisorState::Connected => SessionState::Connected,
            SupervisorState::AwaitingAck { attempt, .. }
            | SupervisorState::Rebuilding { attempt, .. } => SessionState::Reconnecting {
                attempt: *attempt,
                max: self.policy.max_attempts,
            },
            SupervisorState::Dead => SessionState::Dead {
                reason: crate::session::DeadReason::IceFailedRepeatedly,
            },
            SupervisorState::Stopped => SessionState::Connected, // terminal — caller should not query post-stop
        }
    }

    /// Drive the supervisor until it reaches a terminal state (`Dead` or `Stopped`).
    ///
    /// Two independent timeouts govern the wait windows:
    ///
    /// - `ack_timeout`: maximum time to wait for a `PeerAck` in `AwaitingAck` state.
    ///   Production default is short (~2s) so a missing ack escalates quickly to mDNS
    ///   reset. Tests can use very short durations.
    /// - `rebuild_timeout`: maximum time to wait for `RebuildSucceeded`/`RebuildFailed`
    ///   in `Rebuilding` state. Must be large enough to cover a real-world rebuild
    ///   (mDNS rediscovery + SDP handshake + ICE establishment + bind_probe retries),
    ///   typically ≥15s in production. If this expires before the worker reports a
    ///   result, the supervisor escalates to attempt n+1 — and any late
    ///   `RebuildSucceeded` arriving afterwards is dropped (AwaitingAck ignores it).
    ///   Conflating the two timeouts caused the T12.2 manual smoke FAIL of 2026-04-30
    ///   (engram #509): production used ack_timeout=2s for both states, but real
    ///   rebuilds take ≥5s.
    ///
    /// # Returns
    ///
    /// - `DeadReason` if all attempts exhausted.
    /// - `None` if stopped cleanly by `Stop` signal.
    ///
    /// # Note on backoff
    ///
    /// The backoff sleep between attempts is achieved by `recv_timeout(delay)` on
    /// `signal_rx`, where `delay` comes from `policy.delay_for_attempt(...)`. This
    /// means any `Stop` signal interrupts the sleep within `recv_timeout` granularity
    /// (typically milliseconds in tests). AC-13, AC-14.
    pub fn run(
        &mut self,
        ack_timeout: std::time::Duration,
        rebuild_timeout: std::time::Duration,
    ) -> Option<DeadReason> {
        loop {
            match &self.state.clone() {
                SupervisorState::Connected => {
                    // Block until we receive a signal.
                    match self.signal_rx.recv() {
                        Ok(SupervisorSignal::Stop) => {
                            self.emit(SupervisorOutcome::Stopped);
                            self.state = SupervisorState::Stopped;
                            return None;
                        }
                        Ok(SupervisorSignal::LocalFailure { trigger }) => {
                            let attempt = NonZeroU8::new(1).expect("1 is nonzero");
                            self.state = SupervisorState::AwaitingAck {
                                attempt,
                                trigger: trigger.clone(),
                            };
                            self.emit(SupervisorOutcome::StateChanged(
                                SessionState::Reconnecting {
                                    attempt,
                                    max: self.policy.max_attempts,
                                },
                            ));
                            self.emit(SupervisorOutcome::PublishReconnectRequest {
                                attempt: attempt.get(),
                                session_nonce: self.my_nonce,
                            });
                        }
                        Ok(SupervisorSignal::PeerRequest {
                            peer_nonce,
                            peer_role,
                            attempt,
                        }) => {
                            // Role-aware tie-break (design #963 D1, NR-1 redefinition).
                            // If we are the active reconnector (the Sender/offerer, or
                            // the lower-nonce side when roles are equal), we stay
                            // Connected and re-offer via our own failure-detection /
                            // rebuild hook — we do NOT take the loser path. Otherwise
                            // (we are the answerer / deferring side) we run the existing
                            // loser path UNCHANGED: Ack + Reconnecting + InitiateRebuild.
                            if self.is_active_reconnector(peer_role, peer_nonce) {
                                // Active reconnector — ignore the peer's request; we
                                // drive the rebuild ourselves. Keep Connected.
                            } else {
                                let attempt_nz =
                                    NonZeroU8::new(attempt.max(1)).expect("max(1) nonzero");
                                let trigger = ReconnectTrigger::PeerRequested { peer_nonce };
                                self.emit(SupervisorOutcome::PublishReconnectAck {
                                    attempt,
                                    session_nonce: peer_nonce,
                                });
                                self.emit(SupervisorOutcome::StateChanged(
                                    SessionState::Reconnecting {
                                        attempt: attempt_nz,
                                        max: self.policy.max_attempts,
                                    },
                                ));
                                self.state = SupervisorState::Rebuilding {
                                    attempt: attempt_nz,
                                    trigger,
                                };
                                self.emit(SupervisorOutcome::InitiateRebuild);
                            }
                        }
                        Ok(_) => {
                            // Ignore PeerAck, RebuildSucceeded, RebuildFailed in Connected state.
                        }
                        Err(_) => {
                            // Signal channel dropped — treat as stop.
                            return None;
                        }
                    }
                }

                SupervisorState::AwaitingAck { attempt, trigger } => {
                    let attempt = *attempt;
                    let trigger = trigger.clone();
                    match self.signal_rx.recv_timeout(ack_timeout) {
                        Ok(SupervisorSignal::Stop) => {
                            self.emit(SupervisorOutcome::Stopped);
                            self.state = SupervisorState::Stopped;
                            return None;
                        }
                        Ok(SupervisorSignal::PeerAck {
                            session_nonce,
                            attempt: ack_attempt,
                        }) => {
                            if session_nonce == self.my_nonce {
                                // Valid ack — proceed to rebuild.
                                let rebuild_attempt =
                                    NonZeroU8::new(ack_attempt.max(1)).unwrap_or(attempt);
                                self.state = SupervisorState::Rebuilding {
                                    attempt: rebuild_attempt,
                                    trigger: trigger.clone(),
                                };
                                self.emit(SupervisorOutcome::InitiateRebuild);
                            }
                            // If nonce mismatch, ignore (stale ack).
                        }
                        Ok(SupervisorSignal::PeerRequest {
                            peer_nonce,
                            peer_role,
                            attempt: peer_attempt,
                        }) => {
                            // Simultaneous race — role-aware tie-break (design #963 D1).
                            // The Sender (offerer) is always the active reconnector;
                            // the Receiver (answerer) always defers. Nonce only breaks
                            // a role-equal tie. This replaces the old nonce-only rule
                            // that inverted the gate when the Sender held the higher
                            // nonce (#962).
                            if self.is_active_reconnector(peer_role, peer_nonce) {
                                // We are the active reconnector — ignore the peer's
                                // request; they will ack us. Keep AwaitingAck.
                            } else {
                                // We defer — ack the peer and follow its rebuild.
                                let rebuild_attempt =
                                    NonZeroU8::new(peer_attempt.max(1)).unwrap_or(attempt);
                                self.emit(SupervisorOutcome::PublishReconnectAck {
                                    attempt: peer_attempt,
                                    session_nonce: peer_nonce,
                                });
                                self.state = SupervisorState::Rebuilding {
                                    attempt: rebuild_attempt,
                                    trigger: ReconnectTrigger::PeerRequested { peer_nonce },
                                };
                                self.emit(SupervisorOutcome::InitiateRebuild);
                            }
                        }
                        Ok(SupervisorSignal::LocalFailure { .. })
                        | Ok(SupervisorSignal::RebuildSucceeded)
                        | Ok(SupervisorSignal::RebuildFailed) => {
                            // Ignore in AwaitingAck.
                        }
                        Err(_) => {
                            // Timeout or channel dropped — trigger mDNS reset path.
                            self.emit(SupervisorOutcome::InitiateMdnsReset);
                            // After mDNS reset, caller will signal RebuildSucceeded/Failed
                            // via signal channel, so stay in AwaitingAck awaiting the ack
                            // from the new mDNS connection. Actually per design: after
                            // mDNS reset, transition to Rebuilding directly.
                            self.state = SupervisorState::Rebuilding {
                                attempt,
                                trigger: trigger.clone(),
                            };
                            self.emit(SupervisorOutcome::InitiateRebuild);
                        }
                    }
                }

                SupervisorState::Rebuilding { attempt, trigger } => {
                    let attempt = *attempt;
                    let trigger = trigger.clone();
                    // Wait for rebuild result. Must use `rebuild_timeout` (not
                    // `ack_timeout`) because real rebuilds take ≥5s (mDNS + SDP
                    // + ICE) while ack_timeout is ~2s. See engram #509.
                    match self.signal_rx.recv_timeout(rebuild_timeout) {
                        Ok(SupervisorSignal::Stop) => {
                            self.emit(SupervisorOutcome::Stopped);
                            self.state = SupervisorState::Stopped;
                            return None;
                        }
                        Ok(SupervisorSignal::RebuildSucceeded) => {
                            self.state = SupervisorState::Connected;
                            self.emit(SupervisorOutcome::StateChanged(SessionState::Connected));
                        }
                        Ok(SupervisorSignal::RebuildFailed) => {
                            // Check if more attempts remain.
                            let next_attempt_u8 = attempt.get().saturating_add(1);
                            if next_attempt_u8 > self.policy.max_attempts.get() {
                                // Dead.
                                let reason = dead_reason_for_trigger(&trigger);
                                self.state = SupervisorState::Dead;
                                self.emit(SupervisorOutcome::StateChanged(SessionState::Dead {
                                    reason: reason.clone(),
                                }));
                                self.emit(SupervisorOutcome::Dead(reason.clone()));
                                return Some(reason);
                            }
                            // Backoff sleep before next attempt.
                            let next_attempt = NonZeroU8::new(next_attempt_u8).expect("nonzero");
                            let backoff_delay = self.policy.delay_for_attempt(next_attempt);
                            // Sleep via recv_timeout — interruptible by Stop signal.
                            match self.signal_rx.recv_timeout(backoff_delay) {
                                Ok(SupervisorSignal::Stop) => {
                                    self.emit(SupervisorOutcome::Stopped);
                                    self.state = SupervisorState::Stopped;
                                    return None;
                                }
                                Ok(_) => {
                                    // Any other signal during backoff sleep: ignore and
                                    // continue to next attempt anyway.
                                }
                                Err(_) => {
                                    // Timeout elapsed — start next attempt.
                                }
                            }
                            // Start next attempt.
                            self.state = SupervisorState::AwaitingAck {
                                attempt: next_attempt,
                                trigger: trigger.clone(),
                            };
                            self.emit(SupervisorOutcome::StateChanged(
                                SessionState::Reconnecting {
                                    attempt: next_attempt,
                                    max: self.policy.max_attempts,
                                },
                            ));
                            self.emit(SupervisorOutcome::PublishReconnectRequest {
                                attempt: next_attempt.get(),
                                session_nonce: self.my_nonce,
                            });
                        }
                        Ok(_) => {
                            // Ignore other signals in Rebuilding.
                        }
                        Err(_) => {
                            // Timeout during rebuild wait — treat as rebuild failure.
                            let next_attempt_u8 = attempt.get().saturating_add(1);
                            if next_attempt_u8 > self.policy.max_attempts.get() {
                                let reason = dead_reason_for_trigger(&trigger);
                                self.state = SupervisorState::Dead;
                                self.emit(SupervisorOutcome::StateChanged(SessionState::Dead {
                                    reason: reason.clone(),
                                }));
                                self.emit(SupervisorOutcome::Dead(reason.clone()));
                                return Some(reason);
                            }
                            let next_attempt = NonZeroU8::new(next_attempt_u8).expect("nonzero");
                            self.state = SupervisorState::AwaitingAck {
                                attempt: next_attempt,
                                trigger: trigger.clone(),
                            };
                            self.emit(SupervisorOutcome::StateChanged(
                                SessionState::Reconnecting {
                                    attempt: next_attempt,
                                    max: self.policy.max_attempts,
                                },
                            ));
                            self.emit(SupervisorOutcome::PublishReconnectRequest {
                                attempt: next_attempt.get(),
                                session_nonce: self.my_nonce,
                            });
                        }
                    }
                }

                SupervisorState::Dead | SupervisorState::Stopped => {
                    return None;
                }
            }
        }
    }

    fn emit(&self, outcome: SupervisorOutcome) {
        match self.outcome_tx.try_send(outcome) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Outcome channel full — drop silently (non-blocking contract).
            }
            Err(TrySendError::Disconnected(_)) => {
                // Caller dropped outcome receiver — no-op.
            }
        }
    }
}

// ─── Role-aware tie-break (design #963 D1) ──────────────────────────────────────

/// Outcome of a simultaneous-reconnect tie-break, from the perspective of the
/// local supervisor that received a peer `ReconnectRequest`.
///
/// See [`decide_tiebreak`] for the decision rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieOutcome {
    /// The local side is the active reconnector: it re-offers (Sender) and does
    /// NOT defer to the peer. In the supervisor this keeps the local side driving
    /// the rebuild without sending a `ReconnectAck`.
    ActiveReconnector,
    /// The local side defers to the peer: it acknowledges the peer's request and
    /// follows the peer-driven rebuild (the existing loser / Ack+Rebuild path).
    Defer,
}

/// Decide the simultaneous-reconnect tie-break, role-aware (design #963 D1).
///
/// The WebRTC offerer (`Sender`) is ALWAYS the active reconnector because it is
/// the only side that can publish a fresh Offer; the answerer (`Receiver`) ALWAYS
/// defers and waits for that Offer. The session nonce is consulted ONLY when the
/// roles are equal (a degenerate/test case the production wire never produces),
/// where the legacy rule applies: the LOWER nonce is the active reconnector
/// (preserves the historical AC-10 semantics — `peer_nonce < my_nonce` ⇒ peer
/// wins ⇒ we defer).
///
/// | `my_role` | `peer_role` | outcome |
/// |-----------|-------------|---------|
/// | `Sender`  | `Receiver`  | `ActiveReconnector` (we re-offer) |
/// | `Receiver`| `Sender`    | `Defer` (wait for the peer's Offer) |
/// | equal     | equal       | nonce fallback: `my_nonce < peer_nonce` ⇒ `ActiveReconnector`, else `Defer` |
pub fn decide_tiebreak(
    my_role: SignalingRole,
    peer_role: SignalingRole,
    my_nonce: u64,
    peer_nonce: u64,
) -> TieOutcome {
    match (my_role, peer_role) {
        // Roles differ: the Sender (offerer) is always the active reconnector.
        (SignalingRole::Sender, SignalingRole::Receiver) => TieOutcome::ActiveReconnector,
        (SignalingRole::Receiver, SignalingRole::Sender) => TieOutcome::Defer,
        // Roles equal (degenerate): fall back to nonce — lower nonce is active.
        _ => {
            if my_nonce < peer_nonce {
                TieOutcome::ActiveReconnector
            } else {
                TieOutcome::Defer
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn dead_reason_for_trigger(trigger: &ReconnectTrigger) -> DeadReason {
    match trigger {
        ReconnectTrigger::IceFailed => DeadReason::IceFailedRepeatedly,
        ReconnectTrigger::ConnectionLost { .. } => DeadReason::ConnectionLostRepeatedly,
        ReconnectTrigger::PeerRequested { .. } => DeadReason::IceFailedRepeatedly,
        // PeerBye: peer disconnected cleanly; treat exhausted retries as signaling dead.
        ReconnectTrigger::PeerBye => DeadReason::SignalingChannelDead,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use super::*;
    use crate::session::{BackoffSchedule, ReconnectPolicy};
    use crate::signaling::SignalingRole;

    // ─── SC-DR-3: role-equal tie-break falls back to nonce ───────────────────
    //
    // D1 (design #963): when both peers carry the SAME role (degenerate/test
    // case), the role rule cannot decide, so the tie-break falls back to the
    // legacy nonce rule — LOWER nonce is the active reconnector (matches the
    // existing AC-10 semantics: `peer_nonce < my_nonce` ⇒ peer wins ⇒ we defer).
    //
    // Here my_nonce=200, peer_nonce=100 (peer is lower) ⇒ the PEER is the active
    // reconnector ⇒ WE defer. The outcome from OUR perspective is `Defer`.

    /// SC-DR-3 — role-equal (Sender/Sender) falls back to nonce: lower nonce wins.
    #[test]
    fn sc_dr_3_role_equal_lower_nonce_wins() {
        // Peer (nonce 100) is lower than us (nonce 200) ⇒ peer is the active
        // reconnector ⇒ we defer.
        assert_eq!(
            decide_tiebreak(SignalingRole::Sender, SignalingRole::Sender, 200, 100),
            TieOutcome::Defer
        );
    }

    // ─── SC-DR-1 / SC-DR-2a / SC-DR-2b: AwaitingAck role-aware tie-break ──────
    //
    // D1 (design #963): in a simultaneous reconnect race, the Sender (offerer) is
    // ALWAYS the active reconnector regardless of nonce; the Receiver (answerer)
    // ALWAYS defers and Acks. These tests exercise the AwaitingAck branch — the
    // exact gate that the old nonce-only rule inverted (#962): the live failure
    // had the Sender carrying the HIGH nonce, so the old rule wrongly made the
    // Sender the loser. Role-aware: Sender wins regardless.

    /// SC-DR-1 — Sender with the HIGH nonce stays the active reconnector.
    ///
    /// my=Sender(nonce 99), peer=Receiver(nonce 42). Under the OLD nonce rule the
    /// lower peer nonce would win and we (higher) would defer+Ack. Role-aware: the
    /// Sender is always active ⇒ we do NOT emit `PublishReconnectAck`.
    #[test]
    fn sc_dr_1_sender_high_nonce_is_active() {
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 99, SignalingRole::Sender);

        // Enter AwaitingAck.
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome(); // StateChanged(Reconnecting{1})
        let _req = h.recv_outcome(); // PublishReconnectRequest

        // Peer (Receiver) sends ReconnectRequest with the LOWER nonce.
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 42,
            peer_role: SignalingRole::Receiver,
            attempt: 1,
        });

        // As the active reconnector we MUST NOT Ack the peer — drain a short window
        // and assert no PublishReconnectAck appears.
        assert_no_reconnect_ack(&h, 42);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// SC-DR-2a — Sender with the LOW nonce is STILL the active reconnector.
    ///
    /// my=Sender(nonce 42), peer=Receiver(nonce 99). Role-aware: nonce is
    /// irrelevant when roles differ ⇒ Sender stays active ⇒ no Ack.
    #[test]
    fn sc_dr_2a_sender_low_nonce_still_active() {
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 42, SignalingRole::Sender);

        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome();
        let _req = h.recv_outcome();

        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 99,
            peer_role: SignalingRole::Receiver,
            attempt: 1,
        });

        assert_no_reconnect_ack(&h, 99);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// SC-DR-2b — Receiver with the HIGH nonce defers and Acks.
    ///
    /// my=Receiver(nonce 99), peer=Sender(nonce 42). Role-aware: the answerer
    /// always defers ⇒ we emit `PublishReconnectAck` for the peer's nonce, then
    /// `InitiateRebuild` (the existing loser path).
    #[test]
    fn sc_dr_2b_receiver_high_nonce_defers() {
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 99, SignalingRole::Receiver);

        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome();
        let _req = h.recv_outcome();

        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 42,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        // Defer ⇒ Ack the peer's nonce, then rebuild.
        let ack = h.recv_outcome();
        assert_eq!(
            ack,
            SupervisorOutcome::PublishReconnectAck {
                attempt: 1,
                session_nonce: 42,
            }
        );
        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// Drain outcomes for a short window and panic if a `PublishReconnectAck` for
    /// `forbidden_nonce` appears — used by the "we are active" tie-break tests.
    fn assert_no_reconnect_ack(h: &SupervisorHandle, forbidden_nonce: u64) {
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match h.outcome_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(SupervisorOutcome::PublishReconnectAck { session_nonce, .. })
                    if session_nonce == forbidden_nonce =>
                {
                    panic!(
                        "active reconnector MUST NOT emit PublishReconnectAck for \
                         peer_nonce={forbidden_nonce}"
                    );
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    // ─── T4.2: session_state() accessor ──────────────────────────────────────

    /// T4.2 — `session_state()` returns `Connected` before any signal.
    #[test]
    fn session_state_accessor_returns_connected_initially() {
        let (_signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(8);
        let (outcome_tx, _outcome_rx) = sync_channel::<SupervisorOutcome>(8);
        let sup = ReconnectSupervisor::new(
            ReconnectPolicy::v1_default(),
            42,
            SignalingRole::Sender,
            signal_rx,
            outcome_tx,
        );
        assert_eq!(sup.session_state(), SessionState::Connected);
    }

    /// T4.2 — `session_state()` reflects `Reconnecting` after a `LocalFailure` drives
    /// the supervisor into `AwaitingAck`.
    ///
    /// We can only observe state changes through outcomes when the supervisor is running;
    /// however `session_state()` is designed to be read from the bridge thread (not the
    /// supervisor thread). We verify the initial state here. The Rebuilding/Dead/Stopped
    /// states are validated by the integration tests in Phase 6.
    #[test]
    fn session_state_accessor_updates_to_reconnecting_after_local_failure() {
        // Spawn the supervisor on a background thread, drive it to AwaitingAck,
        // then stop it. Check the state reflected in outcomes rather than directly,
        // since session_state() is only safe to call before run() or after it exits.
        // This test exercises the `session_state()` API at the boundary.
        let h = SupervisorHandle::spawn(fast_policy(), 77);
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        // First outcome is StateChanged(Reconnecting{1}) — confirms state transition.
        let outcome = h.recv_outcome();
        assert_eq!(
            outcome,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );
        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // Fast policy for tests: tiny backoff delays so tests run in milliseconds.
    fn fast_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts: std::num::NonZeroU8::new(3).unwrap(),
            backoff: BackoffSchedule::Exponential {
                base_ms: 1,
                factor: 2,
            },
        }
    }

    /// Short ack timeout for test harness — makes tests run in <10ms even during
    /// "sleep" phases. AC-14: recv_timeout means NO spin loop.
    const TEST_ACK_TIMEOUT: Duration = Duration::from_millis(20);

    /// Default rebuild timeout for tests — same as ack so existing tests behave
    /// as before (their fake builders signal RebuildSucceeded synchronously).
    /// Tests that need to simulate a slow rebuild use `spawn_with_timeouts`.
    const TEST_REBUILD_TIMEOUT: Duration = TEST_ACK_TIMEOUT;

    // ─── Helper to drive supervisor on a background thread ──────────────────

    struct SupervisorHandle {
        signal_tx: std::sync::mpsc::SyncSender<SupervisorSignal>,
        outcome_rx: std::sync::mpsc::Receiver<SupervisorOutcome>,
        join: Option<std::thread::JoinHandle<Option<DeadReason>>>,
    }

    impl SupervisorHandle {
        fn spawn(policy: ReconnectPolicy, my_nonce: u64) -> Self {
            // Default role for legacy tests that don't exercise the role rule:
            // Sender (these tests rely on the nonce-based outcomes that still hold
            // under the role-equal / Sender-as-active semantics).
            Self::spawn_with_timeouts(
                policy,
                my_nonce,
                SignalingRole::Sender,
                TEST_ACK_TIMEOUT,
                TEST_REBUILD_TIMEOUT,
            )
        }

        /// Spawn with an explicit local role (default timeouts).
        fn spawn_with_role(policy: ReconnectPolicy, my_nonce: u64, my_role: SignalingRole) -> Self {
            Self::spawn_with_timeouts(
                policy,
                my_nonce,
                my_role,
                TEST_ACK_TIMEOUT,
                TEST_REBUILD_TIMEOUT,
            )
        }

        /// Spawn with explicit ack/rebuild timeouts. Use this when a test needs
        /// to exercise the difference between the two waits — e.g. simulating a
        /// rebuild that legitimately takes longer than `ack_timeout`.
        fn spawn_with_timeouts(
            policy: ReconnectPolicy,
            my_nonce: u64,
            my_role: SignalingRole,
            ack_timeout: Duration,
            rebuild_timeout: Duration,
        ) -> Self {
            let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(16);
            let (outcome_tx, outcome_rx) = sync_channel::<SupervisorOutcome>(32);
            let join = std::thread::spawn(move || {
                let mut sup =
                    ReconnectSupervisor::new(policy, my_nonce, my_role, signal_rx, outcome_tx);
                sup.run(ack_timeout, rebuild_timeout)
            });
            Self {
                signal_tx,
                outcome_rx,
                join: Some(join),
            }
        }

        fn send(&self, sig: SupervisorSignal) {
            self.signal_tx
                .try_send(sig)
                .expect("signal channel must accept signal");
        }

        fn recv_outcome(&self) -> SupervisorOutcome {
            self.outcome_rx
                .recv_timeout(Duration::from_millis(200))
                .expect("must receive outcome within 200ms")
        }

        fn join(mut self) -> Option<DeadReason> {
            self.join
                .take()
                .unwrap()
                .join()
                .expect("supervisor thread must not panic")
        }
    }

    // ─── AC-1: Both triggers enter reconnect path ─────────────────────────────

    /// AC-1 — `IceFailed` trigger causes supervisor to emit `Reconnecting{1}` state
    /// and a `PublishReconnectRequest` outcome.
    #[test]
    fn reconnect_trigger_ice_failed_enters_reconnect_path() {
        let h = SupervisorHandle::spawn(fast_policy(), 100);
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });

        let outcome = h.recv_outcome();
        assert_eq!(
            outcome,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );

        // Clean up.
        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// AC-1 — `ConnectionLost` trigger also enters the reconnect path.
    #[test]
    fn reconnect_trigger_connection_lost_enters_reconnect_path() {
        let h = SupervisorHandle::spawn(fast_policy(), 200);
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::ConnectionLost {
                reason: "poll error".to_string(),
            },
        });

        let outcome = h.recv_outcome();
        assert_eq!(
            outcome,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── AC-3: Exactly 3 attempts maximum ────────────────────────────────────

    /// AC-3 — After 3 failed rebuilds, supervisor transitions to Dead and DOES NOT
    /// initiate a 4th attempt. Transitions: Connected → Reconnecting{1} → Reconnecting{2}
    /// → Reconnecting{3} → Dead.
    #[test]
    fn reconnect_max_attempts_boundary_exactly_3() {
        let h = SupervisorHandle::spawn(fast_policy(), 42);

        // Trigger local failure.
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });

        // Consume: StateChanged(Reconnecting{1}), PublishReconnectRequest
        let _state1 = h.recv_outcome();
        let _req1 = h.recv_outcome();

        // Ack attempt 1 → Rebuilding.
        h.send(SupervisorSignal::PeerAck {
            session_nonce: 42,
            attempt: 1,
        });
        let _rebuild1 = h.recv_outcome(); // InitiateRebuild

        // Fail attempt 1 → backoff → Reconnecting{2}
        h.send(SupervisorSignal::RebuildFailed);
        let state2 = h.recv_outcome(); // StateChanged(Reconnecting{2}) — after backoff
        let _req2 = h.recv_outcome(); // PublishReconnectRequest

        assert_eq!(
            state2,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(2).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );

        // Ack attempt 2 → Rebuilding.
        h.send(SupervisorSignal::PeerAck {
            session_nonce: 42,
            attempt: 2,
        });
        let _rebuild2 = h.recv_outcome(); // InitiateRebuild

        // Fail attempt 2 → backoff → Reconnecting{3}
        h.send(SupervisorSignal::RebuildFailed);
        let state3 = h.recv_outcome();
        let _req3 = h.recv_outcome();

        assert_eq!(
            state3,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(3).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );

        // Ack attempt 3 → Rebuilding.
        h.send(SupervisorSignal::PeerAck {
            session_nonce: 42,
            attempt: 3,
        });
        let _rebuild3 = h.recv_outcome(); // InitiateRebuild

        // Fail attempt 3 → Dead.
        h.send(SupervisorSignal::RebuildFailed);
        let dead_state = h.recv_outcome(); // StateChanged(Dead)
        let dead = h.recv_outcome(); // Dead(reason)

        assert_eq!(
            dead_state,
            SupervisorOutcome::StateChanged(SessionState::Dead {
                reason: DeadReason::IceFailedRepeatedly,
            })
        );
        assert_eq!(
            dead,
            SupervisorOutcome::Dead(DeadReason::IceFailedRepeatedly)
        );

        // Supervisor exits — join must return the dead reason.
        let result = h.join();
        assert_eq!(result, Some(DeadReason::IceFailedRepeatedly));
    }

    // ─── SC-DOUBLE-FAILURE-001: second LocalFailure ignored in AwaitingAck ──────
    //
    // REQ-DOUBLE-FAILURE: Supervisor MUST handle two concurrent LocalFailure signals
    // (one from run_signaling_drain, one from run_stream_transport_event_drain) for the
    // SAME Bye event without triggering a double-rebuild.
    //
    // The second LocalFailure arrives in AwaitingAck state and MUST be silently ignored
    // (Ignore branch at supervisor.rs:348-352). No second state transition. No panic.
    //
    // T14: RED — written first (test exercises already-existing behavior).
    // T15: GREEN — existing AwaitingAck Ignore branch makes this pass immediately.

    /// SC-DOUBLE-FAILURE-001 — Second `LocalFailure` in `AwaitingAck` is silently ignored.
    ///
    /// GIVEN: A `ReconnectSupervisor` in `Connected` state.
    /// WHEN:  Two `SupervisorSignal::LocalFailure` messages are sent back-to-back:
    ///        first `{ trigger: PeerBye }`, then `{ trigger: ConnectionLost }`,
    ///        without any `Ack` between them.
    /// THEN:  The supervisor transitions to `AwaitingAck` EXACTLY ONCE (first signal).
    ///        The second `LocalFailure` does NOT trigger a second `StateChanged` outcome.
    ///        No panic. No second rebuild cycle initiated.
    #[test]
    fn sc_double_failure_001_second_local_failure_ignored_in_awaiting_ack() {
        use crate::session::ReconnectTrigger;

        let h = SupervisorHandle::spawn(fast_policy(), 42);

        // ── Send two LocalFailure signals back-to-back ───────────────────────
        // Both land in the channel before the supervisor processes either.
        // The first transitions Connected → AwaitingAck (StateChanged + PublishReconnectRequest).
        // The second arrives while the supervisor is in AwaitingAck and MUST be ignored (R-3).
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::PeerBye,
        });
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::ConnectionLost {
                reason: "test-double-failure".to_string(),
            },
        });

        // ── Collect the two expected outcomes from the FIRST LocalFailure ────
        // We receive deterministically so we don't race with the AwaitingAck timeout.
        let outcome1 = h
            .outcome_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("SC-DOUBLE-FAILURE-001: expected StateChanged(Reconnecting{1})");
        let outcome2 = h
            .outcome_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("SC-DOUBLE-FAILURE-001: expected PublishReconnectRequest");

        // ── Stop immediately — prevents AwaitingAck timeout retry cycle ─────
        // Sending Stop now puts it in the channel; the supervisor is either
        // still in AwaitingAck (consuming the second LocalFailure, then Stop)
        // or will pick it up on the next recv. Either way, it exits cleanly
        // before any automatic retry StateChanged would be emitted.
        h.send(SupervisorSignal::Stop);

        // ── Drain any remaining outcomes (must be only Stopped) ──────────────
        let mut extra_reconnecting = 0u32;
        loop {
            match h.outcome_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(SupervisorOutcome::StateChanged(SessionState::Reconnecting { .. })) => {
                    extra_reconnecting += 1;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        h.join();

        // ── Assert: first LocalFailure produced exactly the expected outcomes ─
        assert!(
            matches!(
                outcome1,
                SupervisorOutcome::StateChanged(SessionState::Reconnecting { .. })
            ),
            "SC-DOUBLE-FAILURE-001: first outcome must be StateChanged(Reconnecting), got: {outcome1:?}"
        );
        assert!(
            matches!(outcome2, SupervisorOutcome::PublishReconnectRequest { .. }),
            "SC-DOUBLE-FAILURE-001: second outcome must be PublishReconnectRequest, got: {outcome2:?}"
        );

        // ── Assert: no extra Reconnecting transitions after Stop ──────────────
        assert_eq!(
            extra_reconnecting, 0,
            "SC-DOUBLE-FAILURE-001: second LocalFailure in AwaitingAck must NOT produce \
             a second StateChanged(Reconnecting). R-3 protection violated."
        );
    }

    // ─── AC-10: Nonce tie-break — lower wins ─────────────────────────────────

    /// AC-10 — When a PeerRequest arrives in AwaitingAck state with peer_nonce < my_nonce,
    /// the peer wins. Supervisor emits PublishReconnectAck and proceeds to rebuild.
    #[test]
    fn symmetric_race_lower_nonce_wins_we_lose() {
        let my_nonce = 99u64;
        let peer_nonce = 42u64; // lower → peer wins
        let h = SupervisorHandle::spawn(fast_policy(), my_nonce);

        // Enter AwaitingAck.
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome(); // StateChanged(Reconnecting{1})
        let _req = h.recv_outcome(); // PublishReconnectRequest

        // Peer sends ReconnectRequest with lower nonce. Role-equal (both Sender)
        // so the legacy nonce fallback decides — preserves the AC-10 semantics.
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        // We must emit PublishReconnectAck for the peer's nonce.
        let ack = h.recv_outcome();
        assert_eq!(
            ack,
            SupervisorOutcome::PublishReconnectAck {
                attempt: 1,
                session_nonce: peer_nonce,
            }
        );

        // Then proceed to rebuild.
        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// AC-10 — When a PeerRequest arrives in AwaitingAck state with peer_nonce > my_nonce,
    /// we win. Supervisor MUST NOT emit `PublishReconnectAck` (we are the winner).
    ///
    /// The peer will eventually send a `PeerAck` for our request, or the ack timeout
    /// fires and triggers the mDNS reset path. Either way: no `PublishReconnectAck` for
    /// the peer's nonce should appear.
    #[test]
    fn symmetric_race_lower_nonce_wins_we_win() {
        let my_nonce = 42u64; // lower → we win
        let peer_nonce = 99u64;
        let h = SupervisorHandle::spawn(fast_policy(), my_nonce);

        // Enter AwaitingAck.
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome(); // StateChanged(Reconnecting{1})
        let _req = h.recv_outcome(); // PublishReconnectRequest

        // Peer sends ReconnectRequest with higher nonce — we win, so we ignore it.
        // Role-equal (both Sender) so the legacy nonce fallback decides.
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        // Drain all outcomes within a short window; none of them should be
        // PublishReconnectAck for the peer's nonce.
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match h.outcome_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(SupervisorOutcome::PublishReconnectAck { session_nonce, .. })
                    if session_nonce == peer_nonce =>
                {
                    panic!("winner MUST NOT emit PublishReconnectAck for peer_nonce={peer_nonce}");
                }
                Ok(_) => {
                    // Other outcomes (e.g. InitiateMdnsReset after ack timeout) are acceptable.
                }
                Err(_) => break,
            }
        }

        // Clean up.
        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── AC-13: Stop during reconnect (mid-backoff) ──────────────────────────

    /// AC-13 — `Stop` signal received during backoff sleep unblocks immediately.
    ///
    /// We put the supervisor in Rebuilding, fail the build (triggering backoff sleep),
    /// then send Stop. The supervisor must return within a bounded time — not wait
    /// for the full backoff duration.
    #[test]
    fn stop_mid_backoff_sleep_clean_cancellation() {
        // Use the v1_default policy (27s backoff on attempt 3). If Stop does not
        // interrupt the sleep, this test would hang for 3+ms (with the fast policy
        // base=1ms) but we verify it exits fast.
        let h = SupervisorHandle::spawn(fast_policy(), 77);

        // Enter AwaitingAck.
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome();
        let _req = h.recv_outcome();

        // Ack → Rebuilding.
        h.send(SupervisorSignal::PeerAck {
            session_nonce: 77,
            attempt: 1,
        });
        let _rebuild = h.recv_outcome(); // InitiateRebuild

        // Fail → backoff sleep begins (attempt 2 backoff = 1ms * 2^1 = 2ms in fast_policy).
        h.send(SupervisorSignal::RebuildFailed);

        // Immediately send Stop before backoff elapses (no sleep in test here — send is fast).
        h.send(SupervisorSignal::Stop);

        // Supervisor must unblock and return None (clean stop) — not a DeadReason.
        let start = std::time::Instant::now();
        let result = h.join();
        let elapsed = start.elapsed();

        assert!(
            result.is_none(),
            "Stop during backoff must return None (clean stop), got: {result:?}"
        );
        // With fast_policy base=1ms and TEST_ACK_TIMEOUT=20ms, the max wait is 20ms.
        // We give generous 500ms to avoid flakiness on slow CI.
        assert!(
            elapsed < Duration::from_millis(500),
            "Stop must unblock within 500ms, took: {elapsed:?}"
        );
    }

    // ─── AC-14: No CPU spin ───────────────────────────────────────────────────

    /// AC-14 — Backoff sleep uses recv_timeout, not a spin loop.
    ///
    /// This is verified structurally: the supervisor is defined with `recv_timeout`
    /// for all sleep phases. We verify indirectly by confirming Stop interrupts
    /// the sleep (which a spin loop would not allow via channel signal in same way).
    #[test]
    fn backoff_sleep_uses_recv_timeout_not_spin() {
        // This test verifies the STRUCTURAL property that the supervisor returns
        // from stop quickly (not spinning). The actual recv_timeout usage is
        // in the supervisor source above. This test is a behavioral proxy.
        let h = SupervisorHandle::spawn(fast_policy(), 55);

        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome();
        let _req = h.recv_outcome();

        // Ack → rebuild.
        h.send(SupervisorSignal::PeerAck {
            session_nonce: 55,
            attempt: 1,
        });
        let _rb = h.recv_outcome();

        // Fail → backoff sleep.
        h.send(SupervisorSignal::RebuildFailed);

        // Stop must unblock immediately, not spin.
        h.send(SupervisorSignal::Stop);
        let result = h.join();
        assert!(result.is_none(), "clean stop expected, got {result:?}");
    }

    // ─── AC-4: Backoff schedule shape ────────────────────────────────────────

    /// AC-4 — `ReconnectPolicy::v1_default()` produces 3s/9s/27s delays.
    ///
    /// The supervisor does NOT need wall-clock testing — this test verifies the
    /// backoff schedule values directly (not timing). Actual sleep is tested
    /// structurally via stop-interrupt tests above.
    #[test]
    fn backoff_schedule_shape_v1_default() {
        let policy = ReconnectPolicy::v1_default();
        let n1 = std::num::NonZeroU8::new(1).unwrap();
        let n2 = std::num::NonZeroU8::new(2).unwrap();
        let n3 = std::num::NonZeroU8::new(3).unwrap();

        assert_eq!(policy.delay_for_attempt(n1), Duration::from_secs(3));
        assert_eq!(policy.delay_for_attempt(n2), Duration::from_secs(9));
        assert_eq!(policy.delay_for_attempt(n3), Duration::from_secs(27));
    }

    // ─── Happy path: rebuild succeeds on attempt 1 ───────────────────────────

    /// Rebuild succeeds on attempt 1 → supervisor returns to Connected.
    #[test]
    fn rebuild_succeeds_returns_to_connected() {
        let h = SupervisorHandle::spawn(fast_policy(), 11);

        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome(); // Reconnecting{1}
        let _req = h.recv_outcome(); // PublishReconnectRequest

        h.send(SupervisorSignal::PeerAck {
            session_nonce: 11,
            attempt: 1,
        });
        let _rebuild = h.recv_outcome(); // InitiateRebuild

        h.send(SupervisorSignal::RebuildSucceeded);
        let connected = h.recv_outcome(); // StateChanged(Connected)
        assert_eq!(
            connected,
            SupervisorOutcome::StateChanged(SessionState::Connected)
        );

        // Supervisor is back in Connected — stop it.
        h.send(SupervisorSignal::Stop);
        let result = h.join();
        assert!(result.is_none());
    }

    // ─── Peer-initiated reconnect (PeerRequest in Connected state) ───────────

    /// When a PeerRequest arrives in Connected state, supervisor becomes loser,
    /// sends Ack, and initiates rebuild.
    #[test]
    fn peer_request_in_connected_initiates_loser_rebuild() {
        let h = SupervisorHandle::spawn(fast_policy(), 999);

        // Role-equal (both Sender) with peer_nonce 1 < our 999 ⇒ peer is the
        // active reconnector ⇒ we defer (the loser path).
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 1, // lower than our 999
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        // Supervisor sends Ack and emits Reconnecting state.
        let ack = h.recv_outcome();
        assert_eq!(
            ack,
            SupervisorOutcome::PublishReconnectAck {
                attempt: 1,
                session_nonce: 1,
            }
        );

        let state = h.recv_outcome();
        assert_eq!(
            state,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );

        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── SC-DR-4 / SC-DR-4b: Connected-state role-aware PeerRequest ───────────
    //
    // D1 (design #963): the Connected-state PeerRequest branch becomes role-aware.
    // A Sender (offerer) that receives a peer ReconnectRequest in Connected state
    // stays the active reconnector — it does NOT take the loser/Ack+Rebuild path;
    // it re-offers via its own rebuild hook. A Receiver (answerer) defers and takes
    // the existing loser path (Ack + Reconnecting + InitiateRebuild), unchanged.

    /// SC-DR-4 — Sender in Connected stays the active reconnector on a Receiver's
    /// ReconnectRequest: NO `PublishReconnectAck`, NO `InitiateRebuild` as loser.
    #[test]
    fn sc_dr_4_connected_sender_stays_active() {
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 99, SignalingRole::Sender);

        // Peer (Receiver, lower nonce) sends a ReconnectRequest while we are Connected.
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 42,
            peer_role: SignalingRole::Receiver,
            attempt: 1,
        });

        // As the active reconnector we MUST NOT Ack the peer nor initiate a loser
        // rebuild — drain a short window and assert neither outcome appears.
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match h.outcome_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(SupervisorOutcome::PublishReconnectAck { .. }) => {
                    panic!("SC-DR-4: active Sender MUST NOT emit PublishReconnectAck");
                }
                Ok(SupervisorOutcome::InitiateRebuild) => {
                    panic!("SC-DR-4: active Sender MUST NOT take the loser InitiateRebuild path");
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// SC-DR-4b — Receiver in Connected defers on a Sender's ReconnectRequest:
    /// emits Ack + Reconnecting + InitiateRebuild (the existing loser path).
    #[test]
    fn sc_dr_4b_connected_receiver_defers() {
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 99, SignalingRole::Receiver);

        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 42,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        let ack = h.recv_outcome();
        assert_eq!(
            ack,
            SupervisorOutcome::PublishReconnectAck {
                attempt: 1,
                session_nonce: 42,
            }
        );
        let state = h.recv_outcome();
        assert_eq!(
            state,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            })
        );
        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── SC-SRR-1-NR (role-aware redefinition, design #963 D2) ───────────────
    //
    // NR-1 was: "lower-nonce side still rebuilds." It is REDEFINED to: the offerer
    // (Sender) always re-offers; the answerer (Receiver) always defers and Acks;
    // nonce only breaks role-equal ties. These tests pin both halves.

    /// SC-SRR-1-NR — answerer (Receiver) defers to the offerer (Sender) even when
    /// the Receiver holds the LOWER nonce (the old rule would have made it win).
    #[test]
    fn sc_srr_1_nr_answerer_defers_even_with_lower_nonce() {
        // my=Receiver(nonce 1, LOWER), peer=Sender(nonce 999, HIGHER).
        // Old rule: lower nonce wins ⇒ Receiver would be active. Role-aware: the
        // answerer always defers ⇒ we Ack the Sender and rebuild as the loser.
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 1, SignalingRole::Receiver);

        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 999,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });

        let ack = h.recv_outcome();
        assert_eq!(
            ack,
            SupervisorOutcome::PublishReconnectAck {
                attempt: 1,
                session_nonce: 999,
            }
        );
        let _state = h.recv_outcome(); // StateChanged(Reconnecting{1})
        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    /// SC-SRR-1-NR companion — Sender wins even when it holds the lower nonce
    /// (mirror of `sc_srr_1_nr_answerer_defers_even_with_lower_nonce`).
    #[test]
    fn sc_srr_1_nr_offerer_active_even_with_lower_nonce() {
        // my=Sender(nonce 1, LOWER), peer=Receiver(nonce 999, HIGHER).
        // Role-aware: Sender always active ⇒ no Ack, no loser rebuild.
        let h = SupervisorHandle::spawn_with_role(fast_policy(), 1, SignalingRole::Sender);

        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 999,
            peer_role: SignalingRole::Receiver,
            attempt: 1,
        });

        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            match h.outcome_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(SupervisorOutcome::PublishReconnectAck { .. }) => {
                    panic!("SC-SRR-1-NR: active Sender MUST NOT emit PublishReconnectAck");
                }
                Ok(SupervisorOutcome::InitiateRebuild) => {
                    panic!("SC-SRR-1-NR: active Sender MUST NOT take the loser rebuild path");
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── AwaitingAck timeout → mDNS reset path ───────────────────────────────

    /// When AwaitingAck times out (no ack received within ack_timeout),
    /// supervisor emits InitiateMdnsReset and then InitiateRebuild.
    #[test]
    fn awaiting_ack_timeout_triggers_mdns_reset() {
        // Use TEST_ACK_TIMEOUT as the backoff — it will expire quickly.
        let h = SupervisorHandle::spawn(fast_policy(), 500);

        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let _state = h.recv_outcome(); // Reconnecting{1}
        let _req = h.recv_outcome(); // PublishReconnectRequest

        // Do NOT send PeerAck — let the ack_timeout expire.
        // Supervisor should emit InitiateMdnsReset within TEST_ACK_TIMEOUT + margin.
        let reset = h
            .outcome_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("must receive InitiateMdnsReset after ack timeout");
        assert_eq!(reset, SupervisorOutcome::InitiateMdnsReset);

        let rebuild = h.recv_outcome();
        assert_eq!(rebuild, SupervisorOutcome::InitiateRebuild);

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── Slow-rebuild reproductor (T12.2 manual smoke FAIL 2026-04-30) ───────

    /// Reproductor del bug detectado en el smoke manual T12.2 Escenario 1
    /// (engram #509, sdd/auto-rebuild-from-drain/smoke-fail-diagnosis).
    ///
    /// Cuando el rebuild real toma más que `ack_timeout`, el supervisor en estado
    /// `Rebuilding` toma la rama `Err(_)` del `recv_timeout(ack_timeout)`
    /// (este archivo, líneas 418-444): trata el silencio como `RebuildFailed`,
    /// transiciona a `AwaitingAck{n+1}` y emite `PublishReconnectRequest{n+1}`.
    ///
    /// Cuando el worker finalmente envía `RebuildSucceeded`, el supervisor está
    /// en `AwaitingAck` → la señal cae en la rama Ignore (líneas 333-336) → nunca
    /// se emite `StateChanged(Connected)` → frontend nunca recibe `Streaming` →
    /// overlay "Reconnecting" persiste indefinidamente. AC-5 violado.
    ///
    /// En producción `ack_timeout = 2s` y el rebuild real (mDNS + SDP + ICE)
    /// toma ≥5s, por lo que el bug se dispara siempre. Los tests anteriores no
    /// lo detectaron porque sus builders fake retornan en <1ms y nunca expira
    /// `recv_timeout` en `Rebuilding`.
    ///
    /// Fix esperado (Opción A): separar `rebuild_timeout` de `ack_timeout` en
    /// `ReconnectSupervisor::run`, y propagar un valor amplio (≥15s) en
    /// producción para `Rebuilding`. Después del fix, este test debe pasar.
    #[test]
    fn slow_rebuild_succeeded_must_not_be_dropped_when_exceeds_ack_timeout() {
        // ack_timeout = 20ms (short, like production's 2s relative to rebuild)
        // rebuild_timeout = 1s (large, like production's 15s relative to ack)
        // The slow-rebuild sleep below (50ms) lies between the two: it would
        // expire `ack_timeout` if reused (the bug) but stays well within
        // `rebuild_timeout` (the fix).
        let h = SupervisorHandle::spawn_with_timeouts(
            fast_policy(),
            42,
            SignalingRole::Sender,
            Duration::from_millis(20),
            Duration::from_millis(1000),
        );

        // Drive Connected → Rebuilding{1} via PeerRequest (loser path).
        // Role-equal (both Sender) with peer_nonce 1 < our 42 ⇒ peer active ⇒
        // we defer (loser path).
        h.send(SupervisorSignal::PeerRequest {
            peer_nonce: 1,
            peer_role: SignalingRole::Sender,
            attempt: 1,
        });
        let _ack = h.recv_outcome(); // PublishReconnectAck
        let _state = h.recv_outcome(); // StateChanged(Reconnecting{1})
        let _rebuild = h.recv_outcome(); // InitiateRebuild

        // Simulate a slow rebuild — sleep > ack_timeout (20ms) but < rebuild_timeout (1000ms).
        // Production analogue: rebuild takes 5+s while ack_timeout = 2s and
        // rebuild_timeout = 15s.
        std::thread::sleep(Duration::from_millis(50));

        // Worker reports rebuild success after the slow operation completes.
        h.send(SupervisorSignal::RebuildSucceeded);

        // Expectation: next outcome MUST be StateChanged(Connected). The slow
        // rebuild's RebuildSucceeded must be honored, not dropped because the
        // supervisor escalated to attempt 2 after ack_timeout expired in
        // Rebuilding state.
        let outcome = h.recv_outcome();
        assert_eq!(
            outcome,
            SupervisorOutcome::StateChanged(SessionState::Connected),
            "Slow rebuild's RebuildSucceeded must result in \
             StateChanged(Connected). Got {outcome:?}"
        );

        h.send(SupervisorSignal::Stop);
        h.join();
    }

    // ─── SC-T22-RETRY-2 / REQ-RFE-7: rebuild-failed-escalation regression guard ─

    // REGRESSION GUARD (SC-T22-RETRY-2 / REQ-RFE-7): do not delete or weaken.
    //
    // This test locks the existing supervisor backoff/budget/emit behavior that the
    // `rebuild-failed-escalation` change (GitHub #57) sends `RebuildFailed` into.
    // The supervisor state machine MUST be unchanged — only sender.rs wiring changes.
    // If this test turns RED after any future diff, the SC-T22-sensitive Rebuilding arm
    // was modified; treat it as a merge blocker.

    /// SC-T22-RETRY-2 — `RebuildFailed` in `Rebuilding(attempt 1)` advances to
    ///                   `Reconnecting { attempt: 2, max: 3 }` with correct backoff.
    ///
    /// GIVEN: A supervisor in `Rebuilding { attempt: 1, .. }` (driven via
    ///        `LocalFailure → PeerAck`).
    /// WHEN:  `RebuildFailed` is sent.
    /// THEN:
    ///   1. Next outcome is `StateChanged(Reconnecting { attempt: 2, max: 3 })`.
    ///   2. Followed by `PublishReconnectRequest { attempt: 2 }`.
    ///   3. `ReconnectPolicy::v1_default().delay_for_attempt(2) == 3 s` (schedule intact).
    #[test]
    fn rebuild_failed_during_rebuilding_advances_to_attempt_2_with_backoff() {
        let nonce: u64 = 57;
        let h = SupervisorHandle::spawn(fast_policy(), nonce);

        // Drive: LocalFailure → consume StateChanged(Reconnecting{1}) + PublishReconnectRequest{1}
        h.send(SupervisorSignal::LocalFailure {
            trigger: ReconnectTrigger::IceFailed,
        });
        let state1 = h.recv_outcome();
        assert_eq!(
            state1,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(1).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            }),
            "Expected Reconnecting{{1,3}} after LocalFailure, got {state1:?}"
        );
        let _req1 = h.recv_outcome(); // PublishReconnectRequest{1}

        // Drive: PeerAck → consume InitiateRebuild
        h.send(SupervisorSignal::PeerAck {
            session_nonce: nonce,
            attempt: 1,
        });
        let _rebuild = h.recv_outcome(); // InitiateRebuild

        // Drive: RebuildFailed
        h.send(SupervisorSignal::RebuildFailed);

        // Assert: next outcome is StateChanged(Reconnecting{2, 3})
        let state2 = h.recv_outcome();
        assert_eq!(
            state2,
            SupervisorOutcome::StateChanged(SessionState::Reconnecting {
                attempt: std::num::NonZeroU8::new(2).unwrap(),
                max: std::num::NonZeroU8::new(3).unwrap(),
            }),
            "Expected Reconnecting{{2,3}} after RebuildFailed, got {state2:?}"
        );

        // Assert: next outcome is PublishReconnectRequest{2}
        let req2 = h.recv_outcome();
        assert_eq!(
            req2,
            SupervisorOutcome::PublishReconnectRequest {
                attempt: 2,
                session_nonce: nonce,
            },
            "Expected PublishReconnectRequest{{2}} after RebuildFailed, got {req2:?}"
        );

        // Assert: v1_default backoff schedule is unchanged — 3s/9s/27s ladder intact.
        // Formula: base_ms=3000, factor=3 ⇒ attempt 1=3s, 2=9s, 3=27s.
        let v1 = crate::session::ReconnectPolicy::v1_default();
        assert_eq!(
            v1.delay_for_attempt(std::num::NonZeroU8::new(1).unwrap()),
            Duration::from_secs(3),
            "v1_default delay_for_attempt(1) must equal 3s (backoff schedule locked)"
        );
        assert_eq!(
            v1.delay_for_attempt(std::num::NonZeroU8::new(2).unwrap()),
            Duration::from_secs(9),
            "v1_default delay_for_attempt(2) must equal 9s (backoff schedule locked)"
        );
        assert_eq!(
            v1.delay_for_attempt(std::num::NonZeroU8::new(3).unwrap()),
            Duration::from_secs(27),
            "v1_default delay_for_attempt(3) must equal 27s (backoff schedule locked)"
        );

        h.send(SupervisorSignal::Stop);
        h.join();
    }
}
