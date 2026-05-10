# Tasks: hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Slice 6 R2)

> Change: `hw-encoder-mft-nvenc-mid-stream-idr-mechanism`
> Branch: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` @ `efc0f36` (off master `c48ae46`)
> Strict TDD: ACTIVE — test runner `cargo nextest run --workspace`
> Artifact store: hybrid (this file + engram topic_key `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/tasks`)
> Delivery strategy: `auto-chain` (session-cached). Single PR expected. `size:exception` NOT needed.
> Date: 2026-05-10

---

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~+50 add / ~−300 delete → net ~−250 LOC |
| 400-line budget risk | Low |
| Chained PRs recommended | No |
| Suggested split | Single PR |
| Delivery strategy | auto-chain (cached) — single-PR applicable |
| Chain strategy | N/A |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Low

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | Phase A+B+C cleanup + unit tests + probe inventory | PR 1 | All deletions + new tests fit budget; single PR |

---

## Strict TDD Cadence (LOCKED)

Phase 0 probes (P0, P0.b, P1, P2-NVENC, P2-Intel) ARE the empirical RED evidence. They are committed and `#[ignore]`-gated; their results are documented in engram #800/#801/#807/#808/#809.

GREEN gate: Phase A cleanup commit (Mechanism G/CleanPoint deletion + ForceKeyFrame as sole mechanism) + Phase B unit tests + Phase D cross-vendor smoke. The cleanup commit MUST compile clean + clippy clean BEFORE it lands. Cross-vendor smoke MUST pass on both Host A AND Host B with 0 NEW regressions.

This is non-negotiable per project Strict TDD policy. The probes are NOT optional regression evidence — they are the formal RED→GREEN cadence.

---

## Phase 0 — Empirical RED Evidence (COMPLETE on branch `efc0f36`)

> Phase 0 is DONE. ForceKeyFrame infrastructure is already on branch (Batch 2 `beda9ed`).
> Probes P0/P0.b/P1/P2-NVENC/P2-Intel are committed, `#[ignore]`-gated, and documented in engram #800/#801/#807/#808/#809.

