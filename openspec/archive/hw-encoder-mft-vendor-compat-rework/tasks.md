# Tasks: hw-encoder-mft-vendor-compat-rework
# (Slice 2 — Intel QSV stream-change renegotiation)

> Strict TDD ACTIVE. Test runner: `cargo nextest run --workspace`.
> Inputs: spec #633 (R1–R15, 22 scenarios), design #634 (DD1–DD10, 3-commit sequence), proposal #632 (D1–D8, single-PR lock).
> Artifact store: hybrid (engram `sdd/hw-encoder-mft-vendor-compat-rework/tasks` + this file).
> Delivery: SINGLE PR (`feat/hw-encoder-mft-stream-change-handling`) per D7.
> Branch from: master HEAD `3c8bc48`.
> Date: 2026-05-07.

---

## 1. Inputs

| Artifact       | Topic key                                             | Observation ID |
|----------------|-------------------------------------------------------|----------------|
| Spec           | `sdd/hw-encoder-mft-vendor-compat-rework/spec`        | #633           |
| Design         | `sdd/hw-encoder-mft-vendor-compat-rework/design`      | #634           |
| Proposal       | `sdd/hw-encoder-mft-vendor-compat-rework/proposal`    | #632           |
| Predecessor tasks (format reference) | `sdd/hw-encoder-mft-rework/tasks` | #598 |

---

## Commit Grouping Strategy

| Commit | Phase | Scope | Spec/Design refs | LOC |
|--------|-------|-------|-----------------|-----|
| C1 | Phase 1 | `tests/windows_mft_encode.rs` line 241–242 only | R8, DD7, D5 | ≤3 |
| C2 | Phase 2 | `src/encode/windows_mft.rs` | R1–R7, R9, DD1–DD6, DD8–DD9 | ~45 |
| C3 | Phase 3 (optional) | `src/encode/windows_mft.rs` (fmt only) | — | ≤10 |

No chained PRs. All three commits land in one PR (D7). Forecast ≤58 LOC total, well under 400-line budget.

---

## Phase 0 — Anchor (no commit; verification gate before any code change)

Tasks are sequential; all must pass before Phase 1 begins.

- [ ] **(T0.1)** Run `git rev-parse HEAD` and confirm output is `3c8bc48`. If HEAD differs, STOP and report to user.
  _Sources: delivery baseline._

- [ ] **(T0.2)** Read `crates/sm-infra/src/encode/windows_mft.rs` line 1317. Confirm the exact line reads:
  ```
  Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),
  ```
  This is the swallow line that is being replaced. Design note: earlier drafts referenced line 1338 — root-cause #630 and master `3c8bc48` both confirm **line 1317** is correct.
  _Sources: DD2, root-cause #630._

- [ ] **(T0.3)** Read `crates/sm-infra/tests/windows_mft_encode.rs` lines 241–242. Confirm current order is:
  ```rust
  producer.join().expect("producer thread should not panic");
  enc.stop().expect("stop should succeed");
  ```
  This is the WRONG order (join before stop) that Phase 1 will fix.
  _Sources: R8, DD7, D5._

- [ ] **(T0.4)** Read `crates/sm-infra/src/encode/windows_mft.rs` lines 589–630. Confirm `try_setup_output_type` exists with signature:
  ```rust
  fn try_setup_output_type(mft: &IMFTransform, w: u32, h: u32, framerate: u32, bitrate_bps: u32) -> Result<(), EncoderError>
  ```
  and that its body uses `GetOutputAvailableType(0, 0)` → SetUINT64 FRAME_SIZE → SetUINT64 FRAME_RATE → SetUINT32 AVG_BITRATE → `SetOutputType(0, &out_type, 0)`. This is the COM sequence that `renegotiate_output_type` (T2.1) will mirror.
  _Sources: R2, DD1._

- [ ] **(T0.5)** Read `crates/sm-infra/src/encode/windows_mft.rs` lines 1108–1148 (pump_loop HO arm). Confirm the current error arm classifier checks `reason.contains("ProcessOutput: 0x80004005")` and that the `Err(e)` arm is the outermost match branch. Phase 2 (T2.4) will insert the `renegotiate` check BEFORE this branch.
  _Sources: R6, DD4._

- [ ] **(T0.6)** Run `cargo nextest run --workspace` (no `--features hw-encoder`, no `--run-ignored`). Confirm GREEN exit. Record pass count (baseline for CI gate in Phase 3). If RED, STOP and report.
  _Sources: R13, AC-5._

