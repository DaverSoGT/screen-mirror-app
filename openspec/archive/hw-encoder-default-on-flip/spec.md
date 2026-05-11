# Spec: hw-encoder-default-on-flip

| Field | Value |
|-------|-------|
| **Change** | `hw-encoder-default-on-flip` |
| **Project** | screen-mirror-app |
| **Date** | 2026-05-10 |
| **Branch baseline** | `efcac92` (master post-Slice-6-R2) |
| **Artifact store** | hybrid (engram `sdd/hw-encoder-default-on-flip/spec` + this file) |
| **Strict TDD** | ACTIVE — `cargo nextest run --workspace` |
| **Scope** | S (small) — ~15 LOC across 4 files |
| **Inputs** | Proposal #820, Exploration #819, sdd-init v15 #186 |
| **Delivery** | auto-chain → single PR |

---

## §1 — Domain: Files This Change Governs

### In-scope files

| File | Current state | Required change |
|------|---------------|-----------------|
| `crates/sm-infra/Cargo.toml:14` | `default = []` | flip to `default = ["hw-encoder"]` |
| `crates/sm-infra/Cargo.toml:8–13` | stale comment citing unreolved Bucket A bugs + deadlock | replace with current-state comment citing Slice 6 R2 closure (#816) and env var kill-switch |
| `crates/sm-infra/Cargo.toml:18–22` | `hw-encoder = []` feature doc-comment with "OPT-IN ONLY" language | replace with default-on language; drop "OPT-IN ONLY" text |
| `crates/sm-infra/README.md:113–148` | "Hardware encoder smoke tests" section instructs users to pass `--features hw-encoder` | update to default-on language; retire the `--features hw-encoder` instruction for normal builds |
| `CHANGELOG.md:13` | `## [Unreleased]` (empty) | add `## [0.2.0]` section per D1 and D5 with `### Changed` + `### Documentation` sub-headings |
| `src-tauri/Cargo.toml:3` | `version = "0.1.0"` | bump to `version = "0.2.0"` per D1 |

### Explicit exclusions (MUST NOT change in this slice)

| File / Surface | Reason |
|----------------|--------|
| `crates/sm-infra/src/encode/factory.rs` | env-var kill-switch + auto-fallback already implemented; zero source diff |
| `crates/sm-infra/src/encode/windows_mft.rs` | encoder logic complete; zero source diff |
| `crates/sm-domain/**` | domain ports are stable; zero diff |
| `.github/workflows/ci.yml` | CI handles the flip correctly as-is; zero diff |
| `.github/workflows/security.yml` | no change needed; zero diff |
| `src-tauri/src/**` | UI disclosure deferred per D3; zero source diff |
| `crates/sm-infra/tests/windows_mft_encode.rs` | HW integration tests remain `#[ignore]`; zero diff |
| `Cargo.lock` | lock file may regenerate via normal cargo operation; no manual edit |

---

## §2 — Empirical Evidence Anchors

| Engram ID | Topic key | What it proves |
|-----------|-----------|----------------|
| #816 | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/archive-report` | Slice 6 R2 CLOSED: Bug 1 (mid-stream IDR) closed vendor-uniformly; ForceKeyFrame ICodecAPI mechanism proven on both hosts; 27/27 PASS on Intel QSV (Host A) and NVIDIA NVENC (Host B); net −250 LOC |
| #815 | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/verify-report` | Slice 6 R2 APPROVED_WITH_CARRY_FORWARD; 20/20 AC VERIFIED; W1/W2 documented |
| #809 | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/...` Phase 0 P2 | ForceKeyFrame vendor-uniform: NVENC IDR at idx 0 (~0 ms), Intel QSV IDR at idx 1 (~33 ms); both within tolerance |
| #186 | `sdd-init/screen-mirror-app` | sdd-init v15 — declares `hw-encoder-default-on-flip` UNBLOCKED; scope S; 24h soak recommended |
| #819 | `sdd/hw-encoder-default-on-flip/explore` | Feature gate topology confirmed: single declaration point `crates/sm-infra/Cargo.toml:14`; macOS/Linux zero-impact; 15 unit tests migrate from "not compiled" to "compiled+run" on Windows CI without HW |
| #820 | `sdd/hw-encoder-default-on-flip/proposal` | D1–D5 locked; MVC confirmed ~15 LOC across 4 files; risks accepted |

---

## §3 — Functional Requirements

### R1 — Cargo.toml `[features].default` value

**Headline**: The `sm-infra` crate's `default` feature set MUST equal `["hw-encoder"]` exactly.

**Body**: After this slice, `crates/sm-infra/Cargo.toml` line 14 MUST read `default = ["hw-encoder"]` with no additional features included in that array. No other Cargo.toml in the workspace (workspace root, `src-tauri/Cargo.toml`, `crates/sm-domain/Cargo.toml`) defines or overrides the `hw-encoder` feature — the single declaration point controls the shipped binary's default encoder path.

**Evidence**: D1 (version bump), D4 (comment replacement), exploration #819 §1.1 (single declaration point confirmed).

**Scenarios**: S1.

---

### R2 — Cargo.toml stale comment removed and replaced

**Headline**: The comment block at `crates/sm-infra/Cargo.toml:8–13` citing unreolved Bucket A bugs and the deadlock MUST be fully replaced with language that reflects the current state.

**Body**: The replacement comment MUST acknowledge that Bug 1 (mid-stream IDR) is CLOSED as of Slice 6 R2 (engram #816), name the `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` env var as the runtime kill-switch, and describe the new default as default-on with automatic SW fallback. The stale Bucket A text ("known unresolved Bucket A bugs", "vendor-specific async event priming", "GetEvent stop-signal starvation", "deadlocks on real GPU hosts", "pump_loop redesign") MUST NOT appear in the final file. Equally, the `hw-encoder = []` feature doc-comment at lines 18–22 MUST drop "OPT-IN ONLY" and the statement that "the path does not currently work end-to-end on hardware"; the replacement MUST reflect production-ready status per #816.

**Evidence**: D4 (replace, don't append), exploration #819 §7.3, proposal #820 §3.

**Scenarios**: S2, S3 (comment), S4 (feature doc-comment).

---

### R3 — README HW encoder section reflects default-on

**Headline**: The `crates/sm-infra/README.md` "Hardware encoder smoke tests" section MUST NOT instruct users to pass `--features hw-encoder` for normal builds.

**Body**: The section at lines 113–148 currently describes `hw-encoder` as opt-in and provides a `cargo nextest run -p sm-infra --features hw-encoder` command for normal invocation. After this slice, the section MUST reflect that `hw-encoder` is default-on for Windows builds. Commands invoking `--features hw-encoder` for standard or smoke-test purposes MUST be updated to omit the flag (or explicitly note it is no longer required). The `--run-ignored only` invocation for hardware-only integration tests MAY retain any feature flags required for that specific use case, but MUST clarify they are not needed for the default build. The env-var kill-switch (`SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1`) documentation MUST be retained verbatim.

**Evidence**: D4, exploration #819 §7.3.

**Scenarios**: S5.

---

### R4 — CHANGELOG contains `[0.2.0]` section

**Headline**: `CHANGELOG.md` MUST contain a new `## [0.2.0]` section above `## [0.1.0]` following Keep a Changelog 1.1 format.

**Body**: The section MUST use `### Changed` for the default-on encoder flip entry and `### Documentation` for the Cargo.toml comment and README update entries, per D5. The `### Changed` entry MUST note that: (1) `WindowsMftH264Encoder` is now the default encoder on Windows; (2) hosts without compatible MFT hardware automatically fall back to `WindowsOpenH264Encoder`; (3) the env var `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` forces the software path at runtime without a rebuild. The `## [Unreleased]` heading MUST remain above `## [0.2.0]` per Keep a Changelog convention. The section date MUST be appended to the heading (e.g. `## [0.2.0] - 2026-05-10`). The `[Unreleased]` and `[0.2.0]` link references at the bottom of the file MUST be updated.

**Evidence**: D1 (v0.2.0 minor bump), D5 (Keep a Changelog `### Changed` + `### Documentation`).

**Scenarios**: S6, S7.

---

### R5 — `src-tauri/Cargo.toml` version bumped to `0.2.0`

**Headline**: `src-tauri/Cargo.toml:3` `version` field MUST be `"0.2.0"`.

**Body**: The shipped Tauri binary (`screen-mirror`) is the primary artifact that users download and install. It MUST carry version `0.2.0` to match the CHANGELOG entry and D1's minor-bump rationale. The field is at `src-tauri/Cargo.toml:3` (`version = "0.1.0"` today). No other fields in `src-tauri/Cargo.toml` change. The workspace root `Cargo.toml` has no `version` field for the virtual manifest — only `src-tauri/Cargo.toml` carries the user-facing version.

**Evidence**: D1 (version bump rationale: RFC 1105, adding default feature = minor; first default-on for shipped binary = semantically significant).

**Scenarios**: S8.

---

### R6 — `factory.rs` env var contract unchanged

**Headline**: The `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` env var branch in `crates/sm-infra/src/encode/factory.rs:95` MUST remain unchanged; zero source-code diff for `factory.rs` in this slice.

**Body**: The env var check at `factory.rs:95` (`std::env::var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER").as_deref() == Ok("1")`) is the runtime kill-switch that allows operators and testers to bypass the default HW path without a rebuild. Its semantics, key name, and comparison value MUST NOT be altered in this slice. The entire `build_video_encoder_with` function and surrounding `HwEncoderConstructor` / `SwEncoderConstructor` type aliases MUST remain identical to `efcac92`.

**Evidence**: Exploration #819 §4.1, proposal #820 §2 (explicit out-of-scope for `factory.rs`).

**Scenarios**: S9, S16.

---

### R7 — `windows_mft.rs` source unchanged

**Headline**: `crates/sm-infra/src/encode/windows_mft.rs` MUST have zero source diff versus baseline `efcac92`.

**Body**: The `WindowsMftH264Encoder` implementation, including its 15 unit tests inside `#[cfg(test)] mod tests`, the `new_for_validation_test()` bypass constructor, the `ForceKeyFrame` atomic flag, the `CODECAPI_AVEncVideoForceKeyFrame` integration, and all `#[cfg(all(target_os = "windows", feature = "hw-encoder"))]` module-level gates MUST remain untouched. This slice changes the feature's default status, not its implementation.

**Evidence**: Proposal #820 §2, exploration #819 §1.2.

**Scenarios**: S10.

---

### R8 — `sm-domain` crate unchanged

**Headline**: The `crates/sm-domain` crate MUST have zero diff versus baseline `efcac92`.

**Body**: The domain ports (`VideoEncoder`, `CaptureSource`, `VideoSender`, `VideoReceiver`, and the `session`/`signaling`/`supervisor`/`transport` modules) MUST NOT be modified. The `sm-domain` hexagonal invariant test (`tests/no_platform_deps.rs`) MUST continue to pass. D3's explicit deferral of the UI disclosure follow-up (`hw-encoder-backend-disclosure-in-sender-diagnostics`) means no new `VideoEncoder::backend_kind()` trait method or equivalent is introduced.

**Evidence**: D3 (UI disclosure deferred), proposal #820 §2.

**Scenarios**: S11.

---

### R9 — Clippy clean

**Headline**: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` MUST exit 0 with zero warnings after the flip.

**Body**: The `clippy` CI job already runs with `--all-features`, which means `windows_mft.rs` is currently Clippy-checked on Windows CI. The default-on flip does NOT introduce new compilation surface to Clippy. This requirement confirms no new warnings are introduced by the Cargo.toml comment rewrite or any inadvertent formatting change. The `--locked` flag confirms `Cargo.lock` is consistent.

**Evidence**: Exploration #819 §3.2 (clippy unchanged after flip), sdd-init #186 (strict TDD mode active).

**Scenarios**: S12.

---

### R10 — `cargo nextest run --workspace` GREEN with default features post-flip

**Headline**: `cargo nextest run --workspace` (default features, as changed by this slice) MUST be GREEN on Windows CI after the flip; pre-existing `#[ignore]` tests remain skipped.

**Body**: After the flip, the `test (windows-latest)` CI job runs `cargo nextest run --workspace --no-tests=warn` with default features, which now include `hw-encoder`. This compiles and runs the 15 unit tests in `windows_mft.rs::tests` (all pure-logic, no GPU required, using `new_for_validation_test()`). All 15 MUST pass. The 8 integration tests in `tests/windows_mft_encode.rs` compile but remain skipped (`#[ignore]`). The 3 factory unit tests (`env_var_override_selects_software_encoder`, `init_failed_falls_back_to_software_encoder`, `invalid_config_propagates_without_fallback`) already run today and MUST continue to pass (they use injected constructors, not the real MFT path). Zero tests are added or removed — this is a migration from "not compiled" to "compiled+run".

**Evidence**: Exploration #819 §2.1, §2.4.

**Scenarios**: S13, S14.

---

### R11 — 15 unit tests in `windows_mft.rs::tests` PASS in Windows CI

**Headline**: The 15 unit tests in `crates/sm-infra/src/encode/windows_mft.rs::tests` MUST compile and PASS in the Windows CI `test` job after the flip, without requiring GPU hardware.

**Body**: These tests are currently gated by `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` at the file level. After the flip, the `feature = "hw-encoder"` condition is satisfied by the new default. All 15 tests use either `new_for_validation_test()` (a bypass constructor, line 1915) or test pure functions — none require COM/MFT initialization or GPU access. The tests and their names (as inventoried in exploration #819 §2.1) MUST each pass individually. No GPU precondition is required for any of these 15 tests.

**Evidence**: Exploration #819 §2.1 (full test inventory with CI-runnable/HW-required flags).

**Scenarios**: S13, S14.

---

### R12 — 24h soak documented before merge

**Headline**: A 24h soak per D2 MUST be documented in the PR body and saved to engram before the PR is merged to master.

**Body**: Per D2, the soak runs in parallel on Host A (Intel QSV) and Host B (NVIDIA NVENC) for a minimum of 24 calendar hours. Pass criteria: zero `tracing::error!` from `sm_infra::encode::*`; zero panics; zero unexpected SW fallback (i.e. no `InitFailed` on a host where HW is known-present); zero encoder crashes; viewer-reconnect IDR verified at soak start and soak end. The evidence artifact MUST be saved as engram observation with topic key `sdd/hw-encoder-default-on-flip/soak-report` (type: `discovery`). The PR body MUST cite the engram ID and include SHA-256 checksums of the raw log archives. The soak MUST complete BEFORE `TF.4` merge to master.

**Evidence**: D2 (soak acceptance criteria, timing, rollback), proposal #820 §3.

**Scenarios**: S17, S18.

---

### R13 — CI workflow files unchanged

**Headline**: All files under `.github/workflows/` MUST remain unchanged (zero diff vs `efcac92`).

**Body**: The existing CI configuration already handles the default-on flip correctly. The `clippy` job uses `--all-features` (hw-encoder path already exercised). The `check` and `test` jobs use workspace defaults (will now compile and run `windows_mft.rs` on Windows CI without any workflow change needed). No new jobs, steps, matrix entries, or env vars are added. The `security.yml` workflow is also unaffected.

**Evidence**: Exploration #819 §3.1, §3.2.

**Scenarios**: S15.

---

### R14 — macOS and Linux CI jobs continue PASSING unchanged

**Headline**: macOS and Linux CI jobs MUST continue to PASS with zero new compilation and zero new test runs on those platforms.

**Body**: The `hw-encoder` feature is doubly gated by `cfg(all(target_os = "windows", feature = "hw-encoder"))` at the module and use-site level. Additionally, the `windows` crate dependency is scoped to `[target.'cfg(target_os = "windows")'.dependencies]` and is never pulled in on macOS/Linux. Flipping `default = ["hw-encoder"]` has ZERO effect on macOS and Linux builds — no new code compiles, no new tests run, no new link symbols are needed. The `check (macos-latest)`, `test (macos-latest)`, `check (ubuntu-latest)`, `test (ubuntu-latest)`, `msrv (ubuntu)`, `fmt (ubuntu)`, and `js-test (ubuntu)` jobs MUST all PASS with no change in behavior.

**Evidence**: Exploration #819 §1.2, §3.2, §5.3; proposal #820 §2.

**Scenarios**: S19, S20.

---

### R15 — PR conformance: conventional commit, no AI attribution

**Headline**: The PR title MUST be a single conventional-commit line; the commit history MUST NOT contain "Co-Authored-By" or AI attribution.

**Body**: The PR title follows the pattern: `chore(infra): enable hw-encoder by default on Windows (v0.2.0)`. No `BREAKING CHANGE:` footer is permitted (the SW fallback preserves behavior for all users without HW). No `Co-Authored-By:` trailer and no AI attribution line appear in any commit in the PR. Commit messages use conventional commits format.

**Evidence**: Project rules (CLAUDE.md), D5 (CHANGELOG style), proposal #820 §6.

**Scenarios**: S21.

---

## §4 — Acceptance Scenarios

### S1 — Cargo.toml default feature value is exactly `["hw-encoder"]`

**GIVEN** the slice is applied to baseline `efcac92`  
**WHEN** `grep` inspects `crates/sm-infra/Cargo.toml`  
**THEN** the line `default = ["hw-encoder"]` is present, and no line `default = []` exists  
**Maps to**: R1  
**CI-runnable**: YES (file check)

---

### S2 — Cargo.toml stale Bucket A comment is absent

**GIVEN** the slice is applied  
**WHEN** `grep` inspects `crates/sm-infra/Cargo.toml` for the stale text  
**THEN** none of the following strings appear: `"known unresolved"`, `"Bucket A"`, `"vendor-specific async event"`, `"GetEvent stop-signal"`, `"deadlocks on real GPU hosts"`, `"pump_loop redesign"`  
**Maps to**: R2  
**CI-runnable**: YES (file check)

---

### S3 — Cargo.toml new comment cites Slice 6 R2 closure

**GIVEN** the slice is applied  
**WHEN** `grep` inspects `crates/sm-infra/Cargo.toml`  
**THEN** a comment block above `default = ["hw-encoder"]` references `#816` (or "Slice 6 R2") AND references `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER`  
**Maps to**: R2  
**CI-runnable**: YES (file check)

---

### S4 — `hw-encoder = []` feature doc-comment no longer says "OPT-IN ONLY"

**GIVEN** the slice is applied  
**WHEN** `grep` inspects `crates/sm-infra/Cargo.toml` for the `hw-encoder` feature entry  
**THEN** the strings `"OPT-IN ONLY"` and `"does not currently work end-to-end"` are absent  
**Maps to**: R2  
**CI-runnable**: YES (file check)

---

### S5 — README HW encoder section does not instruct `--features hw-encoder` for normal builds

**GIVEN** the slice is applied  
**WHEN** the README section "Hardware encoder smoke tests" is read  
**THEN** no command line for a non-HW-only invocation contains `--features hw-encoder`; the section MUST state that `hw-encoder` is the default for Windows builds  
**Maps to**: R3  
**CI-runnable**: YES (file check)

---

### S6 — CHANGELOG contains `## [0.2.0]` section

**GIVEN** the slice is applied  
**WHEN** `grep` inspects `CHANGELOG.md`  
**THEN** a line matching `## [0.2.0]` exists, with a date suffix  
**Maps to**: R4  
**CI-runnable**: YES (file check)

---

### S7 — CHANGELOG `[0.2.0]` section contains flip entry and env var callout

**GIVEN** the slice is applied  
**WHEN** the `## [0.2.0]` section of `CHANGELOG.md` is read  
**THEN** it contains a `### Changed` sub-heading with an entry that names the default-on flip, names `WindowsMftH264Encoder`, names `WindowsOpenH264Encoder` as the automatic fallback, and names `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` as the kill-switch  
**Maps to**: R4  
**CI-runnable**: YES (file check)

---

### S8 — `src-tauri/Cargo.toml` version is `0.2.0`

**GIVEN** the slice is applied  
**WHEN** `grep` inspects `src-tauri/Cargo.toml`  
**THEN** `version = "0.2.0"` is present and `version = "0.1.0"` is absent  
**Maps to**: R5  
**CI-runnable**: YES (file check)

---

### S9 — `factory.rs` diff versus `efcac92` is 0 lines

**GIVEN** the slice is applied  
**WHEN** `git diff efcac92 HEAD -- crates/sm-infra/src/encode/factory.rs` is run  
**THEN** the output is empty (zero diff)  
**Maps to**: R6  
**CI-runnable**: YES (git check)

---

### S10 — `windows_mft.rs` diff versus `efcac92` is 0 lines

**GIVEN** the slice is applied  
**WHEN** `git diff efcac92 HEAD -- crates/sm-infra/src/encode/windows_mft.rs` is run  
**THEN** the output is empty (zero diff)  
**Maps to**: R7  
**CI-runnable**: YES (git check)

---

### S11 — `sm-domain` diff versus `efcac92` is 0 lines

**GIVEN** the slice is applied  
**WHEN** `git diff efcac92 HEAD -- crates/sm-domain/` is run  
**THEN** the output is empty (zero diff); the domain hexagonal invariant test (`tests/no_platform_deps.rs`) PASSES  
**Maps to**: R8  
**CI-runnable**: YES (git check + CI test)

---

### S12 — Clippy exits 0 with zero warnings

**GIVEN** the slice is applied on a Windows host  
**WHEN** `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` is run  
**THEN** exit code is 0 and no warning lines appear in stderr  
**Maps to**: R9  
**CI-runnable**: YES (Windows CI clippy job)

---

### S13 — `cargo nextest run --workspace` exits 0 on Windows CI

**GIVEN** the slice is applied  
**WHEN** the `test (windows-latest)` CI job runs `cargo nextest run --workspace --no-tests=warn`  
**THEN** exit code is 0; all non-ignored tests PASS; `#[ignore]`-annotated tests are skipped (not counted as failures)  
**Maps to**: R10, R11  
**CI-runnable**: YES (Windows CI test job)

---

### S14 — All 15 `windows_mft.rs::tests` PASS individually

**GIVEN** the slice is applied on a Windows host (CI or local)  
**WHEN** `cargo nextest run -p sm-infra` is run (default features, which now include `hw-encoder`)  
**THEN** all 15 tests enumerated in exploration #819 §2.1 appear in the test run output with status PASS; none are skipped due to a `cfg` gate; none require GPU access to pass  
**Maps to**: R10, R11  
**CI-runnable**: YES (Windows CI, no GPU needed)

---

### S15 — CI workflow files unchanged

**GIVEN** the slice is applied  
**WHEN** `git diff efcac92 HEAD -- .github/` is run  
**THEN** the output is empty (zero diff)  
**Maps to**: R13  
**CI-runnable**: YES (git check)

---

### S16 — Env var `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` forces SW path

**GIVEN** the slice is applied, running on a Windows host with compatible HW  
**WHEN** the process is started with `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` in the environment  
**THEN** `build_video_encoder` returns a `WindowsOpenH264Encoder` instance; `WindowsMftH264Encoder::new()` is never called; the factory unit test `env_var_override_selects_software_encoder` PASSES  
**Maps to**: R6  
**CI-runnable**: YES (factory unit test already in CI)  
**Note**: The factory test uses an injected mock constructor — it does not require GPU. The full runtime verification (against real MFT on a GPU host) is BLOCKED_ON_SOAK.

---

### S17 — Soak Host A 24h completes zero error / zero unexpected SW fallback

**GIVEN** the slice is built and deployed to Host A (Intel QSV)  
**WHEN** a 24h screen-mirroring session runs with `RUST_LOG=sm_infra::encode=info`  
**THEN** zero `tracing::error!` from `sm_infra::encode::*`; zero `InitFailed` fallback messages (SW fallback log) unless Host A is unexpectedly degraded; IDR recovery verified at soak start and soak end via viewer reconnect  
**Maps to**: R12  
**CI-runnable**: NO — BLOCKED_ON_SOAK (manual, Host A)

---

### S18 — Soak Host B 24h completes zero error / zero unexpected SW fallback

**GIVEN** the slice is built and deployed to Host B (NVIDIA NVENC)  
**WHEN** a 24h screen-mirroring session runs with `RUST_LOG=sm_infra::encode=info`  
**THEN** zero `tracing::error!` from `sm_infra::encode::*`; zero unexpected SW fallback; IDR recovery verified at soak start and soak end  
**Maps to**: R12  
**CI-runnable**: NO — BLOCKED_ON_SOAK (manual, Host B)

---

### S19 — macOS CI test job PASSES with no new tests run

**GIVEN** the slice is applied  
**WHEN** the `test (macos-latest)` CI job runs  
**THEN** exit code is 0; the test count is IDENTICAL to the pre-flip run on macOS (zero new tests, zero new compilations); `windows_mft.rs` is NOT compiled  
**Maps to**: R14  
**CI-runnable**: YES (macOS CI test job)

---

### S20 — Linux CI test job PASSES with no new tests run

**GIVEN** the slice is applied  
**WHEN** the `test (ubuntu-latest)` CI job runs  
**THEN** exit code is 0; the test count is IDENTICAL to the pre-flip run on Linux; `windows_mft.rs` is NOT compiled  
**Maps to**: R14  
**CI-runnable**: YES (Ubuntu CI test job)

---

### S21 — PR conforms to conventional commit rules, no AI attribution

**GIVEN** the PR is submitted  
**WHEN** the PR title and commit history are inspected  
**THEN** the PR title is a single conventional-commit line (e.g. `chore(infra): enable hw-encoder by default on Windows (v0.2.0)`); no commit message contains `Co-Authored-By:` or any AI attribution; no `BREAKING CHANGE:` footer is present  
**Maps to**: R15  
**CI-runnable**: YES (manual PR review)

---

## §5 — Test Mapping Table

| Scenario | Test / Action | Host | CI-runnable | BLOCKED_ON_SOAK |
|----------|---------------|------|-------------|-----------------|
| S1 | `grep 'default = \["hw-encoder"\]' crates/sm-infra/Cargo.toml` | any | YES | NO |
| S2 | `grep` for stale Bucket A strings in Cargo.toml | any | YES | NO |
| S3 | `grep` for `#816` / `Slice 6 R2` + env var name in Cargo.toml | any | YES | NO |
| S4 | `grep` for `OPT-IN ONLY` / `does not currently work` in Cargo.toml | any | YES | NO |
| S5 | `grep` README for `--features hw-encoder` in non-HW-only commands | any | YES | NO |
| S6 | `grep '## \[0.2.0\]' CHANGELOG.md` | any | YES | NO |
| S7 | Read CHANGELOG `[0.2.0]` section for required entries | any | YES | NO |
| S8 | `grep 'version = "0.2.0"' src-tauri/Cargo.toml` | any | YES | NO |
| S9 | `git diff efcac92 HEAD -- crates/sm-infra/src/encode/factory.rs` | any | YES | NO |
| S10 | `git diff efcac92 HEAD -- crates/sm-infra/src/encode/windows_mft.rs` | any | YES | NO |
| S11 | `git diff efcac92 HEAD -- crates/sm-domain/` + nextest domain tests | any | YES | NO |
| S12 | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Windows CI | YES | NO |
| S13 | `cargo nextest run --workspace --no-tests=warn` (Windows CI `test` job) | Windows CI | YES | NO |
| S14 | `cargo nextest run -p sm-infra` — 15 tests named in #819 §2.1 PASS | Windows (CI or local) | YES | NO |
| S15 | `git diff efcac92 HEAD -- .github/` | any | YES | NO |
| S16 | `env_var_override_selects_software_encoder` factory unit test | Windows CI | YES | NO |
| S17 | 24h manual soak Host A (Intel QSV) — log inspection + IDR verify | Host A | NO | YES |
| S18 | 24h manual soak Host B (NVIDIA NVENC) — log inspection + IDR verify | Host B | NO | YES |
| S19 | macOS CI `test` job — count unchanged | macOS CI | YES | NO |
| S20 | Ubuntu CI `test` job — count unchanged | Ubuntu CI | YES | NO |
| S21 | PR title + commit history inspection | n/a (manual) | YES (manual) | NO |

---

## §6 — Frozen Surfaces

These surfaces MUST NOT change in this slice. Any diff touching them is a spec violation.

| Surface | File / Path | Constraint | Evidence |
|---------|-------------|-----------|----------|
| `sm-domain` trait ports | `crates/sm-domain/**` | Zero diff. `VideoEncoder`, `CaptureSource`, `VideoSender`, `VideoReceiver` signatures unchanged. No `backend_kind()` or similar added. | D3 deferral |
| Factory env var contract | `factory.rs:95` — `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER` | Key name, value comparison, and branch logic MUST NOT change. | R6, S9 |
| `windows_mft.rs` encoder source | `crates/sm-infra/src/encode/windows_mft.rs` | Zero source diff. All 15 unit tests unchanged. | R7, S10 |
| CI workflow files | `.github/workflows/ci.yml`, `.github/workflows/security.yml` | Zero diff. | R13, S15 |
| Phase 0 probes (Slice 6 R2) | `crates/sm-infra/src/encode/windows_mft.rs` (probe tests, `#[ignore]`) | These `#[ignore]`-gated regression probes are part of #816 archive. Zero diff. | #816 archive |
| Slice 1–6 R2 archives | Engram #604, #699, #728, #773, #791, #816 | Immutable historical records. Not modified by this slice. | — |
| `tests/windows_mft_encode.rs` | `crates/sm-infra/tests/windows_mft_encode.rs` | Zero diff. All 8 HW integration tests remain `#[ignore]`. | Exploration #819 §2.2 |
| `src-tauri/src/**` | All Tauri source files | Zero diff. No UI encoder disclosure. | D3 deferral |

---

## §7 — Deleted Code Register

N/A — this slice deletes no executable code. The comment block replacement at `crates/sm-infra/Cargo.toml:8–13` and `18–22` replaces documentation text within TOML comments; no Rust source, test, or configuration logic is deleted.

---

## §8 — Risks

| Risk | Severity | Likelihood | Spec Handling |
|------|----------|------------|---------------|
| Driver variance on unknown GPU vendors (AMD, virtualised Windows, older Intel Arc) — `WindowsMftH264Encoder::new()` may return `InitFailed` | LOW after-flip | MEDIUM (unknown fleet) | Automatic `InitFailed → OpenH264` fallback in `factory.rs` provides silent recovery. Soak (D2) on known-good hosts validates happy path. Users on unsupported HW get identical SW experience to v0.1.0. |
| Soak surfaces a regression not caught by CI (e.g. thermal throttling, sustained bitrate drop, IDR failure after 6+ hours) | MEDIUM (impacts release confidence) | LOW (both hosts passed 27/27 Slice 6 R2 smokes) | R12 gates merge on soak completion. Env var kill-switch (`SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1`) is a zero-rebuild rollback. Raw logs archived per D2. |
| Cargo.toml comment rewrite contains inaccurate text (new comment itself becomes stale) | LOW | LOW | R2 requires comment to cite #816 and name the env var — both are stable facts. S2/S3 scenarios are testable post-apply. |
| Windows CI compilation regression on default features after flip | LOW | LOW | Exploration #819 §3.2 confirms `clippy --all-features` already exercises this path. 15 unit tests are pure-logic (no GPU, no COM). R9/R10 scenarios are CI-runnable. |
| macOS/Linux CI break from feature change | NONE | NONE | All HW encoder code is doubly gated `cfg(all(target_os = "windows", feature = "hw-encoder"))`. Windows crate in Windows-only target dependency. R14/S19/S20 confirm zero impact. |
| `src-tauri/Cargo.toml` version bump triggers unintended downstream effects (e.g. tauri updater URL change) | LOW | LOW | The Tauri updater and release URL are driven by the git tag, not `Cargo.toml` version. The version field is metadata only in v0.1.0 / v0.2.0 (no update server configured yet). |

---

## §9 — Strict TDD Cadence

This slice is a config + docs change. No new executable logic is introduced. The TDD cadence is adapted accordingly.

### C0 — Probes: N/A

No new Phase 0 probes are needed. The empirical evidence base is complete: #816 (Slice 6 R2 archive, 27/27 PASS both hosts), #815 (verify APPROVED), #809 (P2 cross-vendor ForceKeyFrame), #819 (feature gate topology confirmed). The "probe" equivalent for this slice is `cargo check --workspace` succeeding on Windows with the new defaults — already verified by the `clippy --all-features` path.

### C1 — RED (observable failure state)

**Commit content**: flip `crates/sm-infra/Cargo.toml:14` from `default = []` to `default = ["hw-encoder"]` + replace stale comment block (Cargo.toml:8–13, 18–22 per D4).

**Why RED**: Before C1, the `check (windows-latest)` CI job does NOT compile `windows_mft.rs` (feature not in default). After C1 lands, the `test (windows-latest)` job must run 15 new unit tests. If any of the 15 tests has a latent bug, CI turns RED here — making the failure observable and attributable to this exact commit.

**GREEN gate for C1**: CI all-green on the flip commit — check/test/clippy on Windows; check/test on macOS and Linux unchanged.

### C2 — GREEN (complete MVC)

**Commit content**: `crates/sm-infra/README.md` update (R3) + `CHANGELOG.md` `[0.2.0]` section (R4) + `src-tauri/Cargo.toml` version bump to `0.2.0` (R5).

**Why single PR**: All four file changes are mechanical and low-risk. Splitting them across PRs adds review overhead without benefit. The 400-line budget risk is NONE (<15 LOC total). A single PR provides atomic merge.

**GREEN gate for C2**: CI all-green on the full PR; local `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` + `cargo nextest run --workspace` PASS on Windows.

### C3 — POLISH: N/A

No formatting or clippy fixes expected for a Cargo.toml + Markdown change. If CI flags a Markdown formatting issue, fix it in C2 before opening the PR.

### C4 — SOAK (pre-merge gate, not a commit)

After CI is green, before merge: run 24h parallel soak on Host A + Host B per D2. Document in PR body. Save soak-report engram observation with topic key `sdd/hw-encoder-default-on-flip/soak-report`. Merge ONLY after soak passes.

**Invariant**: C1 is committed before C2. C2 is the PR tip. Soak is post-CI, pre-merge. No soak-skip path exists.

---

## §10 — Acceptance Criteria Checklist

| AC | Requirement | Scenario(s) | Verifiable |
|----|-------------|-------------|-----------|
| AC-1 | `crates/sm-infra/Cargo.toml:14` = `default = ["hw-encoder"]` | S1 | CI / file check |
| AC-2 | Stale Bucket A comment absent from `crates/sm-infra/Cargo.toml` | S2 | CI / file check |
| AC-3 | New Cargo.toml comment cites #816 + `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER` | S3 | CI / file check |
| AC-4 | `hw-encoder = []` doc-comment contains no "OPT-IN ONLY" or "does not currently work" text | S4 | CI / file check |
| AC-5 | README "Hardware encoder smoke tests" section reflects default-on; no `--features hw-encoder` for normal builds | S5 | CI / file check |
| AC-6 | `CHANGELOG.md` contains `## [0.2.0]` section with date | S6 | CI / file check |
| AC-7 | `CHANGELOG.md [0.2.0]` `### Changed` entry names encoder flip + fallback + env var kill-switch | S7 | CI / file check |
| AC-8 | `src-tauri/Cargo.toml` version = `"0.2.0"` | S8 | CI / file check |
| AC-9 | `factory.rs` diff vs `efcac92` = 0 lines | S9 | git check |
| AC-10 | `windows_mft.rs` diff vs `efcac92` = 0 lines | S10 | git check |
| AC-11 | `sm-domain` diff vs `efcac92` = 0 lines; hexagonal invariant test PASSES | S11 | git check + CI |
| AC-12 | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` exits 0 | S12 | Windows CI clippy job |
| AC-13 | `cargo nextest run --workspace` GREEN on Windows CI | S13 | Windows CI test job |
| AC-14 | All 15 `windows_mft.rs::tests` PASS without GPU on Windows CI | S14 | Windows CI test job |
| AC-15 | `.github/workflows/` diff vs `efcac92` = 0 lines | S15 | git check |
| AC-16 | `env_var_override_selects_software_encoder` factory unit test PASSES | S16 | Windows CI test job |
| AC-17 | 24h soak on Host A (Intel QSV) PASSES per D2 criteria | S17 | Manual — BLOCKED_ON_SOAK |
| AC-18 | 24h soak on Host B (NVIDIA NVENC) PASSES per D2 criteria | S18 | Manual — BLOCKED_ON_SOAK |
| AC-19 | macOS CI `test` job PASSES; test count unchanged | S19 | macOS CI test job |
| AC-20 | Linux CI `test` job PASSES; test count unchanged | S20 | Ubuntu CI test job |
| AC-21 | PR title is conventional commit; no AI attribution or `Co-Authored-By:` | S21 | Manual PR review |

---

## §11 — SDD Chain Anchors

### Baseline and predecessor

| Anchor | Commit / Ref | Description |
|--------|-------------|-------------|
| Pre-Slice-6-R2 master tip | `c48ae46` | Last commit before Slice 6 R2 branch was merged |
| Post-Slice-6-R2 master tip (this spec's baseline) | `efcac92` | PR #22 merge; Bug 1 CLOSED; hw-encoder-default-on-flip UNBLOCKED |
| Follow-up candidate | `hw-encoder-backend-disclosure-in-sender-diagnostics` | D3 follow-up: expose encoder backend in `sender_diagnostics` (XS, post-v0.2.0) |

### Engram artifact chain

| Phase | Topic key | Engram ID |
|-------|-----------|-----------|
| Explore | `sdd/hw-encoder-default-on-flip/explore` | #819 |
| Proposal | `sdd/hw-encoder-default-on-flip/proposal` | #820 |
| **Spec (this)** | `sdd/hw-encoder-default-on-flip/spec` | _(saved after this file)_ |
| Design | `sdd/hw-encoder-default-on-flip/design` | pending |
| Tasks | `sdd/hw-encoder-default-on-flip/tasks` | pending |
| Apply | `sdd/hw-encoder-default-on-flip/apply-progress` | pending |
| Verify | `sdd/hw-encoder-default-on-flip/verify-report` | pending |
| Archive | `sdd/hw-encoder-default-on-flip/archive-report` | pending |

### Evidence chain (supporting safety of this spec)

| Engram ID | Role |
|-----------|------|
| #816 | Slice 6 R2 archive — Bug 1 CLOSED, ForceKeyFrame vendor-uniform proven |
| #815 | Slice 6 R2 verify — APPROVED_WITH_CARRY_FORWARD, 20/20 AC VERIFIED |
| #809 | Phase 0 P2 — cross-vendor ForceKeyFrame proof (NVENC idx 0, QSV idx 1) |
| #186 | sdd-init v15 — UNBLOCKED declaration, 24h soak recommendation, scope S |
| #819 | Exploration — feature gate topology, test inventory, CI analysis |
| #820 | Proposal — D1–D5 locked, MVC scope, risks accepted |
