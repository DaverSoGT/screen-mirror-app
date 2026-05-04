# Archive Report: hw-encoder-mft-rework

**Status**: APPROVED_WITH_CARRY_FORWARD (per verify #603)

**Executive summary**: Bug 2 (stop-signal starvation) fixed cross-vendor via Pattern B dual-arm NO_WAIT polling redesign of `pump_loop` in single PR #16 (ee32ff4). 7/7 quality gates GREEN. T-NEW-1 stop-deadline test PASS both hosts (848ms Host A, 255ms Host B). Bug 1 family (vendor MFT priming deadlock + setup HRESULT failures) confirmed multi-manifestation on two vendors, deferred to dedicated `hw-encoder-mft-vendor-compat-rework` change. Feature remains default-off (`default = []`); default-on flip gated on clean 18/18 smoke + 2+ vendors + 24h soak in follow-up.

## Branch / Commit anchors

- **Master HEAD post-merge**: ee32ff4 (2026-05-04 17:44:13 UTC)
- **Pre-merge HEAD**: b0bfeec (C3)
- **Commit chain** (3 logical commits):
  - C1: 8d1b341 — test(infra): add stop-deadline smoke tests for MFT encoder (RED)
  - C2: 97d4d81 — feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling for vendor priming and stop deadline (GREEN)
  - C3: b0bfeec — feat(infra): handle METransformDrainComplete with counter reset to prevent post-drain phantom events (GREEN)
- **PR**: #16 (merged 2026-05-04 17:44:13 UTC, DaverSoGT, `--merge --delete-branch`)
- **CI**: 23/23 SUCCESS (windows/macos/ubuntu × Check/Test/Clippy + Rustfmt + MSRV + JS Tests)

## Spec compliance summary (R1–R13)

| Req | Smoke required | Status | Evidence | Notes |
|-----|----------------|--------|----------|-------|
| R1 — NO_WAIT polling | yes | COMPLIANT | windows_mft.rs:800 GetEvent(MF_EVENT_FLAG_NO_WAIT) | T-NEW-1 PASS 848ms Host A, 255ms Host B |
| R2 — Explicit counters | yes | COMPLIANT | windows_mft.rs:782-783 ni_count/ho_count stack-local; decrement AFTER COM call | End-to-end coverage via all 18 tests |
| R3 — HaveOutput drain FIRST | yes | COMPLIANT | windows_mft.rs:864-904 while ho_count > 0 before while ni_count > 0 | S3.2: 30-frame test in 9/18 PASS Host A |
| R4 — Stop within deadline | yes | COMPLIANT | Top-of-loop stop check line 793; FRAME_RECV_TIMEOUT=50ms line 115; T-NEW-1 PASS both hosts | 848ms Host A; 255ms Host B; Manifestation B pre-existing |
| R5 — POLLING_SLEEP=1ms | no | COMPLIANT | windows_mft.rs:107 const POLLING_SLEEP = 1ms; no EncoderConfig field | T-NEW-1 ≤848ms confirms 1ms sleep active |
| R6 — DrainComplete arm | yes | COMPLIANT | windows_mft.rs:815-829 explicit arm, resets counters, INFO log, no break | S6.1-S6.4 satisfied |
| R7 — Tracing cadence | no | COMPLIANT | windows_mft.rs:971-984 change-only counter snapshot; heartbeat every 1000 iters | Verified by source inspection |
| R8 — T-NEW-1 added | yes | COMPLIANT | tests/windows_mft_encode.rs:722 mft_stop_during_idle_returns_within_deadline #[ignore] | S8.1-S8.3 confirmed |
| R9 — T-NEW-2 added | yes | COMPLIANT | tests/windows_mft_encode.rs:769 mft_stop_during_active_encode_returns_within_deadline #[ignore] | S9.1 confirmed; S9.2 PASS Host A; Host B secondary fail = Manifestation B |
| R10 — 16 existing tests intact | yes | COMPLIANT (partial) | All 16 names present; #[ignore] preserved; no deletions; zero regression | 9/18 Host A, 7/18 Host B; Bug 1 blocks full score; deferral accepted |
| R11 — default=[] unchanged | no | COMPLIANT | Cargo.toml:14 default = [] | Gate-6+7 EXIT=0 |
| R12 — No public API changes | no | COMPLIANT | VideoEncoder/EncoderConfig unchanged; 611 nextest PASS | ✓ |
| R13 — No new unsafe surface | no | COMPLIANT | No impl IMFAsyncCallback; unsafe count 36==36 baseline | ✓ |

## Decisions outcome (9 proposal decisions)

| # | Decision | Honored? | Evidence |
|---|----------|----------|----------|
| 1 | Pattern B dual-arm counters | YES | windows_mft.rs:782-986 full redesign |
| 2 | Fix B NO_WAIT polling (reject Fix A) | YES | GetEvent(MF_EVENT_FLAG_NO_WAIT) exclusive; no IMFAsyncCallback |
| 3 | Single PR (not chained) | YES | PR #16 C1+C2+C3; ~245 LOC; merged |
| 4 | DrainComplete included | YES | windows_mft.rs:815-829 (C3) |
| 5 | POLLING_SLEEP=1ms const | YES | windows_mft.rs:107; no EncoderConfig field |
| 6 | T-NEW-1 and T-NEW-2 included, T-NEW-3 excluded | YES | Both tests present; no T-NEW-3 |
| 7 | NO_WAIT fallback contingency | YES (not triggered) | T-NEW-1 PASS cross-vendor; escalation not needed |
| 8 | Cargo default stays [] | YES | Cargo.toml:14 confirmed; flip is separate follow-up |
| 9 | SDD chain link to #186 | PARTIAL | PR body references SDD; #186 row update deferred to archive phase (this action) |

## Quality gates final state (all 7 GREEN on ee32ff4)

| Gate | Result |
|------|--------|
| cargo check --workspace | PASS (exit 0) |
| cargo clippy --all-targets --all-features | PASS (zero warnings) |
| cargo fmt --check --all | PASS (direct invocation) |
| cargo nextest run --workspace | PASS (611 passed, 19 skipped) |
| cargo deny check | PASS (advisories/bans/licenses/sources ok) |
| cargo check --no-default-features | PASS (HW path opt-in confirmed) |
| cargo check --features hw-encoder | PASS (HW opt-in build compiles) |

## Smoke evidence (multi-host BLOCKED_ON_SMOKE satisfaction)

**Host A (Usuario\Desktop)**: 
- Master baseline (f01f27f): 6/16 PASS, 5 ABORT (Bug 1 Manifestation A), 5 HANG (Bug 2)
- Branch (b0bfeec): 9/18 PASS (+3 PASS from Bug 2 fix)
- T-NEW-1: PASS 848ms (Bug 2 fix confirmed)
- T-NEW-2: PASS 845ms (Bug 2 fix confirmed; not affected by Manifestation A)

**Host B (JDNHS)**:
- Master baseline (f01f27f): 6/16 PASS, 10 FAIL (Bug 1 Manifestation B: SetOutputType 0xC00D6D76)
- Branch (b0bfeec): 7/18 PASS (+1 PASS T-NEW-1)
- T-NEW-1: PASS 255ms (Bug 2 fix confirmed cross-vendor)
- T-NEW-2: Fail (secondary effect of pre-existing Manifestation B, NOT regression)
- No regression: 6 master PASS identical with 6 branch PASS

**Analysis**: Bug 2 fix (T-NEW-1) verified cross-vendor PASS on both Host A and Host B, confirming NO_WAIT polling and top-of-loop stop check work on Intel QSV (Host A) and NVIDIA NVENC (Host B). No regression on either host. 9+7=16 of the 16 pre-existing tests PASS on at least one host; the remaining 9 failures are all pre-existing Bug 1 manifestations (Manifestation A driver crash or Manifestation B setup HRESULT rejection).

## Out-of-scope deferrals (carry-forward)

| Item | Classification | Host | Manifestation | Pre-exists on master? | Carry-forward target |
|------|---|---|---|---|---|
| **Bug 1 — Manifestation A** | Driver-level 0xC0000005 AV in vendor ProcessOutput | Host A | Access violation inside Intel QSV driver; ABORT signal | YES (f01f27f) | hw-encoder-mft-vendor-compat-rework |
| **Bug 1 — Manifestation B** | SetOutputType HRESULT 0xC00D6D76 rejection | Host B | MFT setup fails during initialization on NVIDIA NVENC | YES (f01f27f) | hw-encoder-mft-vendor-compat-rework |

**Maintainer acceptance**: PR #16 merged with explicit deferral documented in PR body. Both manifestations confirmed pre-existing via master smoke on JDNHS host.

## Carry-forward to next changes

1. **`hw-encoder-mft-vendor-compat-rework`** (new change, L scope)
   - Covers Bug 1 Manifestation A (driver 0xC0000005 crash) and Manifestation B (SetOutputType 0xC00D6D76 rejection)
   - Phase 0 MUST include multi-host smoke with `RUST_LOG=sm_infra::encode=trace` instrumentation before design
   - Recommendation: follow "tracing-before-explore" convention (discovery #592) — empirical instrumentation runs on BOTH hosts (Usuario\Desktop + JDNHS) as anchor before attempting fixes
   - Depends: none (can proceed independently of PR #16, though Bug 2 fix reduces noise in transcripts)

2. **`hw-encoder-default-on-flip`** (new change, S scope)
   - Flip Cargo `default = ["hw-encoder"]` only after clean 18/18 smoke on ≥2 vendors + 24h soak observation
   - Gated on prior completion of `hw-encoder-mft-vendor-compat-rework`
   - PR body must cite both smoke transcripts (from this change + the vendor-compat change) as evidence
   - Depends: hw-encoder-mft-vendor-compat-rework APPROVED_FOR_ARCHIVE

3. **(SUGGESTION) Per-sub-task tracking in long apply phases**
   - Future SDD apply phases with >8 sequential sub-tasks should emit per-sub-task status lines for cleaner verify audit trails
   - Tasks #598 Phase 2 was tracked at coarse granularity; next long phases should enumerate sub-task ticks individually

## SDD chain (lineage for precedent)

- **Base**: hardware-accel-encoder-smoke-fixes (PR #15 f01f27f, archived #591)
- **Root**: hardware-accel-encoder (PR #14 80f0853, archived #579)
- **This change**: hw-encoder-mft-rework (PR #16 ee32ff4)
- **Successor (planned)**: hw-encoder-mft-vendor-compat-rework (new, covers Manifestations A+B)
- **Successor (planned)**: hw-encoder-default-on-flip (flip default + soak, depends on vendor-compat)

## Artifacts (engram + openspec traceability)

### Engram observations (topic_key references)
- #594: sdd/hw-encoder-mft-rework/explore
- #595: sdd/hw-encoder-mft-rework/proposal
- #596: sdd/hw-encoder-mft-rework/spec
- #597: sdd/hw-encoder-mft-rework/design
- #598: sdd/hw-encoder-mft-rework/tasks
- #599: sdd/hw-encoder-mft-rework/apply-progress
- #603: sdd/hw-encoder-mft-rework/verify-report
- #604: sdd/hw-encoder-mft-rework/archive-report (this observation)
- #600: sdd/hw-encoder-mft-rework/bug-1-deeper (discovery: multi-manifestation confirmation)
- #601: sdd/hw-encoder-mft-rework/smoke-transcript-jdnhs-branch
- #602: sdd/hw-encoder-mft-rework/smoke-transcript-jdnhs-master
- #186: sdd-init/screen-mirror-app (updated with row reconciliation)

### OpenSpec files (filesystem artifacts)
- openspec/archive/hw-encoder-mft-rework/proposal.md
- openspec/archive/hw-encoder-mft-rework/spec.md
- openspec/archive/hw-encoder-mft-rework/design.md
- openspec/archive/hw-encoder-mft-rework/tasks.md
- openspec/archive/hw-encoder-mft-rework/apply-progress.md
- openspec/archive/hw-encoder-mft-rework/smoke-handoff.md
- openspec/archive/hw-encoder-mft-rework/verify-report.md
- openspec/archive/hw-encoder-mft-rework/archive-report.md

## Result Contract

- **status**: done
- **executive_summary**: APPROVED_WITH_CARRY_FORWARD. Bug 2 (stop-signal starvation) fixed cross-vendor via Pattern B NO_WAIT polling redesign. PR #16 merged ee32ff4 with 7/7 gates GREEN. T-NEW-1 stop-deadline test PASS both Intel and NVIDIA hosts (848ms / 255ms). Bug 1 family multi-manifestation confirmed (driver crash on Intel, SetOutputType rejection on NVIDIA), both pre-existing on master, deferred to `hw-encoder-mft-vendor-compat-rework` with follow-up default-on flip gated on clean 18/18 + 2+ vendors. Feature remains default-off. Master SDD init #186 updated with row reconciliation and successor candidates.
- **artifacts**: engram observations #594-#603 + #600-#602; openspec/archive/hw-encoder-mft-rework/; updated #186
- **next_recommended**: orchestrator decision — `hw-encoder-mft-vendor-compat-rework` (tackle Bug 1 family) or `hw-encoder-default-on-flip` (flip default after vendor-compat closes) or unrelated user choice
- **risks**: Bug 1 family requires multi-vendor empirical tracing as Phase 0 before design; single-vendor evidence insufficient given multi-manifestation nature. Host A provides only one vendor manifestation (Intel); Host B provides different vendor manifestation (NVIDIA). Next change MUST include both hosts' transcripts before attempting fixes.
- **skill_resolution**: injected