- [ ] **(T0.7)** Cut branch from master:
  ```
  git checkout master
  git checkout -b feat/hw-encoder-mft-stream-change-handling
  ```
  _Sources: delivery, D7._

---

## Phase 1 — RED + Test Discipline (commit C1)

> **TDD discipline**: C1 converts an existing indefinite HANG into a clean, bounded assertion failure. No new test function is added. This is a RED-discipline commit per D8.

Tasks are sequential.

- [ ] **(T1.1 — RED, R8, S8.1, DD7)** In `crates/sm-infra/tests/windows_mft_encode.rs`, swap lines 241–242. Change FROM:
  ```rust
  producer.join().expect("producer thread should not panic");
  enc.stop().expect("stop should succeed");
  ```
  TO:
  ```rust
  enc.stop().expect("stop should succeed");
  producer.join().expect("producer thread should not panic");
  ```
  This is a single edit, 2-line swap, zero logical change to assertions. After this swap: if the encoder stalls, `enc.stop()` unblocks the bounded channel → producer exits → `producer.join()` returns → the test reaches `assert!(keyframe_count >= 1)` which fails within ~10s instead of hanging indefinitely.

- [ ] **(T1.2)** Run `cargo nextest run --workspace` (no `--run-ignored`). Confirm pass count is UNCHANGED (the `mft_thirty_frame_smoke_emits_at_least_one_keyframe` test is `#[ignore]`-gated, invisible to CI). GREEN required before committing.
  _Sources: R13, AC-5._

- [ ] **(T1.3 — VERIFICATION GATE, manual, Host A)** On Host A (Intel QSV, `Usuario\Desktop`), run the 30-frame test in isolation:
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --test-threads=1 mft_thirty_frame_smoke_emits_at_least_one_keyframe
  ```
  **Expected result**: The test now FAILS (assertion error `assert!(keyframe_count >= 1)` with `keyframe_count = 0`) within ≤10s, rather than hanging indefinitely. This is the RED evidence that the ordering fix surfaces the bug cleanly.
  
  Record the test output to engram as topic_key `sdd/hw-encoder-mft-vendor-compat-rework/c1-red-evidence`.

  If the test PASSES (not hangs, not fails) on this baseline: the root cause may have been addressed by other means. Stop and report before proceeding.
  _Sources: R8, S8.1, DD7, D5._

- [ ] **(T1.4)** Commit C1 with exact subject and body:
  ```
  test(infra): swap stop/join order in 30-frame smoke to expose stall as failure not hang

  enc.stop() must precede producer.join() so a stalled encoder does not
  deadlock the test. The bounded SyncSender channel fills when the pipeline
  stalls; producer.join() blocks indefinitely without stop() draining the
  receiver first.

  RED evidence: mft_thirty_frame_smoke_emits_at_least_one_keyframe now
  reports assert!(keyframe_count >= 1) failure within ~10s on Host A
  instead of hanging >30s.

  Spec R8, design DD7, proposal D5.
  ```
  NO `Co-Authored-By` footer.
  _Sources: R8, DD7._

---

## Phase 2 — GREEN core (commit C2)

> **TDD discipline**: C2 makes the existing RED tests GREEN by implementing the stream-change renegotiation. The 30-frame smoke (now RED per C1) and the 8 Host-A timing-out tests are the integration-level test suite. No new test functions required (D8).

Sub-tasks are sequential within Phase 2; each is a single tight edit batch.

- [ ] **(T2.1 — R2, R3, DD1)** Add new function `renegotiate_output_type` immediately after `try_setup_output_type` (after line 630, before the `// ── Encoder thread ──` comment at line 632). Signature and behaviour:
  ```rust
  fn renegotiate_output_type(
      mft: &IMFTransform,
      w: u32,
      h: u32,
      framerate: u32,
      bitrate_bps: u32,
  ) -> Result<(), EncoderError> {
      // WHY: async MFT spec — no flush, no NOTIFY_BEGIN_STREAMING resend (R4/R5, DD8)
      let out_type: IMFMediaType = unsafe { mft.GetOutputAvailableType(0, 0) }.map_err(|e| {
          EncoderError::EncodeFailed(format!("renegotiate: GetOutputAvailableType: 0x{:08X}", e.code().0))
      })?;
      unsafe {
          out_type.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64)).map_err(|e| {
              EncoderError::EncodeFailed(format!("renegotiate: SetUINT64 FrameSize: 0x{:08X}", e.code().0))
          })?;
          out_type.SetUINT64(&MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1).map_err(|e| {
              EncoderError::EncodeFailed(format!("renegotiate: SetUINT64 FrameRate: 0x{:08X}", e.code().0))
          })?;
          out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps).map_err(|e| {
              EncoderError::EncodeFailed(format!("renegotiate: SetUINT32 Bitrate: 0x{:08X}", e.code().0))
          })?;
          mft.SetOutputType(0, &out_type, 0).map_err(|e| {
              EncoderError::EncodeFailed(format!("renegotiate: SetOutputType: 0x{:08X}", e.code().0))
          })?;
      }
      Ok(())
  }
  ```
  Rules:
  - MUST map every COM error to `EncoderError::EncodeFailed` with `"renegotiate: <step>: 0x{HRESULT}"` prefix (DD9 inner HRESULT, NOT trigger HRESULT). `EncoderError::InitFailed` MUST NOT appear.
  - Function is module-private (no `pub` or `pub(crate)`) per DD1.
  - The WHY comment on the first line of the body is REQUIRED (R4, R5, DD8).
  - Run `cargo clippy --features hw-encoder -- -D warnings` after this task. Zero warnings required.
  _Sources: R2, R3, R4, R5, DD1, DD8, DD9, D1._

