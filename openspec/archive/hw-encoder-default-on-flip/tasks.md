# Tasks: hw-encoder-default-on-flip

| Field | Value |
|-------|-------|
| **Change** | `hw-encoder-default-on-flip` |
| **Project** | screen-mirror-app |
| **Branch baseline** | `efcac92` (master post-Slice-6-R2) |
| **Date** | 2026-05-10 |
| **Strict TDD** | ACTIVE — `cargo nextest run --workspace` |
| **Artifact store** | hybrid (engram `sdd/hw-encoder-default-on-flip/tasks` + this file) |
| **Delivery strategy** | `auto-chain` (cached this session) |
| **Spec** | engram #822 |
| **Design** | engram #821 |

---

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~15 LOC across 4 files |
| 400-line budget risk | **Low** |
| Chained PRs recommended | **No** |
| Suggested split | Single PR |
| Delivery strategy | `auto-chain` |
| Chain strategy | N/A — single PR |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: size-exception
400-line budget risk: Low

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | Flip default + refresh comments + docs + version bump + soak | Single PR | 4 files, ~15 LOC, single atomic commit; soak pre-merge |

---

## Strict TDD Cadence (config-only slice)

This change flips a single Cargo.toml feature gate — no new executable code is written. The TDD cadence is adapted per DD9:

