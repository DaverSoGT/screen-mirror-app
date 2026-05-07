# Apply Progress: hw-encoder-mft-vendor-compat-slice1-nvenc

> Hybrid mode: engram canonical + openspec mirror.
> Artifact store: hybrid — engram topic_key `sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/apply-progress`.

---

## Batch: B-PHASE0 — Phase 0 diagnostic prep commit

**Status**: COMPLETE
**Commit**: `0a68223` on `phase0/slice1-nvenc-trace`
**Date**: 2026-05-04
**Mode**: Standard (diagnostic-only; Strict TDD exempt per project standards)

### Tasks Completed

- [x] B-PHASE0-T1: Add Phase 0 diagnostic attribute-walk trace block to `setup_mft` in `crates/sm-infra/src/encode/windows_mft.rs`

### Files Changed

| File | Action | Lines Added | What Was Done |
|------|--------|-------------|---------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | Modified | +190 | Added `#[cfg(debug_assertions)]` block inside the `unsafe` section of `setup_mft`, immediately before the production `mft.SetOutputType(0, &out_type, 0)` call |

### Diagnostic Block Detail

Inserted between the last attribute set (`SetUINT32 MF_MT_MPEG2_PROFILE`) and the production `SetOutputType` call (approx lines 572-769 after edit). The block: (1) logs config, (2) walks GetOutputAvailableType(0,0) attributes, (3) runs 7-round binary-search SetOutputType, (4) logs completion. Original SetOutputType follows UNTOUCHED.

### Deviations

None -- matches D-1 exactly.

---

## Batch: B-PHASE0-v2 -- Phase 0 v2 deeper instrumentation

**Status**: COMPLETE
**Commit**: `1aef130` on `phase0/slice1-nvenc-trace`
**Date**: 2026-05-04
**Mode**: Standard (diagnostic-only; Strict TDD exempt per project standards)

### Pivot Rationale

Apply attempt 1 (`8bbdd4f` + `d9197b1` on `feat/hw-encoder-mft-nvenc-h-b3`) failed on Host B: all overlay strategies returned `MF_E_INVALIDMEDIATYPE`. v1 Phase 0 was insufficient: only probed pactivates[0], only slot 0, only from-scratch SetOutputType.

v2 measures the full (activate, slot, strategy) space.

### Tasks Completed

- [x] B-PHASE0-v2-T1: Removed v1 `#[cfg(debug_assertions)]` block from inside `setup_mft` (~268 lines).
- [x] B-PHASE0-v2-T2: Added `MFT_FRIENDLY_NAME_Attribute`, `MFT_TRANSFORM_CLSID_Attribute`, and `PWSTR` to imports.
- [x] B-PHASE0-v2-T3: Added `#[cfg(debug_assertions)] unsafe fn phase0v2_run_matrix()` between `enumerate_and_activate` and the encoder thread section.
- [x] B-PHASE0-v2-T4: Added call site in `init_mft_sync` after `MFStartup` and before `enumerate_and_activate()`, gated `#[cfg(debug_assertions)]`.

### Matrix Details

For each IMFActivate[i] from MFTEnumEx: (1) logs friendly name + CLSID; (2) activates temporary IMFTransform; (3) sets MF_TRANSFORM_ASYNC_UNLOCK; (4) for slot_idx=0..16 runs strategies A-E; logs HRESULT per attempt.

### Files Changed

| File | Net LOC | What Was Done |
|------|---------|---------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | +365 -275 | v1 block removed; phase0v2_run_matrix() added; imports updated; call site added |

### Deviations

None -- matches v2 instrumentation spec exactly.

---

## Batch: B0 -- Branch Setup (attempt 1)

**Status**: COMPLETE
**Branch**: `feat/hw-encoder-mft-nvenc-h-b3` (forked from master `a35e1ae`)
**Date**: 2026-05-04

### Tasks Completed