- [ ] **(T2.2 — R7, DD5)** In `collect_output`, add `tracing::trace!` log immediately after the `ProcessOutput` match block (after the `match ... { Ok(()) => {} ... }` block closes, before `let sample = match output.pSample.take()`). The log must appear on BOTH the Ok path and the error paths, so place it in the `Ok(())` arm and in the Err arms that do not immediately return. Actually, per DD5, the cleanest approach: add the trace BEFORE the match, impossible. Instead, add it as the first statement in the `Ok(())` arm AND replicate in the STREAM_CHANGE arm (T2.3 handles STREAM_CHANGE arm separately):

  In the `Ok(())` arm:
  ```rust
  Ok(()) => {
      tracing::trace!(dw_status = output.dwStatus, status, "collect_output: ProcessOutput ok");
  }
  ```

  NOTE: The `status` variable (call-level `u32`) is already declared at line 1312 (`let mut status: u32 = 0`). The `output.dwStatus` field is already available via `MFT_OUTPUT_DATA_BUFFER::dwStatus`. Both fields are currently declared but never logged.
  _Sources: R7, S7.1, S7.2, DD5._

- [ ] **(T2.3 — R1, R9, DD2, DD3, DD6)** Replace the `MF_E_TRANSFORM_STREAM_CHANGE` arm in `collect_output` (currently at line 1317). Change FROM:
  ```rust
  Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),
  ```
  TO:
  ```rust
  Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
      tracing::trace!(
          dw_status = output.dwStatus,
          status,
          "collect_output: STREAM_CHANGE — renegotiating output type"
      );
      *output_format_known = None;                                              // DD6: reset BEFORE renegotiate
      renegotiate_output_type(mft, w, h, framerate, bitrate_bps)?;             // DD9: propagate inner HRESULT
      return Ok(None);
  }
  ```

  This also requires extending the `collect_output` function signature to accept the new parameters. Change FROM:
  ```rust
  fn collect_output(
      mft: &IMFTransform,
      output_format_known: &mut Option<bool>,
      frame_timestamp: std::time::Duration,
      seq: &mut u64,
  ) -> Result<Option<EncodedPacket>, EncoderError>
  ```
  TO:
  ```rust
  fn collect_output(
      mft: &IMFTransform,
      output_format_known: &mut Option<bool>,
      frame_timestamp: std::time::Duration,
      seq: &mut u64,
      w: u32,
      h: u32,
      framerate: u32,
      bitrate_bps: u32,
  ) -> Result<Option<EncodedPacket>, EncoderError>
  ```

  The `(w, h)` values come from `effective_dimensions(cfg)` resolved ONCE before the loop in `pump_loop` (DD3). The `framerate` and `bitrate_bps` values come from `cfg` fields. Apply agent must update the `pump_loop` call site to pass these 4 new arguments (T2.4 handles the HO-arm error classifier update; this task handles the call-site arity change for the call on line 1112).

  Rules:
  - `*output_format_known = None` MUST appear BEFORE `renegotiate_output_type(...)` call (DD6: failure-path leaves cache invalid, which is safer than leaving a stale format cached).
  - The `?` on `renegotiate_output_type` propagates `Err(EncoderError::EncodeFailed("renegotiate: ..."))` up to `pump_loop`'s HO arm (T2.4 catches it).
  - `return Ok(None)` remains as the success path result (per R1).
  _Sources: R1, R9, S1.1, S9.1, DD2, DD3, DD6, D1, D6._

