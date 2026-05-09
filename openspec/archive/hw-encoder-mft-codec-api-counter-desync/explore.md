# Exploration: hw-encoder-mft-codec-api-counter-desync (Slice 4)

> Artifact store: hybrid — engram (this observation) + openspec/changes/hw-encoder-mft-codec-api-counter-desync/explore.md
> Date: 2026-05-09
> Master tip: 9eee001 / e0f8232 (post-Slice-3 cleanup)

---

## Executive Summary

`apply_pending_codec_settings()` is called unconditionally at the TOP of the NeedInput servicing loop (line 1221) — BEFORE any `ProcessInput` call. Intel QSV transiently enters a non-accepting state after an `ICodecAPI::SetValue` call while a NeedInput credit is outstanding, causing the subsequent `ProcessInput` to return `MF_E_NOTACCEPTING` (0xC00D36B5) even though `ni_count > 0`. This hits the `debug_assert!(false)` panic at line 1266 and immediately exits the pump loop. NVENC tolerates the same ICodecAPI call sequence without entering non-accepting state (T8 PASSES on NVENC, confirmed Host B smoke #721). Recommended fix is **Approach B** (reorder): move `apply_pending_codec_settings()` to AFTER a successful `ProcessInput`, combined with a Phase 0 probe to confirm the exact trigger sequence empirically before design lock.

---

## Current State

### pump_loop structure (relevant excerpt, ~lines 1219–1280)

```
while ni_count > 0 {
    // ← ICodecAPI calls happen HERE (top of loop, before ProcessInput)
    let force_keyframe = apply_pending_codec_settings(codec_api, state);

    match rx.recv_timeout(FRAME_RECV_TIMEOUT) {
        Ok(frame) => {
            // ... NV12 convert ...
            match submit_frame(mft, &nv12_scratch, ..., force_keyframe) {
                Ok(()) => { ni_count -= 1; }  // line ~1259
                Err(e) => {
                    if reason.contains("ProcessInput: 0xC00D36B5") {
                        // MF_E_NOTACCEPTING
                        debug_assert!(false, "...counter logic wrong");  // line 1266
                        tracing::error!(...);
                        return;  // kills the pump thread
                    }
                    // other errors: skip frame, consume credit
                    ni_count -= 1;
                }
            }
        }
        Err(RecvTimeoutError::Timeout) => { break; }
        Err(RecvTimeoutError::Disconnected) => { COMMAND_DRAIN; break; }
    }
}
```

### apply_pending_codec_settings() (lines 1018–1043)

Two ICodecAPI operations, each conditional on an atomic flag:
1. `keyframe_pending` (AtomicBool): if set, calls `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_BOOL=true)`. Clears the flag via swap.
2. `pending_bitrate` (AtomicU32): if non-zero, calls `ICodecAPI::SetValue(CODECAPI_AVEncCommonMeanBitRate, VT_UI4=new_bps)`. Clears the field via swap.

Returns `force_keyframe: bool` (the value of `keyframe_pending` before clearing).

NOTE: keyframe path uses `ICodecAPI::SetValue(ForceKeyFrame)` — NOT `MFSampleExtension_CleanPoint`. The CleanPoint attribute is set on the IMFSample itself inside `submit_frame()` when `force_keyframe=true`. So keyframe signaling uses BOTH paths: ICodecAPI (for drivers that honor it) AND CleanPoint on the sample.

### MftEncoderShared atomics

- `keyframe_pending: AtomicBool` — set by `request_keyframe()` (caller thread), consumed by `apply_pending_codec_settings()` (pump thread) via swap(false, AcqRel)
- `pending_bitrate: AtomicU32` — set by `set_bitrate()` (caller thread, 0 = no-op sentinel), consumed via swap(0, AcqRel)

### The debug_assert invariant

`debug_assert!(false, "MF_E_NOTACCEPTING on serviced NeedInput credit — counter logic wrong")`

This encodes the invariant: "if ni_count > 0 and we call ProcessInput, the MFT MUST accept the frame (because we only call ProcessInput when the MFT itself said it was ready via NeedInput event)." The assert is CORRECT for the steady state. The desync is introduced by the ICodecAPI call intervening between the NeedInput event and the ProcessInput call — a window Intel QSV uses to transiently withdraw its readiness.

---

## Failing Tests (T6, T7, T8)

All three are currently in MASTER BODY (timeout failure mode, not panic) because the codec_api desync was discovered during Slice 3 T6/T7/T8 restructure attempts (commit b7cdb6f reverted them).

### T7.1 — `mft_request_keyframe_marks_next_packet_as_keyframe` (lines 307–394)

Cadence:
1. send_frame(0) → recv_pkt() → assert IDR (initial IDR)
2. send_frame(1..=3) → recv_pkt() × 3 (P-frames, priming)
3. `enc.request_keyframe()` ← sets keyframe_pending=true
4. send_frame(4)
5. recv_pkt() → assert is_keyframe=true + Annex-B SPS (NAL type 7)

Desync trigger: at step 3, keyframe_pending=true is set. In the next pump_loop NeedInput iteration, apply_pending_codec_settings() fires ICodecAPI::SetValue(ForceKeyFrame) before ProcessInput(frame 4). Intel QSV enters non-accepting state → ProcessInput returns MF_E_NOTACCEPTING → debug_assert fires → pump thread exits → recv_pkt() times out.

### T7.2 — `mft_keyframe_flag_cleared_after_idr_emitted` (lines 396–457)

Same root cause as T7.1 but checks that the flag is cleared after forced IDR (next packet is P-frame). Cadence: send 0 (IDR), 1–2 (P-frames), request_keyframe(), send 3 → assert forced IDR, send 4 → assert P-frame (flag cleared).

### T8.2 — `mft_set_bitrate_updates_encoder_without_restart` (lines 461–520)

Cadence:
1. Encode 3 frames at 4 Mbps (frames 0–2, recv each pkt)
2. `enc.set_bitrate(8_000_000)` ← sets pending_bitrate=8_000_000
3. Encode 3 more frames (frames 3–5), recv each pkt

Desync trigger: at step 2, pending_bitrate is set. In the next NeedInput iteration, apply_pending_codec_settings() fires ICodecAPI::SetValue(MeanBitRate) before ProcessInput(frame 3). Intel QSV enters non-accepting state → MF_E_NOTACCEPTING → pump exits → recv(frame 3) times out.

T8.2 PASSES on NVENC (Host B smoke #721) — NVENC does not withdraw NeedInput readiness after ICodecAPI bitrate change. This is the vendor-specificity confirmation.

Note: T6.1 (`mft_first_real_packet_is_annex_b`) is a DIFFERENT test — it is a single-frame Annex-B detection test now PASSING on both hosts (recovered by flush() in Slice 3). It is NOT a codec_api desync test. T7 and T8 carry-forward are the two codec_api test clusters.

---

## Vendor Detection: Current State

The codebase does NOT have an explicit is_intel_qsv flag in MftEncoderShared or passed to pump_loop. Vendor identity is logged at probe time (`friendly_name` and `clsid_str` from `MFT_FRIENDLY_NAME_Attribute` and `MFT_TRANSFORM_CLSID_Attribute`) but this information is not stored for runtime use.

Intel QSV H.264 MFT CLSID: `{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}` (from task context). This CLSID is present in `probe_and_select_mft` logs but not persisted to the encoder struct.

To add vendor-conditional logic, either:
(a) Store the CLSID/friendly_name string in `WindowsMftH264Encoder` during `probe_and_select_mft`
(b) Read the CLSID from the `IMFActivate` on the encoder thread (same activate, just re-query)
(c) Use a boolean flag `is_intel_qsv: bool` derived from CLSID comparison

---

## Approach Comparison

| # | Approach | Mechanism | Pros | Cons | Effort | Verdict |
|---|----------|-----------|------|------|--------|---------|
| A | Retry on NOTACCEPTING | catch MF_E_NOTACCEPTING in ProcessInput path, sleep briefly, retry up to N times | Minimal code change; preserves current call order; debug_assert stays meaningful | Masks real counter bugs; retry count and sleep duration are guesses; `return` path currently exits pump — changing it to retry changes error semantics; slippery slope | Low-Med | CANDIDATE (Phase 0 must show retry actually recovers, not loops indefinitely) |
| B | Reorder codec_api AFTER ProcessInput | move apply_pending_codec_settings() call to AFTER ni_count -= 1 (successful ProcessInput) | Eliminates the root cause; never calls ICodecAPI while MFT is still "deciding" to accept; NVENC unaffected (was already tolerant); debug_assert semantics preserved | Keyframe flag is applied to frame N+1 instead of frame N — slight semantic shift (request_keyframe() takes effect one frame later); bitrate update applies one frame later | Low | **RECOMMENDED** |
| C | Relax debug_assert | change debug_assert!(false) to tracing::warn! + retry counter; return only after N failures | Keeps the ICodecAPI call order; assert becomes observable without killing thread | Does NOT fix the underlying desync — Intel QSV still won't accept the frame; without retry logic, the thread still returns; must be combined with A | Low | PARTIAL — combine with A |
| D | Vendor-conditional | gate ICodecAPI call on is_intel_qsv flag (CLSID comparison during probe) | Surgical; leaves NVENC path unchanged; can implement a different strategy for Intel | Adds vendor detection infrastructure (store CLSID or bool in struct, pass to pump_loop or check in MftEncoderShared); fragile to Intel driver updates that change behavior across driver versions; anti-pattern per Slice 3 explore recommendation | Med | CANDIDATE if B has semantic problems |
| E | Drain-before-codec-api | fire COMMAND_DRAIN, wait for DrainComplete, THEN apply ICodecAPI, then restart stream | Guarantees MFT is idle when ICodecAPI is called | COMMAND_DRAIN is terminal on Intel QSV (Slice 3 empirical evidence, flush() doc comment) — cannot use mid-stream; would break continuity of encoding session; non-starter for T7/T8 which encode before and after the change | High | REJECTED — DRAIN is terminal |
| F | Defer codec_api to post-ProcessOutput | call apply_pending_codec_settings() in HaveOutput service loop (not NeedInput) | Ensures MFT has produced output (stable state) before ICodecAPI | Semantic mismatch: keyframe request arrives with a frame but would only take effect AFTER the output of that frame is drained; could miss the target frame entirely; complex timing | Med | RISKY — wrong semantic frame |

### Recommendation: Approach B (Reorder)

Move `apply_pending_codec_settings()` from the TOP of the NeedInput `while ni_count > 0` loop to AFTER `ni_count -= 1` (successful `ProcessInput`). The ICodecAPI call will now execute while the MFT is processing the frame it just accepted — not while it is being asked to accept.

```rust
// BEFORE (current — desync-prone):
while ni_count > 0 {
    let force_keyframe = apply_pending_codec_settings(codec_api, state);
    // ... recv frame ...
    match submit_frame(..., force_keyframe) {
        Ok(()) => { ni_count -= 1; }
        ...
    }
}

// AFTER (proposed — reordered):
while ni_count > 0 {
    let force_keyframe = state.keyframe_pending.load(Ordering::Acquire);
    // ... recv frame ...
    match submit_frame(..., force_keyframe) {
        Ok(()) => {
            ni_count -= 1;
            // Apply ICodecAPI AFTER successful ProcessInput — MFT is now processing, stable
            apply_pending_codec_settings(codec_api, state);
        }
        ...
    }
}
```

Wait — this requires a small API adjustment: `force_keyframe` must be read before the frame is submitted (to set CleanPoint on the sample), but the ICodecAPI `ForceKeyFrame` call can happen after. So the split is:
- READ `keyframe_pending` and `pending_bitrate` atomics BEFORE ProcessInput (to determine CleanPoint)
- CALL ICodecAPI AFTER successful ProcessInput

This means splitting `apply_pending_codec_settings` into:
1. `peek_codec_settings(state) -> (bool, u32)` — reads atomics, returns (force_keyframe, new_bps), does NOT clear flags
2. `commit_codec_settings(codec_api, state, force_keyframe, new_bps)` — calls ICodecAPI, clears flags

OR more simply:
- Keep force_keyframe determination inline (just read + swap the AtomicBool before ProcessInput)
- Call the full `apply_pending_codec_settings()` after ProcessInput (slightly redundant ForceKeyFrame call order, but harmless since CleanPoint on the sample is the authoritative keyframe signal)

The simplest approach: extract the `force_keyframe` bool BEFORE submit_frame (from keyframe_pending swap), submit the frame with CleanPoint attribute, THEN call apply_pending_codec_settings() to do the ICodecAPI calls after the frame is in-flight.

**Semantic impact of B**: ICodecAPI ForceKeyFrame fires AFTER the frame is submitted. Intel QSV already has the frame with CleanPoint=1 attribute. The ICodecAPI ForceKeyFrame is thus redundant for the current frame (CleanPoint handles it) but Intel QSV still processes it for the NEXT frame. Net effect: the ICodecAPI call arrives slightly later but the CleanPoint sample attribute is the true IDR trigger. This is acceptable semantics.

**Bitrate impact**: `set_bitrate()` change takes effect on the NEXT frame after successful ProcessInput of the current frame. One-frame latency for bitrate change. This is acceptable and consistent with the async nature of rate control in HW encoders.

---

## Phase 0 Trace Plan (MANDATORY before design lock)

Per `tracing-before-explore` convention #592 (same discipline as Slice 3 Phase 0 #710).

### Probe P0-A: Confirm MF_E_NOTACCEPTING trigger sequence

```rust
/// P0-A: reproduce codec_api counter desync on Intel QSV
/// Hypothesis: ICodecAPI::SetValue(bitrate) BEFORE ProcessInput, with ni_count > 0,
/// causes MF_E_NOTACCEPTING on subsequent ProcessInput.
#[test]
#[ignore = "Phase 0 trace — Intel QSV only; run manually with RUST_LOG=sm_infra::encode=trace"]
fn phase0_codec_api_before_processinput_triggers_notaccepting() {
    // 1. Start encoder at 640x480
    // 2. Send 3 frames, recv 3 pkts (prime the pipeline)
    // 3. Capture pump loop log output — observe ICodecAPI interleaving
    // 4. Call set_bitrate() between frames — observe MF_E_NOTACCEPTING vs normal
    // 5. Confirm panic/return path vs retry recovery
    //
    // Expected log sequence on Intel QSV (current code):
    //   pump_loop: counter snapshot ni_count=1 ...
    //   ICodecAPI::SetValue(bitrate) ...
    //   pump_loop: MF_E_NOTACCEPTING — counter desync (should be unreachable)
    //   pump_loop exited cleanly  ← thread died
    //
    // Expected log sequence with Approach B applied:
    //   pump_loop: counter snapshot ni_count=1 ...
    //   ProcessInput OK (frame submitted)
    //   ICodecAPI::SetValue(bitrate) ...
    //   pump_loop: counter snapshot ni_count=0 ...  ← clean
}
```

### Probe P0-B: Confirm reorder resolves desync

Apply Approach B locally (reorder apply_pending_codec_settings) and re-run the same scenario. Goal: confirm ProcessInput completes normally, ICodecAPI call does not interfere.

Both probes should be added as `#[ignore]`-gated tests retained as regression tests after the fix (same pattern as ea7994f in Slice 3 — Phase 0 probes as permanent `#[ignore]` guards).

### Decision tree

- P0-A confirms MF_E_NOTACCEPTING → P0-B confirms reorder fixes: proceed with Approach B
- P0-A does NOT reproduce (timeout instead of panic): re-examine — may be a timing/race condition; may need deeper tracing or synthetic frame cadence tuning
- P0-B still fails after reorder: escalate to Approach D (vendor-conditional) or A (retry)

---

## Open Questions (OQ list)

OQ-1 (BLOCKS design lock): Does Approach B (reorder) actually prevent MF_E_NOTACCEPTING on Intel QSV? Empirical confirmation via Phase 0 probe P0-B required before spec.

OQ-2: Is the one-frame keyframe semantic lag (ForceKeyFrame ICodecAPI fires after CleanPoint sample is submitted) visible to the caller? If keyframe_pending was set BETWEEN frames and the CleanPoint attribute correctly marks the IDR, the ICodecAPI call may be redundant entirely. Verify: can we DROP the ICodecAPI ForceKeyFrame call and rely on CleanPoint alone for all vendors? (If yes, this simplifies the fix significantly — only bitrate ICodecAPI remains in apply_pending_codec_settings.)

OQ-3: Does T7.1 require the forced keyframe to arrive as the FIFTH packet (frame index 4)? Under Approach B, if ICodecAPI ForceKeyFrame fires after frame 4's ProcessInput, does Intel QSV still mark the frame 4 output as IDR (CleanPoint already set), or does it require the ICodecAPI ForceKeyFrame to arrive BEFORE the sample? Phase 0 P0-B should capture this.

OQ-4: For the keyframe_pending restoration code paths (timeout + frame-dim-mismatch guard at lines 1244, 1287, 1295): under Approach B (read-before, commit-after), if the frame is DROPPED (dim mismatch) or TIMEOUT happens after we've already swapped keyframe_pending=false, we must restore it. Does splitting the read from the ICodecAPI call require changes to these restoration paths? (Likely yes — need to read the bool before recv_timeout, restore if not submitted.)

OQ-5 (LOW): Does calling `ICodecAPI::SetValue(MeanBitRate)` on Intel QSV after ProcessInput (while the MFT is encoding) ever return an error? Current code logs warn and continues (non-fatal). Phase 0 P0-A trace would reveal this in the logs.

---

## Predecessor Patterns to Reuse

From Slice 2 (hw-encoder-mft-vendor-compat-rework #699):
- Inherent-method-vs-trait pattern: fixes that are vendor-specific stay as inherent methods or internal pump_loop logic — never exposed on VideoEncoder trait (sm-domain FROZEN).
- Single-PR for M-scope changes that fit under 400 LOC budget — this change forecasts ~50–80 LOC.

From Slice 3 (hw-encoder-mft-single-frame-flush #728):
- Retain Phase 0 probes as `#[ignore]`-gated regression tests (C0 commit before C1 RED).
- TDD sequence: C0 evidence → C1 RED → C2 GREEN → C3 polish → (optional C4 fallback).
- debug_assert!(false) is meaningful — changing it to warn + continue is non-trivial because the current return; below it kills the thread. Any retry approach must also fix the error path.
- flush() doc comment pattern: document single-shot semantics, vendor quirks, test-affordance-only.
- `#[allow]` over `#[expect]` for cfg-gated items (convention #580).

---

## Affected Files (forecast)

- `crates/sm-infra/src/encode/windows_mft.rs`
  - `apply_pending_codec_settings()` — reorder or split (B)
  - `pump_loop()` NeedInput inner loop — adjust call site
  - keyframe_pending restoration guards (lines 1244, 1287, 1295) — may need adjustment
- `crates/sm-infra/tests/windows_mft_encode.rs`
  - Add Phase 0 probe tests (P0-A, P0-B) as `#[ignore]`
  - Restore T7.1, T7.2, T8.2 to GREEN bodies (remove carry-forward comment, update test logic)
- NO changes to `crates/sm-domain/` (FROZEN)
- NO changes to `Cargo.toml` features

LOC forecast:
- Production: ~30–50 LOC changed (apply_pending_codec_settings refactor + call site adjustment + restoration guard updates)
- Phase 0 probes: ~60–80 LOC (2 probe tests)
- Test GREEN bodies (T7.1, T7.2, T8.2): ~20–30 LOC restoration
- Total: ~110–160 LOC — well under 400-line budget
- Single PR: YES (no chaining needed)

---

## Risks

| Sev | Likelihood | Risk | Mitigation |
|-----|------------|------|------------|
| HIGH | MED | Phase 0 P0-A does NOT reproduce panic (timeout instead) — may be a frame-timing race | Add explicit sleep between set_bitrate() and send_frame() in probe; tune synthetic frame cadence |
| HIGH | MED | Approach B's one-frame semantic lag for ForceKeyFrame causes T7.1 to fail (IDR arrives on frame 5 not 4) | Phase 0 P0-B captures this; fallback: drop ICodecAPI ForceKeyFrame entirely, rely on CleanPoint only (OQ-2) |
| MED | LOW | keyframe_pending restoration paths (3 sites) need changes under approach B that introduce a new race | Read + unconditional swap-before-recv_timeout; only restore if frame was not submitted |
| MED | LOW | Intel QSV behavior may vary by driver version — fix that works on Host A may not be universal | Regression probes retained as `#[ignore]` tests cover future regressions; document driver version |
| LOW | LOW | T6/T7/T8 master-body carry-forward comment becomes stale confusion after fix | Remove carry-forward comment in C1 RED commit when restoring test bodies |

**BLOCKS design lock**: OQ-1 (Phase 0 P0-A + P0-B empirical confirmation).