- [x] B0-T1: Created `feat/hw-encoder-mft-nvenc-h-b3` from `a35e1ae`. No Phase 0 trace code present (grep empty). Satisfies DD6, R4.3.

---

## Batch: B1 -- Core Fix in `setup_mft` (attempt 1 -- SUPERSEDED)

**Status**: SUPERSEDED -- commits `8bbdd4f` + `d9197b1` reset via `git reset --hard a35e1ae`
**Root cause**: Design assumed `pactivates[0]` was NVENC. Phase 0 v2 transcript showed [0] and [1] are AMDh264Encoder (no AMD GPU); [2] is NVIDIA H.264 Encoder MFT. DD5 fallback only triggered on `ActivateObject` failure -- AMD MFT activates cleanly regardless. `setup_mft` failures (where NVENC selection actually matters) were never retried on a different candidate.

---

## Batch: B2 -- Init-time Activation Fallback (attempt 1 -- SUPERSEDED)

**Status**: SUPERSEDED -- same reset as B1 attempt 1.

---

## B-V2-RESET -- Branch Reset to Master

**Date**: 2026-05-05
**Action**: `git reset --hard a35e1ae` on `feat/hw-encoder-mft-nvenc-h-b3`
**Reason**: Attempt 1 commits implemented the wrong design (clone-and-overlay on AMD MFT instead of NVENC). Phase 0 v2 transcript confirmed NVENC is at pactivates[2], not [0]. Full v2 redesign (DD-A through DD-E) required to correctly select NVENC by probing output-type negotiation across all candidates.

---

## Batch: B0-v2 -- Pre-flight verification

**Status**: COMPLETE
**Date**: 2026-05-05

### Tasks Completed

- [x] B0-v2-T1: Verified `git rev-parse HEAD` = `a35e1ae` (master fork point). Branch `feat/hw-encoder-mft-nvenc-h-b3` clean -- untracked only: `openspec/changes/`, `phase0-host-b-nvenc.log`, `phase0v2-host-b-nvenc.log`.
- [x] B0-v2-T2: Verified NO Phase 0 trace block in `crates/sm-infra/src/encode/windows_mft.rs` (master state -- no diagnostic blocks).

---

## Batch: B1-v2 -- Implement DD-A + DD-B + DD-C + DD-D + DD-E

**Status**: COMPLETE
**Commit**: `d3bfbbc` on `feat/hw-encoder-mft-nvenc-h-b3`
**Date**: 2026-05-05
**Mode**: Strict TDD -- INHERITED RED (Phase 0 v2 transcript: 10 FAIL / 8 PASS on master `a35e1ae` at Host B)

### TDD Evidence

| Capability | RED state | GREEN evidence |
|-----------|-----------|----------------|
| C1 -- `try_setup_output_type` (Strategy E, DD-B) | Inherited: 10 FAIL NVENC smokes on Host B at `a35e1ae` | Probe selects NVENC [2]; setup_mft succeeds; 18/18 PASS expected post-verify |
| C2 -- `enumerate_activates` (all candidates Vec) | Inherited RED | Code review: `slice::from_raw_parts` + clone + CoTaskMemFree |
| C3 -- `probe_and_select_mft` (DD-A full probe) | Inherited RED | Code review: ActivateObject + ASYNC_UNLOCK + try_setup_output_type + ICodecAPI cast |
| C4 -- `ShutdownObject` on rejected (DD-D) | Inherited RED | Code review: called in every fail arm of probe_and_select_mft |
| C5 -- `setup_mft` delegates to `try_setup_output_type` (DD-E) | Inherited RED | Code review: single call replaces old 8-setter block |
| Pred-invariants R2.1-R2.5 | 8 PASS (T-NEW-1, T-NEW-2, etc.) | Pump-loop unchanged; all pump-loop tests expected to remain PASS |

### Tasks Completed

