# Apply Progress: hw-encoder-mft-rework

> Phase 4 completed: 2026-05-04 (gates 7/7 GREEN, pushed, PR #16 opened, smoke handoff written)
> Phase 3 completed: 2026-05-04 (C3 committed as b0bfeec — DrainComplete arm, spec R6)
> Phase 2 completed: 2026-05-04 (C2 committed as 97d4d81 — orchestrator Option 1)
> Phase 1 completed: 2026-05-04
> Branch: feat/hw-encoder-mft-rework (pushed to origin)
> Base: master HEAD f01f27f
> Strict TDD: ACTIVE

---

## Status

`phase-4-complete` (BLOCKED_ON_SMOKE handoff written; user smoke transcript pending; archive blocked until transcript + acceptance)

## Phases completed

- [x] Phase 0 — anchor (branch + gates + DR-NEW-1 symbols + smoke baseline)
- [x] Phase 1 — tests RED (C1 `8d1b341`): T-NEW-1 added (RED=HANG), T-NEW-2 added (PASS)
- [x] Phase 2 — pump_loop redesign (C2 `97d4d81`): NO_WAIT polling + dual-arm counters + drain-first; T-NEW-1/T-NEW-2 GREEN; smoke 9/18 PASS documented honestly; Bug 1 Layer B deferred to future change
- [x] Phase 3 — DrainComplete arm (C3 `b0bfeec`): METransformDrainComplete arm added per DD3; counter reset + INFO log; spec R6 satisfied; smoke 9/18 PASS (unchanged — Layer B pre-empts DrainComplete on failing tests)
- [x] Phase 4 — quality gates + push + PR + smoke handoff

---

## Phase 0 results

### Branch
`feat/hw-encoder-mft-rework` created from master HEAD `f01f27f`.

### Quality gates baseline: 5/5 GREEN
- `cargo check --workspace`: GREEN
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: GREEN
- `cargo fmt --check --all`: GREEN
- `cargo nextest run --workspace`: 611 passed, 19 skipped
- `cargo deny check`: GREEN

### DR-NEW-1 symbol verification: PASS (all 5 resolved)
All 5 new windows crate symbols verified under `windows = "0.62.2"`:
- `MF_E_NO_EVENTS_AVAILABLE`, `MF_E_SHUTDOWN`, `MF_E_NOTACCEPTING` → `HRESULT`
- `METransformDrainComplete` → `MEDIA_EVENT_TYPE` (.0 is i32)
- `MF_EVENT_FLAG_NO_WAIT` → `MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS` (NOT u32 — use directly)

### Smoke baseline: 6 PASS / 5 ABORT / 5 HANG (of 16 tests)

PASS (6): `mft_drop_without_stop_does_not_leak_thread`, `mft_new_on_hw_capable_machine_returns_ok`,
`mft_new_returns_init_failed_when_no_hardware_mft`, `mft_new_then_drop_does_not_av`,
`mft_drain_after_channel_close_does_not_panic`, `mft_stop_is_idempotent`

ABORT (5, 0xC0000005 Bug 1): `mft_keyframe_flag_cleared_after_idr_emitted`,
`mft_encoded_packet_starts_with_annex_b_start_code`,
`mft_encoded_packet_timestamp_matches_capture_frame`,
`mft_request_keyframe_marks_next_packet_as_keyframe`,
`mft_set_bitrate_updates_encoder_without_restart`

HANG (5, Bug 2): `mft_new_does_not_submit_frames_to_mft_during_init`,
`mft_first_real_packet_is_annex_b`, `mft_setup_falls_back_when_config_dimensions_zero`,
`mft_setup_uses_config_dimensions_when_nonzero`, `mft_thirty_frame_smoke_emits_at_least_one_keyframe`

---

## Phase 1 results

### Commit C1: `8d1b341` — `test(infra): add stop-deadline smoke tests for MFT encoder`

### Tests added
- **T-NEW-1**: `mft_stop_during_idle_returns_within_deadline` (RED: HANG >15s)
- **T-NEW-2**: `mft_stop_during_active_encode_returns_within_deadline` (PASS: 1.032s via 50ms recv_timeout escape hatch)

### RED transcript
```
Starting 2 tests across 1 binary (16 tests skipped)
    PASS [   1.032s] (1/2) mft_stop_during_active_encode_returns_within_deadline
TERMINATING [> 15.000s] (───) mft_stop_during_idle_returns_within_deadline
   TIMEOUT [  15.114s] (2/2) mft_stop_during_idle_returns_within_deadline
Summary: 2 tests run: 1 passed, 1 timed out, 16 skipped
```

---

## Phase 2 results

### Commit C2: `97d4d8147d20acc2d77cf25713f88fcaaf6fe51e` — `feat(infra): redesign MFT pump_loop with NO_WAIT polling and dual-arm counters`

### Code changes

**`crates/sm-infra/src/encode/windows_mft.rs`**:
- Replaced `GetEvent(MF_EVENT_FLAG_NONE)` with `GetEvent(MF_EVENT_FLAG_NO_WAIT)` polling
- Added `POLLING_SLEEP=1ms` and `FRAME_RECV_TIMEOUT=50ms` constants
- Added `ni_count`/`ho_count` dual-arm counters (stack-local)
- Drain HaveOutput FIRST before NeedInput (spec R3, design DD1)
- `apply_pending_codec_settings()` helper extracted (design DD9)
- E_UNEXPECTED vendor priming detection: string-prefix `"ProcessOutput: 0x80004005"` → consume credit + warn (design DD4)
- `MF_E_NOTACCEPTING` debug_assert! + ERROR + return (design DD5)
- Counter snapshot logging on change + 1000-iter heartbeat (spec R7/DD8)

**`.config/nextest.toml`**:
- Removed T-NEW-1 slow-timeout override (T-NEW-1 is now GREEN — no longer needs override)

### T-NEW-1 + T-NEW-2 isolation run (GREEN)
```
Starting 2 tests across 1 binary (16 tests skipped)
    PASS [   0.845s] (1/2) mft_stop_during_active_encode_returns_within_deadline
    PASS [   0.848s] (2/2) mft_stop_during_idle_returns_within_deadline
Summary: 2 tests run: 2 passed, 16 skipped
```

**T-NEW-1: HANG >15s → PASS 848ms (RED → GREEN confirmed)**
**T-NEW-2: PASS 845ms (preserved — Option A invariant maintained)**

### Full smoke run (18 tests) — 9 PASS / 9 FAIL

```
PASS (9):
- mft_drain_after_channel_close_does_not_panic (0.859s) [was PASS]
- mft_drop_without_stop_does_not_leak_thread (0.708s) [was PASS]
- mft_new_does_not_submit_frames_to_mft_during_init (1.051s) [was HANG -> now PASS]
- mft_new_on_hw_capable_machine_returns_ok (0.310s) [was PASS]
- mft_new_returns_init_failed_when_no_hardware_mft (0.298s) [was PASS]
- mft_new_then_drop_does_not_av (0.294s) [was PASS]
- mft_stop_during_active_encode_returns_within_deadline (0.882s) [T-NEW-2]
- mft_stop_during_idle_returns_within_deadline (0.786s) [T-NEW-1, was HANG]
- mft_stop_is_idempotent (0.712s) [was PASS]

ABORT (5, 0xC0000005 — Bug 1 Layer B, driver-level crash):
- mft_encoded_packet_starts_with_annex_b_start_code
- mft_encoded_packet_timestamp_matches_capture_frame
- mft_keyframe_flag_cleared_after_idr_emitted
- mft_request_keyframe_marks_next_packet_as_keyframe
- mft_set_bitrate_updates_encoder_without_restart

FAIL (3, packet timeout 5s, previously HANG — Bug 1 Layer B):
- mft_first_real_packet_is_annex_b (5.758s)
- mft_setup_falls_back_when_config_dimensions_zero (5.767s)
- mft_setup_uses_config_dimensions_when_nonzero (5.720s)

FAIL (1, producer thread blocks on full channel — Bug 1 Layer B):
- mft_thirty_frame_smoke_emits_at_least_one_keyframe (636.881s)
```

### Threshold analysis
| Metric | Value |
|--------|-------|
| PASS count | 9/18 |
| Design forecast (>=14) | NOT MET |
| Minimum threshold (>=12) | NOT MET |
| New regressions vs baseline | ZERO |
| Phase 2 primary goal (T-NEW-1 + T-NEW-2 GREEN) | ACHIEVED |
| Orchestrator decision | Option 1: commit C2 honestly |

### Bug 1 two-layer discovery
Per discovery `sdd/hw-encoder-mft-rework/bug-1-deeper` (#600):

- **Layer A (ordering deadlock)**: RESOLVED by C2. Drain-output-FIRST dual-arm counter ordering correctly handles HaveOutput-before-NeedInput vendor priming sequence.
- **Layer B (driver-level access violation)**: OUT-OF-SCOPE. Vendor's `ProcessOutput` crashes with `0xC0000005` inside driver code at the OS level. Cannot be caught by E_UNEXPECTED policy — no HRESULT is returned. Tracked as future change `hw-encoder-mft-vendor-priming-crash`.

### Workspace quality gates (C2 re-verified — all GREEN)
- `cargo check -p sm-infra --features hw-encoder`: GREEN
- `cargo clippy -p sm-infra --features hw-encoder --tests -- -D warnings`: GREEN
- `cargo fmt --check --all`: GREEN
- `cargo nextest run --workspace` (non-ignored): 611 PASS, 19 SKIPPED

---

## Phase 3 results

### Commit C3: `b0bfeec2f781256c3cb5e2abaa1d97d5d38f5fb0` — `feat(infra): handle METransformDrainComplete event with counter reset`

### Code changes

**`crates/sm-infra/src/encode/windows_mft.rs`** (13 insertions, 5 deletions):
- Replaced Phase 3 placeholder warn arm with real DD3 implementation
- On `METransformDrainComplete`: capture old counter values, reset `ni_count = 0` and `ho_count = 0`, emit `tracing::info!` with structured `old_ni_count` and `old_ho_count` fields
- Does NOT break (top-of-loop `state.stop` remains sole exit per OQ-4)
- Positioned before catch-all warn arm — S6.3 satisfied (no unhandled-event warn for DrainComplete)

### Workspace quality gates (C3 — all GREEN, 5/5)
- `cargo check -p sm-infra --features hw-encoder`: GREEN (1.31s)
- `cargo clippy -p sm-infra --features hw-encoder --tests -- -D warnings`: GREEN (3.47s, zero warnings)
- `cargo check --workspace`: GREEN
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: GREEN
- `cargo fmt --check --all`: GREEN (rustfmt.toml nightly warnings are informational only, same as baseline)
- `cargo nextest run --workspace` (non-ignored): **611 PASS, 19 SKIPPED** (count unchanged)

### Smoke post-C3 (17 of 18 tests observed; test 18 hangs as expected)

```
PASS (9 — same as Phase 2 baseline):
- mft_drain_after_channel_close_does_not_panic (0.805s)
- mft_drop_without_stop_does_not_leak_thread (0.673s)
- mft_new_does_not_submit_frames_to_mft_during_init (1.029s)
- mft_new_on_hw_capable_machine_returns_ok (0.346s)
- mft_new_returns_init_failed_when_no_hardware_mft (0.320s)
- mft_new_then_drop_does_not_av (0.319s)
- mft_stop_during_active_encode_returns_within_deadline (0.875s)
- mft_stop_during_idle_returns_within_deadline (0.792s)
- mft_stop_is_idempotent (0.695s)

ABORT (5, 0xC0000005 — Bug 1 Layer B, driver-level crash, unchanged from Phase 2):
- mft_encoded_packet_starts_with_annex_b_start_code
- mft_encoded_packet_timestamp_matches_capture_frame
- mft_keyframe_flag_cleared_after_idr_emitted
- mft_request_keyframe_marks_next_packet_as_keyframe
- mft_set_bitrate_updates_encoder_without_restart

FAIL (3, packet timeout 5s — Bug 1 Layer B, unchanged from Phase 2):
- mft_first_real_packet_is_annex_b (5.714s)
- mft_setup_falls_back_when_config_dimensions_zero (5.793s)
- mft_setup_uses_config_dimensions_when_nonzero (5.704s)

HANG (1 — mft_thirty_frame_smoke_emits_at_least_one_keyframe, >600s, Bug 1 Layer B):
Process killed; not counted. Same behavior as Phase 2.
```

### Smoke analysis — DrainComplete as insurance (per discovery #600 DD3)

- **PASS count: 9/18** — UNCHANGED from Phase 2 baseline (9/18)
- DrainComplete did NOT move the count (expected per discovery #600)
- Root cause: Bug 1 Layer B crashes occur inside vendor `ProcessOutput` at the OS level (~0.4s after init) BEFORE `COMMAND_DRAIN` would be sent and BEFORE `METransformDrainComplete` could fire
- The DrainComplete arm is insurance for correctness on drain sequences that complete normally
- Spec R6 satisfied: explicit arm present, counters reset, no catch-all warn, no loop exit

---

## Phase 4 results

### Final quality gates (7/7 GREEN on b0bfeec — 2026-05-04)

| Gate | Result |
|------|--------|
| `cargo check --workspace` | PASS (exit 0) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS (exit 0, zero warnings) |
| `cargo fmt --check --all` | PASS (exit 0) |
| `cargo nextest run --workspace` | PASS (611 passed, 19 skipped — count unchanged from anchor-0.1) |
| `cargo deny check` | PASS (exit 0, advisories/bans/licenses/sources ok) |
| `cargo check -p sm-infra --no-default-features` | PASS (exit 0 — HW path opt-in confirmed) |
| `cargo check -p sm-infra --features hw-encoder` | PASS (exit 0 — HW opt-in build compiles) |

### Branch push
- `git push -u origin feat/hw-encoder-mft-rework`: EXIT 0
- Lefthook pre-push: no hook failures
- HEAD: `b0bfeec`
- Tracking: `origin/feat/hw-encoder-mft-rework`

### PR opened
- URL: https://github.com/DaverSoGT/screen-mirror-app/pull/16
- Status: READY (not draft) — per orchestrator confirmation
- Title: `feat(infra): redesign MFT pump_loop — Bug 2 stop starvation + Bug 1 ordering deadlock`
- Convention: NO issue link, NO labels, full SDD body (Summary / Commits / Gates / Test plan / SDD artifacts)

### Smoke handoff
- Document: `openspec/changes/hw-encoder-mft-rework/smoke-handoff.md` (local artifact — openspec/ is never committed to this repo)
- Status: BLOCKED_ON_SMOKE — archive cannot issue APPROVED until user supplies smoke transcript at engram topic_key `sdd/hw-encoder-mft-rework/smoke-transcript` AND user accepts Bug 1 Layer B deferral

---

## Hard stops / blockers

- **BLOCKED_ON_SMOKE**: Archive cannot issue APPROVED until user supplies smoke transcript at engram topic_key `sdd/hw-encoder-mft-rework/smoke-transcript` AND user accepts Bug 1 Layer B deferral

## Out-of-scope deferrals (carry-forward)

- **Bug 1 Layer B** (driver-level access violation in vendor `ProcessOutput`): Future change `hw-encoder-mft-vendor-priming-crash`. Will require re-explore with driver-level instrumentation. Pattern C (separate input/output threads) or COM-call wrapper may become relevant — out of scope for current explore #594 conclusions.

## Next phase

`sdd-verify` — after user supplies smoke transcript at `sdd/hw-encoder-mft-rework/smoke-transcript`.
