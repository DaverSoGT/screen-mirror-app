# Spec: hw-encoder-mft-single-frame-flush (Slice 3 — Intel QSV single-frame drain)

> Phase: SDD spec. Inputs: proposal #707, explore #701, predecessor spec #633.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-single-frame-flush/spec` + `openspec/changes/hw-encoder-mft-single-frame-flush/spec.md`).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`). Delivery: single PR, branch `feat/hw-encoder-mft-single-frame-flush`.
> Date: 2026-05-09.

---

## 1. Inputs

| Observation | Topic key | Role |
|-------------|-----------|------|
| #707 | `sdd/hw-encoder-mft-single-frame-flush/proposal` | 8 locked decisions D1–D8, open questions OQ-1–OQ-5, AC-1–AC-7, DoD |
| #701 | `sdd/hw-encoder-mft-single-frame-flush/explore` | Root-cause analysis, 8-test list, candidate approaches, Phase 0 gate |
| #633 | `sdd/hw-encoder-mft-vendor-compat-rework/spec` | Predecessor spec — format template, R/S notation, BLOCKED_ON_SMOKE convention |
| #186 | `sdd-init/screen-mirror-app` | Strict TDD mode, BLOCKED_ON_SMOKE rule, project conventions |

---

## 2. Domain

This spec governs:

- `crates/sm-infra/src/encode/windows_mft.rs` — `WindowsMftH264Encoder`, `MftEncoderShared`, `pump_loop`, `collect_output`
- `crates/sm-infra/tests/windows_mft_encode.rs` — the 8 single-frame-expectation integration tests listed in R8

It explicitly excludes:

- `crates/sm-domain/src/encode.rs` — FROZEN (R14)
- `crates/sm-infra/Cargo.toml` — FROZEN (`default = []`, R15)
- Any other crate

---

## 3. Functional Requirements (R1–R15)

### R1 — Public `flush()` inherent method on `WindowsMftH264Encoder`

**Statement**: `WindowsMftH264Encoder` MUST expose `pub fn flush(&self)` as an inherent method. The method signals end-of-burst to the encoder pump loop. It MUST NOT be added to the `VideoEncoder` trait.

**Rationale**: D1 (Approach C chosen), D2 (sm-domain FROZEN). Tests import `WindowsMftH264Encoder` directly and can call inherent methods without trait involvement.

**Scenarios**: S1, S3, S4, S9

---

### R2 — `flush()` uses `AtomicBool` flag stored on `MftEncoderShared`

**Statement**: `MftEncoderShared` MUST gain a new field `drain_pending: AtomicBool` initialised to `false`. `flush()` MUST call `drain_pending.store(true, Release)`. No other fields on `MftEncoderShared` or `EncodeState` are modified.

**Rationale**: D3 — atomic flag mechanism chosen over mpsc command channel or condvar. Fits existing poll cadence. Minimal diff surface.

**Scenarios**: S2, S3

---

### R3 — Pump loop drain-flag check site: post-NeedInput, pre-top-of-loop

**Statement**: `pump_loop` MUST check `drain_pending` AFTER the NeedInput inner loop (all available NeedInput credits consumed or channel empty) and BEFORE returning to the top-of-loop `state.stop` poll. The check MUST use `drain_pending.compare_exchange(true, false, Acquire, Relaxed)`. On success (flag was `true`), `pump_loop` MUST call `mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)` exactly ONCE and then continue the outer loop (no break).

**Rationale**: D3. Code locality with the channel-disconnect DRAIN site. One DRAIN per flag-set (idempotent across pump iterations between flag-set and consumption).

**Scenarios**: S1, S2, S5

---

### R4 — `flush()` is callable while `frame_tx` is alive (channel open)

**Statement**: `flush()` MUST operate correctly whether `frame_tx` is in scope (alive) or has been dropped. Calling `flush()` MUST NOT panic, MUST NOT return an error, and MUST NOT require the caller to drop `frame_tx` first.

