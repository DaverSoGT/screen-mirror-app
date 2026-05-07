# Tasks: hw-encoder-mft-vendor-compat-slice1-nvenc

> Artifact store: hybrid — engram canonical (`sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/tasks`)
> + openspec mirror at this file.
> Branch: production-fix branch forked from master `a35e1ae` (NOT from `phase0/slice1-nvenc-trace`).
> Strict TDD: ENABLED. RED is the 10 failing smoke tests on Host B (Phase 0 #152).

---

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~80–120 total (production ~40–55 net; tests 0; branch setup ~5) |
| 400-line budget risk | Low |
| Chained PRs recommended | No |
| Suggested split | Single PR (per DD8) |
| Delivery strategy | auto-chain |
| Chain strategy | size-exception not needed; single PR |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: stacked-to-main
400-line budget risk: Low

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | Entire Slice 1 fix (B0+B1+B2+B3) | Single PR against master `a35e1ae` | ~40–55 net production LOC; tests unchanged; 7 gates GREEN + Host B 18/18 PASS transcript |

---

## B0 — Branch Setup

- [x] B0-T1 Create branch `feat/hw-encoder-mft-nvenc-h-b3` from master `a35e1ae`; confirm no Phase 0 trace code is present (grep confirmed empty). Satisfies DD6, R4.3.
  - **Evidence**: `git switch -c feat/hw-encoder-mft-nvenc-h-b3` from `a35e1ae`; grep returned no output.

---

## B1 — Core Implementation (attempt 1 — SUPERSEDED)

> Commits `8bbdd4f` + `d9197b1` were reset. Root cause: design targeted pactivates[0] (AMD MFT, no AMD hardware). Phase 0 v2 transcript (engram #160) identified NVENC at pactivates[2]. Full v2 redesign required.

---

## B2 — Init-time Activation Fallback (attempt 1 — SUPERSEDED)

> Same reset as B1 attempt 1.

---

## B-V2-RESET

- [x] `git reset --hard a35e1ae` executed. Branch `feat/hw-encoder-mft-nvenc-h-b3` returned to master state.

---

## B1-v2 — v2 Redesign: DD-A + DD-B + DD-C + DD-D + DD-E — Commit `d3bfbbc`

> **Inherited RED state**: Phase 0 v2 transcript (engram #160): 10 FAIL / 8 PASS on master `a35e1ae` at Host B. NVENC at pactivates[2] never reached — AMD MFT at [0] activates cleanly but fails silently in setup_mft.

- [x] B1-v2-T1 RED confirmed — branch at `a35e1ae` (same RED state as Phase 0 v2).
- [x] B1-v2-T2 Imports: added IMFMediaType, MFT_FRIENDLY_NAME_Attribute, MFT_TRANSFORM_CLSID_Attribute, PWSTR; removed MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.
- [x] B1-v2-T3 H264_PROFILE_MAIN constant removed.
- [x] B1-v2-T4 `fn try_setup_output_type(mft, w, h, framerate, bitrate_bps)` added (DD-B/DD-E): GetOutputAvailableType(0,0) + overlay FRAME_SIZE+FRAME_RATE+AVG_BITRATE + SetOutputType. No retries. No DeleteItem. No INTERLACE_MODE/MPEG2_PROFILE/PAR overlay.
- [x] B1-v2-T5 `fn enumerate_activates()` added (DD-A): full MFTEnumEx array cloned into Vec, CoTaskMemFree after.
- [x] B1-v2-T6 `fn probe_and_select_mft(activates)` added (DD-A/DD-D): ActivateObject + ASYNC_UNLOCK + try_setup_output_type + ICodecAPI cast; warn+ShutdownObject on each failure; last_err tracking. Friendly-name + CLSID logging.
- [x] B1-v2-T7 `init_mft_sync` rewritten: enumerate_activates() + probe_and_select_mft(). Old enumerate_and_activate call removed.
- [x] B1-v2-T8 `enumerate_and_activate` function removed.
- [x] B1-v2-T9 `setup_mft` output-type block replaced: GetAttributes + ASYNC_UNLOCK removed (now in probe). Step 7b = try_setup_output_type(mft, w, h, config.framerate, config.bitrate_bps)?
- [x] B1-v2-T10 Friendly-name + CLSID logging in probe_and_select_mft.
- [x] B1-v2-T11 Module docstring + new() docstring updated.

---

## B2-v2 — Verification + Quality Gates + Smoke Evidence

- [x] B2-v2-T1 Quality gates: handoff to user (7 gates listed in apply-progress).
- [x] B2-v2-T2 Host B smoke: handoff to user (18/18 PASS expected).
- [x] B2-v2-T3 Predecessor invariants: pump_loop unchanged; T-NEW-1, T-NEW-2 in unmodified code paths.
- [x] B2-v2-T4 API freeze: `git diff a35e1ae HEAD -- crates/sm-domain/` => empty.
- [x] B2-v2-T5 Code review: all untestable-path checks PASS (enumerate_activates guard, probe fallback, ShutdownObject in all fail arms, try_setup_output_type as single source of truth).
- [x] B2-v2-T6 Commit: `d3bfbbc` on `feat/hw-encoder-mft-nvenc-h-b3`.

---

## B-V3-REFACTOR — Single-thread COM ownership — Commit `95455ff`

> **Root cause**: Phase 0 v3 trace (commit `fdc7b98`) confirmed H-AV3: NVENC's `IMFTransform`
> AVs deterministically when used from a thread different from the one that called `ActivateObject`.
> ccd2e43 fixed H-AV2 (skip double SetOutputType) but left H-AV3 (cross-thread transfer) open.
> This batch eliminates cross-thread `IMFTransform` / `ICodecAPI` entirely.

- [x] B-V3-T1 Struct: replaced `mft: Option<IMFTransform>` + `codec_api: Option<ICodecAPI>` with `winning_activate: Option<IMFActivate>`.
- [x] B-V3-T2 `init_mft_sync` return type: `-> Result<IMFActivate, EncoderError>`.
- [x] B-V3-T3 `probe_and_select_mft` return type: `-> Result<IMFActivate, EncoderError>`; winner's IMFTransform ShutdownObject'd after probe; ICodecAPI cast removed.
- [x] B-V3-T4 `new()`: stores `winning_activate: Some(activate)`.
- [x] B-V3-T5 `start()`: transfers `IMFActivate` via `ComSend`; removed `ComSend<IMFTransform>` and `ComSend<ICodecAPI>`.
- [x] B-V3-T6 `run_encoder_thread`: takes `activate: IMFActivate`; inside thread: ActivateObject → fresh mft → GetAttributes + ASYNC_UNLOCK → setup_mft → ICodecAPI cast → IMFMediaEventGenerator cast → pump_loop.
- [x] B-V3-T7 `setup_mft`: REVERTED ccd2e43 skip; restored `try_setup_output_type` call at start (same-thread — safe).
- [x] B-V3-T8 `Drop`: simplified to `winning_activate.take()` before MFShutdown; removed mft/codec_api take calls.
- [x] B-V3-T9 `ComSend` safety comment: updated to note only `IMFActivate` crosses thread boundary.
- [x] B-V3-T10 All docstrings updated to reflect new thread model.

---

## Coverage Matrix

| R-row | Task ID(s) | Capability | Test / Evidence |
|-------|-----------|-----------|-----------------|
| R1.1 — NVENC SetOutputType accepts cloned base | B1-T3 | C1 | B3-T2 (18/18 PASS transcript) |
| R1.2 — MAJOR_TYPE + SUBTYPE always present | B1-T3 | C1 | B3-T2 (all 10 previously-failing PASS) |
| R1.3 — FRAME_SIZE, FRAME_RATE, PAR preserved | B1-T3 | C1 | B3-T2 |
| R1.4 / S1.3 — Enumeration loop graceful exit | B1-T2 | C2 | B3-T5 (code review) |
| R1.4 ext — Profile retry-without-profile | B1-T4 | C3 | B3-T5 (code review) + B3-T2 (path logged) |
| R2.1–R2.5 — Pump loop invariants preserved | (no change) | — | B3-T3 (T-NEW-1, T-NEW-2 PASS) |
| R3 (reduced to no-op; H-B1 refuted) | (no tasks) | — | B3-T4 (no new MftEncoderShared fields) |
| R4 (Phase 0 instrumentation NOT carried) | B0-T1 | C6 | B0-T1 (grep confirms absence) |
| R5.1–R5.4 / S5.2 — Enumeration fallback | B2-T1, B2-T2 | C5 | B3-T5 (code review, single-GPU path via B3-T2) |
| R6.1/R6.2/R6.3 — 18/18 Host B PASS | B1-T2..B1-T4, B2-T1, B2-T2 | C1–C5 | B3-T2 (transcript) |
| R7 — 7 quality gates GREEN | B3-T1 | — | B3-T1 (handoff to user) |
| R8 — Public API frozen | (no change to sm-domain) | — | B3-T4 (byte-identical check — CONFIRMED) |

---

## Notes

1. **RED state** is inherited from Phase 0 (#152): 10 failing smokes on Host B against master. Production branch starts from `a35e1ae` which carries the same RED state.
2. **C2, C3, C4 untestable paths** are documented per DD7: NVENC returns OK at `n=0` (C2 loop continuation never fires on Host B); profile rejection on cloned base may or may not occur at apply-time (C3 retry path); multi-GPU fallback `i=1+` requires dual-GPU hardware (C4/C5). All three are code-review-only per spec A4 and DD7.
3. **DD5 init-only scope limitation**: B2 covers `ActivateObject` + `ICodecAPI` cast failures only. `setup_mft` post-init failures are deferred to `hw-encoder-mft-enumeration-fallback-deep`.
4. **`DeleteItem` verified available**: `windows = "0.62.2"` at line 15883 of `MediaFoundation/mod.rs` — used directly (no fallback).
5. **`INTERLACE_MODE` rejection risk (R-2)**: handled proactively — third retry (delete INTERLACE_MODE) implemented in B1-T4 as the C4 guard arm.
6. **Smoke evidence handoff**: B3-T2 transcript MUST be attached to the verify-report artifact. R6.3 requires the terminal output showing 18/18 PASS.
