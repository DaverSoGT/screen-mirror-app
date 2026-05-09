# Tasks: hw-encoder-mft-single-frame-flush (Slice 3)

> Strict TDD ACTIVE. Test runner: `cargo nextest run --workspace`.
> Inputs: design #712 (10 DDs), spec #708 (R1–R15, S1–S14), proposal #707 (D1–D8 single-PR), Phase 0 trace #710.
> Delivery: single-PR (D5 lock).
> Artifact store: hybrid (engram `sdd/hw-encoder-mft-single-frame-flush/tasks` + `openspec/changes/hw-encoder-mft-single-frame-flush/tasks.md`).
> Branch: `feat/hw-encoder-mft-single-frame-flush` from master HEAD `daa9522`.
> Date: 2026-05-09.

---

## Inputs

| Artifact | Topic key | ID |
|----------|-----------|-----|
| Proposal | sdd/hw-encoder-mft-single-frame-flush/proposal | #707 |
| Spec | sdd/hw-encoder-mft-single-frame-flush/spec | #708 |
| Design (primary) | sdd/hw-encoder-mft-single-frame-flush/design | #712 |
| Phase 0 trace | sdd/hw-encoder-mft-single-frame-flush/phase-0-trace | #710 |
| Predecessor tasks (format) | sdd/hw-encoder-mft-vendor-compat-rework/tasks | #635 |

---

## Commit Grouping Strategy

| Commit | Phase | Scope | Spec/Design refs | Net LOC |
|--------|-------|-------|-----------------|---------|
| C1 RED | Phase 1 | `tests/windows_mft_encode.rs` + stub in `windows_mft.rs` | R14, DD6, DD7, DD9 | ~40 ins / ~0 del |
| C2 GREEN | Phase 2 | `src/encode/windows_mft.rs` | R1–R6, R8, DD1–DD5, DD3–DD4, DD9 | ~26 ins / ~1 del |
| C3 POLISH | Phase 3 | `windows_mft.rs` (fmt only) | — | ≤10 |

No chained PRs. All commits land in one single PR. Forecast ~63 ins / ~26 del = ~37 net LOC. Well under 400-line budget.

---

## Phase 0 — Anchor (no commit; verification gate before any code change)

Tasks are sequential. All must pass before Phase 1 begins.