- [ ] **(T2.4 — R6, DD4)** In `pump_loop`'s HO arm error classifier (currently lines 1131–1146), add the `renegotiate` error check BEFORE the existing `"ProcessOutput: 0x80004005"` branch. Change FROM:
  ```rust
  Err(e) => {
      let reason = e.to_string();
      if reason.contains("ProcessOutput: 0x80004005") {
  ```
  TO:
  ```rust
  Err(e) => {
      let reason = e.to_string();
      if reason.contains("renegotiate") {
          tracing::error!("pump_loop: renegotiate_output_type failed: {e}");
          return;
      }
      if reason.contains("ProcessOutput: 0x80004005") {
  ```
  ORDER MATTERS: `renegotiate` check FIRST. The existing `"ProcessOutput: 0x80004005"` E_UNEXPECTED vendor-priming branch follows unchanged.

  Rules:
  - `tracing::error!` is the correct level per DD5 and existing pump_loop fatal-error convention (OQ-D resolved).
  - `return;` exits pump_loop cleanly. No retry. Packet channel disconnects (consumer sees recv() → Err(Disconnected)).
  - No new `EncoderError` variants (R14).
  _Sources: R6, S6.1, DD4, D3, OQ-D._

- [ ] **(T2.5 — R4, R5, DD8)** Confirm the WHY comment added in T2.1 reads exactly:
  ```rust
  // WHY: async MFT spec — no flush, no NOTIFY_BEGIN_STREAMING resend (R4/R5, DD8)
  ```
  This is the only WHY comment required in `renegotiate_output_type`. If the apply agent placed it correctly in T2.1, this is a verification-only task (no edit needed).
  _Sources: R4, R5, DD8, D2._

- [ ] **(T2.6)** Run `cargo build --features hw-encoder` on Host A. Verify clean compile, zero errors. If compile fails, fix before proceeding.
  _Sources: R13, AC-5._

- [ ] **(T2.7)** Run `cargo nextest run --workspace` (without `--features hw-encoder`, no `--run-ignored`). Verify GREEN, pass count unchanged from T0.6 baseline. This confirms no cross-platform regression in non-HW test suite.
  _Sources: R13, AC-5._

- [ ] **(T2.8)** Run `cargo clippy --features hw-encoder --tests -- -D warnings`. Zero warnings required. Fix any clippy lints before committing.
  _Sources: R13._

- [ ] **(T2.9)** Commit C2 with exact subject and 5-10 line body:
  ```
  feat(infra): handle MF_E_TRANSFORM_STREAM_CHANGE via renegotiate_output_type

  Add renegotiate_output_type() private helper that mirrors try_setup_output_type()
  COM sequence (GetOutputAvailableType + SetOutputType) but maps errors to
  EncoderError::EncodeFailed("renegotiate: <step>: 0x{HRESULT}") (DD9).

  collect_output: STREAM_CHANGE arm now resets output_format_known=None (DD6)
  then calls renegotiate_output_type (R1, DD2). collect_output signature extended
  with (w, h, framerate, bitrate_bps) (DD3). pump_loop HO error arm gains
  renegotiate-string classifier before E_UNEXPECTED branch (R6, DD4).
  Always-on trace!(dwStatus, status) added after ProcessOutput (R7, DD5).

  No flush, no NOTIFY_BEGIN_STREAMING resend (async MFT spec, R4/R5, DD8).
  sm-domain FROZEN, default=[] unchanged (R14, R15).

  Spec R1, R6, DD1, DD9.
  ```
  NO `Co-Authored-By` footer.
  _Sources: R1, R6, DD1, DD9._

---

## Phase 3 — Polish (commit C3, OPTIONAL)

- [ ] **(T3.1)** Run `cargo fmt --check --all`. If diff is NON-EMPTY:
  1. Run `cargo fmt --all`.
  2. Run `cargo nextest run --workspace` again — must still be GREEN.
  3. Commit: `style(infra): cargo fmt for stream-change handling`
  
  If `cargo fmt --check --all` reports NO diff: SKIP this commit entirely. Do NOT create an empty commit.
  _Sources: design C3 (optional)._