- [x] T0.DONE — All Phase 0 probes committed; infrastructure present; P2 breakthrough confirmed (#809)

---

## Phase A — Cleanup Commit (Delete-Heavy)

> Goal: Replace Mechanism G + CleanPoint write path with vendor-uniform ForceKeyFrame as the sole IDR mechanism. Single atomic commit (or 2 if cascade lints require a split). All sub-tasks TA.1–TA.14 must succeed before commit TA.15 lands.

- [ ] **TA.1** — Delete Mechanism G handler block from `pump_loop` NeedInput service path
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~1670–1877 (~205 LOC, outside `Ok(frame)` arm)
  - **Deps**: none
  - **Refs**: DD3, R5, S6
  - **AC**: `grep -n "ActivateObject\|keyframe_recreate_pending\|setup_mft\|START_OF_STREAM"` in pump_loop returns 0 matches inside the former G handler block; file compiles

- [ ] **TA.2** — Delete CleanPoint swap block from `pump_loop` NeedInput path
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~1531–1537 (~7 LOC inside `Ok(frame)` arm)
  - **Deps**: none (independent deletion)
  - **Refs**: DD3, R6, S7
  - **LOC delta**: −7
  - **AC**: `grep -n "cleanpoint_pending"` inside `pump_loop` returns 0 matches; `submit_frame` call site has `force_cleanpoint` arg removed; file compiles

- [ ] **TA.3** — Delete `request_keyframe_via_recreate()` method (~30 LOC)
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~2199–2203
  - **Deps**: TA.1 (handler block deleted; no remaining callers)
  - **Refs**: DD4, R5, S6
  - **LOC delta**: −30
  - **AC**: `grep -rn "request_keyframe_via_recreate"` in `crates/sm-infra/src/` returns 0 matches; file compiles

- [ ] **TA.4** — Delete `request_keyframe_via_cleanpoint()` method (~20 LOC)
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~2228–2232
  - **Deps**: TA.2 (write path deleted; no remaining callers)
  - **Refs**: DD4, R6, S7
  - **LOC delta**: −20
  - **AC**: `grep -rn "request_keyframe_via_cleanpoint"` in `crates/sm-infra/src/` returns 0 matches; file compiles

- [ ] **TA.5** — Delete `keyframe_recreate_pending: AtomicBool` field and remaining readers
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` line ~186 (field declaration) + Default init
  - **Deps**: TA.1, TA.3 (all read/write sites removed)
  - **Refs**: DD2, R5, S6
  - **LOC delta**: −3
  - **AC**: `grep -rn "keyframe_recreate_pending"` in `crates/sm-infra/src/` returns 0 matches; `MftEncoderShared` struct compiles cleanly without the field

- [ ] **TA.6** — Delete `cleanpoint_pending: AtomicBool` field and remaining readers
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` line ~200 (field declaration) + Default init
  - **Deps**: TA.2, TA.4 (all read/write sites removed)
  - **Refs**: DD2, R6, S7
  - **LOC delta**: −3
  - **AC**: `grep -rn "cleanpoint_pending"` in `crates/sm-infra/src/` returns 0 matches; struct compiles

- [ ] **TA.7** — Delete `mft_activate_factory: Option<IMFActivate>` field; verify no remaining readers
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` line ~281 + `start()` take at ~363 + `Drop` at ~430
  - **Deps**: TA.1, TA.3 (only Mechanism G used this field)
  - **Refs**: DD2, R5, S6
  - **LOC delta**: −5
  - **AC**: `grep -rn "mft_activate_factory"` in `crates/sm-infra/src/` returns 0 matches; struct + Drop compile cleanly

- [ ] **TA.8** — Simplify `request_keyframe()` trait impl body to 1-line atomic store (no vendor dispatch)
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~396–405 (replace `match self.vendor { ... }` with `self.state.force_keyframe_icodecapi_pending.store(true, Release)`)
  - **Deps**: none (ForceKeyFrame path already present; dispatch arm is the only deletion)
  - **Refs**: DD1, DD5, R1, R9, S1, S10
  - **LOC delta**: −8 / +2
  - **AC**: `grep -n "match.*vendor"` in `request_keyframe()` returns 0 matches; `grep -n "EncoderVendor"` in IDR dispatch context returns 0; method compiles; `force_keyframe_icodecapi_pending.store(true, Release)` is present

- [ ] **TA.9** — Replace DD10 inline-comment block (actual location lines ~1236–1238 per DD6 discovery)
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` inside `fire_pending_codec_settings` doc-comment
  - **Deps**: none (comment edit; independent)
  - **Refs**: DD6, R8, S9
  - **LOC delta**: −3 / +10
  - **AC**: No text matching "Intel QSV does not honor" or "NVENC honored CleanPoint" exists in file; replacement comment cites #809 (P2 evidence: NVENC idx 0, Intel QSV idx 1), #808 (Chromium/FFmpeg/HCK), and names all three retracted overclaims (Slice 4 VT_BOOL/AFTER; Slice 5 G vendor-uniform; Slice 5 DD10 CleanPoint)

- [ ] **TA.10** — Delete Slice 5 round-3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`
  - **File**: `crates/sm-infra/tests/windows_mft_encode.rs` line ~2013 (~280 LOC probe body)
  - **Deps**: TA.1, TA.3 (probe calls `request_keyframe_via_recreate()` which will be gone)
  - **Refs**: DD8, R15, S17
  - **LOC delta**: −280
  - **AC**: `grep -n "phase0_intel_qsv_idr_via_imftransform_recreate"` in test file returns 0 matches; file compiles

- [ ] **TA.11** — Delete Slice 5 round-1 probes `phase0_intel_qsv_idr_via_drain_resume_first_frame_is_idr` and `phase0_intel_qsv_idr_via_drain_resume_latency_measure`
  - **File**: `crates/sm-infra/tests/windows_mft_encode.rs` lines ~1653 and ~1819
  - **Deps**: none (these probes compile cleanly; deletion is architectural housekeeping per DD8 discovery)
  - **Refs**: DD8, R15 (§RETAIN limit — not in retained list)
  - **LOC delta**: −~170
  - **AC**: `grep -n "phase0_intel_qsv_idr_via_drain_resume"` returns 0 matches; file compiles; the 5 retained Phase 0 probes (named in R14) are all still present

- [ ] **TA.12** — Decide and document DD13 `request_keyframe_via_force_keyframe_icodecapi()` visibility
  - **File**: `crates/sm-infra/src/encode/windows_mft.rs` lines ~2261–2265
  - **Deps**: none
  - **Refs**: DD13, R14
  - **Resolution**: RETAIN as `pub` named method (NOT inlined into trait body, NOT `pub(crate)`). Integration test probes in `tests/` crate require `pub` visibility. Drop "Phase 0 probe escape hatch" framing from doc-comment; document as named production mechanism identical to trait `request_keyframe()` but explicit about underlying ICodecAPI mechanism; add cross-ref to trait method + cite #809 + #808
  - **AC**: `request_keyframe_via_force_keyframe_icodecapi` is `pub`; doc-comment no longer says "escape hatch"; doc-comment mentions `CODECAPI_AVEncVideoForceKeyFrame`, #809, #808, and defers to trait method for callers; retained probes compile

- [ ] **TA.13** — Run `cargo check --tests --features hw-encoder -p sm-infra` — must compile clean
  - **Deps**: TA.1–TA.12 all applied
  - **Refs**: R16, S18
  - **AC**: Exit code 0; zero compiler errors

- [ ] **TA.14** — Run `cargo clippy --all-targets --all-features --locked -- -D warnings` — must be clean
  - **Deps**: TA.13 compiles clean
  - **Refs**: R16, S18
  - **AC**: Exit code 0; zero clippy warnings

- [ ] **TA.15** — Single cleanup commit (or 2 if cascade lints require split)
  - **Deps**: TA.1–TA.14 all green
  - **Refs**: DD12 (C2 GREEN commit)
  - **Commit message** (primary): `refactor(infra): replace Mechanism G + CleanPoint with vendor-uniform ForceKeyFrame ICodecAPI mid-stream IDR (Slice 6 R2)`
  - **Commit body**: cite #809 P2 evidence (NVENC idx 0, Intel QSV idx 1); name 3 retracted overclaims; net LOC delta; note cross-vendor gate pending
  - **AC**: `git log --oneline -1` shows conventional-commit title; `git diff HEAD~1 --stat` shows net LOC is approximately −250 with no unexpected files modified; `cargo nextest run --workspace` passes (CI-runnable tests only; hardware tests remain `#[ignore]`-gated)

---

## Phase B — Add CI-Runnable Unit Tests (Strict TDD R18)

> Goal: Add 3 atomic-flag unit tests that verify ForceKeyFrame mechanism semantics without hardware. CI-runnable.

- [ ] **TB.1** — Unit test: `force_keyframe_icodecapi_pending_defaults_to_false_on_construction`
  - **File**: `crates/sm-infra/tests/windows_mft_encode.rs` (or inline `#[cfg(test)]` module in `windows_mft.rs`)
  - **Deps**: TA.15 committed
  - **Refs**: R18, S19
  - **AC**: Test constructs `WindowsMftH264Encoder` (or accesses shared state directly via test constructor); `load(Acquire)` on `force_keyframe_icodecapi_pending` returns `false`; test passes in `cargo nextest run --workspace` without hardware

- [ ] **TB.2** — Unit test: `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true`
  - **File**: same as TB.1
  - **Deps**: TB.1 added (same file, sequential)
  - **Refs**: R18, R1, S20
  - **AC**: Test calls `encoder.request_keyframe()`; subsequent `load(Acquire)` on the flag returns `true`; test passes without hardware

- [ ] **TB.3** — Unit test: `force_keyframe_icodecapi_pending_swap_consumes_to_false`
  - **File**: same as TB.1
  - **Deps**: TB.2 added
  - **Refs**: R18, R2, S21
  - **AC**: Test arms flag via `request_keyframe()`; calls `force_keyframe_icodecapi_pending.swap(false, AcqRel)`; asserts return value is `true` (previous value) AND subsequent `load(Acquire)` returns `false` (consumed); test passes without hardware

- [ ] **TB.4** — Compile + clippy clean with new tests
  - **Deps**: TB.1–TB.3 added
  - **Refs**: R16, R17
  - **AC**: `cargo clippy --all-targets --all-features --locked -- -D warnings` exits 0; `cargo nextest run --workspace` shows all 3 new tests PASS

- [ ] **TB.5** — Commit new unit tests
  - **Deps**: TB.4 passes
  - **Commit message**: `test(infra): add CI-runnable unit tests for ForceKeyFrame mechanism (Slice 6 R2)`
  - **Refs**: R18
  - **AC**: Conventional-commit title; 3 new tests visible in `cargo nextest run --workspace` output; no hardware required to run

---

## Phase C — Phase 0 Probe Inventory Comment (DD10)

> Goal: Add top-of-file comment documenting all 5 retained Phase 0 probes with cross-references. Satisfies DD10 new requirement.

- [ ] **TC.1** — Add top-of-file inventory comment in `windows_mft_encode.rs`
  - **File**: `crates/sm-infra/tests/windows_mft_encode.rs` (top of file, before module-level `use` block)
  - **Deps**: TA.10, TA.11 (deleted probes are absent; correct retained list is known)
  - **Refs**: DD10, R14, S17
  - **Content required**: List all 5 retained probes by name, with:
    - Engram observation cross-reference (#800, #801, #807, #809, #809)
    - Classification: "falsification evidence" (probes 1–3) vs "success evidence" (probes 4–5)
    - Reference to deleted probes preserved in Slice 5 archive (#791) + engram #779/#780/#783
  - **AC**: Comment present at top of file; lists exactly 5 probes; no reference to deleted probes as present; comment is `//` style (not doc-comment)

- [ ] **TC.2** — Compile clean after comment
  - **Deps**: TC.1
  - **Refs**: R16
  - **AC**: `cargo check --tests --features hw-encoder -p sm-infra` exits 0

- [ ] **TC.3** — Commit probe inventory comment
  - **Deps**: TC.2 passes
  - **Commit message**: `docs(test): document retained Phase 0 probes for Slice 6 R2 (regression evidence)`
  - **Refs**: DD10
  - **AC**: Conventional-commit title; diff touches only `tests/windows_mft_encode.rs`

---

## Phase D — Cross-Vendor Smoke Pre-Merge Gate (USER INTERACTION REQUIRED)

> This phase is a USER INTERACTION GATE. The user runs the smoke on both hosts and returns trace logs. The orchestrator analyzes. DO NOT open a PR until TD.5 PASSES.

- [ ] **TD.1** — Push branch to origin
  - **Deps**: TC.3 committed
  - **AC**: `git push origin feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` exits 0; remote branch updated to include TA.15 + TB.5 + TC.3

- [ ] **TD.2** — Produce handoff message for user (Host A + Host B smoke commands)
  - **Deps**: TD.1
  - **Refs**: DD14, R11, R12, R13
  - **AC**: Orchestrator produces two PowerShell command lines:
    - **Host A (Intel QSV)**: `cargo nextest run --workspace --features sm-infra/hw-encoder` (full suite, no test filter, runs everything except `#[ignore]`-gated)
    - **Host B (NVENC)**: same command
    - Handoff message also lists the 5 retained Phase 0 probes command: `cargo nextest run -E 'test(phase0_) and not test(_drain_resume_)' --run-ignored=ignored-only --features sm-infra/hw-encoder`

- [ ] **TD.3** — USER: Run Host A (Intel QSV) smoke; return trace log
  - **Deps**: TD.2 handoff delivered
  - **AC (user action)**: User runs the Host A command; pastes or attaches nextest output

- [ ] **TD.4** — USER: Run Host B (NVENC) smoke; return trace log
  - **Deps**: TD.2 handoff delivered (parallel with TD.3)
  - **AC (user action)**: User runs the Host B command; pastes or attaches nextest output

- [ ] **TD.5** — Orchestrator analyzes both traces
  - **Deps**: TD.3 + TD.4 results received
  - **Refs**: DD14, R11, R12, R13, R17, S4, S12, S13, S14, S15, S16, S18
  - **Pass criterion**:
    - T7.1 (`mft_request_keyframe_marks_next_packet_as_keyframe`) PASSES on BOTH Host A AND Host B
    - T7.2 (`mft_keyframe_flag_cleared_after_idr_emitted`) PASSES on BOTH Host A AND Host B
    - T8.2 (`mft_set_bitrate_updates_encoder_without_restart`) PASSES on BOTH hosts
    - 0 NEW regressions vs pre-existing baselines (Slice 4/5 carry-forward flakes EXCLUDED per #789 + #790)
  - **AC**: Orchestrator reports PASS or FAIL with specific test names for any failures

- [ ] **TD.6** — CONDITIONAL: If smoke FAILS — diagnose, fix, re-run
  - **Deps**: TD.5 returns FAIL
  - **Refs**: DD14
  - **AC**: Root cause identified (regression from cleanup, or pre-existing flake); fix committed with message `fix(infra): <description> (Slice 6 R2 smoke fix)`; TD.3–TD.5 repeated until PASS; loop exits only on full PASS

---

## Phase E — SDD Verify (Formal)

> Run sdd-verify against spec #811 + design #812. Address any CRITICAL or WARNING findings before opening PR.

- [ ] **TE.1** — Run `sdd-verify` against spec engram #811 and design engram #812
  - **Deps**: TD.5 PASSES (smoke GREEN on both hosts)
  - **Refs**: AC-1 through AC-20 in spec §11
  - **AC**: `sdd-verify` phase produces a verify-report with status APPROVED or APPROVED_WITH_CARRY_FORWARD; no CRITICAL findings

- [ ] **TE.2** — Address any CRITICAL or WARNING findings from TE.1
  - **Deps**: TE.1 report received
  - **AC**: All CRITICAL findings fixed; all WARNINGs either fixed or recorded as explicit carry-forward with justification; verify re-run confirms 0 CRITICAL

---

## Phase F — PR + Merge

> Open PR after TE.1 APPROVED. Merge only after CI green.

- [ ] **TF.1** — Open PR with conventional-commit title
  - **Deps**: TE.2 resolved
  - **Refs**: branch-pr standards, DD14
  - **PR title**: `refactor(infra): replace Mechanism G with vendor-uniform ForceKeyFrame for mid-stream IDR (Slice 6 R2)`
  - **AC**: PR open on GitHub targeting `master`; title matches conventional-commit pattern `^refactor\(infra\): `

- [ ] **TF.2** — PR body complete
  - **Deps**: TF.1
  - **Required sections**:
    - Summary (1–3 bullets: what changed, why, net LOC delta)
    - Cross-vendor smoke evidence: Host A trace summary (T7.1/T7.2/T8.2 PASS) + Host B trace summary
    - Phase 0 evidence cross-reference: engram #809 (P2 breakthrough), engram #801 (G falsified on NVENC), engram #807 (CleanPoint falsified)
    - Slice 5 retroactive corrections: 3 corrected overclaims (Slice 4 VT_BOOL/AFTER; Slice 5 G vendor-uniform; Slice 5 DD10 CleanPoint)
    - Test plan checklist (AC-1 through AC-20 from spec §11, checked off)
  - **AC**: All 5 required sections present; test plan checklist itemizes AC-1 through AC-20

- [ ] **TF.3** — PR validation gates satisfied
  - **Deps**: TF.2
  - **AC**: Type label `type:refactor` applied; CI workflow 100% green (all jobs pass); no merge conflicts with master

- [ ] **TF.4** — Merge PR
  - **Deps**: TF.3 gates satisfied
  - **AC**: Merged to `master` using standard merge style (verify by checking `git log --merges --oneline -5 master` — project uses merge commits, NOT squash); `master` HEAD updated; branch can be deleted post-merge

---

## Phase G — Archive + sdd-init v15

> Post-merge housekeeping. All actions on `master` after TF.4.

- [ ] **TG.1** — Move openspec directory to archive
  - **Deps**: TF.4 merged
  - **Action**: `mv openspec/changes/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/ openspec/archive/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/`
  - **AC**: `openspec/archive/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/` exists with all 6 files (explore.md, proposal.md, spec.md, design.md, tasks.md, apply-progress.md + archive-report.md to be added)

- [ ] **TG.2** — Produce Slice 6 R2 archive-report including retroactive corrections section
  - **Deps**: TG.1 complete
  - **Required section**: `## Retroactive Corrections to Slice 5 (and 4)` naming all 3 overclaims:
    - Slice 4: "Intel QSV does not honor ForceKeyFrame mid-stream" — wrong timing (AFTER) + wrong variant (VT_BOOL); #809 corrects
    - Slice 5: "Mechanism G is vendor-uniform" — #801 falsified (29/29 P-frames on NVENC)
    - Slice 5 DD10: "NVENC honored CleanPoint" — #807 falsified (30/30 P-frames)
  - **Refs**: DD11, spec §9
  - **AC**: `archive-report.md` present in `openspec/archive/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/`; contains `## Retroactive Corrections` section; engram `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/archive-report` saved

- [ ] **TG.3** — Refresh `sdd-init/screen-mirror-app` to v15
  - **Deps**: TG.2 complete
  - **Required updates to v14 (#186)**:
    - Roadmap: Slice 6 R2 archived; `hw-encoder-mft-nvenc-mid-stream-idr-mechanism` added to archived changes table
    - Discoveries: 3 retroactive corrections from TG.2 (named above)
    - Mid-stream IDR convention updated: `ForceKeyFrame BEFORE+VT_UI4` is the canonical mechanism; Mechanism G obsolete
    - HW encoder Bug 1 fully CLOSED (NVENC T7.1/T7.2 now GREEN post Slice 6 R2)
    - Next direction: `hw-encoder-default-on-flip` ready (no longer gated by NVENC IDR closure)
    - SDD chain links for Slice 6 R2 added
  - **AC**: Engram topic_key `sdd-init/screen-mirror-app` updated (upsert) to v15; Slice 6 R2 change appears in archived changes table; next candidate `hw-encoder-default-on-flip` marked unblocked

- [ ] **TG.4** — Housekeeping commit on `master`
  - **Deps**: TG.1–TG.3 complete
  - **Action**: Stage openspec archive move + archive-report; commit to master directly (per convention #732)
  - **Commit message**: `chore(repo): archive Slice 6 R2 SDD artifacts`
  - **AC**: Conventional-commit title; diff shows only `openspec/` path changes; `git push origin master` exits 0

---

## Parallel Execution Map

```
Phase A (TA.1–TA.15)
  TA.1 ─┐
  TA.2 ─┼──► TA.5 (after TA.1+TA.3)
  TA.3 ─┤      TA.6 (after TA.2+TA.4)
  TA.4 ─┘      TA.7 (after TA.1+TA.3)
  TA.8 (independent)
  TA.9 (independent)
  TA.10 (after TA.1+TA.3)
  TA.11 (independent — compile-only dependency)
  TA.12 (independent)
  └──► TA.13 → TA.14 → TA.15

Phase B (TB.1–TB.5) — sequential (same file)
  TA.15 → TB.1 → TB.2 → TB.3 → TB.4 → TB.5

Phase C (TC.1–TC.3) — sequential, can overlap with Phase B
  TA.10+TA.11 done → TC.1 → TC.2 → TC.3

Phase D (TD.1–TD.6) — USER INTERACTION GATE
  TB.5 + TC.3 → TD.1 → TD.2 → TD.3 ∥ TD.4 → TD.5 → [TD.6 if needed]

Phase E (TE.1–TE.2) — sequential after TD.5 PASS

Phase F (TF.1–TF.4) — sequential after TE.2

Phase G (TG.1–TG.4) — sequential after TF.4
  TG.1 → TG.2 ∥ TG.3 → TG.4
```

---

## Task Summary

| Phase | Tasks | Focus |
|-------|-------|-------|
| Phase 0 | DONE | Empirical RED evidence (probes on branch) |
| Phase A | 15 | Cleanup commit: delete Mechanism G + CleanPoint + dead fields + old probes |
| Phase B | 5 | Add 3 CI-runnable unit tests for ForceKeyFrame flag semantics |
| Phase C | 3 | Phase 0 probe inventory comment (top of test file) |
| Phase D | 6 | Cross-vendor smoke pre-merge gate (USER INTERACTION) |
| Phase E | 2 | SDD verify (formal) |
| Phase F | 4 | PR + merge |
| Phase G | 4 | Archive + sdd-init v15 |
| **Total** | **39** | **8 phases** |

---

## Spec → Task Mapping

| Req | Task(s) |
|-----|---------|
| R1 | TA.8, TB.2 |
| R2 | TA.2, TB.3 |
| R3 | TA.2 (ForceKeyFrame block retained with VT_UI4; verified by grep) |
| R4 | TA.2 (non-fatal HRESULT: retained from existing implementation; verified by code inspection) |
| R5 | TA.1, TA.3, TA.5, TA.7 |
| R6 | TA.2, TA.4, TA.6 |
| R7 | TA.2 (READ path in collect_output explicitly NOT touched) |
| R8 | TA.9 |
| R9 | TA.8 |
| R10 | TA.12 (doc-comment latency contract) |
| R11 | TD.3, TD.5 |
| R12 | TD.4, TD.5 |
| R13 | TD.5 |
| R14 | TA.10, TA.11, TC.1 |
| R15 | TA.10, TA.11 |
| R16 | TA.13, TA.14, TB.4 |
| R17 | TD.5 |
| R18 | TB.1, TB.2, TB.3 |

---

## Risks

1. **TA.7 `mft_activate_factory` blast radius** — design DD2 notes two reshape options; tasks resolve to "delete field entirely" since TA.1+TA.3 eliminate all readers. If a reader was missed by Grep, TA.13 compile will catch it. Mitigation: run `grep -rn "mft_activate_factory\|winning_activate"` before committing TA.7.

2. **TD.3/TD.4 user availability** — Phase D is a USER INTERACTION GATE. Apply cannot auto-proceed past TD.2. If the user cannot run smoke on one host, the PR MUST NOT open. Mitigation: orchestrator documents this constraint explicitly at TD.2 handoff.

3. **TA.11 round-1 probe deletion breadth** — DD8 discovery identifies 2 additional probes deletable beyond spec R15 (which only lists the round-3 probe). Confirm by Grep that these probes reference no symbols outside the deleted Mechanism G/CleanPoint scope before deleting. Mitigation: TA.13 compile catches any missed dependency.