- **C0 (PROBES)**: N/A. Empirical evidence is already complete (#816, #809, #819). No probe code to commit. The "passive probes" are the 15 existing unit tests in `windows_mft.rs::tests` that silently migrate from "not compiled" to "compiled+run" on Windows CI after the flip.
- **C1 (FLIP COMMIT)**: `crates/sm-infra/Cargo.toml` flip + comment refresh in one atomic commit. Windows CI on this commit surfaces the 15 unit tests migrating from not-compiled → compiled+run. This is the RED→GREEN moment for the feature gate.
- **C2 (DOCS COMMIT)**: `README.md` + `CHANGELOG.md` + `src-tauri/Cargo.toml` version bump. **Recommendation: fold C1 + C2 into a single atomic commit** per DD6 (no follow-up release-tag slice; all four files in one squash-merge unit). If the apply agent finds a reason to split, document it.
- **C3 (POLISH)**: N/A unless `rustfmt` or `clippy` surfaces something (unlikely for TOML/MD edits).
- **C4 (SOAK GATE)**: 24h soak on Host A (Intel QSV) + Host B (NVENC) in parallel, pre-merge. Documented as engram `sdd/hw-encoder-default-on-flip/soak-report`. PRE-MERGE gate, not a commit.

Invariant: soak must complete before merge. No soak-skip path.

---

## Phase A — Code and Doc Edits (single atomic commit)

Sequential within this phase; TA.6–TA.10 can run in parallel after TA.1–TA.5 complete.

- [x] **TA.1** — Flip `default = []` to `default = ["hw-encoder"]` in `crates/sm-infra/Cargo.toml:14`.
  - Verify the line reads `default = []` before editing; if the line number has drifted, grep `^default` to locate it.
  - AC: `grep -n 'default = \["hw-encoder"\]' crates/sm-infra/Cargo.toml` exits 0.
  - Refs: R1, S1, DD1, AC-1. Depends on: none.

- [x] **TA.2** — Replace stale comment block at `crates/sm-infra/Cargo.toml:8–13` and OPT-IN doc-comment at lines 18–22.
  - New comment MUST cite Slice 6 R2 (#816), ForceKeyFrame VT_UI4 mechanism, automatic SW fallback, and `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` kill-switch.
  - Stale strings that MUST NOT remain: "known unresolved Bucket A bugs", "vendor-specific async event priming", "GetEvent stop-signal starvation", "deadlocks on real GPU hosts", "pump_loop redesign", "OPT-IN ONLY", "does not currently work end-to-end on hardware".
  - AC: `grep -c "Bucket A\|OPT-IN ONLY\|deadlocks on real GPU" crates/sm-infra/Cargo.toml` == 0; `grep -c "#816\|SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER" crates/sm-infra/Cargo.toml` >= 1.
  - Refs: R2, S2, S3, S4, DD1, AC-2, AC-3, AC-4. Depends on: TA.1.

- [x] **TA.3** — Update `crates/sm-infra/README.md` lines ~113–131 (HW encoder section).
  - Remove `--features hw-encoder` from normal build commands (redundant post-flip).
  - Re-frame env-var section from "useful for debugging" to "documented runtime rollback path".
  - Retain smoke-test commands, preconditions block, and factory fallback paragraph.
  - AC: `grep -n "\-\-features hw-encoder" crates/sm-infra/README.md` returns 0 matches for normal build invocations; env-var section still present.
  - Refs: R3, S5, DD2, AC-5. Depends on: none (parallel with TA.1/TA.2).

- [x] **TA.4** — Add `## [0.2.0] - 2026-05-10` section to `CHANGELOG.md` immediately above the existing `## [0.1.0]` entry.
  - Must include `### Changed` (default-on flip, WindowsMftH264Encoder, WindowsOpenH264Encoder fallback, env-var kill-switch), `### Documentation` (Cargo.toml + README refresh), and `### Compatibility` (HW host, no-HW SW fallback, macOS/Linux no-impact, env-var rollback) sub-headings.
  - Update link references at file bottom to include `[0.2.0]`.
  - AC: `grep "## \[0.2.0\] - 2026-05-10" CHANGELOG.md` exits 0; `grep "SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER" CHANGELOG.md` exits 0.
  - Refs: R4, S6, S7, DD3, AC-6, AC-7. Depends on: none (parallel with TA.1/TA.2/TA.3).

- [x] **TA.5** — Bump `version` field in `src-tauri/Cargo.toml` from `0.1.0` to `0.2.0`.
  - **LINE-NUMBER DRIFT RISK (DD4)**: Design confirms the `version` field is at **line 3**, NOT line 7. MUST run `grep -n "^version" src-tauri/Cargo.toml` BEFORE editing to confirm exact line. Edit only that line.
  - AC: `grep "^version = \"0.2.0\"" src-tauri/Cargo.toml` exits 0; no other fields in the file changed.
  - Refs: R5, S8, DD4, AC-8. Depends on: none (parallel with TA.1–TA.4).

- [x] **TA.6** — Run `cargo check --workspace` with default features post-flip.
  - AC: exit code 0; no compilation errors.
  - Refs: R10, S13. Depends on: TA.1 (flip must be applied first).

- [x] **TA.7** — Run `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
  - AC: exit code 0; zero warnings.
  - Refs: R9, S12, AC-12. Depends on: TA.1, TA.2.

- [x] **TA.8** — Run `cargo nextest run --workspace`.
  - AC: all tests GREEN; pre-existing `#[ignore]` integration tests remain skipped (8 HW tests in `tests/windows_mft_encode.rs`); 3 factory mock tests PASS; 15 `windows_mft.rs::tests` PASS.
  - Refs: R10, R11, S13, S14, AC-13, AC-14. Depends on: TA.1, TA.2.

- [x] **TA.9** — Verify all 15 unit tests in `windows_mft.rs::tests` appear as PASS in nextest output.
  - Grep the nextest run output for each of the 15 test names (inventoried in #819 §2.1). None should be skipped or absent.
  - AC: 15 distinct test lines with status `PASS`; zero with status `SKIP` or `IGNORE` among the 15.
  - Refs: R11, S14, AC-14. Depends on: TA.8.

- [x] **TA.10** — Verify zero source diff in frozen files vs baseline `efcac92`.
  - Run: `git diff efcac92 -- crates/sm-infra/src/encode/factory.rs crates/sm-infra/src/encode/windows_mft.rs crates/sm-domain/`
  - AC: output is empty (zero lines). Any non-empty output is a blocking defect.
  - Refs: R6, R7, R8, S9, S10, S11, AC-9, AC-10, AC-11. Depends on: none (can run anytime after checkout).

- [x] **TA.11** — Single commit with all four file edits (TA.1–TA.5).
  - Commit message: `chore(infra): enable hw-encoder by default on Windows (v0.2.0)`
  - Commit body: cite #816 (Slice 6 R2 evidence) and DD6 (atomic-bump policy — no follow-up release-tag slice).
  - NO `Co-Authored-By:`, NO AI attribution.
  - AC: `git log --oneline -1` shows the conventional commit subject; `git diff efcac92 --stat` shows exactly 4 files changed.
  - Refs: R15, S21, DD6, AC-21. Depends on: TA.1–TA.10 all passing.

---

## Phase B — Pre-merge CI Gate

TB.1 is sequential first; TB.2–TB.4 sequential after.

- [ ] **TB.1** — Push branch to origin.
  - AC: `git push origin HEAD` exits 0; remote tracks the commit.
  - Depends on: TA.11.

- [ ] **TB.2** — Open PR.
  - Title (max 70 chars): `chore(infra): enable hw-encoder by default on Windows (v0.2.0)`
  - Body must include: change summary, spec link (engram #822), design link (engram #821), DD4 line-drift resolved, DD6 atomic-bump rationale, soak plan (24h Host A + Host B, pre-merge), 4-file diff overview, rollback tiers (DD7).
  - Full PR (not draft).
  - AC: PR open on GitHub; title matches exactly.
  - Depends on: TB.1.

- [ ] **TB.3** — Verify CI all-green.
  - Expected: Check (windows/macos/ubuntu), Test (windows/macos/ubuntu), Clippy (windows/macos/ubuntu), Rustfmt, MSRV, JS Tests — 12 jobs total.
  - AC: 12/12 SUCCESS. Any failure is blocking.
  - Depends on: TB.2.

- [ ] **TB.4** — Confirm PR mergeability.
  - AC: `mergeable: MERGEABLE` and `mergeStateStatus: CLEAN` (via `gh pr view --json mergeable,mergeStateStatus`).
  - Depends on: TB.3.

---

## Phase C — Soak (24h, both hosts in parallel, pre-merge)

TC.2 and TC.3 run in parallel. TC.4 requires both to complete.

- [ ] **TC.1** — Produce soak handoff message with exact PowerShell commands per host.
  - Commands:
    ```powershell
    git checkout <branch-name>
    cargo build --release
    $env:RUST_LOG = "sm_infra::encode=info,sm_infra=warn"
    cargo run --release -- <args> 2>&1 | Tee-Object -FilePath host_A_soak.log
    ```
  - Instruct: run for 24h continuous (do not close the shell); exercise viewer reconnect at t=0 and t=24h.
  - Depends on: TB.3 (CI green).

- [ ] **TC.2** — USER soak run on Host A (Intel QSV) — 24h minimum.
  - Deliver: `host_A_soak.log` file.
  - Depends on: TC.1. PARALLEL with TC.3.

- [ ] **TC.3** — USER soak run on Host B (NVENC) — 24h minimum.
  - Deliver: `host_B_soak.log` file.
  - Depends on: TC.1. PARALLEL with TC.2.

- [ ] **TC.4** — Orchestrator analyzes soak logs (both hosts).
  - Run per log:
    ```powershell
    Select-String -Pattern "ERROR" host_A_soak.log | Measure-Object | Select Count
    Select-String -Pattern "falling back to software encoder" host_A_soak.log | Measure-Object | Select Count
    ```
  - AC per host: ERROR count == 0; SW-fallback count == 0; zero panics; viewer reconnect <= 1s at start AND end of soak (manual confirmation).
  - Depends on: TC.2, TC.3.

- [ ] **TC.5** — CONDITIONAL: if soak FAIL on either host — diagnose, fix, re-run TC.1–TC.4.
  - Document root cause; if fix requires source change, open a new micro-PR and re-run full soak.
  - Depends on: TC.4 (gated by failure).

- [ ] **TC.6** — Save soak-report to engram.
  - topic_key: `sdd/hw-encoder-default-on-flip/soak-report`; type: discovery.
  - Include: per-host duration, grep results (counts for ERROR + SW-fallback), viewer-reconnect timing, host hardware identification (GPU model, driver version), log file SHA-256.
  - Depends on: TC.4 PASS.

---

## Phase D — sdd-verify (formal)

- [ ] **TD.1** — Run `sdd-verify` against spec #822 + design #821.
  - AC: status `APPROVED` or `APPROVED_WITH_CARRY_FORWARD`; zero CRITICAL findings.
  - Depends on: TC.6 (soak evidence available).

- [ ] **TD.2** — Address any CRITICAL or WARNING findings from TD.1.
  - If CRITICAL: fix + re-run TD.1.
  - If WARNING only: document resolution or accepted carry-forward.
  - Depends on: TD.1.

---

## Phase E — Merge

- [ ] **TE.1** — Update PR body with soak evidence + verify-report engram ID.
  - Add section: soak results (per-host duration, ERROR=0, SW-fallback=0, reconnect timing), engram soak-report topic key, verify-report engram ID.
  - AC: PR body has "Soak" section citing both hosts; verify-report ID present.
  - Depends on: TC.6, TD.1.

- [ ] **TE.2** — Label check.
  - `type:chore` label does NOT exist in this repo (verified previously). PR ships without label.
  - AC: documented; no blocking action required.
  - Depends on: TB.2.

- [ ] **TE.3** — Confirm CI still green after PR body edit.
  - AC: 12/12 SUCCESS (body edits do not re-trigger CI, but confirm status is unchanged).
  - Depends on: TE.1.

- [ ] **TE.4** — Merge PR.
  - Command: `gh pr merge --merge --delete-branch`
  - AC: master HEAD updated to the merge commit; branch deleted on remote; `git log origin/master --oneline -1` shows the commit.
  - Depends on: TE.3, TD.2.

---

## Phase F — Archive and sdd-init v16

- [ ] **TF.1** — Move `openspec/changes/hw-encoder-default-on-flip/` to `openspec/archive/hw-encoder-default-on-flip/`.
  - AC: archive dir contains all 7 SDD artifacts (explore, proposal, spec, design, tasks, apply-progress, verify-report) plus new `archive-report.md`.
  - Depends on: TE.4.

- [ ] **TF.2** — Produce `archive-report.md`.
  - Include: outcome summary (v0.2.0 shipped, hw-encoder default-on), soak evidence summary (per host), follow-up anchor (DD8 → `hw-encoder-backend-disclosure-in-sender-diagnostics`, XS scope), factory.rs `\ SAFETY:` paste artifact note (out-of-scope XS cleanup candidate for a future micro-PR).
  - Depends on: TF.1.

- [ ] **TF.3** — Refresh `sdd-init/screen-mirror-app` engram to v16.
  - Update: v0.2.0 SHIPPED; hw-encoder default-on; follow-up candidates updated (DD8 promoted to "v2 / Next Direction Candidates"); soak precedent established (24h soak as operational gate pattern).
  - Depends on: TF.2.

- [ ] **TF.4** — Housekeeping commit on master.
  - Message: `chore(repo): archive hw-encoder-default-on-flip SDD artifacts`
  - Push to origin.
  - AC: `git log origin/master --oneline -1` shows the commit; `openspec/archive/hw-encoder-default-on-flip/` present in tree.
  - Depends on: TF.3.

---

## Task Summary

| Phase | Tasks | Focus | Sequential/Parallel |
|-------|-------|-------|---------------------|
| A | 11 (TA.1–TA.11) | Code + doc edits, local validation | TA.1–TA.5 parallel; TA.6–TA.10 parallel after flip; TA.11 last |
| B | 4 (TB.1–TB.4) | Push, PR open, CI gate | Sequential |
| C | 6 (TC.1–TC.6) | 24h soak (human-driven) | TC.2+TC.3 parallel; rest sequential |
| D | 2 (TD.1–TD.2) | Formal sdd-verify | Sequential |
| E | 4 (TE.1–TE.4) | PR update + merge | Sequential |
| F | 4 (TF.1–TF.4) | Archive + sdd-init refresh | Sequential |
| **Total** | **31** | | |

---

## Strict TDD Cadence Evidence

| Task | File Touched | Test/Probe | Status After Task |
|------|-------------|------------|-------------------|
| TA.1 | `crates/sm-infra/Cargo.toml` | Feature gate flip — 15 unit tests become compiled | RED→GREEN (compile gate) |
| TA.2 | `crates/sm-infra/Cargo.toml` | grep-based AC (stale text absent, new text present) | VERIFIED by grep |
| TA.3 | `crates/sm-infra/README.md` | grep-based AC (no `--features` in normal build) | VERIFIED by grep |
| TA.4 | `CHANGELOG.md` | grep-based AC ([0.2.0] section present) | VERIFIED by grep |
| TA.5 | `src-tauri/Cargo.toml` | grep-based AC (version = "0.2.0") | VERIFIED by grep |
| TA.6 | — | `cargo check --workspace` exit 0 | GREEN |
| TA.7 | — | `cargo clippy ... -D warnings` exit 0 | GREEN |
| TA.8 | — | `cargo nextest run --workspace` | GREEN (15+3 PASS, 8 SKIP) |
| TA.9 | — | 15 windows_mft unit test names in nextest output | VERIFIED |
| TA.10 | — | `git diff efcac92 -- factory.rs windows_mft.rs sm-domain/` | VERIFIED (zero diff) |
| TA.11 | — | Conventional commit; 4-file stat | COMMITTED |
| TB.3 | — | 12/12 CI jobs SUCCESS | CI GREEN |
| TC.4 | — | Soak logs: ERROR=0, SW-fallback=0 per host | SOAK PASS |
| TD.1 | — | sdd-verify: 0 CRITICAL | VERIFIED |

---

## Risks Carried Forward from Design

| Risk | Source | Mitigation in Tasks |
|------|--------|---------------------|
| Line-number drift in `src-tauri/Cargo.toml` | DD4 | TA.5 explicit verify-first (`grep -n "^version"` before edit) |
| Soak human error (incomplete run, closed terminal) | DD5 | TC.4 explicit grep counts; TC.6 engram persistence with SHA-256 |
| Follow-up anchor lost after archive | DD8 | TF.3 sdd-init v16 captures `hw-encoder-backend-disclosure-in-sender-diagnostics` under "Next Direction Candidates" |
| `factory.rs` `\ SAFETY:` paste artifact | sdd-init v15 | TF.2 archive-report flags as out-of-scope XS cleanup candidate |
| Partial revert risk (4-file atomic commit) | DD6 | TA.11 single-commit requirement; DD7 Tier 2 rollback documented |

---

## Notes

- **Atomicity (DD6)**: All four file edits ship in one squash-merge commit. The apply agent MUST NOT split TA.1–TA.5 across separate commits unless a blocking reason is documented.
- **Soak is human-driven**: TC.2 and TC.3 block on the user running the soak session. The orchestrator should not auto-chain past TC.1 without user confirmation that soak is underway.
- **No new tests**: This slice adds zero new test files. The 15 migrating unit tests are pre-existing. Reviewers must not flag "missing tests" per DD9.
- **Label absence**: `type:chore` does not exist in this repo. TE.2 is documented but non-blocking.