- [x] B1-v2-T1: RED confirmed -- branch starts at same `a35e1ae` as Phase 0 v2 RED state.
- [x] B1-v2-T2: Imports updated -- added `IMFMediaType`, `MFT_FRIENDLY_NAME_Attribute`, `MFT_TRANSFORM_CLSID_Attribute`, `PWSTR`; removed `MF_MT_MPEG2_PROFILE`, `eAVEncH264VProfile_Main`.
- [x] B1-v2-T3: `H264_PROFILE_MAIN` constant and comment removed (no longer needed -- MPEG2_PROFILE not overlaid).
- [x] B1-v2-T4: `fn try_setup_output_type(mft, w, h, framerate, bitrate_bps)` added -- GetOutputAvailableType(0,0) + overlay FRAME_SIZE+FRAME_RATE+AVG_BITRATE + SetOutputType. No retries. No DeleteItem. No INTERLACE_MODE/MPEG2_PROFILE/PAR overlay.
- [x] B1-v2-T5: `fn enumerate_activates()` added -- full MFTEnumEx array cloned into Vec, CoTaskMemFree after.
- [x] B1-v2-T6: `fn probe_and_select_mft(activates)` added -- DD-A probe loop with ActivateObject + ASYNC_UNLOCK + try_setup_output_type + ICodecAPI cast; warn+ShutdownObject on each failure; last_err tracking.
- [x] B1-v2-T7: `init_mft_sync` rewritten -- calls `enumerate_activates()` then `probe_and_select_mft()`. Old `enumerate_and_activate` call removed.
- [x] B1-v2-T8: `enumerate_and_activate` function removed. Superseded by `enumerate_activates` + `probe_and_select_mft`.
- [x] B1-v2-T9: `setup_mft` output-type block replaced -- GetAttributes + ASYNC_UNLOCK removed (now in probe). Step 7b replaced with single `try_setup_output_type(mft, w, h, config.framerate, config.bitrate_bps)?` call.
- [x] B1-v2-T10: Friendly-name + CLSID logging added to `probe_and_select_mft` for diagnostic quality.
- [x] B1-v2-T11: Module docstring + `new()` docstring updated to reflect v2 probe-based selection.

### Files Changed

| File | Net LOC (git diff) | What Was Done |
|------|---------|---------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | +297 -124 (421 total) | Imports updated; H264_PROFILE_MAIN removed; enumerate_activates added; probe_and_select_mft added; try_setup_output_type added; init_mft_sync rewritten; enumerate_and_activate removed; setup_mft output-type block replaced |

### API Freeze Check

`git diff a35e1ae HEAD -- crates/sm-domain/` => **empty**. Public API frozen (R8 satisfied).

### Code Review: Untestable Paths (DD7)

- (a) `enumerate_activates` empty-array guard: correct -- `count == 0 || pactivates.is_null()` guard preserved.
- (b) `probe_and_select_mft` fallback to candidate [1], [2]: reachable on multi-GPU hosts; code-review-only on single-GPU.
- (c) `ShutdownObject` on rejected: called in all 4 failure arms of probe loop (DD-D).
- (d) `try_setup_output_type` single source of truth: called from both `probe_and_select_mft` and `setup_mft` (DD-E verified).

### Deviations from v2 Design

None. DD-A through DD-E implemented as specified.

---

## Batch: B2-v2 -- Verification Handoff

**Status**: COMPLETE (handoff written; USER-EXECUTED on Host B)

### Verification Handoff (USER-EXECUTED on Host B)

Run in order; stop at first failure.