**Rationale**: D1 — distinguishes Approach C from Approach A3 (tests close `frame_tx` early). Allows tests to retain `stop()` control.

**Scenarios**: S3, S8

---

### R5 — DrainComplete handling inherited from Slice 2 UNCHANGED

**Statement**: The `METransformDrainComplete` handler in `pump_loop` (which resets `ni_count`/`ho_count` to 0 and does NOT break) MUST NOT be modified. The new flag-driven DRAIN path reuses the same DrainComplete handling path as the existing channel-disconnect DRAIN.

**Rationale**: D3 — additive change. Channel-disconnect DRAIN path is NOT modified.

**Scenarios**: S1, S6

---

### R6 — STREAM_CHANGE handler from Slice 2 UNCHANGED

**Statement**: The `MF_E_TRANSFORM_STREAM_CHANGE` arm in `collect_output` (calling `renegotiate_output_type`) introduced in PR #18 MUST NOT be modified. If the vendor emits STREAM_CHANGE during a flag-driven DRAIN sequence, the existing handler executes; `collect_output` returns `Ok(None)`; `pump_loop` continues.

**Rationale**: D1, Slice 2 R1. Additive change.

**Scenarios**: S6

---

### R7 — sm-domain `VideoEncoder` trait surface MUST NOT change

**Statement**: `crates/sm-domain/src/encode.rs` SHALL have empty diff relative to master `daa9522`. No new `EncoderError` variants. No new trait methods.

**Rationale**: D2. R14 from Slice 2 inherited. sm-domain FROZEN.

**Scenarios**: S10

---

### R8 — `flush()` doc comment MUST warn about transient non-accepting state and streaming callers

**Statement**: The `/// ` doc comment on `flush()` MUST state: (1) one-shot DRAIN semantics — vendor enters non-accepting state after COMMAND_DRAIN until DrainComplete fires; (2) vendor-dependent behaviour (Intel QSV emits pending output; behaviour on other vendors is best-effort); (3) production streaming callers relying on continuous encode MUST NOT call `flush()` — it is intended for test affordance only.

**Rationale**: D1 OQ-A (locked yes-doc-comment). Prevents accidental production misuse.

**Scenarios**: (code-review verifiable)

---

### R9 — The 8 specific tests MUST pass on Host A (Intel QSV) after the fix

**Statement**: After `enc.flush()` is called between the final `frame_tx.send(...)` and the corresponding `pkt_rx.recv_timeout(...)` in each test body, the following 8 tests MUST PASS on Host A (Intel QSV):

1. `mft_encoded_packet_starts_with_annex_b_start_code`
2. `mft_first_real_packet_is_annex_b`
3. `mft_encoded_packet_timestamp_matches_capture_frame`
4. `mft_setup_uses_config_dimensions_when_nonzero`
5. `mft_setup_falls_back_when_config_dimensions_zero`
6. `mft_request_keyframe_marks_next_packet_as_keyframe`
7. `mft_keyframe_flag_cleared_after_idr_emitted`
8. `mft_set_bitrate_updates_encoder_without_restart`

All 8 tests reside in `crates/sm-infra/tests/windows_mft_encode.rs`. Test bodies for T6/T7/T8 (multi-phase) receive a flush call at the design-deferred location (OQ-3); design resolves placement after Phase 0 evidence (D6).

**Rationale**: D4 (8-test scope). AC-1 (Host A primary).

**Scenarios**: S1, S11.1 – S11.8

---

### R10 — No regression on currently-passing tests