- [ ] **(T3.2)** Run `cargo clippy --features hw-encoder --tests -- -D warnings` one final time after any fmt changes. Zero warnings. Fix any new lints introduced by fmt reformat.
  _Sources: R13._

---

## Phase 4 — BLOCKED_ON_SMOKE (USER-DRIVEN GATE)

> NOT a commit. Apply phase exits with a handoff note to the user.
> The apply agent MUST NOT issue APPROVED or merge the PR before both smoke transcripts are saved.

- [ ] **(T4.1 — BLOCKED_ON_SMOKE, Host A primary, R11, AC-1, AC-2)** USER ACTION: On Host A (Intel QSV, `Usuario\Desktop`), run:
  ```powershell
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode `
    --run-ignored=all --test-threads=1 --no-fail-fast
  ```
  **Expected**: 18/18 PASS (or ≥17/18 with at most 1 new failure mode per AC-2). Previously 9/18 PASS + 1 hang on master 3c8bc48 (#628).

  Tests that were timing out and must now PASS (R11):
  1. mft_encoded_packet_starts_with_annex_b_start_code
  2. mft_encoded_packet_timestamp_matches_capture_frame
  3. mft_request_keyframe_marks_next_packet_as_keyframe
  4. mft_keyframe_flag_cleared_after_idr_emitted
  5. mft_set_bitrate_updates_encoder_without_restart
  6. mft_first_real_packet_is_annex_b
  7. mft_setup_uses_config_dimensions_when_nonzero
  8. mft_setup_falls_back_when_config_dimensions_zero

  T-NEW-1 (`mft_stop_during_idle_returns_within_deadline`) and T-NEW-2 (`mft_stop_during_active_encode_returns_within_deadline`) must remain PASS (R10, AC-3).
  _Sources: R10, R11, S10.1, S10.2, S11.1–S11.8, AC-1, AC-2, AC-3._

- [ ] **(T4.2 — BLOCKED_ON_SMOKE, Host B regression, R12, AC-4)** USER ACTION: On Host B (NVENC, `JDNHS\OneDrive`), run:
  ```powershell
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode `
    --run-ignored=all --test-threads=1 --no-fail-fast
  ```
  **Expected**: ≥16/18 PASS. Pre-existing 2 failures (force-IDR tests, tracked under separate change `hw-encoder-mft-nvenc-setup-fix`) are accepted. No currently-passing test on Host B SHALL regress.
  _Sources: R12, S12.1, AC-4._