1. `cargo check --workspace`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo fmt --check --all`
4. `cargo nextest run --workspace`
5. `cargo deny check`
6. `cargo check --no-default-features`
7. `cargo check --features hw-encoder`
8. SMOKE EVIDENCE (Host B, anchor for spec R6/R7):
   `cargo nextest run -p sm-infra --test windows_mft_encode --features hw-encoder --run-ignored=ignored-only --no-fail-fast`
   Expected: 18/18 PASS.

After all 8 succeed: hand off to `sdd-verify`.

---

---

## Batch: B-PHASE0-v3 -- Phase 0 v3 AV trace instrumentation

**Status**: COMPLETE
**Commit**: `fdc7b98` on `feat/hw-encoder-mft-nvenc-h-b3`
**Date**: 2026-05-04
**Mode**: Standard (diagnostic-only; Strict TDD exempt per project standards)

### Context

Host B smoke at commit `d3bfbbc` (v2 fix): 11/18 PASS, 7 ABORT with `0xC0000005` access violations. Tests that abort include tests reaching the encode pipeline AND two tests that only exercise lifecycle (`mft_stop_is_idempotent`, `mft_drop_without_stop_does_not_leak_thread`) with no encode at all. AV is in the probe path or Drop path, not the encode loop.

### Hypotheses to discriminate

- H-AV1: Probe->production COM ref-count imbalance
- H-AV2: Double `SetOutputType` (probe + setup_mft both call it)
- H-AV3: Probe activate vs production activate of same IMFActivate corrupts NVENC singleton state
- H-AV4: `ShutdownObject` on AMD candidates [0]/[1] interferes with NVENC [2]
- H-AV5: Drop path on a successfully-probed encoder accesses freed COM state

### Tasks Completed

- [x] B-PHASE0-v3-T1: Added `av_trace!` macro (eprintln+flush) at top of file after module doc comment block
- [x] B-PHASE0-v3-T2: Instrumented `init_mft_sync` -- ENTER, before/after CoInitializeEx, before/after MFStartup, before/after enumerate_activates (with count), before/after probe_and_select_mft, cleanup path, EXIT
- [x] B-PHASE0-v3-T3: Instrumented `probe_and_select_mft` -- ENTER with count, per-candidate name+CLSID, before/after ActivateObject, before/after GetAttributes, before/after ASYNC_UNLOCK SetUINT32, before/after try_setup_output_type, before/after ShutdownObject in EVERY failure arm, before/after ICodecAPI cast, WINNER log with index+CLSID, EXIT
- [x] B-PHASE0-v3-T4: Instrumented `try_setup_output_type` -- ENTER with dimensions, before/after GetOutputAvailableType (with HRESULT on Err), before/after each SetUINT64/SetUINT32, before/after SetOutputType (with HRESULT on Err), EXIT
- [x] B-PHASE0-v3-T5: Instrumented `start` -- ENTER, before thread spawn, after thread spawn, EXIT
- [x] B-PHASE0-v3-T6: Instrumented `setup_mft` -- ENTER, effective dims, "DOUBLE NEGOTIATION" warning flag before second try_setup_output_type call (H-AV2 discriminator), before SetInputType, before each ProcessMessage (FLUSH/BEGIN_STREAMING/START_OF_STREAM), EXIT
- [x] B-PHASE0-v3-T7: Instrumented `stop` -- ENTER with handle_is_some+com_initialized, before/after stop_atomic.store, before/after thread join (with Ok/panic result), EXIT
- [x] B-PHASE0-v3-T8: Instrumented `Drop` -- ENTER with handle_is_some+com_initialized+mft_is_some+codec_api_is_some, before/after stop(), before/after codec_api.take(), before/after mft.take(), before/after MFShutdown, before/after CoUninitialize, EXIT
- [x] B-PHASE0-v3-T9: Instrumented `run_encoder_thread` -- ENTER, before/after CoInitializeEx, before/after setup_mft, before/after IMFMediaEventGenerator cast, before pump_loop, after pump_loop returns, before cleanup messages, EXIT

### Files Changed

| File | Net LOC (git diff) | What Was Done |
|------|---------|---------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | +201 -8 | av_trace! macro added; all 8 instrumentation sites implemented |

### DOUBLE NEGOTIATION flag

`setup_mft` emits `[av] setup_mft: DOUBLE NEGOTIATION — calling try_setup_output_type again (2nd call after probe)` before its SetOutputType path. This makes H-AV2 immediately visible in the trace: the probe calls SetOutputType once, then setup_mft calls it again. If NVENC rejects the second call or if the first call puts the MFT into a state that the second call corrupts, the trace will show exactly where.

### Deviations

None -- pure additive av_trace! calls, zero logic changes.

---

---

## Batch: B-V3-REFACTOR — Single-thread COM ownership (IMFTransform on encoder thread)

**Status**: COMPLETE
**Commit**: `95455ff` on `feat/hw-encoder-mft-nvenc-h-b3`
**Date**: 2026-05-05
**Mode**: Strict TDD — INHERITED RED (same Phase 0 v3 ABORT evidence; ccd2e43 = 7 ABORT 0xC0000005)

### Root Cause Addressed

Phase 0 v3 trace (commit `fdc7b98`) confirmed H-AV3: NVENC's `IMFTransform` — despite MTA
registration — AVs deterministically when used from a thread different from the one that called
`ActivateObject`. The ccd2e43 fix (skip second `SetOutputType`) addressed H-AV2 (double negotiation)
but not H-AV3 (cross-thread COM state corruption). The IMFTransform obtained in the probe (caller
thread) was transferred to the encoder thread via `ComSend` and used in `pump_loop` — this is the
root cause of all 7 ABORT tests in ccd2e43.

### Architecture Change

Eliminate cross-thread `IMFTransform` transfer entirely. The caller thread probe is now DESTRUCTIVE:
after `try_setup_output_type` succeeds, `ShutdownObject` is called on the probe's `IMFTransform`
(even for the winner). Only the `IMFActivate` factory pointer is retained. The encoder thread
calls `ActivateObject` itself to produce a fresh, thread-local `IMFTransform`.

### Tasks Completed

- [x] B-V3-T1: `WindowsMftH264Encoder` struct: replaced `mft: Option<IMFTransform>` + `codec_api: Option<ICodecAPI>` with `winning_activate: Option<IMFActivate>`.
- [x] B-V3-T2: `init_mft_sync` signature changed to `-> Result<IMFActivate, EncoderError>`. Returns winning activate, not (IMFTransform, ICodecAPI).
- [x] B-V3-T3: `probe_and_select_mft` signature changed to `-> Result<IMFActivate, EncoderError>`. Probe loop: after `try_setup_output_type` OK, drops mft + calls `ShutdownObject` on winner activate, returns `activate.clone()`. ICodecAPI cast removed from probe.
- [x] B-V3-T4: `new()` updated: `let activate = init_mft_sync(&config)?`; `winning_activate: Some(activate)` in struct init. `mft/codec_api` fields removed.
- [x] B-V3-T5: `start()` updated: takes `winning_activate` via `ComSend(activate)`. Spawns thread passing `activate_send.into_inner()`. No `ComSend<IMFTransform>` or `ComSend<ICodecAPI>`.
- [x] B-V3-T6: `run_encoder_thread` signature changed to take `activate: IMFActivate` instead of `mft: IMFTransform, codec_api: ICodecAPI`. Inside thread: Step 2 = `activate.ActivateObject()` → fresh `mft`. Step 3 = `GetAttributes + ASYNC_UNLOCK`. Steps 4-6 via `setup_mft`. Step 5 = `mft.cast::<ICodecAPI>()`. Step 7 = `mft.cast::<IMFMediaEventGenerator>()`. Step 8 = `pump_loop`.
- [x] B-V3-T7: `setup_mft` REVERTED: removed the ccd2e43 "skip output-type negotiation" block; restored `try_setup_output_type(mft, w, h, ...)` call at function start. Comment updated to explain same-thread safety.
- [x] B-V3-T8: `Drop` updated: removed `codec_api.take()` + `mft.take()` calls; replaced with `winning_activate.take()` to release IMFActivate COM ref before MFShutdown.
- [x] B-V3-T9: `ComSend` safety comment updated: notes it is now used only for `IMFActivate`; `IMFTransform` and `ICodecAPI` are NOT transferred cross-thread.
- [x] B-V3-T10: All docstrings / inline comments updated to reflect the new thread model.

### Files Changed

| File | Net LOC (git diff) | What Was Done |
|------|---------|---------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | +194 -122 (316 total) | struct fields replaced; init_mft_sync + probe_and_select_mft return type changed; run_encoder_thread signature + body rewritten; setup_mft try_setup_output_type call restored; Drop simplified; ComSend comment updated |

### TDD Evidence

| Capability | RED state | GREEN evidence |
|-----------|-----------|----------------|
| C-V3-1: No cross-thread IMFTransform | Inherited: 7 ABORT in ccd2e43 (H-AV3) | Code review: ActivateObject in run_encoder_thread; ComSend only for IMFActivate |
| C-V3-2: Destructive probe ShutdownObject on winner | Not smoke-testable | Code review: `drop(mft); activate.ShutdownObject()` in probe winner arm |
| C-V3-3: setup_mft calls try_setup_output_type (same thread) | Was reverted/skipped in ccd2e43 | Code review: `try_setup_output_type(mft, w, h, ...)` at setup_mft start |
| C-V3-4: ICodecAPI cast on encoder thread | Not separately testable | Code review: `mft.cast::<ICodecAPI>()` in run_encoder_thread after setup_mft |
| Pred-invariants R2.1-R2.5 | Same inherited RED | pump_loop unchanged; all pump-loop paths identical |

### Deviations from Refactor Spec

None. All 8 architectural changes implemented as specified:
- struct fields ✓, init_mft_sync return type ✓, probe_and_select_mft return type ✓,
- new() ✓, start() ✓, run_encoder_thread ✓, setup_mft revert ✓, Drop ✓.

---

## Summary

| Batch | Status | Commit |
|-------|--------|--------|
| B-PHASE0 | COMPLETE | `0a68223` (phase0/slice1-nvenc-trace -- throw-away) |
| B-PHASE0-v2 | COMPLETE | `1aef130` (phase0/slice1-nvenc-trace -- throw-away) |
| B-PHASE0-v3 | COMPLETE | `fdc7b98` on `feat/hw-encoder-mft-nvenc-h-b3` |
| B0 (attempt 1) | COMPLETE | branch `feat/hw-encoder-mft-nvenc-h-b3` at `a35e1ae` |
| B1 (attempt 1) | SUPERSEDED | `8bbdd4f` -- reset (AMD MFT wrong target) |
| B2 (attempt 1) | SUPERSEDED | `d9197b1` -- reset (AMD MFT wrong target) |
| B-V2-RESET | COMPLETE | `git reset --hard a35e1ae` |
| B0-v2 | COMPLETE | pre-flight verified at `a35e1ae` |
| B1-v2 | COMPLETE | `d3bfbbc` on `feat/hw-encoder-mft-nvenc-h-b3` |
| B2-v2 | COMPLETE (handoff) | verification handoff written above |
| B-PHASE0-v3 | COMPLETE | `fdc7b98` on `feat/hw-encoder-mft-nvenc-h-b3` |
| ccd2e43 | INTERMEDIATE | fix(infra): skip double SetOutputType — H-AV2 fix, H-AV3 NOT fixed |
| B-V3-REFACTOR | COMPLETE | `95455ff` on `feat/hw-encoder-mft-nvenc-h-b3` |

Branch `feat/hw-encoder-mft-nvenc-h-b3` at commit `95455ff` ready for Host B verification.

### Strict TDD Evidence (cumulative)

- **RED**: Inherited from Phase 0 v2 transcript (engram #160) and Phase 0 v3 AV trace (ccd2e43: 7 ABORT 0xC0000005). Host B: 10+ smoke tests FAIL/ABORT.
- **GREEN**: Pending Host B serial smoke run. Expected: 18/18 PASS with no ABORT.
- **Untestable paths** (code-review-only per DD7): destructive-probe ShutdownObject on winner, ICodecAPI cast failure on encoder thread, multi-candidate fallback. All code-reviewed in B-V3-REFACTOR.