**Statement**: Tests passing on master `daa9522` (10/18 on Host A; 16/18 on Host B per archive #699 / observation #696) MUST continue to pass on the fix branch. Host A target after fix: 18/18 PASS (17/18 acceptable only if the remaining fail is attributable to a separately-tracked issue, NOT a new regression). Host B: 16/18 PASS maintained or better.

**Rationale**: AC-2, AC-3, D8.

**Scenarios**: S7, S12

---

### R11 — Quality gates: all 7 GREEN before merge

**Statement**: The following gates MUST be GREEN on the merge SHA:

1. `cargo build --workspace --locked`
2. `cargo nextest run --workspace` (non-hardware, CI-runnable tests)
3. `cargo clippy --all-targets --all-features --locked -- -D warnings`
4. `cargo fmt --check --all`
5. `mft_thirty_frame_smoke_emits_at_least_one_keyframe` PASSES on Host A
6. 18/18 target PASS on Host A (or 17/18 with tracked exception)
7. 16/18 or better PASS on Host B

**Rationale**: AC-5, DoD.

**Scenarios**: S7, S12

---

### R12 — Phase 0 trace evidence REQUIRED before design lock

**Statement**: A 1-frame DRAIN trace transcript on Host A (Intel QSV) MUST be saved to engram topic `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace` BEFORE the design phase begins. The transcript MUST confirm one of three outcomes: GOOD (1-frame DRAIN emits packet), PARTIAL (1-frame DRAIN empty; 2-frame emits), or BAD (DRAIN never produces output). Design phase MUST NOT start without this evidence.

**Rationale**: D6, `tracing-before-explore` convention #592.

**Scenarios**: S13

---

### R13 — BLOCKED_ON_SMOKE: verify phase CANNOT issue APPROVED without post-fix smoke transcripts

**Statement**: Verify CANNOT produce an APPROVED result until the following engram topics contain post-fix smoke transcripts:

- `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-a-branch` (Host A, fix branch)
- `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-b-regression` (Host B, regression check)

**Rationale**: BLOCKED_ON_SMOKE rule per init #186. Convention #582.

**Scenarios**: S7, S12

---

### R14 — Strict TDD 3-commit sequence: RED before GREEN

**Statement**: The branch MUST be developed in this commit order:

- **C1 (RED)**: adds `enc.flush()` call sites to the 8 failing tests + a no-op stub `pub fn flush(&self) {}` so tests COMPILE. On this commit, the 8 tests STILL TIME OUT (no-op flush preserves RED at runtime). Commit message: `test(infra): assert single-frame intel-qsv tests flush before recv`.
- **C2 (GREEN)**: replaces the no-op stub with the real implementation — `drain_pending.store(true, Release)`, `AtomicBool` field on `MftEncoderShared`, pump_loop drain-flag check firing `ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)`. The 8 tests now PASS. Commit message: `feat(infra): flush() drains MFT pipeline via COMMAND_DRAIN flag`.
- **C3 (optional polish)**: `style(infra): cargo fmt windows_mft.rs` — only if `cargo fmt --check --all` reports diff after C2.

No GREEN commit WITHOUT a prior RED commit. No single-commit squash of RED+GREEN.

**Rationale**: D7. Strict TDD per init #186 v11. Matches Slice 2 pattern.

**Scenarios**: S14

---

### R15 — `default = []` for `hw-encoder` feature MUST remain unchanged

**Statement**: `crates/sm-infra/Cargo.toml` SHALL still read `default = []`. The `hw-encoder` feature remains opt-in. This is NOT the `hw-encoder-default-on-flip` slice.

**Rationale**: D8, init #186 v11. Separate slice gates on both this slice AND `hw-encoder-mft-nvenc-keyframe-flag`.

**Scenarios**: S10

---

## 4. Scenarios (S1–S14)

### S1 — flush() before recv_timeout produces encoded packet (1-frame baseline)

**Given**: `WindowsMftH264Encoder` created, `enc.start(frame_rx, pkt_tx)` called. 1 synthetic frame submitted via `frame_tx.send(...)`. `enc.flush()` called immediately after. `frame_tx` still in scope (alive).
**When**: pump_loop consumes `drain_pending` flag (`compare_exchange` succeeds), fires `ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)`. Vendor emits `METransformHaveOutput`, then `METransformDrainComplete`.
**Then**: An `EncodedPacket` arrives in `pkt_rx` via `recv_timeout(Duration::from_secs(5))` — no timeout. BLOCKED_ON_SMOKE: Y. (OQ-1 / Phase 0 gate)

---

### S2 — flush() is idempotent within a single pump iteration

**Given**: `enc.flush()` called twice in rapid succession before the pump_loop consumes the flag.
**When**: pump_loop's `compare_exchange(true, false, Acquire, Relaxed)` fires once and resets the flag to `false`. The second `flush()` call stores `true` again (re-arms).
**Then**: Each `flush()` call that is consumed by pump_loop fires exactly ONE `COMMAND_DRAIN`. Multiple flush calls between pump iterations collapse to at most one DRAIN (the last `store(true)` wins). No panic, no double-DRAIN from a single pump pass.
BLOCKED_ON_SMOKE: N (structural — atomic semantics guaranteed by `compare_exchange`).

---

### S3 — flush() callable while frame_tx alive

**Given**: `frame_tx` is in scope, alive, NOT dropped.
**When**: `enc.flush()` called.
**Then**: `drain_pending.store(true, Release)` executes atomically. No panic, no error returned, no channel operation required. pump_loop fires DRAIN on next iteration after NeedInput inner loop. BLOCKED_ON_SMOKE: N (structural).

---

### S4 — flush() before any frame submitted (edge case)

**Given**: `enc.start(frame_rx, pkt_tx)` called. No frames submitted. `enc.flush()` called immediately.
**When**: pump_loop consumes `drain_pending` flag and fires `ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)` on an empty pipeline.
**Then**: Vendor emits `METransformDrainComplete` immediately (empty flush — no output). No panic, no error. pkt_rx remains empty (no packet emitted). BLOCKED_ON_SMOKE: Y (vendor behaviour on empty DRAIN — Phase 0 secondary check).

---

### S5 — drain_pending re-arms after DrainComplete (multi-burst pattern)

**Given**: 1 frame submitted, `enc.flush()` called, DrainComplete fires, pump_loop resets `ni_count`/`ho_count` (existing R5 handler). After DrainComplete, caller submits another frame and calls `enc.flush()` again.
**When**: `drain_pending.store(true, Release)` re-arms the flag. pump_loop fires DRAIN a second time after the second NeedInput inner loop.
**Then**: Second DRAIN produces second packet. No deadlock, no infinite loop, no panic. BLOCKED_ON_SMOKE: Y (design-deferred OQ-4 resolved by Phase 0).

---

### S6 — STREAM_CHANGE during DRAIN servicing handled by Slice 2 code

**Given**: Vendor emits `MF_E_TRANSFORM_STREAM_CHANGE` during `collect_output` while pump_loop is servicing a flag-driven DRAIN.
**When**: `collect_output` hits the existing STREAM_CHANGE arm, calls `renegotiate_output_type(mft, w, h, framerate, bitrate_bps)`.
**Then**: `renegotiate_output_type` returns `Ok(())`; `collect_output` returns `Ok(None)`; pump_loop continues without error. `output_format_known` reset to `None`. No regression from Slice 2 (R6). BLOCKED_ON_SMOKE: Y.

---

### S7 — 30-frame smoke unchanged (no flush() call)

**Given**: `mft_thirty_frame_smoke_emits_at_least_one_keyframe` as on master `daa9522` (PR #18). No `enc.flush()` call in this test. Stop-triggered drain via channel disconnect continues to be the drain mechanism.
**When**: 30 frames submitted, stop()/producer.join() teardown runs.
**Then**: At least 1 IDR + at least 10 P-frames received, all within 10s deadline. Test PASSES on Host A (Intel QSV) and Host B (NVENC). BLOCKED_ON_SMOKE: Y.

---

### S8 — flush() then frame_tx drop (flush + disconnect sequence)

**Given**: `enc.flush()` called. `drain_pending` = true. Then `frame_tx` dropped (channel disconnects) before pump_loop consumes the flag.
**When**: pump_loop NeedInput inner loop hits `RecvTimeoutError::Disconnected`. The existing Disconnected arm fires `COMMAND_DRAIN` and breaks. pump_loop then checks `drain_pending` — flag was `true` but the channel-disconnect DRAIN already fired.
**Then**: At most one `COMMAND_DRAIN` fires (the channel-disconnect DRAIN wins; or design documents ordering). No double-DRAIN panic. No error. BLOCKED_ON_SMOKE: N (structural — design must document ordering). **Design-deferred: OQ-B ordering detail**.

---

### S9 — RED discipline: flush() call sites compile on C1 (no-op stub)

**Given**: master `daa9522` (no real flush() implementation — only stub `pub fn flush(&self) {}`).
**When**: the 8 test bodies with `enc.flush()` call sites compile.
**Then**: `cargo build` succeeds (no compile error). The 8 tests STILL TIME OUT at `recv_timeout` (stub does nothing). RED preserved at runtime level. BLOCKED_ON_SMOKE: N (compile check only).

---

### S10 — sm-domain and Cargo.toml unchanged

**Given**: any commit on `feat/hw-encoder-mft-single-frame-flush`.
**When**: `git diff master daa9522 -- crates/sm-domain/src/encode.rs crates/sm-infra/Cargo.toml`.
**Then**: diff = 0 lines changed. BLOCKED_ON_SMOKE: N (git-verifiable).

---

### S11.1 — mft_encoded_packet_starts_with_annex_b_start_code PASSES (Host A)

**Given**: `enc.flush()` inserted after `frame_tx.send(make_synthetic_frame(640, 480, 0))`.
**When**: `pkt_rx.recv_timeout(Duration::from_secs(5))`.
**Then**: EncodedPacket arrives (no timeout); `pkt.data[..4] == [0x00, 0x00, 0x00, 0x01]`. BLOCKED_ON_SMOKE: Y.

---

### S11.2 — mft_first_real_packet_is_annex_b PASSES (Host A)

**Given**: `enc.flush()` inserted after `frame_tx.send(make_synthetic_frame(640, 480, 0))`.
**When**: `pkt_rx.recv_timeout(Duration::from_secs(5))`.
**Then**: First EncodedPacket has `data[..4] == [0x00, 0x00, 0x00, 0x01]`. BLOCKED_ON_SMOKE: Y.

---

### S11.3 — mft_encoded_packet_timestamp_matches_capture_frame PASSES (Host A)

**Given**: 1 frame submitted with timestamp 500 ms. `enc.flush()` called.
**When**: `pkt_rx.recv_timeout(Duration::from_secs(5))`.
**Then**: `pkt.timestamp == Duration::from_millis(500)`. BLOCKED_ON_SMOKE: Y.

---

### S11.4 — mft_setup_uses_config_dimensions_when_nonzero PASSES (Host A)

**Given**: Encoder configured with 640×480, 1 frame submitted. `enc.flush()` called.
**When**: `pkt_rx.recv_timeout(Duration::from_secs(5))`.
**Then**: EncodedPacket received (config dimensions used, no fallback). BLOCKED_ON_SMOKE: Y.

---

### S11.5 — mft_setup_falls_back_when_config_dimensions_zero PASSES (Host A)

**Given**: Encoder configured with 0×0 (sentinel), frame is 1920×1080. 1 frame submitted. `enc.flush()` called.
**When**: `pkt_rx.recv_timeout(Duration::from_secs(5))`.
**Then**: EncodedPacket received (1920×1080 fallback applied). BLOCKED_ON_SMOKE: Y.

---

### S11.6 — mft_request_keyframe_marks_next_packet_as_keyframe PASSES (Host A)

**Given**: Multi-phase test. `enc.flush()` call site determined by Phase 0 (OQ-3 design-deferred). Covers initial IDR drain + P-frames + forced IDR.
**When**: All `recv_pkt()` calls within 3s each.
**Then**: `forced_idr.is_keyframe == true`; `forced_idr.data[..4] == [0x00,0x00,0x00,0x01]`; `forced_idr.data[4] & 0x1F == 0x07`. BLOCKED_ON_SMOKE: Y.

---

### S11.7 — mft_keyframe_flag_cleared_after_idr_emitted PASSES (Host A)

**Given**: Multi-phase test. `enc.flush()` call site determined by Phase 0 (OQ-3 design-deferred).
**When**: All `recv_pkt()` calls within 3s each.
**Then**: `forced.is_keyframe == true`; `after_idr.is_keyframe == false`. BLOCKED_ON_SMOKE: Y.

---

### S11.8 — mft_set_bitrate_updates_encoder_without_restart PASSES (Host A)

**Given**: Multi-phase test (3 frames at 4 Mbps, `set_bitrate(8_000_000)`, 3 more frames). `enc.flush()` call site determined by Phase 0 (OQ-3 design-deferred).
**When**: All `recv_pkt()` calls within 3s each; `set_bitrate(8_000_000)` returns `Ok(())`.
**Then**: 6 packets total received; encoder thread alive after bitrate update; channel not disconnected. BLOCKED_ON_SMOKE: Y.

---

### S12 — NVENC regression check (Host B)

**Given**: Host B (NVENC) running with the `flush()` API present but `enc.flush()` never called by the 10 previously-passing tests or the smoke test.
**When**: Full 18-test suite runs on Host B.
**Then**: Same 16/18 PASS as master `daa9522` baseline. No new failures introduced. BLOCKED_ON_SMOKE: Y.

---

### S13 — Phase 0 trace gate before design lock

**Given**: Phase 0 smoke script runs on Host A (Intel QSV) with `RUST_LOG=sm_infra::encode=trace`. 1 frame submitted, `frame_tx` dropped, `recv_timeout(10s)`.
**When**: `COMMAND_DRAIN` fires (channel-disconnect trigger). Trace log captured.
**Then**: Transcript saved to engram topic `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace` with GOOD/PARTIAL/BAD classification. Design phase reads this topic before proceeding. BLOCKED_ON_SMOKE: Y (gate, not verifiable without running).

---

### S14 — RED commit compiles; 8 tests fail at runtime; GREEN commit makes them pass

**Given**: C1 (RED) commit with `enc.flush()` call sites + no-op stub.
**When**: `cargo nextest run --workspace` on C1.
**Then**: All 8 named tests compile, run, and FAIL with `RecvTimeoutError::Timeout` (not compile error). BLOCKED_ON_SMOKE: Y.

**Given**: C2 (GREEN) commit with real implementation.
**When**: `cargo nextest run --workspace` on C2 (or post-smoke on Host A with `--ignored`).
**Then**: All 8 named tests PASS. No previously-passing test regresses. BLOCKED_ON_SMOKE: Y.

---

## 5. Test Mapping Table

| Scenario | Test name | File | BLOCKED_ON_SMOKE |
|----------|-----------|------|-----------------|
| S1 | mft_encoded_packet_starts_with_annex_b_start_code | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S2 | (structural — AtomicBool semantics) | crates/sm-infra/src/encode/windows_mft.rs | N |
| S3 | (structural — flush() call while frame_tx alive) | crates/sm-infra/src/encode/windows_mft.rs | N |
| S4 | mft_encoded_packet_starts_with_annex_b_start_code (0-frame edge) | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S5 | (design-deferred multi-burst — OQ-4) | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S6 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S7 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S8 | (structural — flush+disconnect ordering, design-deferred) | crates/sm-infra/src/encode/windows_mft.rs | N |
| S9 | `cargo build --workspace --locked` on C1 | crates/sm-infra/tests/windows_mft_encode.rs | N |
| S10 | git diff on sm-domain/Cargo.toml | crates/sm-domain/src/encode.rs | N |
| S11.1 | mft_encoded_packet_starts_with_annex_b_start_code | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.2 | mft_first_real_packet_is_annex_b | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.3 | mft_encoded_packet_timestamp_matches_capture_frame | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.4 | mft_setup_uses_config_dimensions_when_nonzero | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.5 | mft_setup_falls_back_when_config_dimensions_zero | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.6 | mft_request_keyframe_marks_next_packet_as_keyframe | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.7 | mft_keyframe_flag_cleared_after_idr_emitted | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S11.8 | mft_set_bitrate_updates_encoder_without_restart | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S12 | Full 18-test run on Host B | crates/sm-infra/tests/windows_mft_encode.rs | Y |
| S13 | Phase 0 smoke trace (1-frame DRAIN, Host A) | smoke-trace.ps1 / ad-hoc | Y |
| S14 | `cargo nextest run --workspace` on C1, then C2 | crates/sm-infra/tests/windows_mft_encode.rs | Y |

---

## 6. Acceptance Criteria Checklist

- [ ] **AC-1** (Phase 0 trace): 1-frame DRAIN trace on Host A saved to engram topic `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace` with GOOD/PARTIAL/BAD classification BEFORE design phase begins.
- [ ] **AC-2** (Host A primary — 8 tests): All 8 named single-frame tests PASS on the fix branch on Host A. Currently 0/8 on master `daa9522`.
- [ ] **AC-3** (Host A total): 18/18 PASS target. 17/18 acceptable only if the remaining fail is a pre-existing separately-tracked issue (NOT a new regression).
- [ ] **AC-4** (Host B regression): 16/18 PASS maintained or improved. The 2 NVENC keyframe-flag fails are pre-existing and NOT regressions. No currently-passing Host B test regresses.
- [ ] **AC-5** (cross-vendor smoke): `mft_thirty_frame_smoke_emits_at_least_one_keyframe` PASSES on Host A and Host B. T-NEW-1 (`mft_stop_during_idle_returns_within_deadline`) and T-NEW-2 (`mft_stop_during_active_encode_returns_within_deadline`) remain GREEN on both hosts.
- [ ] **AC-6** (CI quality gates): `cargo build --workspace --locked`, `cargo nextest run --workspace`, `cargo clippy --all-targets --all-features --locked -- -D warnings`, and `cargo fmt --check --all` ALL GREEN.
- [ ] **AC-7** (sm-domain frozen): `git diff master daa9522 -- crates/sm-domain/src/encode.rs crates/sm-infra/Cargo.toml` = 0 lines.
- [ ] **AC-8** (LOC budget): final diff ≤ 50 lines. No chained PRs. Single PR, branch `feat/hw-encoder-mft-single-frame-flush`.
- [ ] **AC-9** (TDD audit): C1 (RED) commit exists in branch history before C2 (GREEN) commit. `git log --oneline` on the branch shows both.
- [ ] **AC-10** (post-fix smoke transcripts saved): engram topics `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-a-branch` and `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-b-regression` contain transcripts before verify phase issues APPROVED.

---

## 7. Risks (carried from proposal #707)

| Sev | Lik | Risk | Spec handling |
|-----|-----|------|---------------|
| HIGH | MED | Intel QSV does NOT honor `MFT_MESSAGE_COMMAND_DRAIN` for 1-frame submissions — the load-bearing assumption of Approach C fails | R12 gates design on Phase 0 trace evidence. D1 fallback: add 1 padding frame before flush; if DRAIN never produces output, escalate to re-explore. |
| MED | LOW | DRAIN is effectively terminal on Intel QSV — tests T6/T7/T8 cannot use flush() at a mid-stream point without blocking subsequent frame submissions | OQ-3 / OQ-4 deferred to design. Phase 0 secondary experiment required (S5). R9 tolerates design-deferred flush placement. |
| LOW | MED | NVENC regression: explicit `ProcessMessage(COMMAND_DRAIN)` while frame channel is open is untested on NVENC | R10, R13, S12 gate verify on Host B regression smoke. AC-4 documents acceptable 16/18 baseline. |
| LOW | LOW | `recv_timeout` deadlines (3s, 5s) may be insufficient if post-DRAIN Intel QSV has added latency | Phase 0 trace measures actual latency (S13). Design may extend deadlines if evidence warrants. |
| LOW | LOW | flush() + channel-disconnect ordering races (S8) | Spec-level: design-deferred (OQ-B). Design must document and pick deterministic ordering. |

---

## 8. Out of Scope (carried from proposal #707 D8)

- NVENC keyframe-flag detection (Host B's 2 known fails) — tracked as `hw-encoder-mft-nvenc-keyframe-flag` (Slice 4).
- Flipping `default = ["hw-encoder"]` — tracked as `hw-encoder-default-on-flip` (gates on this slice AND NVENC slice).
- Adding `flush()` or any drain method to the `VideoEncoder` trait — sm-domain FROZEN.
- Production callers of `flush()` — test affordance only.
- Refactoring channel-disconnect DRAIN site to share code with flag-driven DRAIN site.
- AMD AMF empirical verification — no hardware available.
- Stream-change-specific mock/shim test — requires trait `MftLike` architecture (Slice 2 D8 deferred).

---

## 9. Open Questions for Design Phase

| OQ | Status | Notes |
|----|--------|-------|
| OQ-1 (critical) | PHASE 0 GATE | Does Intel QSV honor DRAIN for 1-frame submissions? Blocks design lock. |
| OQ-2 | LOCKED | Inherent-only; trait FROZEN. |
| OQ-3 | DESIGN-DEFERRED | flush() call site in T6/T7/T8 multi-phase tests. Resolved once OQ-4 is known. |
| OQ-4 | DESIGN-DEFERRED | Is DRAIN terminal? Can encoder accept frames after flush()+DrainComplete? Phase 0 secondary. |
| OQ-5 | TRIVIAL (confirmed) | `mft_drain_after_channel_close_does_not_panic` passes on master daa9522 (in 10/18 PASS set). |
| OQ-A | DESIGN | Doc comment wording on flush() — warning about non-accepting state. R8 states requirement. |
| OQ-B | DESIGN | flush() + disconnect ordering (S8). Design picks deterministic ordering. |

---

## 10. SDD Chain Anchors

- **Predecessor**: PR #18 (`hw-encoder-mft-vendor-compat-rework`, Slice 2, archive #699, master `daa9522`).
- **This slice**: `hw-encoder-mft-single-frame-flush` (Slice 3). Branch `feat/hw-encoder-mft-single-frame-flush`.
- **Successor (blocked on this)**: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 4). Both gate `hw-encoder-default-on-flip`.
- **Engram topic_key chain**:
  - Explore #701: `sdd/hw-encoder-mft-single-frame-flush/explore`
  - Proposal #707: `sdd/hw-encoder-mft-single-frame-flush/proposal`
  - Spec (this): `sdd/hw-encoder-mft-single-frame-flush/spec`
  - Phase 0 trace (PENDING — blocks design): `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`
  - Design (BLOCKED on Phase 0): `sdd/hw-encoder-mft-single-frame-flush/design`
  - Tasks: `sdd/hw-encoder-mft-single-frame-flush/tasks`
  - Apply progress: `sdd/hw-encoder-mft-single-frame-flush/apply-progress`
  - Verify report: `sdd/hw-encoder-mft-single-frame-flush/verify-report`
  - Archive report: `sdd/hw-encoder-mft-single-frame-flush/archive-report`
