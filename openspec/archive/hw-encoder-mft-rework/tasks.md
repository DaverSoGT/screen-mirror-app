# Tasks: hw-encoder-mft-rework

> Strict TDD ACTIVE. Test runner: `cargo nextest run --workspace`.
> Inputs: spec #596 (R1–R13, 33+ scenarios), design #597 (Pattern B + Fix B, 10 DDs, 3-commit sequence), proposal #595 (9 locked decisions, Decision #3 single-PR override active), explore #594, init #186.
> Artifact store: hybrid (engram `sdd/hw-encoder-mft-rework/tasks` + this file).
> Delivery: SINGLE PR (`feat/hw-encoder-mft-rework`) per Decision #3.
> Branch from: master HEAD `f01f27f`.

---

## Commit Grouping Strategy

| Commit | Phase | Scope | Rationale |
|--------|-------|-------|-----------|
| C1 | Phase 1 | `tests/windows_mft_encode.rs` only | Two new smoke tests land RED; `#[ignore]`-gated so CI stays GREEN |
| C2 | Phase 2 | `src/encode/windows_mft.rs` | Full pump_loop redesign: NO_WAIT polling, dual-arm counters, drain-first, DD4/DD5/DD8 error+logging arms, helper extraction, new const + imports |
| C3 | Phase 3 | `src/encode/windows_mft.rs` | Adds `METransformDrainComplete` arm with counter reset; small and self-contained |

No chained PRs. All three commits land in one PR per proposal Decision #3 (diff ~245–265 LOC, inside 400-line budget).

---

## Phase 0 — Anchor (no commit; verification gate before any code change)

These tasks MUST all pass before Phase 1 begins. Any failure is a hard stop.