- [ ] T0.1 Run `git rev-parse HEAD` and confirm output is `daa9522`. If not, abort and report drift.
- [ ] T0.2 Confirm Phase 0 probe tests already present in `crates/sm-infra/tests/windows_mft_encode.rs` at lines 868+: `mft_one_frame_drain_probe_phase_0` and `mft_two_frame_drain_probe_phase_0` both exist with `#[ignore]`. (Phase 0 trace #710 states they were added to the working tree.)
- [ ] T0.3 Run `cargo nextest run --workspace` (no `--run-ignored`). Confirm GREEN baseline: 611 passed, 19 skipped. The 2 probe tests count as skipped (ignored). Record baseline count.
- [ ] T0.4 Confirm git status clean: no staged or unstaged changes in `crates/sm-infra/src/encode/windows_mft.rs` or `crates/sm-infra/tests/windows_mft_encode.rs`. (Probe tests must already be committed or cleanly in place — confirm their status.)
- [ ] T0.5 Cut branch `feat/hw-encoder-mft-single-frame-flush` from `daa9522` if not already on it: `git checkout -b feat/hw-encoder-mft-single-frame-flush`. If branch already exists at correct base, `git checkout feat/hw-encoder-mft-single-frame-flush` and verify `git merge-base HEAD master` == `daa9522`.
- [ ] T0.6 Confirm sdd-init Strict TDD is ACTIVE for this project (engram #186 v11 — `strict_tdd: true`, test runner: `cargo nextest run --workspace`).

---

## Phase 1 — RED (commit C1)

Tasks are sequential.

**Spec refs**: R14 (3-commit TDD sequence), DD6 (test placement strategy), DD7 (commit C1 definition), DD9 (flush() always-pub, no cfg gate).

### T1.1 — Add no-op stub flush() to WindowsMftH264Encoder

In `crates/sm-infra/src/encode/windows_mft.rs`, at the inherent impl block starting at line 1565 (`impl WindowsMftH264Encoder`), add immediately after the opening brace (or after the last existing method's closing brace — whichever keeps the impl block tidy):

```rust
/// (stub — see real implementation in C2)
pub fn flush(&self) {}
```

3 LOC. No `drain_pending` field. No pump_loop change. No doc comment beyond the stub marker. Visibility: `pub` unconditionally (DD9 — `#[cfg(test)]` would break integration tests).

### T1.2 — Add enc.flush() to T1–T5 (single-frame tests, single-line insertions)

In `crates/sm-infra/tests/windows_mft_encode.rs`, insert `enc.flush();` between the last `frame_tx.send(...)` and the subsequent `pkt_rx.recv_timeout(...)` in each of these 5 tests. Exact insertion points per DD6:

- `mft_encoded_packet_starts_with_annex_b_start_code` (~line 160): after `frame_tx.send(make_synthetic_frame(640, 480, 0)).unwrap();`
- `mft_first_real_packet_is_annex_b` (~line 528): after its `frame_tx.send(...)` line
- `mft_encoded_packet_timestamp_matches_capture_frame` (~line 286): after the timestamp frame send
- `mft_setup_uses_config_dimensions_when_nonzero` (~line 603): after its single `frame_tx.send(...)` line
- `mft_setup_falls_back_when_config_dimensions_zero` (~line 633): after its single `frame_tx.send(...)` line

5 insertions = 5 LOC.

### T1.3 — Restructure T6/T7/T8 (multi-phase tests, submit-all → flush() → recv-all)

In `crates/sm-infra/tests/windows_mft_encode.rs`, restructure these 3 tests per DD6. pkt_rx is `sync_channel(16)` so no backpressure (capacity 16 ≫ 5–6 frames).

**T6 — `mft_request_keyframe_marks_next_packet_as_keyframe`**:
Replace current interleaved send/recv body with:
1. send frames 0–3
2. `enc.request_keyframe();` (sets keyframe_pending BEFORE frame 4 submit)
3. send frame 4 (will be forced IDR)
4. `enc.flush();`
5. recv 5 packets in order; assert packet #5 (index 4) `is_keyframe == true` + Annex B start code

**T7 — `mft_keyframe_flag_cleared_after_idr_emitted`**:
Replace with:
1. send frames 0–2
2. `enc.request_keyframe();`
3. send frame 3 (forced IDR), send frame 4 (after IDR)
4. `enc.flush();`
5. recv 5 packets; assert packet #4 (index 3) `is_keyframe == true`; assert packet #5 (index 4) `is_keyframe == false`

**T8 — `mft_set_bitrate_updates_encoder_without_restart`**:
Replace with:
1. send frames 0–2 at 4 Mbps
2. `enc.set_bitrate(8_000_000).unwrap();` — MID-STREAM (preserves "live update" invariant; no flush before)
3. send frames 3–5
4. `enc.flush();`
5. recv 6 packets; assert encoder thread alive; assert `set_bitrate` returned `Ok(())`

Net change T6/T7/T8: ~32 LOC restructure + 3× `enc.flush()`. ~25 LOC deleted from old interleaved bodies.

### T1.4 — Verify cargo build CLEAN on C1

Run: `cargo build --workspace --locked`
Expected: CLEAN. Stub `pub fn flush(&self) {}` compiles; 8 tests call it and compile.
If any compile error: STOP and fix before proceeding.

### T1.5 — Verify cargo nextest GREEN on C1

Run: `cargo nextest run --workspace` (no `--run-ignored`, no `--features hw-encoder`)
Expected: 611 passed, 19 skipped. Exact same count as T0.3 baseline.
If count differs: STOP and investigate before committing.

### T1.6 — BLOCKED_ON_SMOKE: Host A RED confirmation

**Manual gate — user runs on Host A (Intel QSV).**
```
cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --no-capture --no-fail-fast --test-threads=1
```
Expected for C1 (stub): All 8 named single-frame tests STILL TIMEOUT. Proves test-side changes alone do not fix the bug.
Phase 0 probes (`mft_one_frame_drain_probe_phase_0`, `mft_two_frame_drain_probe_phase_0`) still PASS.
Save RED evidence to engram topic `sdd/hw-encoder-mft-single-frame-flush/c1-red-evidence`.
STATUS: BLOCKED_ON_SMOKE

### T1.7 — Commit C1

Subject: `test(infra): assert single-frame intel-qsv tests flush before recv`

Body: Reference DD6 flush placement strategy. Note stub flush() at windows_mft.rs:1565 — no-op, real impl follows in C2.

---

## Phase 2 — GREEN (commit C2)

Tasks are sequential.

**Spec refs**: R1 (flush() inherent), R2 (drain_pending AtomicBool), R3 (pump-loop check site), R5 (DrainComplete UNCHANGED), R6 (STREAM_CHANGE UNCHANGED), R8 (doc comment), DD1–DD5.

### T2.1 — Add drain_pending: AtomicBool field to MftEncoderShared

In `crates/sm-infra/src/encode/windows_mft.rs`, locate `MftEncoderShared` struct (~line 83). Add field after existing atomic fields:

```rust
drain_pending: AtomicBool,
```

1 LOC insertion.

### T2.2 — Initialize drain_pending in Default

Locate the `Default` impl for `MftEncoderShared` (~line 94). Add:

```rust
drain_pending: AtomicBool::new(false),
```

1 LOC insertion.

### T2.3 — Verify imports: AtomicBool and Ordering already imported

Check `use` declarations at top of `windows_mft.rs`. `AtomicBool` is already present (`keyframe_pending: AtomicBool` exists). `Ordering` already present (used by existing atomics). If either is missing, add to the existing `use std::sync::atomic::{...}` line. No new crate dependencies.

### T2.4 — Replace stub flush() with real implementation + doc comment

Replace the 3-LOC stub from T1.1 with:

```rust
/// Signal end-of-burst to the encoder pump loop, requesting a `MFT_MESSAGE_COMMAND_DRAIN`.
///
/// **Single-shot**: After `COMMAND_DRAIN` fires, Intel QSV enters a non-accepting state
/// until `METransformDrainComplete` fires (~250 ms empirically, Phase 0 trace). The encoder
/// is effectively terminal per session on Intel QSV — do not call `flush()` mid-stream.
///
/// **Async**: Returns immediately. The DRAIN fires on the next pump_loop iteration after the
/// NeedInput inner loop completes. Failures surface via `pkt_rx.recv_timeout`.
///
/// **Concurrency-safe**: Backed by `Arc<AtomicBool>`. Multiple concurrent calls collapse to at
/// most one `COMMAND_DRAIN` per pump iteration (last `store(true)` wins; swap-once consumption).
///
/// **Latency**: Empirically ~250 ms on Intel QSV (Host A, Phase 0 trace #710). Plan
/// `recv_timeout` deadlines accordingly (spec S11.x uses 3–5 s — sufficient margin).
///
/// **Production callers MUST NOT call this method.** It is a test affordance for single-burst
/// short-stream tests. Production streaming callers rely on the channel-disconnect DRAIN path.
pub fn flush(&self) {
    self.state.drain_pending.store(true, Ordering::Release);
}
```

~17 LOC. Replaces 3-LOC stub (net +14 LOC on this file).

### T2.5 — Add drain-flag check to pump_loop (DD3/DD4)

In `pump_loop` in `windows_mft.rs`, AFTER the `while ni_count > 0 { … }` NeedInput inner loop (~line 1297) and BEFORE the idle sleep check (~line 1300), insert:

```rust
// DD3/DD4: consume explicit flush() signal — fires exactly one COMMAND_DRAIN per flag-set.
// swap(false) resets atomically; subsequent flush() after DrainComplete re-arms.
if state.drain_pending.swap(false, Ordering::AcqRel) {
    tracing::info!("pump_loop: explicit flush() — sending COMMAND_DRAIN");
    unsafe { let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0); }
}
```

~5 LOC. Do NOT modify the channel-disconnect DRAIN arm at lines 1285–1294 (DD8 deferred). Do NOT break — fall through to idle sleep and continue outer loop. Post-DRAIN flow (STREAM_CHANGE → HaveOutput → DrainComplete) handled by existing Slice 2 code (R5, R6).

### T2.6 — cargo build CLEAN

Run: `cargo build --features hw-encoder`
Expected: CLEAN. No new warnings.

### T2.7 — cargo nextest GREEN

Run: `cargo nextest run --workspace`
Expected: 611 passed, 19 skipped. Baseline maintained.

### T2.8 — cargo clippy ZERO warnings

Run: `cargo clippy --all-targets --all-features --locked -- -D warnings`
Expected: zero warnings. If `drain_pending` triggers `dead_code` lint under non-hw-encoder feature, add:
```rust
#[allow(dead_code)] // allow: drain_pending only read by pump_loop under hw-encoder feature
```
Use `#[allow]` NOT `#[expect]` — project convention per skill-registry #188.

### T2.9 — BLOCKED_ON_SMOKE: Host A GREEN confirmation

**Manual gate — user runs on Host A (Intel QSV).**
```
cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --no-capture --no-fail-fast --test-threads=1
```
Expected for C2: All 8 named single-frame tests PASS. Phase 0 probes PASS.
If any of the 8 still timeout: STOP — do not commit C2.
If T6/T7/T8 fail due to out-of-order post-DRAIN emission: invoke DD6 SCOPE WARNING fallback (split each multi-phase test into separate `#[test]` functions — tracked as `hw-encoder-mft-multi-phase-test-split`).
Save GREEN evidence to engram topic `sdd/hw-encoder-mft-single-frame-flush/c2-green-evidence`.
STATUS: BLOCKED_ON_SMOKE

### T2.10 — Commit C2

Subject: `feat(infra): add flush() to WindowsMftH264Encoder for short-stream output`

Body: Reference DD1 (inherent flush, no trait change), DD2 (drain_pending AtomicBool), DD3/DD4 (pump_loop swap(false, AcqRel) — spec R3 uses compare_exchange; design adopts swap, functionally equivalent), DD8 (channel-disconnect DRAIN spam deferred to hw-encoder-mft-disconnect-drain-once). Fixes 8 single-frame Intel QSV tests that previously timed out.

---

## Phase 3 — Polish (commit C3, OPTIONAL)

### T3.1 — cargo fmt check

Run: `cargo fmt --check --all`
If diff non-empty: run `cargo fmt --all`, re-run `cargo nextest run --workspace` (confirm 611 GREEN), then commit C3.
If diff empty: skip C3 entirely.

### T3.2 — cargo clippy final pass

Run: `cargo clippy --all-targets --all-features --locked -- -D warnings`
Expected: zero warnings. Final pre-smoke gate.

### T3.3 — Commit C3 (only if T3.1 had diff)

Subject: `style(infra): cargo fmt for flush handler`

---

## Phase 4 — Smoke (BLOCKED_ON_SMOKE)

NOT a commit. Apply agent exits with handoff note. User runs manually on Host A and Host B.

- [ ] T4.1 — **(Host A, Intel QSV)** Full test suite smoke run — all 20 tests (18 named + 2 Phase 0 probes):
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --no-capture --no-fail-fast --test-threads=1
  ```
  Expected: ≥18/20 PASS. 8 previously-timing-out tests now PASS (10/18 baseline → 18/18 target). Phase 0 probes PASS. 30-frame smoke PASSES. Save full transcript to engram topic `sdd/hw-encoder-mft-single-frame-flush/smoke-host-a-postfix`.
  STATUS: BLOCKED_ON_SMOKE

- [ ] T4.2 — **(Host B, NVENC)** Regression smoke — same command on Host B:
  Expected: ≥16/20 PASS. No regression vs baseline #696 (16/18 pre-probe). The 2 NVENC keyframe-flag fails remain pre-existing. `flush()` never called by NVENC tests — zero exposure. Save transcript to engram topic `sdd/hw-encoder-mft-single-frame-flush/smoke-host-b-postfix-regression`.
  STATUS: BLOCKED_ON_SMOKE

- [ ] T4.3 — **(Host A, trace inspection)** Run with `RUST_LOG=sm_infra::encode=trace` for the 8 fixed single-frame tests. Confirm:
  1. `[PHASE_0_PROBE_*F] OUTCOME=GOOD` still holds for the 2 probes.
  2. For each of the 8 fixed tests: log line `pump_loop: explicit flush() — sending COMMAND_DRAIN` appears (T2.5 tracing::info!).
  3. For T6/T7/T8 restructured tests: packets arrive in FIFO order. If out-of-order: invoke DD6 SCOPE WARNING fallback before verify.
  STATUS: BLOCKED_ON_SMOKE

Both smoke transcripts (T4.1 + T4.2) MUST be saved before verify can issue APPROVED (R13, spec S7/S12).

---

## Phase 5 — Verify

- [ ] T5.1 Run `sdd-verify`. Verify agent reads: spec #708, design #712, tasks (this), apply-progress, smoke transcripts (`smoke-host-a-postfix` + `smoke-host-b-postfix-regression`). Cannot issue APPROVED without both transcripts (R13). Resolves AC-1 through AC-10.

---

## Phase 6 — Archive

- [ ] T6.1 `sdd-archive` — persist archive-report to engram and openspec.
- [ ] T6.2 Push branch: `git push origin feat/hw-encoder-mft-single-frame-flush`
- [ ] T6.3 PR creation: title `feat(infra): add flush() to WindowsMftH264Encoder for short-stream output`. Body: Summary / 3-commit TDD chain (C1–C3 SHAs) / Quality gates / Test plan (8 tests fixed, Host A + B smoke) / SDD artifact chain (#701 → #707 → #708 → #710 → #712 → tasks #714 → apply → verify → archive). Single PR per D5 lock. NO issue link, NO labels.
- [ ] T6.4 Merge: `gh pr merge --merge --delete-branch`
- [ ] T6.5 Post-merge sdd-init refresh: update engram #186 (increment archived count, new master HEAD, confirm hw-encoder-mft-nvenc-keyframe-flag as next v2 candidate).

---

## Review Workload Forecast

| Metric | Value |
|--------|-------|
| Net LOC (design #712 forecast) | ~37 (~63 ins / ~26 del) |
| Files touched | 2 (`windows_mft.rs`, `windows_mft_encode.rs`) |
| Phases | 6 |
| Total tasks | 26 |
| Chained PRs recommended | No |
| 400-line budget risk | Low (37 ≪ 400) |
| Decision needed before apply | No |
| Delivery strategy | single-pr (locked, D5 proposal #707) |

---

## Files Affected

- `crates/sm-infra/src/encode/windows_mft.rs` (~26 LOC: `drain_pending: AtomicBool` field + Default init + `flush()` with 14-line doc + pump_loop check ~5 LOC; ~1 del for stub replacement)
- `crates/sm-infra/tests/windows_mft_encode.rs` (~37 ins / ~25 del: 5× single-line `enc.flush()` + 3× test body restructure for T6/T7/T8)
- (NO changes) `crates/sm-domain/src/encode.rs` — FROZEN (R7, R14)
- (NO changes) `crates/sm-infra/Cargo.toml` — FROZEN, `default = []` unchanged (R15)

---

## Coverage Matrix (R# → Task)

| Req | DD | Task(s) | BLOCKED_ON_SMOKE |
|-----|-----|---------|-----------------|
| R1 (pub flush inherent) | DD1, DD9 | T1.1, T2.4 | N (compile) |
| R2 (drain_pending AtomicBool) | DD2 | T2.1, T2.2, T2.3 | N (compile) |
| R3 (pump check site, swap) | DD3, DD4 | T2.5 | N (structural) |
| R4 (flush while frame_tx alive) | DD1 | T1.2, T1.3 | Y (smoke) |
| R5 (DrainComplete UNCHANGED) | — | no change | N (code review) |
| R6 (STREAM_CHANGE UNCHANGED) | — | no change | N (code review) |
| R7 (sm-domain FROZEN) | — | no change | N (git diff) |
| R8 (doc comment) | DD5 | T2.4 | N (code review) |
| R9 (8 tests PASS on Host A) | DD6 | T1.2, T1.3, T2.4, T2.5 | Y (T1.6, T2.9, T4.1) |
| R10 (no regression) | — | T4.1, T4.2 | Y |
| R11 (7 quality gates GREEN) | — | T1.4, T1.5, T2.6, T2.7, T2.8, T3.1, T3.2 | Partial |
| R12 (Phase 0 before design) | DD10 | T0.2 (already done, #710) | N |
| R13 (BLOCKED_ON_SMOKE verify gate) | — | T4.1, T4.2 | Y |
| R14 (TDD 3-commit sequence) | DD7 | T1.7, T2.10, T3.3 | Y (T1.6, T2.9) |
| R15 (Cargo.toml default=[] FROZEN) | — | no change | N |

---

## SDD Chain Anchors

- Predecessor: PR #18 (`hw-encoder-mft-vendor-compat-rework`, Slice 2, archive #699, master `daa9522`)
- This slice: `hw-encoder-mft-single-frame-flush` (Slice 3). Branch `feat/hw-encoder-mft-single-frame-flush`.
- Successor (blocked on this): `hw-encoder-mft-nvenc-keyframe-flag` (Slice 4)
- Engram chain: explore #701 → proposal #707 → spec #708 → phase-0-trace #710 → design #712 → tasks #714 → apply-progress → verify-report → archive-report
