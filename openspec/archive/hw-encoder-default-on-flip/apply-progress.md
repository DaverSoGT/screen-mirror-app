# Apply Progress: hw-encoder-default-on-flip

| Field | Value |
|-------|-------|
| **Change** | `hw-encoder-default-on-flip` |
| **Project** | screen-mirror-app |
| **Phase** | A (TA.1–TA.11) DONE |
| **Branch** | `feat/hw-encoder-default-on-flip` |
| **Base** | `efcac92` (master post-Slice-6-R2) |
| **Commit SHA** | `70a6dd8cc202da25e5f9bc93b5679c77000dec8e` |
| **Date** | 2026-05-10 |
| **Strict TDD** | ACTIVE — `cargo nextest run --workspace` |
| **Artifact store** | hybrid |

---

## Phase A Task Status

| Task | Status | Notes |
|------|--------|-------|
| TA.1 | DONE | `default = ["hw-encoder"]` confirmed at line 16 (was line 14 before comment expansion) |
| TA.2 | DONE | Stale Bucket A comment replaced; #816 + SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1 cited; OPT-IN ONLY removed |
| TA.3 | DONE | README HW encoder section updated; `--features hw-encoder` removed from normal builds; kill-switch framed as Tier 1 rollback (DD7) |
| TA.4 | DONE | `## [0.2.0] - 2026-05-10` added with Changed/Documentation/Compatibility sub-headings; link references updated |
| TA.5 | DONE | version 0.1.0 → 0.2.0 at line 3 (DD4 confirmed: NOT line 7 as proposal referenced) |
| TA.6 | DONE | `cargo check --workspace` EXIT 0 in ~20s |
| TA.7 | DONE | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` EXIT 0, zero warnings |
| TA.8 | DONE | `cargo nextest run --workspace` GREEN — 626 tests run, 626 passed, 46 skipped (#[ignore]) |
| TA.9 | DONE | All 15 windows_mft::tests PASS (confirmed by grep — 15 matches) |
| TA.10 | DONE | Zero diff on factory.rs, windows_mft.rs, sm-domain/ |
| TA.11 | DONE | Single atomic commit `70a6dd8` — conventional commit, no Co-Authored-By |

---

## Files Changed

| File | Action | Lines Added | Lines Removed | Notes |
|------|--------|-------------|---------------|-------|
| `crates/sm-infra/Cargo.toml` | Modified | +14 | -7 | Feature flip + comment rewrite |
| `crates/sm-infra/README.md` | Modified | +18 | -8 | HW encoder section default-on reframe |
| `CHANGELOG.md` | Modified | +40 | -1 | [0.2.0] section + updated link refs |
| `src-tauri/Cargo.toml` | Modified | +1 | -1 | version 0.1.0 → 0.2.0 |
| `Cargo.lock` | Auto-updated | +1 | -1 | Normal cargo operation — screen-mirror version bump only |

**Total git stat**: 5 files changed, 69 insertions(+), 20 deletions(-)

---

## Cargo Validation Results

| Gate | Result | Details |
|------|--------|---------|
| `cargo check --workspace` | GREEN | EXIT 0, ~20s, no errors |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | GREEN | EXIT 0, zero warnings |
| `cargo nextest run --workspace` | GREEN | 626 tests run, 626 passed, 46 skipped |

---

## 15 windows_mft::tests — All PASS

| # | Test Name | Status |
|---|-----------|--------|
| 1 | `adapter_is_send_sync` | PASS |
| 2 | `annex_b_contains_idr_detects_idr_after_sps_pps_prefix` | PASS |
| 3 | `annex_b_contains_idr_detects_idr_with_3byte_start_code` | PASS |
| 4 | `annex_b_contains_idr_detects_idr_with_4byte_start_code` | PASS |
| 5 | `annex_b_contains_idr_returns_false_for_p_frame_only` | PASS |
| 6 | `annex_b_contains_idr_returns_false_for_too_short_input` | PASS |
| 7 | `avcc_to_annex_b_converts_known_avcc_payload` | PASS |
| 8 | `effective_dimensions_passes_through_nonzero` | PASS |
| 9 | `effective_dimensions_returns_fallback_for_sentinel_zero` | PASS |
| 10 | `force_keyframe_icodecapi_pending_defaults_to_false_on_construction` | PASS |
| 11 | `force_keyframe_icodecapi_pending_swap_consumes_to_false` | PASS |
| 12 | `new_rejects_zero_bitrate` | PASS |
| 13 | `new_rejects_zero_framerate` | PASS |
| 14 | `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true` | PASS |
| 15 | `set_bitrate_zero_returns_invalid_config` | PASS |

---

## Strict TDD Evidence (DD9 adapted — config-only slice)

| Task | Cycle Role | Evidence |
|------|-----------|---------|
| TA.1 | RED→GREEN | Feature gate flip; 15 unit tests migrate from "not compiled" to "compiled+run" on Windows CI |
| TA.2 | GREEN | Grep AC: stale strings absent, #816 + env-var present |
| TA.3 | GREEN | Grep AC: `--features hw-encoder` absent from normal build invocations |
| TA.4 | GREEN | Grep AC: `[0.2.0]` section + SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER present |
| TA.5 | GREEN | Grep AC: `version = "0.2.0"` at line 3 |
| TA.6–TA.9 | REFACTOR | Workspace check + clippy + nextest all GREEN; 15 unit tests PASS |

---

## DD4 Line Number Deviation

**Design reference**: proposal mentioned line 7; design DD4 corrected to line 3.
**Actual**: `grep -n '^version' src-tauri/Cargo.toml` returned `3:version = "0.1.0"`.
**Action taken**: Confirmed line 3 before edit, as required by DD4.

---

## Deviations from Design

1. **Cargo.lock included in commit stat**: git stat shows 5 files (not 4) because `cargo check` triggered a Cargo.lock update for the `screen-mirror` package version bump. This is correct behavior per §1 exclusions ("lock file may regenerate via normal cargo operation; no manual edit"). The 4 intentionally-edited files are the 4 specified in DD6.

---

## Phase B Boundary Note

**Phase B (push + PR) NOT done by apply per orchestrator boundary.**

The commit is local on branch `feat/hw-encoder-default-on-flip` at SHA `70a6dd8cc202da25e5f9bc93b5679c77000dec8e`. No push, no PR, no merge performed.

---

## Remaining Phases

| Phase | Tasks | Status |
|-------|-------|--------|
| B | TB.1–TB.4 | PENDING (push + PR + CI gate) |
| C | TC.1–TC.6 | PENDING (24h soak, human-driven) |
| D | TD.1–TD.2 | PENDING (sdd-verify) |
| E | TE.1–TE.4 | PENDING (merge) |
| F | TF.1–TF.4 | PENDING (archive + sdd-init v16) |