- [ ] **(anchor-0.1)** Confirm all 5 quality gates are GREEN on master HEAD `f01f27f` with no local changes:
  1. `cargo check --workspace`
  2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  3. `cargo fmt --check --all` (run as `cargo fmt --check --all; echo $?` — NOT piped, per discovery #581)
  4. `cargo nextest run --workspace`
  5. `cargo deny check`

  Expected: all exit 0. Record pass counts in apply-progress.
  **Satisfies**: NF6 baseline; DR-NEW-1 precondition.

- [ ] **(anchor-0.2)** Verify that all 5 new `windows` crate symbols required by the redesign resolve under `--features hw-encoder`. Run:
  ```
  cargo check -p sm-infra --features hw-encoder 2>&1
  ```
  after adding a temporary `use` of each symbol in `windows_mft.rs` (or via `cargo doc` inspection). Symbols to confirm:
  - `MF_E_NO_EVENTS_AVAILABLE` (HRESULT 0x80040204)
  - `MF_E_SHUTDOWN` (HRESULT 0xC00D3E85)
  - `MF_E_NOTACCEPTING` (HRESULT 0xC00D36B5)
  - `METransformDrainComplete`
  - `MF_EVENT_FLAG_NO_WAIT`

  If ANY symbol is missing → STOP. Escalate per DR-NEW-1 (Cargo feature flag fix required before proceeding). Do NOT write any pump_loop code until all 5 resolve.
  **Satisfies**: DR-NEW-1 (windows symbol availability gate).

- [ ] **(anchor-0.3)** Establish smoke baseline. On the HW host, run:
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --no-capture --no-fail-fast
  ```
  Count PASS/FAIL across all 16 existing `mft_*` tests. Expected per transcript #591: ≤3 PASS (the 3 tests that don't call `stop()`). Record exact count in apply-progress as the BEFORE anchor. If count is 0 PASS (total regression), investigate before proceeding.
  **Satisfies**: R10 BEFORE baseline; BLOCKED_ON_SMOKE rule tracking.

---

## Phase 1 — Tests RED (C1)

Add the 2 new stop-deadline smoke tests. Both MUST be RED (hang) under master HEAD. CI must remain GREEN (tests are `#[ignore]`-gated).

- [ ] **(test-1.1)** In `crates/sm-infra/tests/windows_mft_encode.rs`, add test `mft_stop_during_idle_returns_within_deadline`:
  - First line: `init_tracing();`
  - Construct `WindowsMftH264Encoder::new(EncoderConfig { width: 640, height: 480, bitrate_bps: 4_000_000, framerate: 30, ..Default::default() })`, assert `Ok`.
  - Create channels with capacity matching existing tests.
  - Call `enc.start(frame_rx, packet_tx)` → assert `Ok(())`.
  - Sleep 100 ms (let MFT initialize).
  - Send NO frames.
  - Call `enc.stop()`, assert `Ok(())`.
  - Assert `t0.elapsed() < Duration::from_secs(2)` (inline constant per DD7 — NOT a shared module).
  - Gate: `#[ignore = "hardware H.264 MFT required — stop-deadline smoke, run on GPU-capable host"]`
  - Expected RED: test hangs indefinitely at `enc.stop()` → `join()` deadlock because pump_loop is blocked in `GetEvent(MF_EVENT_FLAG_NONE)`.
  - RECORD RED transcript (test does not return within 10s, must be manually killed) in apply-progress.
  **Satisfies**: R8 (S8.1 — IGNORED in CI; S8.3 — RED before fix), R4 (S4.1 idle-path contract defined).

- [ ] **(test-1.2)** In the same file, add test `mft_stop_during_active_encode_returns_within_deadline`:
  - First line: `init_tracing();`
  - Construct encoder, start.
  - Send exactly 5 frames using `make_synthetic_frame(640, 480, i * 33)` spaced at 20 ms apart via `std::thread::sleep(Duration::from_millis(20))`. Do NOT drop `frame_tx` before calling stop.
  - Measure `t0 = Instant::now()`.
  - Call `enc.stop()` with `frame_tx` still in scope, assert `Ok(())`.
  - Assert `t0.elapsed() < Duration::from_secs(2)`.
  - Drop `frame_tx` after the assertion.
  - Gate: `#[ignore = "hardware H.264 MFT required — stop-deadline active encode smoke, run on GPU-capable host"]`
  - Expected RED: test hangs — even mid-stream, GetEvent blocks between MFT events.
  - RECORD RED transcript in apply-progress.
  **Satisfies**: R9 (S9.1 — IGNORED; S9.3 — RED before fix), R4 (S4.2 active-path contract defined).

- [ ] **(gate-1.3)** Run `cargo nextest run --workspace` (no `--run-ignored`). Confirm: count unchanged from anchor-0.1 (both new tests are `#[ignore]`-gated and do NOT run). Zero new failures.
  **Satisfies**: R8/S8.1, R9/S9.1 — CI-invisible while RED.

- [ ] **(chore-1.4)** Commit C1: `test(infra): add MFT stop-deadline smoke tests for idle and active paths (RED)`
  - Commit body includes: RED evidence (both tests hang), test names, spec references (R8, R9, R4), CI gate count unchanged.
  **Satisfies**: Strict TDD RED-before-GREEN discipline per #186.

---

## Phase 2 — pump_loop redesign (C2)

Full rewrite of the pump_loop body. After this commit, T-NEW-1 and T-NEW-2 turn GREEN, and all 16 existing tests should pass on HW. The `METransformDrainComplete` arm is NOT yet in this commit (comes in C3); unhandled DrainComplete events log a WARN but do not crash.

All tasks in this phase are sequential (share `windows_mft.rs`).

- [ ] **(impl-2.1)** Add new imports to the `use windows::...` block at the top of `crates/sm-infra/src/encode/windows_mft.rs`. Add:
  - `MF_E_NO_EVENTS_AVAILABLE`
  - `MF_E_SHUTDOWN`
  - `MF_E_NOTACCEPTING`
  - `METransformDrainComplete`
  - `MF_EVENT_FLAG_NO_WAIT`

  Remove `MF_EVENT_FLAG_NONE` from the import list ONLY IF it has no remaining uses outside pump_loop after the rewrite (verify with `cargo check`).
  **Satisfies**: DR-NEW-1 (symbols now in scope); R13/S13.2 (unsafe block count unchanged).

- [ ] **(impl-2.2)** Add module-scope constant near the `H264_PROFILE_MAIN` constant or other existing constants in `windows_mft.rs`:
  ```rust
  const POLLING_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);
  ```
  **Satisfies**: R5 (PUMP_SLEEP_MS = 1 ms locked; S5.2 — EncoderConfig gains no field); DD6.

- [ ] **(impl-2.3)** Extract helper function `apply_pending_codec_settings` from pump_loop body. The helper is a mechanical extraction of the keyframe-request and bitrate-change logic (current lines 767–788). Signature:
  ```rust
  fn apply_pending_codec_settings(codec_api: &ICodecAPI, state: &MftEncoderShared)
  ```
  Move BOTH the keyframe `SetValue` block and the bitrate `SetValue` block into this function. Call it at the top of the NeedInput arm (before frame fetch). This is a pure refactor — no behavior change. The `unsafe` blocks move inside the helper.
  **Satisfies**: DD9 (readability extract); R13/S13.2 (no new unsafe block count — moved, not added).

- [ ] **(impl-2.4)** Introduce two stack-local counters at pump_loop scope (before the `loop` block):
  ```rust
  let mut ni_count: u32 = 0;  // pending METransformNeedInput credits
  let mut ho_count: u32 = 0;  // pending METransformHaveOutput credits
  ```
  Also introduce sentinel variables for change-only DEBUG logging (DD8):
  ```rust
  let mut last_logged_ni: u32 = u32::MAX;
  let mut last_logged_ho: u32 = u32::MAX;
  let mut iter_count: u64 = 0;
  ```
  **Satisfies**: R2 (counter declaration); DD2; DD8.

- [ ] **(impl-2.5)** Replace the blocking `GetEvent(MF_EVENT_FLAG_NONE)` call (current line 743) with a `MF_EVENT_FLAG_NO_WAIT` call and handle the `MF_E_NO_EVENTS_AVAILABLE` case as idle (no panic, no log spam). The event-fetch section becomes:
  ```rust
  let event_opt = match unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
      Ok(e) => Some(e),
      Err(e) => {
          let code = e.code().0 as u32;
          match code {
              0x8004_0204 => None,  // MF_E_NO_EVENTS_AVAILABLE — idle
              0xC00D_3E85 | 0x8000_4004 => {  // MF_E_SHUTDOWN | E_ABORT
                  tracing::info!("pump_loop: MFT shutdown/abort (0x{:08X}), exiting", code);
                  break;
              }
              _ => {
                  tracing::error!("pump_loop: GetEvent unexpected HRESULT 0x{:08X}", code);
                  break;
              }
          }
      }
  };
  ```
  The rest of the event dispatch (event_type match) is gated on `if let Some(event) = event_opt`.
  **Satisfies**: R1/S1.1 (NO_WAIT flag used; does not block); R1/S1.2 (stop latency bounded); DD1; Fix B.

- [ ] **(impl-2.6)** Rewrite the event_type dispatch (currently the `if event_type == METransformNeedInput ...` chain). Replace with a `match event_type` on the resolved `event_type` value with these arms:

  **NeedInput arm** (`METransformNeedInput.0 as u32`):
  - `ni_count += 1`
  - Emit `tracing::trace!("pump_loop NeedInput ni={} ho={}", ni_count, ho_count)`

  **HaveOutput arm** (`METransformHaveOutput.0 as u32`):
  - `ho_count += 1`
  - Emit `tracing::trace!("pump_loop HaveOutput ni={} ho={}", ni_count, ho_count)`

  **EndOfStream arm** (`MEEndOfStream.0 as u32`):
  - `tracing::info!("pump_loop MEEndOfStream, exiting")`
  - `break`

  **Catch-all** (`_`):
  - `tracing::warn!("pump_loop unhandled event_type=0x{:08X}; continuing", event_type)`
  - (METransformDrainComplete will be 0x00000013 — currently falls here; WARN fires until C3)

  **Satisfies**: R2/S2.1–S2.3 (NI/HO counter increment); R6/S6.3 (DrainComplete WARN is addressed in C3, not here); R13 (no new unsafe surface).

- [ ] **(impl-2.7)** Add the drain-first service section AFTER the event dispatch block (runs every iteration, regardless of whether an event was received). Structure:

  ```rust
  // Drain-first ordering (R3, DD1): drain ALL HaveOutput credits before NeedInput.
  while ho_count > 0 {
      match collect_output(mft, output_format_known, current_ts, &mut seq) {
          Ok(Some(pkt)) => { /* try_send, handle Full/Disconnected */ }
          Ok(None) => {}  // E_UNEXPECTED vendor priming: consume credit + WARN (DD4)
          Err(e) => {
              let reason = e.to_string();
              if reason.starts_with("ProcessOutput: 0x80004005") {
                  // Vendor-priming E_UNEXPECTED — consume credit, WARN, continue (DD4)
                  tracing::warn!("pump_loop: vendor priming E_UNEXPECTED on HaveOutput — consuming credit");
              } else {
                  tracing::error!("collect_output: {e}");
                  break;
              }
          }
      }
      ho_count -= 1;  // decrement AFTER COM call regardless of Ok/Err (DD2)
  }

  // Service NeedInput credits (only when HaveOutput is fully drained).
  while ni_count > 0 {
      apply_pending_codec_settings(codec_api, state);
      match rx.recv_timeout(Duration::from_millis(50)) {
          Ok(frame) => {
              current_ts = frame.timestamp;
              nv12_convert(&frame, &mut nv12_scratch);
              if let Err(e) = submit_frame(mft, &nv12_scratch, frame.timestamp, frame_dur_100ns) {
                  let reason = e.to_string();
                  if reason.contains("MF_E_NOTACCEPTING") || reason.contains("0xC00D36B5") {
                      // Counter desync — unreachable in correct code (DD5)
                      debug_assert!(false, "pump_loop: MF_E_NOTACCEPTING — counter desync (DD5)");
                      tracing::error!("pump_loop: MF_E_NOTACCEPTING from ProcessInput — counter logic error");
                      return;
                  }
                  tracing::warn!("ProcessInput failed: {e}");
                  // skip frame, consume credit (DD2)
              }
              ni_count -= 1;  // decrement AFTER ProcessInput (DD2)
          }
          Err(RecvTimeoutError::Timeout) => {
              // No frame yet; do NOT consume NI credit (DD2). Break inner, re-poll events.
              break;
          }
          Err(RecvTimeoutError::Disconnected) => {
              // Upstream closed; send DRAIN command (existing behavior).
              unsafe { let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0); }
              tracing::info!("pump_loop: frame channel disconnected, sent DRAIN");
              // Do NOT consume NI credit; break inner.
              break;
          }
      }
  }
  ```

  **Satisfies**: R3/S3.1–S3.3 (HaveOutput drained before NeedInput); R2/S2.2/S2.4 (counters decremented post-COM); DD1, DD2, DD4, DD5.

- [ ] **(impl-2.8)** Add the idle sleep at the end of the loop body (after both service loops), conditional on no-event AND no work done:

  ```rust
  // Sleep only when idle: no MFT event AND no counter credits to service (R5, DD6).
  if event_opt.is_none() && ni_count == 0 && ho_count == 0 {
      std::thread::sleep(POLLING_SLEEP);
  }
  ```

  Add heartbeat counter increment:
  ```rust
  iter_count += 1;
  ```

  **Satisfies**: R5/S5.1 (1 ms sleep confirmed); R1/S1.2 (stop latency ≤ 2 × POLLING_SLEEP on idle).

- [ ] **(impl-2.9)** Add change-only DEBUG logging per DD8. After the service loops and before the sleep, emit counter snapshots ONLY when they changed from the last logged values:

  ```rust
  if ni_count != last_logged_ni || ho_count != last_logged_ho {
      tracing::debug!(
          "pump_loop counters: ni={} ho={} iter={}",
          ni_count, ho_count, iter_count
      );
      last_logged_ni = ni_count;
      last_logged_ho = ho_count;
  }
  // Heartbeat: one DEBUG per 1000 iterations regardless of counter change.
  if iter_count % 1000 == 0 {
      tracing::debug!("pump_loop heartbeat iter={} ni={} ho={}", iter_count, ni_count, ho_count);
  }
  ```

  **Satisfies**: R7/S7.1 (1000 idle iterations → 0 counter-change traces + 1 heartbeat); R7/S7.2 (trace on change); DD8.

- [ ] **(test-validate-2.10)** On the HW host, run T-NEW-1 and T-NEW-2 (in isolation first):
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode \
    --run-ignored=all --no-capture --no-fail-fast \
    -E 'test(mft_stop_during_idle_returns_within_deadline) | test(mft_stop_during_active_encode_returns_within_deadline)'
  ```
  Expected: BOTH PASS within 2 s. Record GREEN transcript in apply-progress.
  If either FAILS or HANGS → RED — do NOT commit C2, diagnose first.
  **Satisfies**: R8/S8.2, R9/S9.2, R4/S4.1–S4.2.

- [ ] **(test-validate-2.11)** On the HW host, run ALL 16 existing `mft_*` smoke tests:
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode \
    --run-ignored=all --no-capture --no-fail-fast
  ```
  Expected: ≥14 PASS (per explore §6 forecast; all 13 stop-blocked + 3 already passing = 16/16 target). Record full transcript in apply-progress with PASS count. If <14 PASS, investigate regressions before committing C2.
  **Satisfies**: R10/S10.1–S10.2 (partial — DrainComplete arm not yet added; 18/18 comes after C3).

- [ ] **(gate-2.12)** Run `cargo nextest run --workspace` (no `--run-ignored`). Must be GREEN with count unchanged from anchor-0.1.
  **Satisfies**: NF6 (CI gate after impl).

- [ ] **(gate-2.13)** Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Must exit 0. Use `#[allow(..., reason = "...")]` (NOT `#[expect]`) for any conditional/cfg-gated consumer lint per discovery #580. Zero warnings.
  **Satisfies**: R13 (no new unsafe surface introduced); NF3.

- [ ] **(chore-2.14)** Commit C2: `feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling for vendor priming and stop deadline`
  - Commit body includes: RED→GREEN evidence for T-NEW-1 and T-NEW-2, regression count for existing tests (e.g., "16/16 PASS"), spec references (R1–R5, R7), gate status.
  **Satisfies**: Strict TDD GREEN commit per #186; work-unit-commit convention.

---

## Phase 3 — DrainComplete arm (C3)

Small, isolated addition. Adds the `METransformDrainComplete` arm that resets both counters and continues. This converts the WARN (from Phase 2) into an INFO and eliminates phantom counter state after drain sequences.

- [ ] **(impl-3.1)** In the `match event_type` block inside pump_loop, add the `METransformDrainComplete` arm BEFORE the catch-all `_` arm:

  ```rust
  t if t == METransformDrainComplete.0 as u32 => {
      tracing::info!(
          "pump_loop: METransformDrainComplete received — resetting counters (ni={} ho={})",
          ni_count, ho_count
      );
      ni_count = 0;
      ho_count = 0;
      // Do NOT break — top-of-loop stop check is the sole exit point (DD3, OQ-4 resolved).
  }
  ```

  **Satisfies**: R6/S6.1 (resets both counters); R6/S6.2 (does not exit loop); R6/S6.3 (catch-all WARN no longer fires for DrainComplete); DD3.

- [ ] **(test-validate-3.2)** Re-run all 18 smoke tests on HW host (16 existing + 2 new):
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode \
    --run-ignored=all --no-capture --no-fail-fast
  ```
  Expected: 18/18 PASS. Record full transcript in apply-progress.
  If `METransformDrainComplete` appears in `RUST_LOG=sm_infra::encode=trace` output → confirms R4/DD3 is exercised. If NOT seen → arm is insurance per design DR-R4; intact, behavior documented in apply-progress.
  **Satisfies**: R6/S6.4 (drain-then-stop completes within STOP_DEADLINE_MS), R10/S10.2 (18/18 PASS target).

- [ ] **(gate-3.3)** Run `cargo nextest run --workspace`. GREEN. Count unchanged.
  **Satisfies**: NF6 post-C3.

- [ ] **(gate-3.4)** Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`. Zero warnings.
  **Satisfies**: R13/S13.2.

- [ ] **(chore-3.5)** Commit C3: `feat(infra): handle METransformDrainComplete with counter reset to prevent post-drain phantom events`
  - Commit body: references R6, DD3, OQ-4 resolution; notes whether DrainComplete was observed in smoke trace; 18/18 PASS evidence.
  **Satisfies**: work-unit-commit convention; Strict TDD (behavior confirmed GREEN before commit).

---

## Phase 4 — Quality gates and smoke handoff (no commit)

All 5 quality gates must be GREEN under BOTH default and `hw-encoder` feature configurations before the PR is opened.

- [ ] **(gate-4.1)** `cargo check --workspace`
  Expected: exit 0.
  **Satisfies**: quality gate 1 per #186.

- [ ] **(gate-4.2)** `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  Expected: exit 0, zero warnings.
  Run as: `cargo clippy --workspace --all-targets --all-features -- -D warnings; echo "EXIT=$?"`
  **Satisfies**: quality gate 2 per #186; R13/S13.2.

- [ ] **(gate-4.3)** `cargo fmt --check --all; echo $?`
  Expected: exit 0. CRITICAL: do NOT pipe through grep or tail — exit code must come from fmt directly (discovery #581).
  **Satisfies**: quality gate 3 per #186.

- [ ] **(gate-4.4)** `cargo nextest run --workspace`
  Expected: all non-ignored tests PASS. Count must equal anchor-0.1 count (no regressions, no deletions).
  **Satisfies**: quality gate 4 per #186; R12/S12.2 (`cargo nextest run -p sm-domain` subset also passes here).

- [ ] **(gate-4.5)** `cargo deny check`
  Expected: exit 0 (no license or advisory violations introduced by this change; only import additions from existing `windows` crate).
  **Satisfies**: quality gate 5 per #186.

- [ ] **(gate-4.6)** Run cross-platform check (no HW feature):
  ```
  cargo check --workspace --no-default-features
  cargo clippy --workspace --all-targets -- -D warnings
  ```
  Expected: both exit 0. Validates R11 (default = [] unchanged) and R12/R13 (no domain or unsafe surface changes leak through the feature gate).
  **Satisfies**: R11/S11.2, NF4, NF6.

- [ ] **(verify-4.7)** Structural invariant checks (grep-level, not runtime):
  - Confirm `crates/sm-infra/Cargo.toml` still reads `default = []` — no `hw-encoder` in default array.
  - Confirm no `impl IMFAsyncCallback` in `windows_mft.rs` (grep: `IMFAsyncCallback`).
  - Count `unsafe {` occurrences in `windows_mft.rs`; must be ≤ PR #15 baseline (record baseline count from anchor, compare post-C3).
  - Confirm `EncoderConfig` in `crates/sm-domain/src/encode.rs` has no new fields added.
  **Satisfies**: R11/S11.1, R12/S12.1/S12.3, R13/S13.1/S13.2, NF3, NF4, NF5.

- [ ] **(smoke-handoff-4.8)** Document smoke invocation for user in apply-progress. The user MUST run:
  ```
  cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode \
    --run-ignored=all --no-capture --no-fail-fast
  ```
  Expected: 18 PASSED, 0 FAILED, 0 IGNORED.
  User saves the full stdout/stderr transcript to engram via:
  ```
  mem_save(
    title: "sdd/hw-encoder-mft-rework/smoke-transcript",
    topic_key: "sdd/hw-encoder-mft-rework/smoke-transcript",
    type: "discovery",
    content: "<invocation + host: OS/GPU/driver + ISO 8601 timestamp + full stdout/stderr>"
  )
  ```
  BLOCKED_ON_SMOKE: verify CANNOT issue APPROVED_FOR_ARCHIVE until this transcript is supplied and shows 18/18 PASS. If any test FAILS or HANGS, Decision #7 contingency activates (do NOT merge; escalate to `hw-encoder-mft-async-callback` change).
  **Satisfies**: R1–R4/R6/R8–R10 smoke gate; BLOCKED_ON_SMOKE rule (#186).

- [ ] **(chore-4.9)** Push branch and open PR:
  - Branch: `feat/hw-encoder-mft-rework` (branched from `f01f27f`)
  - PR title: `feat(infra): redesign MFT pump_loop with NO_WAIT polling and dual-arm counters`
  - PR body structure (per project convention — NO issue link, NO labels):

    ```
    ## Summary
    Rewrites `pump_loop` in `WindowsMftH264Encoder` to fix two Bucket A architectural bugs:
    1. Vendor MFT priming deadlock (HaveOutput-before-NeedInput on Intel/NVIDIA/AMD)
    2. Stop-signal starvation (GetEvent(FLAG_NONE) blocked indefinitely on stop())

    Pattern B + Fix B: NO_WAIT polling with dual-arm counters and drain-first ordering.

    ## Commits
    - C1: test(infra): add MFT stop-deadline smoke tests for idle and active paths (RED)
    - C2: feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling for vendor priming and stop deadline
    - C3: feat(infra): handle METransformDrainComplete with counter reset to prevent post-drain phantom events

    ## Gates
    - cargo check --workspace: PASS
    - cargo clippy --workspace --all-targets --all-features -- -D warnings: PASS
    - cargo fmt --check --all: PASS
    - cargo nextest run --workspace: PASS
    - cargo deny check: PASS

    ## Test plan
    Smoke (BLOCKED_ON_SMOKE until transcript supplied):
      cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all
    Expected: 18/18 PASS (16 existing + 2 new stop-deadline tests)

    ## SDD artifacts
    - Proposal: engram #595 / openspec/changes/hw-encoder-mft-rework/proposal.md
    - Spec: engram #596 / openspec/changes/hw-encoder-mft-rework/spec.md
    - Design: engram #597 / openspec/changes/hw-encoder-mft-rework/design.md
    - Tasks: engram sdd/hw-encoder-mft-rework/tasks / openspec/changes/hw-encoder-mft-rework/tasks.md
    ```

  **Satisfies**: branch-pr project convention; single-PR delivery per Decision #3.

---

## Requirements Traceability Matrix (RTM)

| Task(s) | Phase | Requirement | Design DD | Spec Scenarios | Smoke required |
|---------|-------|-------------|-----------|----------------|----------------|
| anchor-0.2 | 0 | DR-NEW-1 | — | S13.2 (unsafe baseline) | no |
| anchor-0.3 | 0 | R10 | — | S10.2 (BEFORE baseline) | yes |
| test-1.1 | 1 | R8 | DD7 | S8.1, S8.3, S4.1 | yes |
| test-1.2 | 1 | R9 | DD7 | S9.1, S9.3, S4.2 | yes |
| impl-2.1 | 2 | R1, R13 | DD1, DD6 | S1.1, S13.2 | no |
| impl-2.2 | 2 | R5 | DD6 | S5.1, S5.2 | no |
| impl-2.3 | 2 | — | DD9 | — (refactor) | no |
| impl-2.4 | 2 | R2, R7 | DD2, DD8 | S2.1–S2.5, S7.1, S7.2 | no |
| impl-2.5 | 2 | R1, R4 | DD1 | S1.1, S1.2, S4.1, S4.2 | yes |
| impl-2.6 | 2 | R2, R6 | DD1, DD2 | S2.1–S2.5, S6.3 | yes |
| impl-2.7 | 2 | R2, R3, R4 | DD1, DD2, DD4, DD5 | S2.2, S2.4, S3.1–S3.3, S4.1, S4.2 | yes |
| impl-2.8 | 2 | R1, R5 | DD6 | S1.2, S5.1 | yes |
| impl-2.9 | 2 | R7 | DD8 | S7.1, S7.2 | no |
| test-validate-2.10 | 2 | R8, R9, R4 | — | S8.2, S9.2, S4.1, S4.2 | yes |
| test-validate-2.11 | 2 | R10 | — | S10.1, S10.2 (partial) | yes |
| impl-3.1 | 3 | R6 | DD3 | S6.1, S6.2, S6.3, S6.4 | yes |
| test-validate-3.2 | 3 | R6, R10 | DD3 | S6.1–S6.4, S10.2 (full) | yes |
| gate-4.1–4.5 | 4 | R10 (NF6) | — | S10.3 (gates) | no |
| gate-4.6 | 4 | R11, R12, R13 | — | S11.2, S12.2, S13.2 | no |
| verify-4.7 | 4 | R11, R12, R13 | — | S11.1, S12.1, S12.3, S13.1, S13.2 | no |
| smoke-handoff-4.8 | 4 | R1–R4, R6, R8–R10 | DD7 | All smoke-required scenarios | yes (BLOCKED) |

---

## Review Workload Forecast

| Metric | Estimate | Basis |
|--------|----------|-------|
| `windows_mft.rs` changed lines | ~165 added, ~111 removed (net +54) | pump_loop 712–847 = 111 LOC current; new body ~165 LOC; helper extraction ~20 LOC moved; const + imports +8 |
| `windows_mft_encode.rs` changed lines | ~80 added, 0 removed | T-NEW-1 ~40 LOC, T-NEW-2 ~40 LOC |
| Total changed lines (gross) | **~245–265 LOC** | 165 added in src + 80 added in tests + ancillary removes |
| 400-line budget risk | **Low** | ~265 LOC << 400-line threshold |
| Chained PRs recommended | **No** | Single PR per proposal Decision #3; estimate confirms budget respected |
| Decision needed before apply | **No** | LOC estimate is consistent with Decision #3 override; no contradiction |

If the apply phase discovers the actual diff materially exceeds 265 LOC (e.g., due to `apply_pending_codec_settings` extraction cascading into call-site changes not anticipated here), the apply agent MUST surface this to the orchestrator BEFORE committing C2 and re-trigger the Review Workload Guard.

---

## Risks (carry-forward + task-level)

| Risk | Severity | Phase discovered | Mitigation |
|------|----------|------------------|------------|
| R1: NO_WAIT vendor compatibility unverified | HIGH | anchor-0.3 + smoke-handoff-4.8 | anchor-0.3 establishes BEFORE baseline; Decision #7 governs escalation if vendor returns wrong HRESULT |
| DR-NEW-1: Windows crate symbols missing | CRITICAL (STOP) | anchor-0.2 | Hard stop at Phase 0 — no Phase 1 code until confirmed |
| DR-NEW-2: E_UNEXPECTED string-prefix match brittle | MEDIUM | impl-2.7 | Inline contract comment; alternative typed-HRESULT refactor deferred per proposal §4 |
| R4: DrainComplete may never fire on vendor | LOW | test-validate-3.2 | DD3 is insurance — arm stays regardless; smoke transcript documents actual behavior |
| R2: BLOCKED_ON_SMOKE process risk | MEDIUM | smoke-handoff-4.8 | Explicit handoff task; verify blocks until transcript supplied |
| DR-NEW-4: debug_assert! for MF_E_NOTACCEPTING panics dev builds | LOW | impl-2.7 | nextest process isolation contains; panic message is diagnostic, not a crash |
| apply_pending_codec_settings extraction | LOW | impl-2.3 | Pure mechanical refactor; compile confirms correctness at gate-2.12 |

---

## Notes

- **Task ordering within Phase 2**: impl-2.1 through impl-2.9 are sequential (same file). The order listed maps to a safe incremental edit sequence: imports → const → helper → counters → GetEvent call → event dispatch → service loops → sleep → logging. Do NOT reorder.
- **`#[allow]` vs `#[expect]`**: any lint suppression added in Phase 2 that applies to conditionally-compiled or cfg-gated items MUST use `#[allow(..., reason = "...")]`, NOT `#[expect(...)]` (discovery #580).
- **`cargo fmt --check` on Windows**: always run as `cargo fmt --check --all; echo $?` — never pipe through grep or tail (discovery #581).
- **Parallel opportunities**: Phase 1 (test writing) and Phase 0 anchor tasks are independent of each other within Phase 0 (all Phase 0 tasks must complete before Phase 1 begins, but Phase 0 tasks 0.1 and 0.2 can be done in any order). Phase 2 impl tasks are sequential. Phase 3 impl-3.1 is independent from Phase 2 gates (but must follow chore-2.14).
- **Phase 6 (convention update)**: the BLOCKED_ON_SMOKE rule is already codified in #186 from the smoke-fixes change. No update to #186 is needed in this change; it is satisfied by self-reference.
- **Decision #7 contingency**: if smoke-handoff-4.8 reveals vendor incompatibility (test HANGS or wrong HRESULT from GetEvent NO_WAIT), do NOT merge the PR. Escalate by creating `hw-encoder-mft-async-callback` change. T-NEW-1/T-NEW-2 survive as diagnostic anchors on the branch.

---

## Result Contract

- **status**: done
- **executive_summary**: 24 ordered tasks across 4 phases (0=anchor, 1=RED tests, 2=pump_loop redesign, 3=DrainComplete arm, 4=gates+handoff); 3 logical commits (C1 RED, C2 GREEN partial, C3 GREEN full); all tasks sequential within phases; delivery is SINGLE PR per Decision #3; ~245–265 LOC total, well within 400-line budget.
- **artifacts**: engram `sdd/hw-encoder-mft-rework/tasks` + `openspec/changes/hw-encoder-mft-rework/tasks.md`
- **next_recommended**: `sdd-apply`
- **risks**: DR-NEW-1 (windows symbol availability — hard stop at Phase 0); R1 (NO_WAIT vendor compat — Decision #7 contingency); DR-NEW-2 (E_UNEXPECTED string-prefix match brittleness); R4 (DrainComplete may be insurance-only); BLOCKED_ON_SMOKE gate on smoke-handoff-4.8
- **skill_resolution**: injected
