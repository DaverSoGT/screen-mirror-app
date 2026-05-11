# Exploration: hw-encoder-default-on-flip

> **Change**: hw-encoder-default-on-flip
> **Project**: screen-mirror-app
> **Date**: 2026-05-10
> **Artifact store**: hybrid (engram topic_key `sdd/hw-encoder-default-on-flip/explore` + `openspec/changes/hw-encoder-default-on-flip/explore.md`)
> **Strict TDD**: ACTIVE (`cargo nextest run --workspace`)
> **Predecessor**: sdd-init v15 (engram #186); Slice 6 R2 CLOSED (archive-report in `openspec/archive/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/archive-report.md`)

---

## 0. Context Summary

v0.1.0 shipped on `master` with `hw-encoder` as an opt-in feature in `sm-infra` (`default = []`). All Bug 1 slices (1–6 R2) are now CLOSED. The MFT encoder uses a vendor-uniform ForceKeyFrame mechanism (`CODECAPI_AVEncVideoForceKeyFrame` VT_UI4=1 BEFORE ProcessInput), proven on both Intel QSV (Host A, 27/27 PASS) and NVIDIA NVENC (Host B, 27/27 PASS). The single remaining gate is enabling the feature by default. sdd-init v15 cleared the change as UNBLOCKED with a recommended 24h soak period before merging to master.

---

## 1. Feature Gate Topology

### 1.1 Where `hw-encoder` Is Declared

**Single declaration point**: `crates/sm-infra/Cargo.toml` lines 7–22.

```toml
[features]
# Default: hw-encoder is OFF. ...
default = []
hw-decoder = []
hw-encoder = []
test-support = []
```

`crates/sm-infra/Cargo.toml:14` — `default = []` is the ONLY line to change.

The workspace root `Cargo.toml` (line 16) references `sm-infra = { path = "crates/sm-infra" }` with NO features override — it inherits sm-infra's published defaults. `src-tauri/Cargo.toml` (line 19) has `sm-infra = { workspace = true }` with no features specified — it also inherits sm-infra's defaults. Neither downstream crate explicitly opts in today, which is why the feature is effectively off for the shipped binary. Changing `default = ["hw-encoder"]` in `sm-infra/Cargo.toml` alone is sufficient to make the feature active for both `sm-infra` builds and `src-tauri` (Tauri app).

### 1.2 Crates/Modules Gated Behind `hw-encoder`

| File | Gate | Notes |
|------|------|-------|
| `crates/sm-infra/src/encode/windows_mft.rs:1` | `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` | Entire file gated; WindowsMftH264Encoder struct + all tests |
| `crates/sm-infra/src/encode/mod.rs:9` | `#[cfg(all(target_os = "windows", feature = "hw-encoder"))]` | `pub mod windows_mft` inclusion |
| `crates/sm-infra/src/encode/mod.rs:18` | `#[cfg(all(target_os = "windows", feature = "hw-encoder"))]` | `pub use windows_mft::WindowsMftH264Encoder` re-export |
| `crates/sm-infra/src/encode/factory.rs:24` | `#[cfg(feature = "hw-encoder")]` | Import of WindowsMftH264Encoder |
| `crates/sm-infra/src/encode/factory.rs:68` | `#[cfg(feature = "hw-encoder")]` | hw_constructor active branch (returns real MFT encoder) |
| `crates/sm-infra/src/encode/factory.rs:73` | `#[cfg(not(feature = "hw-encoder"))]` | hw_constructor stub branch (returns InitFailed, falls to SW) |
| `crates/sm-infra/tests/windows_mft_encode.rs:53` | `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` | Entire integration test file (8 test functions, all `#[ignore]`) |

**Key architectural constraint**: every `hw-encoder` gate is ALSO `target_os = "windows"` (directly or via Cargo's `[target.'cfg(target_os = "windows")'.dependencies]` section). The `windows` crate (COM/MFT symbols) is in `[target.'cfg(target_os = "windows")'.dependencies]` at `crates/sm-infra/Cargo.toml:37–54`, NOT behind `hw-encoder`. This means:

1. The `windows` crate is compiled on Windows regardless of the `hw-encoder` feature.
2. All HW encoder code that uses `windows` types is doubly gated: `cfg(all(target_os = "windows", feature = "hw-encoder"))`.
3. On macOS/Linux, NEITHER the `windows` crate NOR the `windows_mft.rs` module is compiled — the `hw-encoder` feature flip has ZERO compilation impact on non-Windows targets.

### 1.3 All Cargo.toml Files Referencing `hw-encoder`

Only ONE Cargo.toml references `hw-encoder`:
- `crates/sm-infra/Cargo.toml` — declares the feature (lines 14, 22)

Neither `src-tauri/Cargo.toml`, `crates/sm-domain/Cargo.toml`, nor workspace `Cargo.toml` reference the feature by name.

---

## 2. Test Gate Inventory

### 2.1 Unit Tests in `windows_mft.rs` (CI-runnable on Windows with feature active)

The file has `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` at line 1. All 15 unit tests inside `#[cfg(test)] mod tests` at line 1927 are therefore gated behind BOTH conditions. After the default-on flip, these 15 tests will run automatically in the CI `test (windows-latest)` job.

All 15 tests use `new_for_validation_test()` (a bypass constructor that skips COM/MFT, line 1915) or test pure functions — NO GPU required, NO #[ignore].

| Test | Type | CI-runnable | HW required |
|------|------|-------------|-------------|
| `effective_dimensions_returns_fallback_for_sentinel_zero` | unit | YES | NO |
| `effective_dimensions_passes_through_nonzero` | unit | YES | NO |
| `avcc_to_annex_b_converts_known_avcc_payload` | unit | YES | NO |
| `annex_b_contains_idr_detects_idr_with_4byte_start_code` | unit | YES | NO |
| `annex_b_contains_idr_detects_idr_with_3byte_start_code` | unit | YES | NO |
| `annex_b_contains_idr_detects_idr_after_sps_pps_prefix` | unit | YES | NO |
| `annex_b_contains_idr_returns_false_for_p_frame_only` | unit | YES | NO |
| `annex_b_contains_idr_returns_false_for_too_short_input` | unit | YES | NO |
| `new_rejects_zero_bitrate` | unit (validation) | YES | NO |
| `new_rejects_zero_framerate` | unit (validation) | YES | NO |
| `adapter_is_send_sync` | compile-time assert | YES | NO |
| `set_bitrate_zero_returns_invalid_config` | unit (atomic) | YES | NO |
| `force_keyframe_icodecapi_pending_defaults_to_false_on_construction` | unit (atomic) | YES | NO |
| `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true` | unit (atomic) | YES | NO |
| `force_keyframe_icodecapi_pending_swap_consumes_to_false` | unit (atomic) | YES | NO |

### 2.2 Integration Tests in `windows_mft_encode.rs` (HW-required, all `#[ignore]`)

File-level `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` at line 53. All 8 integration test functions have BOTH the `cfg` gate AND `#[ignore]`. After the flip, the file will compile as part of `cargo nextest run --workspace` on the Windows CI job, but ALL tests remain skipped (nextest treats `#[ignore]` as skip unless `--run-ignored only` is passed).

Count: 8 HW-required integration tests (all #[ignore]) in `tests/windows_mft_encode.rs`.

### 2.3 Factory Unit Tests in `factory.rs` (already CI-runnable, NO feature gate on tests)

The 3 tests in `crates/sm-infra/src/encode/factory.rs` (`env_var_override_selects_software_encoder`, `init_failed_falls_back_to_software_encoder`, `invalid_config_propagates_without_fallback`) are in `#[cfg(test)]` only — NO hw-encoder gate. They run TODAY on CI Windows. After the flip, `hw_constructor` changes behavior (routes to real MFT instead of stub), but the tests use INJECTED mock constructors so they are unaffected.

### 2.4 Migration Summary

After the flip:
- **+15 tests migrate from "not compiled" to "compiled and run"** on Windows CI (all pass without HW).
- **+8 integration tests migrate from "not compiled" to "compiled but skipped"** on Windows CI.
- **0 tests change behavior** — HW tests stay `#[ignore]`, mock-injected factory tests are feature-agnostic.
- **0 tests added or removed** — migration only.

---

## 3. CI Job Inventory

### 3.1 Current CI Jobs (`.github/workflows/ci.yml`)

| Job | OS matrix | Feature flags | Current hw-encoder behavior |
|-----|-----------|--------------|----------------------------|
| `check` | windows/macos/ubuntu | `cargo check --workspace` (defaults) | windows_mft.rs excluded from check |
| `test` | windows/macos/ubuntu | `cargo nextest run --workspace --no-tests=warn` (defaults) | windows_mft tests not compiled |
| `clippy` | windows/macos/ubuntu | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | hw-encoder already active via --all-features |
| `fmt` | ubuntu only | `cargo fmt --check --all` | No feature effect |
| `msrv` | ubuntu only | `cargo check --workspace` (defaults) | windows_mft.rs excluded |
| `js-test` | ubuntu only | vitest | No Rust effect |

**Security workflow** (`security.yml`): `cargo deny check` + `cargo audit` on ubuntu, no feature flags.

### 3.2 Post-Flip CI Changes

| Job | Change after flip | Risk |
|-----|-------------------|------|
| `check (windows-latest)` | `windows_mft.rs` NOW compiled under `cargo check` with defaults | LOW — already passing with `--all-features` in clippy |
| `check (macos-latest)` | NO change — `cfg(target_os="windows")` excludes `windows_mft.rs` entirely | NONE |
| `check (ubuntu-latest)` | NO change — same as macOS | NONE |
| `test (windows-latest)` | 15 unit tests in `windows_mft.rs` NOW compiled and run; 8 integration tests compiled but skipped | LOW — tests are pure-logic, no GPU needed |
| `test (macos-latest)` | NO change | NONE |
| `test (ubuntu-latest)` | NO change | NONE |
| `clippy (all)` | NO change — `--all-features` already enables hw-encoder | NONE |
| `msrv (ubuntu)` | NO change — Linux doesn't compile windows_mft.rs | NONE |

**Critical finding**: The `clippy` job already runs with `--all-features`, which means `windows_mft.rs` is already Clippy-checked on Windows CI today. The default-on flip only affects `check` and `test` (defaults). There is NO new compilation surface introduced by this change.

---

## 4. Runtime Fallback Path

### 4.1 Factory Decision Tree (`crates/sm-infra/src/encode/factory.rs`)

```
build_video_encoder(config)
  └── build_video_encoder_with(config, hw_constructor, sw_constructor)
        ├── if SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER == "1" → skip HW → SW (OpenH264)
        ├── else → hw_constructor(config.clone())
        │     [with hw-encoder feature] → WindowsMftH264Encoder::new(config)
        │       ├── validation (bitrate > 0, framerate > 0)
        │       └── init_mft_sync() → MFTEnumEx(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)
        │             ├── for each candidate IMFActivate:
        │             │     ActivateObject → try_setup_output_type (Strategy E: clone + overlay)
        │             │     success → retain IMFActivate, discard IMFTransform → Ok(encoder)
        │             │     fail → ShutdownObject, try next
        │             └── no candidates pass → Err(InitFailed("no hardware MFT"))
        │     [no hw-encoder feature] → Err(InitFailed("hw-encoder feature is disabled"))
        ├── Err(InitFailed(_)) → log_sw_fallback_once(reason) → sw_constructor(config) → Ok(OpenH264)
        └── Err(other) → propagate (InvalidConfig etc.)
```

**Automatic SW fallback**: if `WindowsMftH264Encoder::new()` returns `Err(InitFailed)` (no hardware MFT found, or no GPU present), the factory AUTOMATICALLY falls back to `WindowsOpenH264Encoder`. The user sees no error — only a one-time `tracing::info!` log at level INFO.

**Kill-switch**: `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` env var bypasses HW enumeration entirely and goes straight to OpenH264. Documented in `crates/sm-infra/README.md` lines 133–142.

**No config knob for users yet**: the env var is documented but not exposed in the Tauri UI config. For the soak period, a developer/tester sets it manually.

---

## 5. Default-On User Impact

### 5.1 Windows with compatible HW (Intel Quick Sync / NVIDIA NVENC / AMD AMF)

**Before flip**: OpenH264 software encoder always used (hw-encoder opt-in not set).
**After flip**: `WindowsMftH264Encoder::new()` succeeds → HW encoder used. Encoding is GPU-accelerated, CPU usage drops, bitrate quality potentially higher. Mid-stream IDR via CODECAPI_AVEncVideoForceKeyFrame (vendor-uniform, proven).

Impact: POSITIVE for users. No UX change in the Tauri shell — encoding is transparent.

### 5.2 Windows without compatible HW (no GPU, virtualized, old driver)

**Before flip**: OpenH264 always (same as above — no change for this user class today).
**After flip**: `WindowsMftH264Encoder::new()` returns `Err(InitFailed)` → automatic SW fallback to OpenH264. One-time INFO log. User experience: IDENTICAL to current behavior.

Impact: ZERO degradation. Fallback is automatic and silent.

### 5.3 macOS / Linux

**Before flip**: `windows_mft.rs` is not compiled. `factory.rs` does not exist on non-Windows (gated by `#![cfg(target_os = "windows")]` at line 1 of factory.rs). Encoder selection is not available on these platforms (no capture stack either).
**After flip**: IDENTICAL — `cfg(target_os = "windows")` + `cfg(all(target_os = "windows", feature = "hw-encoder"))` both exclude all HW encoder code. The `windows` crate is in `[target.'cfg(target_os = "windows")'.dependencies]` and never pulled in on macOS/Linux.

Impact: NONE on macOS/Linux.

### 5.4 Shipped Platforms

`src-tauri` ships via `cargo tauri build` on Windows only (MSI + NSIS, per CHANGELOG.md). No macOS/Linux installers exist in v0.1.0. The flip is a Windows-only runtime change.

---

## 6. Soak Strategy Options

### 6.1 What Can Be Observed During a 24h Soak

- **tracing logs**: `tracing::info!` "hardware H.264 MFT unavailable — falling back to software encoder" if no HW found. Any `tracing::warn!` from SetValue rejection (non-fatal per DD13). `RUST_LOG=sm_infra::encode=info` captures factory decisions.
- **Manual smoke**: screen mirroring session with known GPU host — verify video renders without artifacts, IDR recovery works on viewer reconnect.
- **Performance observation**: CPU usage comparison SW vs HW on sender side (informal, no telemetry infra).
- **No automated telemetry**: the app has no crash reporting or metrics backend in v0.1.0. Soak is manual + log-based.

### 6.2 Kill-Switch (Escape Hatch)

**Existing mechanism**: `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` env var in `factory.rs:95`. Already documented. Setting it before launching the Tauri app forces SW path without rebuilding.

This IS a runtime kill-switch — no code change needed for rollback. A developer can set it in the environment or wrap the binary in a launcher script.

**No UI kill-switch**: there is no config file or Tauri settings panel to toggle this. Adding one is Option B in the approach comparison below.

### 6.3 Soak Precedent

This is the FIRST soak-then-merge pattern in this repo. Prior changes (Slices 1–6 R2) went directly to merge after passing CI + manual smoke on the feature branch. The 24h soak recommendation comes from sdd-init v15 (#186) as a risk-management suggestion given this is the first time HW encoder is default-on for end users.

---

## 7. Minimum Viable Change Set

### 7.1 Theoretical Minimum (One Line)

```toml
# crates/sm-infra/Cargo.toml:14 — BEFORE
default = []

# AFTER
default = ["hw-encoder"]
```

That is the entire code change. One line in one file. No other Cargo.toml needs updating. No source files change.

### 7.2 Downstream Feature Dependencies

No other feature in the workspace is gated on `default` status. `hw-decoder = []` is independent and unused. `test-support = []` is dev-only. There is no `default-on` flag or soak-mode-bypass feature. The flip is clean.

### 7.3 Docs/README Updates Required

- `crates/sm-infra/Cargo.toml`: the comment block at lines 8–13 documents WHY hw-encoder is opt-in ("known unresolved Bucket A bugs"). This comment MUST be updated to reflect that Bug 1 is CLOSED and the feature is now default-on.
- `crates/sm-infra/README.md` lines 113–131: section "Hardware encoder smoke tests" says hw-encoder is opt-in. Should be updated to say it is now default-on and `--features hw-encoder` is no longer needed for normal builds.
- `CHANGELOG.md`: `[Unreleased]` section needs an entry for the default-on flip and the new v0.2.0 bump (or v0.1.1 if patch-level).

### 7.4 Release Notes / CHANGELOG

`CHANGELOG.md` currently has `## [Unreleased]` (empty). A new entry is needed. The v0.1.0 "Known Limitations" does NOT list the HW encoder as a limitation (it was opt-in, so users had to know to opt in). No backward-compat concern: the SW fallback preserves behavior for users without HW.

---

## 8. Risks and Unknowns

| Risk | Severity | Mitigation | Open? |
|------|----------|-----------|-------|
| HW driver variance: unknown GPU vendors (AMD, older Intel Arc) may fail MFT probe | MEDIUM | Automatic SW fallback via `InitFailed` path; no user-visible error | Mitigated |
| CI compilation: `check` and `test` jobs now compile `windows_mft.rs` by default on Windows CI | LOW | Clippy `--all-features` already tests this path; 15 new CI-runnable unit tests are pure-logic | Mitigated |
| Performance regression: HW encoder latency higher than SW on some hosts | LOW | Factory logs SW-fallback with reason; `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` as escape hatch | Mitigated |
| UX regression: HW init adds startup latency (synchronous MFT probe in `new()`) | LOW | Probe is already validated at <1s on both Host A and B; only fires once at session start | Low — no mitigation needed |
| Soak evidence: no automated telemetry to detect regressions post-deploy | MEDIUM | Manual log-based soak + `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` rollback without rebuild | Open — accept risk, document in CHANGELOG |
| Comment/doc staleness: existing Cargo.toml comment says "do not enable by default" | LOW | Must update inline comment and README section | Open — part of MVC |

---

## 9. Approach Comparison

| | **A) Single-line flip + 24h soak + merge** | **B) Single-line flip + SM_FORCE_SW env var (documented)** | **C) Phased rollout (Windows-only profile first)** |
|--|--|--|--|
| **Code delta** | 1 line changed | 1 line + README update (env var already exists) | 2 Cargo profiles or conditional workspace feature |
| **Rollback** | Requires reverting Cargo.toml and rebuilding | Set env var at runtime, no rebuild | N/A |
| **Soak visibility** | Manual logs | Manual logs + documented env escape hatch | Same |
| **CI impact** | `check` + `test` (windows) now exercise hw-encoder path with defaults | Same | Same |
| **Complexity** | Minimal | Minimal — env var already exists in factory.rs:95 and README:133 | Medium — profile-based split adds Cargo complexity |
| **macOS/Linux** | No impact (cfg-gated) | No impact | No impact — phasing is pointless here |
| **Recommendation** | VIABLE | RECOMMENDED | NOT RECOMMENDED — phasing adds complexity with no benefit since macOS/Linux are unaffected by the feature at the compilation level |

**Recommendation: Approach B — Single-line flip with documented env var escape hatch.**

Justification: The `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER` env var already exists in the codebase (`factory.rs:95`, documented in `README.md:133`). Approach B costs nothing extra in code — the env var is already there. The only addition is making sure the comment in `Cargo.toml` and the README explicitly call out the escape hatch as the soak rollback mechanism. Approach A is equally viable but leaves the rollback mechanism implicit. Approach C adds unnecessary Cargo complexity since macOS/Linux have zero impact from this change (the feature is fully platform-gated).

---

## 10. Open Questions for Proposal Phase

1. **Version bump**: should the default-on flip be v0.1.1 (patch — behavior improvement, no breaking change) or v0.2.0 (minor — semantics shift for library consumers)? The CHANGELOG policy says: "Minor bumps MAY contain breaking changes." This flip does NOT break anything but is a significant behavior change. **Proposal should lock down version target.**

2. **Soak duration and evidence bar**: sdd-init v15 recommends 24h. Is there a specific pass/fail criterion beyond "no regressions observed"? A definition like "soak on Host A + Host B, both sessions complete without tracing::error! or encoder crash" would make the gate concrete. **Proposal should define soak acceptance criteria.**

3. **Tauri UI disclosure**: should the Tauri sender UI display which encoder is active (HW vs SW)? The `sender_diagnostics` command exists but its payload is unknown. Adding a "Using hardware encoder: true/false" field would give users visibility. **Optional improvement — proposal should decide in/out of scope.**

---

## Affected Files

| File | Change needed | Scope |
|------|--------------|-------|
| `crates/sm-infra/Cargo.toml:14` | `default = []` → `default = ["hw-encoder"]` | MVC (required) |
| `crates/sm-infra/Cargo.toml:8-13` | Update comment (Bug 1 closed, now default-on) | MVC (docs) |
| `crates/sm-infra/README.md:113-131` | Update HW encoder section (default-on, no --features needed) | MVC (docs) |
| `CHANGELOG.md:[Unreleased]` | Add entry for default-on flip | MVC (release) |
| `crates/sm-infra/src/encode/factory.rs` | NO change — env var and fallback already implemented | Not needed |
| `crates/sm-infra/src/encode/windows_mft.rs` | NO change | Not needed |
| `src-tauri/Cargo.toml` | NO change — inherits sm-infra defaults | Not needed |
| `.github/workflows/ci.yml` | NO change — check/test/clippy all handle this correctly | Not needed |

---

## Ready for Proposal

YES. All investigation questions are answered. Approach B is recommended. Three open questions exist for the proposal phase to lock down (version bump, soak criteria, UI disclosure). Code change is trivially small (1 line + 3 doc updates).