- [ ] **(T4.3 — BLOCKED_ON_SMOKE, trace verification, R7, DD5)** USER ACTION: Run `smoke-trace.ps1` on Host A with `RUST_LOG=sm_infra::encode=trace`, pointed at the 30-frame test only. Expected trace pattern: NO 38000+ heartbeat with `ni_count=0`/`ho_count=0` frozen stall (per #629 baseline). Instead: observe steady `METransformNeedInput`/`METransformHaveOutput` cadence OR a trace line containing `"collect_output: STREAM_CHANGE — renegotiating output type"` followed by resumed `HaveOutput` events.
  _Sources: R7, S7.1, S7.2, DD5._

- [ ] **(T4.4)** Save both transcripts to engram:
  - Host A post-fix: topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-a-postfix`
  - Host B regression: topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-b-postfix-regression`
  
  The `sdd-verify` agent REQUIRES both transcripts to exist in engram before it can issue APPROVED.
  _Sources: BLOCKED_ON_SMOKE rule, AC-1–AC-4._

---

## Phase 5 — Verify (`sdd-verify` invocation)

- [ ] **(T5.1)** The verify agent reads:
  - Spec #633 (R1–R15, 22 scenarios)
  - Design #634 (DD1–DD10)
  - Tasks (this artifact)
  - Apply-progress (topic_key `sdd/hw-encoder-mft-vendor-compat-rework/apply-progress`)
  - Smoke transcript Host A (topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-a-postfix`)
  - Smoke transcript Host B (topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-b-postfix-regression`)

  Issues one of: **APPROVED**, **APPROVED_WITH_CARRY_FORWARD**, or **BLOCKED**.
  
  Cannot issue APPROVED if either smoke transcript is absent. The BLOCKED_ON_SMOKE rule is absolute.
  _Sources: BLOCKED_ON_SMOKE rule #586, AC-1–AC-5._

---

## Phase 6 — Archive (`sdd-archive` invocation)

- [ ] **(T6.1 — PR creation)** The archive agent creates PR with:
  - Title: `feat(infra): handle MF_E_TRANSFORM_STREAM_CHANGE for vendor compat`
  - Branch: `feat/hw-encoder-mft-stream-change-handling` → `master`
  - Body sections: Summary / Commits (C1, C2, optional C3) / Gates (7/7 quality gates) / Test plan (18/18 Host A + 16/18 Host B smoke) / SDD artifacts (engram #632, #633, #634, tasks topic_key)
  - NO issue link, NO labels (PR convention from init #186)
  _Sources: D7, PR convention._

- [ ] **(T6.2 — merge)** User merges via:
  ```
  gh pr merge --merge --delete-branch
  ```
  If `--delete-branch` is not honored: `git push origin --delete feat/hw-encoder-mft-stream-change-handling` manually.
  _Sources: D7, PR convention._

- [ ] **(T6.3 — sdd-init refresh)** Archive sub-agent updates `sdd-init/screen-mirror-app` (#186):
  - Increment archived change count.
  - Update master HEAD to post-merge SHA.
  - Refresh v2 candidates list: mark `hw-encoder-mft-vendor-compat-rework` archived; `hw-encoder-default-on-flip` remains next candidate.
  _Sources: sdd-init #186._

---

## 9. Review Workload Forecast

```
Estimated changed lines (production):  ~45 LOC  (windows_mft.rs — renegotiate_output_type ~30 LOC,
                                                   collect_output arm rewrite ~8 LOC,
                                                   collect_output sig + pump_loop call-site ~5 LOC,
                                                   pump_loop HO classifier ~4 LOC,
                                                   trace! log additions ~3 LOC)
Estimated changed lines (test):         ~3 LOC  (windows_mft_encode.rs — 2-line swap + 1-line diff)
Estimated changed lines (style):        ≤10 LOC (cargo fmt — likely 0)
TOTAL CHANGED LINES:                    ~58 LOC
400-line budget risk:                   Low
Chained PRs recommended:                No
Decision needed before apply:           No
Single-PR override locked by:           proposal D7
```

---

## 10. Coverage Matrix (R# → Task → Test → BLOCKED_ON_SMOKE)

| Req | Scenario | Task(s) | Test name | BLOCKED_ON_SMOKE |
|-----|----------|---------|-----------|-----------------|
| R1 | S1.1 | T2.3 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | Y |
| R1 | S1.2 | T2.3 | (code review / diff) | N |
| R2 | S2.1 | T2.1, T2.3 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | Y |
| R3 | S3.1 | T2.1 | (structural type check / code review) | N |
| R4 | S4.1 | T2.1 (WHY comment), T2.3 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | Y |
| R5 | S5.1 | T2.1 (WHY comment) | mft_thirty_frame_smoke_emits_at_least_one_keyframe | Y |
| R6 | S6.1 | T2.4 | (code review + tracing output) | N |
| R7 | S7.1 | T2.2 | smoke-trace.ps1 RUST_LOG=trace (T4.3) | N |
| R7 | S7.2 | T2.2, T2.3 | smoke-trace.ps1 RUST_LOG=trace (T4.3) | N |
| R8 | S8.1 | T1.1 | mft_thirty_frame_smoke_emits_at_least_one_keyframe | Y |
| R9 | S9.1 | T2.3 | (code review; re-detection via smoke) | N (partial) |
| R10 | S10.1 | (no change) | mft_stop_during_idle_returns_within_deadline | Y |
| R10 | S10.2 | (no change) | mft_stop_during_active_encode_returns_within_deadline | Y |
| R11 | S11.1 | T2.1–T2.4 | mft_encoded_packet_starts_with_annex_b_start_code | Y |
| R11 | S11.2 | T2.1–T2.4 | mft_encoded_packet_timestamp_matches_capture_frame | Y |
| R11 | S11.3 | T2.1–T2.4 | mft_request_keyframe_marks_next_packet_as_keyframe | Y |
| R11 | S11.4 | T2.1–T2.4 | mft_keyframe_flag_cleared_after_idr_emitted | Y |
| R11 | S11.5 | T2.1–T2.4 | mft_set_bitrate_updates_encoder_without_restart | Y |
| R11 | S11.6 | T2.1–T2.4 | mft_first_real_packet_is_annex_b | Y |
| R11 | S11.7 | T2.1–T2.4 | mft_setup_uses_config_dimensions_when_nonzero | Y |
| R11 | S11.8 | T2.1–T2.4 | mft_setup_falls_back_when_config_dimensions_zero | Y |
| R12 | S12.1 | (no change; regression guard) | Full 18-test run on Host B | Y |
| R13 | — | T0.6, T1.2, T2.7, T2.8, T3.2 | cargo nextest run --workspace | N |
| R14 | — | T2.1, T2.3 | (code review: no new EncoderError variants) | N |
| R15 | — | (no change) | (code review: Cargo.toml default=[]) | N |

---

## 11. Risk Register

| Risk | Sev | Lik | Task-level mitigation |
|------|-----|-----|-----------------------|
| Renegotiation success on Intel QSV mid-stream not yet empirically confirmed | MED | MED | Mandatory T4.1 Host A smoke. Trace logging (T2.2) captures per-step HRESULT for post-mortem. If smoke fails: escalate to add `COMMAND_FLUSH` retry (separate change). |
| `GetOutputAvailableType` mid-stream may return type with vendor-changed FRAME_SIZE | MED | LOW | T2.2 trace!(dwStatus) captures divergence. T4.3 trace-transcript task specifically checks for this. Follow-up design decides honour-vs-override if seen. |
| Line-number drift: design references line 1317 for swallow + 1108–1148 for HO arm | LOW | LOW | T0.2 and T0.5 are explicit anchor tasks that verify exact line content before any edit. Apply agent must re-read current line numbers before editing. |
| `collect_output` signature extension (4 new args) requires pump_loop call-site update | LOW | LOW | T2.3 explicitly lists both the arm replacement AND the signature extension AND the call-site update as a single atomic batch. Apply agent must not split across commits. |
| DD4 string-match `"renegotiate"` brittle | LOW | LOW | Pattern precedent from #597 DR-NEW-2 (`"ProcessOutput: 0x80004005"`). Typed variant requires unfreezing sm-domain (R14). Acceptable per D3 + DD4 design decision. |
| OQ-C deferred (Ok-path `dwStatus & FORMAT_CHANGE`) | LOW | LOW | DD5 trace logging (T2.2) is the early-warning mechanism. T4.3 trace-transcript inspection specifically watches for this pattern. |
| Host B NVENC regression | LOW | LOW | New code path (`MF_E_TRANSFORM_STREAM_CHANGE` arm) is unreachable from NVENC's pre-streaming `SetOutputType: 0xC00D6D76` failure path. T4.2 mandatory regression smoke confirms. |

---

## Result Contract

- **status**: done
- **executive_summary**: 31 ordered tasks across 7 phases (anchor T0.1–T0.7, RED T1.1–T1.4, GREEN T2.1–T2.9, optional polish T3.1–T3.2, BLOCKED_ON_SMOKE user gate T4.1–T4.4, verify T5.1, archive T6.1–T6.3). 3 logical commits (C1 RED ≤3 LOC, C2 GREEN ~45 LOC, optional C3 fmt ≤10 LOC). Total ≤58 LOC, well within 400-line budget. Single-PR locked by proposal D7. BLOCKED_ON_SMOKE gate enforced at T4.4 before verify can issue APPROVED. Both Host A and Host B smoke transcripts are mandatory pre-conditions for archive.
- **artifacts**: engram `sdd/hw-encoder-mft-vendor-compat-rework/tasks` + `openspec/changes/hw-encoder-mft-vendor-compat-rework/tasks.md`
- **next_recommended**: sdd-apply
- **risks**: MED renegotiation success on Intel QSV unverified (T4.1 gate); LOW line-number drift between design refs and current code (T0.2, T0.5 anchor tasks); LOW collect_output signature extension arity mismatch (T2.3 batches both changes atomically); LOW string-match brittleness (DD4 precedent).
- **skill_resolution**: injected
- **review_workload_forecast**:
  - estimated_production_loc: ~45
  - estimated_test_loc: ~3
  - estimated_style_loc: ≤10
  - total_changed_lines: ~58
  - budget_risk: Low
  - chained_prs_recommended: No
  - decision_needed_before_apply: No
  - single_pr_override_locked_by: proposal D7
