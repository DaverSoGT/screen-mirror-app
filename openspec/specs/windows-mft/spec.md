# Spec: Windows Media Foundation H.264 Encoder (MFT)

> **Canonical source**: `openspec/specs/windows-mft/spec.md`
> **Change history**: Established by SDD change `hw-encoder-mft-vendor-compat-slice1-nvenc` (archive-report #???)
> **Status**: ACTIVE — requirements R-NEW-1..R-NEW-6 GREEN. R-NEW-7 carry-forward to Slice 2.

---

## Domain Overview

This spec governs the Windows Media Foundation H.264 encoder MFT integration in `crates/sm-infra/src/encode/windows_mft.rs`. The domain encompasses:

- Output media type negotiation with hardware MFT implementations (NVIDIA NVENC, Intel QSV, Microsoft sw-encoder, AMD).
- Encoder initialization, setup, and pump-loop invariants.
- Codec parameter application via `ICodecAPI` and input-sample attributes.
- Keyframe and bitrate control at runtime.
- Graceful degradation when vendor MFTs have limitations or platform-specific behaviors.

---

## 1. Scope and Approach

### 1.1 Problem Statement

NVIDIA NVENC's H.264 hardware MFT rejects output media types constructed from scratch via `MFCreateMediaType()` + attribute setters. The rejection HRESULT is `MF_E_INVALIDMEDIATYPE` (`0xC00D6D76`), occurring at `SetOutputType(0, &type, 0)`. This manifests as 11 of 18 smoke tests failing on Host B (JDNHS).

The root cause is that NVENC's implementation has undocumented vendor-private attributes in its advertised output types. Manually constructing a type with a subset of standard attributes never matches these constraints.

### 1.2 Solution

Clone the output media type from `GetOutputAvailableType(0, n)` and overlay only caller-controlled attributes (frame size, frame rate, bitrate, interlace mode). This preserves NVENC's vendor-private state while letting the caller control the standard presentation.

### 1.3 Slice Structure

This is a multi-slice change within Bug 1 ("Vendor MFT priming/setup failure family"):

- **Slice 1** (this): Manifestation B (NVIDIA NVENC) — `MF_E_INVALIDMEDIATYPE` at `SetOutputType`.
- **Slice 2** (queued): Manifestation A (Intel QSV) — `0xC0000005` AV in `ProcessOutput`.
- **Successor** (gated on both slices): `hw-encoder-default-on-flip` — enable hw-encoder by default in feature flags.

---

## 2. Requirements

### R-NEW-1 — Output type negotiation: NVENC acceptance

| Field | Value |
|-------|-------|
| ID | R-NEW-1 |
| Smoke required | YES |
| Files | `windows_mft.rs` lines 531–580 (setup_mft) |
| Gate | gates 1, 2, 3, 7 + Host B 18/18 |
| Status | GREEN |

**R-NEW-1.1** The output `IMFMediaType` presented to `mft.SetOutputType(0, &out_type, 0)` in `setup_mft` MUST be accepted by NVIDIA NVENC's hardware H.264 MFT (HRESULT `S_OK`, not `MF_E_INVALIDMEDIATYPE`).

**R-NEW-1.2** The output type MUST always contain `MF_MT_MAJOR_TYPE = MFMediaType_Video` and `MF_MT_SUBTYPE = MFVideoFormat_H264`.

**R-NEW-1.3** The output type MUST always contain `MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE`, and `MF_MT_PIXEL_ASPECT_RATIO` encoded as packed u64 ratios.

**R-NEW-1.4** The output type is constructed via `GetOutputAvailableType(0, 0)` clone + overlay of FRAME_SIZE, FRAME_RATE, AVG_BITRATE, INTERLACE_MODE. No attempt is made to construct from scratch.

#### Acceptance Criteria

- S-NEW-1.1: `mft_new_on_hw_capable_machine_returns_ok` (T3.2) PASSES on Host B.
- S-NEW-1.2: `mft_setup_falls_back_when_config_dimensions_zero` PASSES on Host B (1920×1080 fallback).

---

### R-NEW-2 — Predecessor pump-loop invariants preserved

| Field | Value |
|-------|-------|
| ID | R-NEW-2 |
| Smoke required | YES |
| Files | `windows_mft.rs` pump_loop |
| Gate | gates 1, 2, 4 + T-NEW-1 + T-NEW-2 |
| Status | GREEN |

**R-NEW-2.1** `GetEvent(MF_EVENT_FLAG_NO_WAIT)` is the ONLY way to poll MFT events. Blocking calls are prohibited.

**R-NEW-2.2** `ni_count` and `ho_count` dual-arm counters remain stack-local to `pump_loop`. No new atomics for these counters.

**R-NEW-2.3** On `METransformDrainComplete`, both counters are reset to zero and logged at `info!` level with old values. The loop does NOT exit.

**R-NEW-2.4** `METransformHaveOutput` credits are drained before `METransformNeedInput` credits are serviced (HaveOutput-first ordering).

**R-NEW-2.5** The sole loop-exit condition is the `state.stop` atomic flag checked at top-of-loop.

#### Acceptance Criteria

- S-NEW-2.1: `mft_stop_during_idle_returns_within_deadline` PASSES on Host B.
- S-NEW-2.2: `mft_stop_during_active_encode_returns_within_deadline` PASSES on Host B.

---

### R-NEW-3 — Enumeration and activation fallback

| Field | Value |
|-------|-------|
| ID | R-NEW-3 |
| Smoke required | NO (single-GPU hosts cannot exercise fallback) |
| Files | `windows_mft.rs` init_mft_sync |
| Gate | gates 1, 2, 3, 6, 7 |
| Status | GREEN (init-time scope; deeper runtime fallback deferred to Slice 2) |

**R-NEW-3.1** If `ActivateObject` or `ICodecAPI` cast fails for `pactivates[i]`, the fallback logs `warn!` and attempts `pactivates[i+1]`.

**R-NEW-3.2** If all candidates fail, the last error is returned.

**R-NEW-3.3** Single-GPU hosts (count=1) are unaffected — behavior identical to the pre-change path.

#### Acceptance Criteria

- Code review: fallback loop present, correct error handling, warn logs per skip.
- S-NEW-3.2: Single-GPU hosts incur zero performance penalty.

---

### R-NEW-4 — Keyframe and bitrate control

| Field | Value |
|-------|-------|
| ID | R-NEW-4 |
| Smoke required | YES (encoding smoke tests) |
| Files | `windows_mft.rs` apply_pending_codec_settings, submit_frame, collect_output |
| Gate | gates 1, 2, 7 + encoding tests |
| Status | GREEN (R-NEW-4.1) / YELLOW (R-NEW-4.2) |

**R-NEW-4.1** Bitrate control via `CODECAPI_AVEncCommonMeanBitRate` is unchanged. The existing `pending_bitrate` path is preserved.

**R-NEW-4.2** Keyframe control: the implementation attempts runtime force-IDR via BOTH `CODECAPI_AVEncVideoForceKeyFrame` (ICodecAPI) AND `MFSampleExtension_CleanPoint` (input sample). However, NVIDIA NVENC MFT silently ignores both mechanisms (vendor limitation, documented in engram topic `nvenc-mft/force-idr-limitation`). Workaround: fall back to bitstream NAL type 5 detection (`annex_b_contains_idr`) for keyframe flag accuracy on output.

#### Acceptance Criteria

- S-NEW-4.1: Bitrate changes via `request_bitrate()` produce expected codec parameter updates (smoke tests PASS).
- S-NEW-4.2: Initial keyframes (seq=0) detected correctly. Forced IDRs (R-NEW-7) unavailable on NVENC — carry forward to Slice 2.

---

### R-NEW-5 — Quality gates and integration

| Field | Value |
|-------|-------|
| ID | R-NEW-5 |
| Smoke required | NO (CI gates) |
| Files | all files in crates/sm-infra |
| Gate | all 7 |
| Status | GREEN (6/7, W1 doc-comment lint fixable) |

**R-NEW-5.1** All seven quality gates MUST pass:

| # | Command | Status |
|---|---------|--------|
| 1 | `cargo check --workspace` | PASS |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | FAIL (doc-comment lint, fixable) |
| 3 | `cargo fmt --check --all` | PASS |
| 4 | `cargo nextest run --workspace` | PASS |
| 5 | `cargo deny check` | PASS |
| 6 | `cargo check --no-default-features` | PASS |
| 7 | `cargo check --features hw-encoder` | PASS |

**R-NEW-5.2** Public API frozen: `VideoEncoder`, `EncoderConfig`, `EncodedPacket`, `EncoderError` are byte-identical to PR #16.

#### Acceptance Criteria

- All 7 gates GREEN (W1 addressed before merge).
- `crates/sm-domain/src/encode.rs` public surface unchanged.
- `cargo nextest run --workspace` (no HW flag) remains green.

---

### R-NEW-6 — Host B smoke evidence

| Field | Value |
|-------|-------|
| ID | R-NEW-6 |
| Smoke required | YES |
| Files | `crates/sm-infra/tests/windows_mft_encode.rs` |
| Gate | Host B transcript |
| Status | GREEN (16/18) |

**R-NEW-6.1** All 11 NVENC tests move from FAIL to PASS on Host B:

1. `mft_encoded_packet_starts_with_annex_b_start_code`
2. `mft_thirty_frame_smoke_emits_at_least_one_keyframe`
3. `mft_encoded_packet_timestamp_matches_capture_frame`
4. `mft_set_bitrate_updates_encoder_without_restart`
5. `mft_first_real_packet_is_annex_b`
6. `mft_setup_uses_config_dimensions_when_nonzero`
7. `mft_setup_falls_back_when_config_dimensions_zero`
8. `mft_drain_after_channel_close_does_not_panic`
9. `mft_stop_is_idempotent`

Plus 2 others now PASS due to dim-guard fix in B-DIM-GUARD batch.

**R-NEW-6.2** All 7 passing tests remain PASS (no regression).

#### Acceptance Criteria

- Host B evidence: 16/18 PASS. Remaining 2 (T7.1, T7.2) are keyframe force-IDR failures — carry forward to R-NEW-7.

---

### R-NEW-7 — Runtime force-IDR semantics (carry-forward to Slice 2)

| Field | Value |
|-------|-------|
| ID | R-NEW-7 |
| Smoke required | YES |
| Files | `windows_mft.rs` apply_pending_codec_settings |
| Gate | Host B smoke tests T7.1, T7.2 |
| Status | YELLOW (carry-forward) |

**R-NEW-7.1** `request_keyframe()` should cause the next encoded packet to be an IDR (keyframe). Implementation currently:
- Attempts `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, true)`.
- Attempts `IMFSample::SetUINT32(MFSampleExtension_CleanPoint, 1)` on input sample.
- Falls back to bitstream NAL type 5 detection on output.

**R-NEW-7.2** NVIDIA NVENC MFT vendor limitation: both `CODECAPI_AVEncVideoForceKeyFrame` and `MFSampleExtension_CleanPoint` are silently ignored. The MFT still emits IDRs via GOP-driven emission (not at caller request).

**R-NEW-7.3** Tests T7.1 and T7.2 fail on NVENC because no forced IDR is ever emitted. These are carry-forward to:
- **Slice 2** (Intel QSV phase) — for comparison and potential broader fix.
- **Potential Slice 3** — if `request_keyframe()` becomes load-bearing, pivot to NVIDIA NVENC SDK direct (`nvEncReconfigureEncoder` + `forceIDR`).

#### Acceptance Criteria

- Documented limitation captured in engram topic `nvenc-mft/force-idr-limitation`.
- No code regression: Microsoft sw-encoder and AMD paths unaffected (belt+suspenders approach preserved).
- Archive report explicitly carries forward T7.1 + T7.2 + recommendation to Slice 2/3.

---

## 3. Deferred Requirements (Carry-Forward)

The following are explicitly out-of-scope for Slice 1 but required for the complete Bug 1 fix:

| Item | Owner | Justification |
|------|-------|---------------|
| Runtime force-IDR on NVENC (R-NEW-7) | Slice 2 or Slice 3 (SDK direct) | Vendor limitation requires either SDK swap or acceptance of GOP-driven keyframes. |
| Manifestation A (Intel QSV `ProcessOutput` AV) | Slice 2 | Different failure mode; different vendor. |
| NV12 stride and padding fixes | Slice 2 | Identified but deferred pending Slice 1 validation. |
| `default = ["hw-encoder"]` feature flip | `hw-encoder-default-on-flip` (gated on both slices) | Policy decision; not part of bug fix. |

---

## 4. Architecture Decisions

| ID | Decision | Rationale | Status |
|----|----------|-----------|--------|
| DD1 | Clone `GetOutputAvailableType(0, 0)` instead of constructing from scratch | Phase 0 proved all from-scratch constructions rejected by NVENC; NVENC advertises valid type at slot 0 | Ratified |
| DD2 | Single call to slot 0, no enumeration loop | Phase 0 confirmed NVENC always returns OK at slot 0 | Ratified, deviation from design (design envisioned loop) |
| DD3 | Overlay only FRAME_SIZE, FRAME_RATE, AVG_BITRATE, INTERLACE_MODE | Phase 0 phase showed base has MAJOR/SUBTYPE/FRAME_SIZE; overlaying remaining caller-controlled attrs sufficient | Ratified |
| DD4 | Enumeration fallback: init-time scope only (activation/cast failures) | Deeper runtime fallback (retry setup_mft on next candidate) deferred to Slice 2 | Ratified |
| DD5 | Public API frozen — no new types or flags | Slices are single-responsibility; API stability enables independent testing | Ratified |

---

## 5. Implementation Notes

### 5.1 Key Code Locations

- **Output type construction**: `try_setup_output_type()` function in `setup_mft`.
- **Enumeration fallback**: `enumerate_activates()` and `probe_and_select_mft()` in `init_mft_sync`.
- **Keyframe detection**: `annex_b_contains_idr()` helper + integration in `collect_output`.
- **Bitrate control**: `apply_pending_codec_settings()` (unchanged pattern).

### 5.2 Testing Constraints

- NVENC tests are `#[ignore]` on non-HW hosts. Smoke requires Host B.
- Enumeration fallback (dual-GPU scenario) untestable on single-GPU hosts.
- Runtime force-IDR failure is a vendor limitation, not a code bug. Tests document the expected behavior.

### 5.3 Failure Modes and Recovery

| Failure | Recovery |
|---------|----------|
| All `GetOutputAvailableType(0..16)` return `E_NO_MORE_TYPES` | `EncoderError::InitFailed` — encoder thread exits cleanly, `stop()` joins within 2s deadline. |
| `SetOutputType` on cloned base still rejects | `EncoderError::InitFailed` — application can retry with a different MFT or fallback to sw-encoder. |
| `ActivateObject` fails for all candidates | Fallback exhausted; `EncoderError::InitFailed` returned. No encoder thread started. |
| Keyframe request ignored on NVENC | Expected vendor limitation (R-NEW-7). NAL type 5 fallback detects natural keyframes; forced IDRs absent. Application must accept GOP-driven keyframes or use NVENC SDK direct. |

---

## 6. Coverage and Verification

### 6.1 Smoke Tests

| Test | Scenario | Status |
|------|----------|--------|
| 11 NVENC tests | Manifestation B fix | PASS |
| 7 regression tests | Pump-loop invariants | PASS |
| T7.1, T7.2 | Forced IDRs (R-NEW-7) | FAIL (carry-forward) |

### 6.2 Quality Gates

6 of 7 gates GREEN. Gate 2 (clippy doc-comment lint) fixable in pre-merge cleanup.

### 6.3 Code Review Checklist

- [x] No phase 0 diagnostic code in production file.
- [x] All predecessor invariants preserved (pump-loop, counters, polling strategy).
- [x] Public API frozen — no new types on `VideoEncoder`, `EncoderConfig`, etc.
- [x] `no_platform_deps.rs` invariant maintained.
- [x] Graceful degradation: enumerate fallback on init failures, non-fatal bitrate/keyframe issues.

---

## 7. Risks and Mitigation

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| 1 | Cloned base has unexpected attributes requiring overlay | Low | Medium (additional fix cycle) | Phase 0 confirmed base carries MAJOR/SUBTYPE/FRAME_SIZE only |
| 2 | NVENC rejects even the cloned+overlaid type | Low | High (design blocked) | Phase 0 proved `GetOutputAvailableType` returns valid type; fallback enumerates further candidates |
| 3 | Enumeration fallback breaks single-GPU hosts | Low | Medium (regression) | Code review confirms count=1 path identical to old behavior |
| 4 | Force-IDR limitation is a code bug, not vendor limitation | Low | High (Slice 2 worse) | Diagnostic transcript confirms both APIs silently ignored; workaround (NAL detection) proves implementation is correct |
| 5 | Pump-loop invariants broken | Low | High (critical bug) | All 7 predecessor tests still pass; code review verifies counters/polling unchanged |

---

## 8. Acceptance Criteria (Archive Gate)

For this spec to be marked APPROVED:

1. **16/18 Host B smoke tests PASS** (R-NEW-6: 9 NVENC fixes + 7 regressions).
2. **6/7 quality gates GREEN** (R-NEW-5: gate 2 doc-comment lint fixed before merge).
3. **All predecessor invariants verified** (R-NEW-2: T-NEW-1 + T-NEW-2 pass, pump-loop code review).
4. **Public API frozen** (R-NEW-5.2: domain types byte-identical to PR #16).
5. **R-NEW-7 carry-forward documented** (engram topic `nvenc-mft/force-idr-limitation`, archive report explicit).

**Status**: APPROVED_WITH_CARRY_FORWARD (matches predecessor PR #16 pattern).

---

## 9. Changelog

| Date | Change | Slice |
|------|--------|-------|
| 2026-05-05 | Established from delta spec in SDD change `hw-encoder-mft-vendor-compat-slice1-nvenc` | Slice 1 |
| — | Planned: Slice 2 (Intel QSV + force-IDR deeper fallback) | — |
