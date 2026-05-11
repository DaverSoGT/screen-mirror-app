# Proposal: hw-encoder-default-on-flip

> **Change**: `hw-encoder-default-on-flip`
> **Project**: screen-mirror-app
> **Date**: 2026-05-10
> **Branch baseline**: `master` @ `efcac92`
> **Artifact store**: hybrid (engram `sdd/hw-encoder-default-on-flip/proposal` + this file)
> **Strict TDD**: ACTIVE (`cargo nextest run --workspace`)
> **Predecessor**: sdd-init v15 (engram #186); exploration #819
> **Slice size**: S (small), UNBLOCKED
> **Delivery strategy**: `auto-chain` → single PR (estimated diff ≤ 15 LOC across 4 files)

---

## 1. Intent

Make the Windows hardware H.264 encoder (`WindowsMftH264Encoder`) the **default-on** code path for `sm-infra` and for the shipped `screen-mirror` Tauri binary, now that the cross-vendor mid-stream IDR mechanism (Bug 1) is closed and validated end-to-end on both Intel QSV (Host A, 27/27 PASS) and NVIDIA NVENC (Host B, 27/27 PASS). The user-visible effect on supported Windows hosts is GPU-accelerated encoding (lower sender CPU, comparable or better bitrate quality, transparent IDR recovery on viewer reconnect). On Windows hosts without a compatible MFT (no GPU, virtualised host, broken driver) the factory's existing automatic `InitFailed → OpenH264` fallback preserves identical UX to v0.1.0. macOS and Linux are unaffected at every level (compile, link, runtime) because `windows_mft.rs` and the `windows` crate are platform-gated.

---

## 2. Scope

### In scope

| Path | Change | LOC |
|------|--------|-----|
| `crates/sm-infra/Cargo.toml:14` | Flip `default = []` → `default = ["hw-encoder"]` | 1 |
| `crates/sm-infra/Cargo.toml:8-13` | Refresh comment block (remove "Bucket A bugs / deadlocks" stale text; replace with "Bug 1 CLOSED vendor-uniformly via ForceKeyFrame ICodecAPI; default-on as of v0.2.0; kill-switch `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1`") | ~6 |
| `crates/sm-infra/Cargo.toml:18-22` | Refresh `hw-encoder` feature doc-comment (remove "OPT-IN ONLY — does not work end-to-end / pump_loop redesign required") | ~4 |
| `crates/sm-infra/README.md:113-147` | Refresh "Hardware encoder smoke tests" section: HW encoder is now default-on, `--features hw-encoder` is no longer required for normal builds; cross-vendor validation (Intel QSV + NVENC) called out; env var kill-switch promoted to first-class rollback mechanism | ~10 |
| `CHANGELOG.md` `[Unreleased]` | Add `### Changed` entry: HW encoder default-on (Bug 1 closure recap, link to archived slice #816), env-var kill-switch documented, no breaking surface change; mark target version `v0.2.0` | ~12 |

### Out of scope (explicit non-changes)

| Path | Reason |
|------|--------|
| `crates/sm-infra/src/encode/factory.rs` | `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER` env var already wired (`factory.rs:95`); automatic `InitFailed → SW` fallback already in place (`factory.rs:101-104`); 3 unit tests use injected constructors and are feature-agnostic |
| `crates/sm-infra/src/encode/windows_mft.rs` | Encoder logic unchanged; only the feature-gate default is flipped |
| `src-tauri/Cargo.toml` | Inherits `sm-infra` defaults via `sm-infra = { workspace = true }` — no override needed |
| `src-tauri/src/commands/sender.rs` (`SenderStats`, `sender_diagnostics`) | See **D3** — UI disclosure of HW vs SW path deferred (requires non-trivial wiring: `Box<dyn VideoEncoder>` erases backend identity) |
| `.github/workflows/ci.yml` | `clippy --all-features` already exercises hw-encoder on Windows CI; `check` and `test` gain compilation of `windows_mft.rs` automatically — no workflow edit needed |
| Workspace `Cargo.toml` | No `sm-infra` features override exists; nothing to change |
| `crates/sm-domain/*` | Domain ports unchanged — `VideoEncoder` trait stays opaque |

---

## 3. Locked decisions

### D1 — Version bump target: **v0.2.0 (minor)**

**What**: Bump the project from `0.1.0` to `0.2.0` in `src-tauri/Cargo.toml:7` (the only versioned package; `sm-infra` and `sm-domain` inherit `edition.workspace`/`rust-version.workspace` but do not currently publish versions). The CHANGELOG entry moves from `[Unreleased]` to a new `## [0.2.0] - 2026-05-1X` section as part of the release commit (not this proposal's PR — release tagging stays a separate slice if desired).

**Why**:
1. **Semantic significance**: This is the FIRST time the shipped Windows binary runs the HW encoder by default. Even though the SW fallback masks failures, the runtime behaviour shift (GPU activation, MFT enumeration at session start, different latency profile per vendor — NVENC ~0ms IDR delay, Intel QSV ~33ms per #809) is meaningful enough that downstream consumers integrating `sm-infra` as a library would observe a different code path activating.
2. **Project semver policy**: `CHANGELOG.md:9-11` states verbatim "Minor bumps (`0.1.0` → `0.2.0`) MAY contain breaking changes. Patch bumps (`0.1.0` → `0.1.1`) are **bug-fix only**." A default-feature flip is by definition NOT a bug-fix. Patch is therefore policy-incorrect.
3. **Cargo features semver convention**: enabling a new default feature is treated as a non-breaking minor change by RFC 1105 / `cargo-semver-checks` heuristics (adding to `default` is `cargo semver = minor`). Removing it later WOULD be breaking, so we lock the minor bump now.
4. **Library consumer signal**: anyone pinning `sm-infra` via `path` or future crates.io publication will see the version change and read the CHANGELOG `### Changed` entry — patch bumps are typically ignored for behaviour review.

**Alternatives rejected**:

| Option | Reason rejected |
|--------|-----------------|
| `v0.1.1` (patch) | Violates project semver policy (`CHANGELOG.md:11` says patch = bug-fix only). Default-feature additions are conventionally minor. |
| `v1.0.0` (major) | Premature — `CHANGELOG.md:8` says "API and behaviour are subject to change without notice until v1.0.0". This is still pre-release. |
| Defer the bump | The flip + bump should land atomically; flipping defaults without a version signal is the worst of both worlds. |

---

### D2 — Soak acceptance criteria: **24h per host, both hosts in parallel, before PR merge**

**What**: After the PR is opened (CI green, ready-for-review state), a 24h continuous soak runs on **each** of:

- **Host A — Intel QSV** (Intel UHD Graphics, current Slice 6 R2 validation host)
- **Host B — NVIDIA NVENC** (current Slice 6 R2 validation host)

Both 24h windows run **in parallel** (calendar time = 24h, not 48h), since the hosts are independent physical machines.

**Pass criteria (ALL must hold for both hosts)**:

1. **Zero `tracing::error!`** from `sm_infra::encode::*` (`windows_mft`, `factory`, or `windows`) over the 24h window. `tracing::warn!` is acceptable and expected from `SetValue` rejection paths (per DD13, non-fatal).
2. **Zero panics** in the sender process.
3. **Zero unexpected SW fallbacks** on Hosts A and B: the one-time `tracing::info!` "falling back to software encoder" line MUST NOT appear (since both hosts have working HW). If it DOES appear, soak FAILS on that host.
4. **Zero encoder crashes / instance-recreate cycles** (HRESULT errors propagated from `windows_mft::pump_loop`).
5. **Manual smoke check at the start AND at the end** of each 24h window: open a sender session, reconnect a viewer mid-session, confirm IDR recovery (Annex-B sample starts with `00 00 00 01 65` / type 5 NAL within 2 frames of reconnect signal).

**Failure criteria (ANY triggers rollback)**:

- Any item above fails.
- Sender process exits non-zero unexpectedly.
- Sustained CPU usage on the sender exceeds the SW-encoder baseline (HW path should be ≤ SW path on CPU; if HW is consistently HIGHER, that is a regression).

**Evidence location**:

- **Primary**: PR body includes a "Soak evidence" section linking to (a) the soak start commit SHA on the feature branch, (b) host identifiers (GPU vendor / driver version), (c) start and end timestamps, (d) sentinel `tracing::error!` and panic counts (from a `grep -c` of the log file or equivalent), (e) screenshots / video of viewer-reconnect IDR recovery at start and end.
- **Secondary**: Engram observation `sdd/hw-encoder-default-on-flip/soak-report` (saved as `type: discovery`) summarising the same.
- **Raw logs**: archived locally (not committed) at `target/soak/<host>/<start_ts>.log`, referenced by SHA-256 in the PR body for tamper-evidence.

**Soak timing relative to merge**:

- Soak STARTS after the PR opens and the orchestrator-driven `sdd-apply` lands the flip on the feature branch (so CI is exercising the new defaults during the same window).
- Soak COMPLETES before merge to `master`. Merging without the 24h evidence is forbidden.
- If soak fails on either host, the PR moves back to draft, the flip is reverted on the branch (NOT on `master` — `master` never saw a broken default), and the failure mode is captured as a new exploration topic.

**Rollback mechanism if soak fails on master after merge (defence in depth)**:

- Users / testers set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to force SW path WITHOUT a rebuild (`factory.rs:95`, documented in `README.md:133-142`). This is the runtime kill-switch and is mentioned in the CHANGELOG entry.
- Hot-revert via a follow-up patch PR that flips `default = []` back. Single line. CI green within ~10 min.

**Why**:
- "24h per host" matches sdd-init v15's recommendation (#186) and is the de-facto industry minimum for encoder soak (Chromium WebRTC soak window is also ~24h for codec changes).
- Parallel scheduling (not 48h serial) keeps slice timeline tight without sacrificing evidence.
- The "zero unexpected SW fallback" criterion is the SINGLE strongest signal that HW init reliably succeeds on real hardware — it falsifies driver-variance regressions that would otherwise hide behind silent fallback.
- Evidence in PR body + engram observation gives both team-shareable artefact (PR) and cross-session recovery (engram).

**Alternatives rejected**:

| Option | Reason rejected |
|--------|-----------------|
| 24h TOTAL (one host) | Halves vendor coverage; Slice 6 R2 corrigendum #816 explicitly warns "Phase 0 empirical must run on ALL target vendors pre-merge" — single-host soaks are exactly the antipattern that produced 3 overclaims in Slices 4/5. |
| 4-hour smoke per host | Too short for thermal / driver-state regressions to surface. NVENC and Intel QSV both have known multi-hour failure modes (driver memory growth) that 4h cannot detect. |
| Soak AFTER merge | Risks landing a broken default on `master` — even with env-var rollback, contributors pulling `master` for unrelated work would have to discover the env var. Pre-merge soak is the safer default for a v0.x project without staged rollout infrastructure. |
| Defer soak to user community | No telemetry infrastructure exists in v0.1.0 — silent regressions would never reach maintainer awareness. |

---

### D3 — Tauri UI disclosure (HW vs SW): **OUT of scope for this slice**

**What**: `SenderStats` / `sender_diagnostics_impl` (`src-tauri/src/commands/sender.rs:368-375, 1404-1418`) does NOT gain a `using_hardware_encoder: bool` field in this slice. The current payload (`dropped_frames_encoder`, `dropped_frames_transport`, `keyframe_requests_received`, `running`) stays unchanged.

**Why**:

1. **Non-trivial wiring required**. `build_video_encoder` (`crates/sm-infra/src/encode/factory.rs:51-55`) returns `Box<dyn VideoEncoder + Send + Sync>`, which is a type-erased trait object. To surface the chosen backend, one of the following must be added:
   - A new method on the `VideoEncoder` trait (`sm-domain::encode`) such as `fn backend_kind(&self) -> EncoderBackendKind` — touches the domain layer, requires a new domain enum, and changes the trait surface (every impl must add it).
   - A tuple/wrapper return type from `build_video_encoder`: `(Box<dyn VideoEncoder>, EncoderBackendKind)` — breaking signature change.
   - A side-channel one-shot/atomic that the factory writes into and the bridge reads — introduces shared mutable state.

   None are 1-line changes; each invites its own design phase. The slice would balloon from ~15 LOC to ~60-100 LOC across 4-5 files, violating the "S" scope sizing from sdd-init v15.

2. **No user demand documented**. v0.1.0 shipped without this disclosure and there is no open issue or roadmap candidate requesting it. The information is observable from `RUST_LOG=sm_infra::encode=info` logs (which fire the SW-fallback line) — sufficient for v0.2.0 power users.

3. **Soak does not depend on UI disclosure**. The 24h soak evidence (D2) reads logs, not the Tauri payload. Adding the UI field would not improve soak quality.

4. **Follow-up captured**. A new roadmap candidate `hw-encoder-backend-disclosure-in-sender-diagnostics` is suggested to track this as a separate XS slice (post v0.2.0), with the implementation choice (trait method vs wrapper enum) decided during its own design phase.

**Alternatives rejected**:

| Option | Reason rejected |
|--------|-----------------|
| Inline tuple return | Breaking signature change for one boolean — disproportionate. |
| `VideoEncoder::backend_kind()` trait method | Domain change; requires adding the method to `WindowsOpenH264Encoder`, `WindowsMftH264Encoder`, all fakes/mocks in test code — touches sm-domain, sm-infra, and src-tauri tests. Out of S scope. |
| One-shot atomic in factory | Introduces shared mutable state for diagnostic-only data — code-smell vs informational logging. |

---

### D4 — Cargo.toml comment refresh policy: **Replace, don't append**

**What**: The existing `Cargo.toml` comment block (`crates/sm-infra/Cargo.toml:8-13`) describes the OPPOSITE situation ("hw-encoder is OFF... deadlocks on real GPU hosts... until the MFT integration is reworked"). It MUST be rewritten end-to-end, not amended. The replacement reads (proposed wording — spec will finalise):

```toml
# Default: hw-encoder is ON (since v0.2.0). The Windows MFT adapter ships the
# vendor-uniform mid-stream IDR mechanism (CODECAPI_AVEncVideoForceKeyFrame,
# VT_UI4 BEFORE ProcessInput) closed in PR #22 (Slice 6 R2). If no compatible
# hardware MFT is found at runtime, the factory falls back to the OpenH264
# software encoder automatically; users can also force the SW path via the
# SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1 environment variable.
default = ["hw-encoder"]
```

The `hw-encoder` feature doc-comment (`Cargo.toml:18-22`) similarly replaces "OPT-IN ONLY — does not currently work end-to-end" with a one-line statement that the feature gates the MFT adapter and the Windows-only `windows` crate features.

**Why**: Leaving stale "deadlocks on real GPU hosts" text in tree-of-record creates a confusing contradiction with `default = ["hw-encoder"]`. Future contributors reading top-down would conclude the project is shipping a known-broken default. The replacement also serves as a documentation anchor: anyone investigating MFT behaviour finds the canonical IDR mechanism reference without having to grep elsewhere.

**Alternatives rejected**: Append-with-strikethrough is unprofessional in Cargo.toml comments. Leaving the old text "for history" duplicates what git blame already provides.

---

### D5 — CHANGELOG style: **Keep a Changelog with `### Changed` + `### Documentation`**

**What**: New `[Unreleased]` content (later moved to `[0.2.0]` at release time) follows the existing v0.1.0 section style — Keep a Changelog headings (`### Added` / `### Changed` / `### Documentation` / etc.), present-tense bullets, paths in inline backticks, link to archived slice in PR/issue form.

Proposed structure (final wording in spec phase):

- `### Changed`
  - HW H.264 encoder enabled by default on Windows. Mid-stream IDR mechanism (ForceKeyFrame ICodecAPI, VT_UI4 BEFORE ProcessInput) validated cross-vendor on Intel QSV + NVIDIA NVENC (Slice 6 R2, PR #22).
  - Automatic SW fallback to OpenH264 if no compatible hardware MFT is found at runtime; one-time `tracing::info!` log records the reason.
- `### Documentation`
  - `crates/sm-infra/README.md` "Hardware encoder smoke tests" section updated for default-on, with `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` documented as the runtime kill-switch.
- (No `### Added`, `### Fixed`, `### Removed`, or `### Security` for this slice.)

**Why**: Project policy is Keep a Changelog (`CHANGELOG.md:5`); matching the v0.1.0 structure preserves diff-friendliness for release notes auto-generation. `### Changed` is the correct heading per Keep a Changelog 1.1 for "default behaviour shifts that do not break the public API".

---

## 4. Empirical evidence anchors

| Engram ID | Title | Why it underpins the flip |
|-----------|-------|---------------------------|
| #816 | Slice 6 R2 archive-report | Bug 1 CLOSED vendor-uniformly; cross-vendor smoke 27/27 PASS both hosts; net −250 LOC; 3 retroactive corrections (Slice 4 ForceKeyFrame, Slice 5 Mechanism G, Slice 5 DD10 CleanPoint) — the foundation that makes default-on safe |
| #815 | Slice 6 R2 verify-report | APPROVED_WITH_CARRY_FORWARD; 20/20 AC VERIFIED; W1/W2 warnings documented and accepted |
| #809 | Phase 0 P2: ForceKeyFrame vendor-uniform proof | NVENC idx 0 (~0ms), Intel QSV idx 1 (~33ms) — both within 30-frame test tolerance; canonical mechanism evidence |
| #808 | Research: Chromium/FFmpeg/HCK canonical ForceKeyFrame timing+variant | Industry precedent (Chromium MediaFoundation, FFmpeg, HCK Win8+ cert mandate VT_UI4 BEFORE ProcessInput) |
| #807 | Phase 0 P1: CleanPoint INPUT-write falsified on NVENC | Confirms output-side semantics for CleanPoint — eliminates a falsified alternative mechanism |
| #801 | Phase 0 P0.b: Mechanism G falsified on NVENC | Proves Mechanism G was Intel-only; reinforces "cross-vendor probes mandatory" lesson |
| #186 | sdd-init v15 | Authoritative roadmap: `hw-encoder-default-on-flip` UNBLOCKED, scope S, 24h soak recommended |
| #819 | Exploration for THIS change | Feature gate topology, test gate inventory, CI matrix impact, MVC scope, approach comparison |

---

## 5. Strict TDD note

This is a **configuration + documentation change with no new executable behaviour**. Strict TDD's RED → GREEN → REFACTOR cycle adapts as follows for this slice:

- **RED**: there is no failing unit test to write because the change is not a logic change — it activates an existing, already-tested code path under the default feature set. The "RED" equivalent is the current state where CI's `check` and `test` jobs (with defaults) do NOT compile `windows_mft.rs` on Windows runners.
- **GREEN**: after the flip, CI's `check (windows-latest)` and `test (windows-latest)` MUST compile `windows_mft.rs` and run the 15 unit tests inside it (all pure-logic, no GPU required — per exploration #819 §2.1). This is the implicit test gate: the +15 tests migrate from "not compiled" to "compiled and run", and they must pass. Plus all existing tests stay green.
- **GREEN gate (explicit, three parts)**:
  1. CI all-green on the PR with the new defaults (check + test + clippy + fmt + msrv + js-test + security).
  2. Soak passes on Host A AND Host B per D2 criteria.
  3. Local pre-PR verification: `cargo clippy --workspace --all-targets --all-features -- -D warnings` AND `cargo nextest run --workspace` both pass on a Windows dev host.
- **REFACTOR**: not applicable — the diff is mechanical (1 toml line + comment refreshes + README + CHANGELOG).

**No new unit or integration tests are added in this slice.** Adding tests for "the default feature is on" would be tautological (`cargo check` without `--no-default-features` IS the test). The 15 unit tests in `windows_mft.rs` and the 8 ignored integration tests are pre-existing and already provide regression coverage for the encoder's logic itself.

---

## 6. Delivery strategy

- **Cached strategy**: `auto-chain`.
- **Plan**: single PR titled `chore(infra): enable hw-encoder by default on Windows (v0.2.0)`.
- **Estimated diff**: ~15 LOC across 4 files (1 toml line flip + ~10 lines of comment/README refresh + ~12 lines CHANGELOG).
- **400-line budget risk**: NONE. Total diff is < 5% of the 400-line budget.
- **No chained/stacked PR slicing required**.
- **No `size:exception` label required**.

The Review Workload Forecast for this slice (to be produced by sdd-tasks) is expected to read:

- Chained PRs recommended: **No**
- 400-line budget risk: **Low**
- Estimated changed lines: **< 30**
- Decision needed before apply: **No**

---

## 7. Risks (with mitigations)

| Risk | Severity | Mitigation | Mitigated? |
|------|----------|-----------|-----------|
| Driver variance — untested vendors (AMD AMF, older Intel Arc, virtualised GPU) may fail MFT probe | MEDIUM | Automatic SW fallback via `InitFailed` path (`factory.rs:101-104`); user-invisible (one-time INFO log) | YES |
| CI compilation regression — `check`/`test (windows-latest)` now compile `windows_mft.rs` with defaults | LOW | `clippy --all-features` (`.github/workflows/ci.yml`) already compiles this path today; 15 new CI-runnable unit tests are pure-logic per #819 §2.1 | YES |
| Performance regression — HW encoder latency higher than SW on some hosts | LOW | Logged via factory; `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` available without rebuild; soak D2 detects via "unexpected fallback OR CPU baseline regression" | YES |
| UX latency — synchronous MFT probe in `WindowsMftH264Encoder::new()` adds session-start latency | LOW | Already validated < 1s on both Host A and Host B per Slice 6 R2; one-time cost per session | YES — accept |
| Soak evidence — no automated telemetry to detect regressions post-deploy | MEDIUM | Manual log-based soak per D2 + env-var kill-switch documented; CHANGELOG explicitly calls out the kill-switch | YES — accept |
| Stale Cargo.toml comments — current text says "hw-encoder OFF, deadlocks on real GPU hosts" | LOW | D4 mandates end-to-end comment rewrite as part of MVC | YES |
| Stale README — current "Hardware encoder smoke tests" section says feature is opt-in via `--features hw-encoder` | LOW | MVC includes README refresh | YES |
| Version bump confusion — patch vs minor | LOW | D1 locks minor (v0.2.0) per project semver policy and Cargo features convention | YES |
| Tauri UI disclosure deferred — users cannot see HW vs SW from the app | LOW | D3 deferred; observable via `RUST_LOG=sm_infra::encode=info`; follow-up candidate captured for post-v0.2.0 | YES — accept |

---

## 8. Acceptance criteria (preview — spec phase will formalise)

The following ACs are the proposal-level statement of done. The spec phase (`sdd-spec`) will refine wording, add scenario coverage (S1..Sn), and produce the requirement IDs (R1..Rn).

- **AC-1**: `crates/sm-infra/Cargo.toml:14` reads `default = ["hw-encoder"]`.
- **AC-2**: `crates/sm-infra/Cargo.toml:8-13` comment block is rewritten per D4 (no stale "deadlocks" / "OPT-IN ONLY" / "pump_loop redesign" text remains).
- **AC-3**: `crates/sm-infra/Cargo.toml:18-22` `hw-encoder` feature doc-comment is rewritten per D4.
- **AC-4**: `crates/sm-infra/README.md` "Hardware encoder smoke tests" section reflects default-on; `--features hw-encoder` is documented as no-longer-required for normal builds; `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` is documented as the runtime kill-switch.
- **AC-5**: `CHANGELOG.md` `[Unreleased]` gains a `### Changed` entry and a `### Documentation` entry per D5; version target `v0.2.0` per D1.
- **AC-6**: `src-tauri/Cargo.toml:7` version field is bumped to `0.2.0` (or left at 0.1.0 if the release-tag slice is separate — to be decided in spec; D1 locks the target version, not the slice boundary for the bump).
- **AC-7**: CI all-green on PR: `check (windows/macos/ubuntu)`, `test (windows/macos/ubuntu)`, `clippy --all-features`, `fmt`, `msrv`, `js-test`, security workflows.
- **AC-8**: Local pre-PR `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo nextest run --workspace` pass on Windows.
- **AC-9**: D2 soak passes — 24h on Host A (Intel QSV) AND 24h on Host B (NVENC), parallel scheduling; zero `tracing::error!` from `sm_infra::encode::*`; zero panics; zero unexpected SW fallback; viewer-reconnect IDR recovery verified at soak start and end.
- **AC-10**: Soak evidence captured in PR body AND saved as engram observation `sdd/hw-encoder-default-on-flip/soak-report`.
- **AC-11**: Env-var kill-switch smoke — setting `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` and launching the Tauri binary still forces the SW path (one-time `tracing::info!` "falling back" line appears OR the SW encoder is selected directly per the env-var branch in `factory.rs:95-97`).
- **AC-12**: No source files outside the 4 in-scope paths are modified.
- **AC-13**: No `.github/workflows/*` files are modified.
- **AC-14**: Conventional Commit: subject is `chore(infra): enable hw-encoder by default on Windows (v0.2.0)` (or equivalent `feat` / `chore` form chosen at apply time; no `BREAKING CHANGE` footer needed since the public API is unchanged).
- **AC-15**: No `Co-Authored-By:` or AI-attribution trailers.

The spec phase will additionally produce:
- Scenarios for the existing 15 unit tests migrating from "not compiled" to "compiled and run" on Windows CI defaults.
- Scenarios for the 8 integration tests staying `#[ignore]` (compiled-but-skipped) on Windows CI defaults.
- A scenario verifying the env-var kill-switch on a real Windows host.
- A scenario for the soak gate per D2.

---

## 9. Open questions remaining for spec / design

1. **Exact CHANGELOG wording** — D5 locks the *style* and *headings*. Precise sentence-level wording for the `### Changed` bullets is deferred to spec.
2. **Exact `Cargo.toml` comment wording** — D4 locks the *content* and *replacement-not-append* policy. The literal text shown in §3-D4 is proposed; spec will finalise.
3. **Exact README section rewrite** — `README.md:113-147` is the rewrite target; spec will produce the final markdown.
4. **Slice boundary for the `src-tauri/Cargo.toml` version bump** — D1 locks the *target* `v0.2.0`. Whether the version bump lands in THIS PR or in a follow-up "release-tag" slice is a tasks-phase decision. Recommendation: include in this PR (atomic with the default-on flip) so the CHANGELOG-to-release linkage stays single-commit.
5. **Whether the soak observation is `type: discovery` or `type: pattern`** — proposal-level call is `discovery`; spec / archive may revise.

These are micro-decisions only; none change scope, risk, or the locked decisions above.

---

## 10. SDD chain anchors

- **Exploration**: engram #819 (`sdd/hw-encoder-default-on-flip/explore`) + `openspec/changes/hw-encoder-default-on-flip/explore.md`
- **Proposal**: this file + engram `sdd/hw-encoder-default-on-flip/proposal`
- **Spec**: pending (`sdd/hw-encoder-default-on-flip/spec`)
- **Design**: pending (`sdd/hw-encoder-default-on-flip/design`)
- **Tasks**: pending (`sdd/hw-encoder-default-on-flip/tasks`)
- **Apply progress**: pending (`sdd/hw-encoder-default-on-flip/apply-progress`)
- **Verify**: pending (`sdd/hw-encoder-default-on-flip/verify-report`)
- **Archive**: pending (`sdd/hw-encoder-default-on-flip/archive-report`)

Predecessor slice: `hw-encoder-mft-nvenc-mid-stream-idr-mechanism` (Slice 6 R2) — archive #816, merged via PR #22 `966e5ee` on 2026-05-10.

---

**Status**: PROPOSAL LOCKED. Ready for parallel `sdd-spec` and `sdd-design`.
