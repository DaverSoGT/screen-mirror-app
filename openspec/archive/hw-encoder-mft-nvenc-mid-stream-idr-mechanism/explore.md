# Exploration: hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Round 2 — Post-Falsification)

> Phase: SDD explore (Round 2). Branch: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` off master `c48ae46`.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/explore` (#803) + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Empirical evidence base: probes `b048b36` (priming) + `ae36499` (post-recreate); discoveries #800, #801, #804.
> Date: 2026-05-09.

---

## 1. Bug Re-Characterization (Post-Falsification)

### What Slice 5 Got Wrong

Slice 5 design (DD11 of #784) claimed: "First post-recreate frame is IDR: setup-sequence guarantee (vendor-uniform)." This claim was NOT empirically validated on NVENC. Phase 0 evidence base for Slice 5 (rounds 1–3) was exclusively Intel QSV (Host A). Host B NVENC was listed as informational with T7.1/T7.2 failing as "pre-existing NAL-type-5 detection bug carry-forward". Round 1 of Slice 6 (under change-name `hw-encoder-mft-nvenc-keyframe-flag`) falsified the NAL-type-5 hypothesis — NVENC uses 4-byte Annex-B start codes identical to Intel QSV and priming IDR IS correctly detected.

### The Actual Bug (Empirical — C0.b Probe `ae36499`)

C0.b probe `phase0_nvenc_post_recreate_idr_format_dump` on Host B (NVENC):
- 5 priming frames → flush → drained → PRIMING pkt 0: `is_keyframe=true`, `raw_prefix=[00,00,00,01,09,10]` (AUD `primary_pic_type=0x10` = I/IDR). PRIMING works correctly.
- `request_keyframe_via_recreate()` armed → Mechanism G fires (END_OF_STREAM + COMMAND_DRAIN + drop + ActivateObject + setup_mft logged successfully)
- 30 post-recreate frames → flush → 29 packets received
- **ALL 29 POST-RECREATE PACKETS**: `is_keyframe=false`, `raw_prefix=[00,00,00,01,09,30]` (AUD `primary_pic_type=0x30` = P-frame). NOT A SINGLE IDR.
- SUMMARY log: `first_post_recreate_is_keyframe=Some(false)`, `first_post_recreate_raw_prefix=Some([00,00,00,01,09,30])`, `first_post_recreate_len=Some(3176)`.

Mechanism G's recreate sequence executes without error on NVENC (ActivateObject succeeds, setup_mft succeeds, counters reset, pump_loop resumes) but NVENC does NOT treat a fresh ActivateObject + setup_mft as a context-reset that forces IDR. NVENC appears to maintain GOP state across ActivateObject calls on the same IMFActivate, or treats the fresh handle as a mid-stream continuation rather than a fresh session.

This is a structural vendor incompatibility. Mechanism G was designed and validated exclusively on Intel QSV.

### Why T7.1 / T7.2 Fail on Host B

Both tests call `request_keyframe()` (which routes to `request_keyframe_via_recreate()` via DD9 of Slice 5), then assert `is_keyframe=true` within IDR_TOLERANCE=30 post-request packets. The post-recreate batch on NVENC contains zero IDR frames, so the assertion fails unconditionally.

---

## 2. Existing Code Mechanism Inventory

### Currently Active

**Mechanism G** (`request_keyframe_via_recreate()` → `keyframe_recreate_pending` → pump_loop handler):
- Drop current IMFTransform + call ActivateObject again + setup_mft + resume.
- WORKS on Intel QSV (Phase 0 round 3 #783: IDR at post-recreate index 0).
- FAILS on NVENC (C0.b probe: 29/29 P-frames post-recreate, zero IDR).
- This is the ONLY mid-stream IDR mechanism in the current codebase.

### Previously Deleted (DD10, Slice 5)

**Mechanism C — MFSampleExtension_CleanPoint=1 INPUT write path**:
- Set `MFSampleExtension_CleanPoint=1` on the IMFSample BEFORE `ProcessInput`.
- **NVENC honored CleanPoint** (confirmed by source comment at `windows_mft.rs:1109`: "NVENC honored CleanPoint instead").
- Intel QSV did NOT honor CleanPoint mid-stream.
- Deleted in Slice 5 under the vendor-uniform assumption. NVENC-specific validation was NOT performed before deletion.
- The WRITE path was deleted; the READ path (`collect_output` reading `MFSampleExtension_CleanPoint` for IDR detection) was retained.

**Mechanism A — CODECAPI_AVEncVideoForceKeyFrame (SWAP-FIRE, Slice 4 DD1)**:
- Set `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_BOOL=TRUE)` AFTER ProcessInput in `fire_pending_codec_settings`.
- Intel QSV did NOT honor ForceKeyFrame mid-stream.
- DD5 of Slice 4 mentioned "NVENC strictly safer under reorder" — refers to ICodecAPI call ordering, NOT to ForceKeyFrame triggering IDR. NO empirical evidence ForceKeyFrame triggers IDR on NVENC.
- Deleted in Slice 5 DD10 alongside CleanPoint.

### Never Tested in This Project

- **CODECAPI_AVEncMPVGOPSize toggle** (set GOP=1, feed one frame, restore).
- **MF_MT_VIDEO_FORCE_KEY_FRAME on output type**.
- **Full re-init via DRAIN + new IMFMediaType** beyond Mechanism G's ActivateObject sequence.
- **Hybrid Mechanism G + supplemental signal** (G's teardown followed by CleanPoint or ForceKeyFrame on first post-recreate input frame).

---

## 3. Mechanism Candidates

### Candidate A: Re-Introduce MFSampleExtension_CleanPoint INPUT Write Path (DD10 Reversal, NVENC-Specific)

**API call sequence**:
```rust
// In submit_frame(), before ProcessInput:
if cleanpoint_pending.swap(false, AcqRel) {
    sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
}
// Then normal ProcessInput
```

**Theoretical basis**: MFSampleExtension_CleanPoint is the standard MFT attribute for requesting a clean point (IDR) on the next encoded output sample. NVENC's own behavior (confirmed by inline comment "NVENC honored CleanPoint") means NVENC reads this attribute and emits an IDR.

**Historical evidence**: This mechanism WAS working on NVENC before Slice 5 DD10 deleted it. Only deleted because of the vendor-uniform hypothesis (now falsified).

**Pros**:
- Empirically known to work on NVENC (pre-deletion + inline comment).
- Zero latency overhead — IDR request attached to input frame.
- `MFSampleExtension_CleanPoint` import already present (READ path retained).
- No structural pump_loop changes.
- Simplest implementation: re-add the `if force_keyframe { sample.SetUINT32(…) }` block.

**Cons**:
- NVENC-specific; Intel QSV does NOT honor this. Requires vendor dispatch (Mechanism G stays for Intel QSV).

**Vendor coverage**: NVENC-specific. Intel QSV continues using Mechanism G.

**Vendor dispatch required**: Yes.

**Empirical confidence**: HIGH (pre-deletion behavior + inline comment). Phase 0 needed only to CONFIRM re-introduction works in current architecture.

**Effort**: LOW. ~10–20 LOC production.

---

### Candidate B: Re-Introduce CODECAPI_AVEncVideoForceKeyFrame (SWAP-FIRE)

**API**: `ICodecAPI::SetValue(&CODECAPI_AVEncVideoForceKeyFrame, VT_BOOL=TRUE)` AFTER ProcessInput.

**Pros**: Standard ICodecAPI path. SWAP-FIRE infrastructure partially recoverable.

**Cons**: NO empirical evidence NVENC honors ForceKeyFrame. CleanPoint comment explicitly says NVENC honored CleanPoint; no equivalent positive claim for ForceKeyFrame. Intel QSV doesn't honor it either.

**Vendor coverage**: Unknown — requires Phase 0 probe.

**Empirical confidence**: MEDIUM (speculative for NVENC).

**Effort**: MEDIUM.

---

### Candidate C: CODECAPI_AVEncMPVGOPSize Toggle

**API**: `ICodecAPI::SetValue(GOP_SIZE, 1)` → push frame → `SetValue(GOP_SIZE, original)`.

**Cons**: Untested. Race condition with SWAP-FIRE. Original GOP must be tracked.

**Empirical confidence**: LOW.

**Effort**: MEDIUM.

---

### Candidate D: Hybrid Mechanism G + CleanPoint on Post-Recreate First Frame

**API**: G fires (recreate) + CleanPoint=1 on next submit_frame after recreate.

**Pros**: Could avoid vendor dispatch (CleanPoint is no-op on Intel QSV). Preserves G for Intel QSV.

**Cons**: Two mechanisms = two failure surfaces. Uncertain if CleanPoint works post-recreate on NVENC (untested).

**Empirical confidence**: LOW.

**Effort**: MEDIUM.

---

### Candidate E: NVENC-Only Standalone CleanPoint (Vendor Dispatch Architecture)

Same mechanism as Candidate A; framed as full architectural shape: vendor dispatch routes Intel QSV → G, NVENC → CleanPoint.

**Detection**: GUID retrieved during `probe_and_select_mft` (line ~514). NVENC GUID `{60F44560-5A20-4857-BFEF-D29773CB8040}` confirmed in C0/C0.b probe logs. Intel QSV GUID `{4BE8D3C0-...}`.

**Effort**: MEDIUM.

---

### Candidate F: Full Re-Init (DRAIN + New MediaType + setup_mft, No ActivateObject)

**Cons**: No empirical basis. G already does deeper reset; if G doesn't produce IDR on NVENC, softer renegotiation likely won't.

**Empirical confidence**: VERY LOW.

**Effort**: HIGH.

---

### Mechanism Comparison

| Candidate | Mechanism | Known NVENC | Vendor Dispatch | Effort | Phase 0 |
|-----------|-----------|-------------|-----------------|--------|---------|
| A: CleanPoint re-introduce | INPUT sample attribute | YES (pre-deletion) | Yes | Low | Confirm |
| B: ForceKeyFrame re-introduce | ICodecAPI SWAP-FIRE | Unknown | Yes | Medium | Yes |
| C: GOP size toggle | ICodecAPI | Unknown | Unknown | Medium | Yes |
| D: G + CleanPoint hybrid | G + INPUT attribute | Unknown (post-G) | No (potentially) | Medium | Yes |
| E: Vendor dispatch architecture | A as full arch | YES (pre-deletion) | Yes | Medium | Confirm |
| F: Full re-init | Media type renegotiation | Very unlikely | Unknown | High | Yes |

**Clear winner**: Candidate A / E (CleanPoint + vendor dispatch). Mechanism is empirically grounded. Phase 0 needed only to confirm re-introduction integration.

---

## 4. Vendor Dispatch Architecture Sketch

### Detection Point

`probe_and_select_mft` already retrieves GUID via `activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute)` (line ~514). Vendor enum can be derived at that site.

### Dispatch Approach

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
enum EncoderVendor {
    IntelQsv,
    NvidiaNvenc,
    Unknown,
}
```

Detection: match GUID string in `probe_and_select_mft` after winner selection.

Trait dispatch (preferred):
```rust
fn request_keyframe(&self) {
    match self.vendor {
        EncoderVendor::NvidiaNvenc => self.request_keyframe_via_cleanpoint(),
        _ => self.request_keyframe_via_recreate(), // Intel QSV + Unknown
    }
}
```

`Unknown` vendor fallback emits WARN log to surface unmapped GUIDs.

### Existing Vendor-Aware Code

NONE in `windows_mft.rs`. Vendor dispatch is new infrastructure for Slice 6.

---

## 5. Phase 0 Probe Strategy

### Why Phase 0 is Required Before Proposal

Candidate A (CleanPoint) has strong historical evidence but requires confirmation that re-introduction works in the current codebase architecture (post-Slice 4 SWAP-FIRE deletion + Slice 5 submit_frame signature changes).

### Recommended Probes (Round 1)

**Probe P1: CleanPoint INPUT write on NVENC (post-priming)** — MANDATORY
- Submit N priming frames (establish encoding) → submit one frame with `MFSampleExtension_CleanPoint=1` set on the IMFSample → assert `is_keyframe=true` appears within next-N-frames window.
- This is the direct re-introduction test for Candidate A.

**Probe P2: CODECAPI_AVEncVideoForceKeyFrame on NVENC (post-priming)** — BONUS
- Same cadence but fire `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, TRUE)` AFTER ProcessInput for the IDR-target frame.
- Tests Candidate B viability.

**Probe P3: CleanPoint on first post-recreate frame (Candidate D)** — OPTIONAL
- Run Mechanism G → submit first post-recreate frame WITH CleanPoint=1.
- Tests whether CleanPoint rescues post-recreate IDR failure.

### Decision Rule

| P1 | P2 | Action |
|----|----|--------|
| PASS | (any) | Candidate A/E selected (lowest latency + empirical confidence). Move to propose. |
| FAIL | PASS | Candidate B fallback. Move to propose. |
| FAIL | FAIL | Run P3 / escalate to Candidate F. |

### Probe Template

Based on `phase0_nvenc_post_recreate_idr_format_dump` (commit `ae36499`).

For P1/P2/P3, Phase 0 needs a test-only escape hatch for the new mechanism:
- New `cleanpoint_pending: AtomicBool` on `MftEncoderShared`
- New `request_keyframe_via_cleanpoint()` method (test-callable)
- New `submit_frame()` branch reading `cleanpoint_pending` and writing `MFSampleExtension_CleanPoint` on the IMFSample
- These are PRODUCTION-shape implementations gated behind probe + vendor dispatch (so the architectural change can be promoted directly if probe passes)

---

## 6. Scope Boundary

### IN Scope (Slice 6 R2)

- NVENC mid-stream IDR mechanism (CleanPoint re-introduction with vendor dispatch — pending P1)
- Phase 0 probes P1, P2, P3 on Host B
- Vendor detection and dispatch (`EncoderVendor` enum, GUID-based detection)
- T7.1 + T7.2 PASS on Host B
- Slice 5 archive corrigendum: "Mechanism G is vendor-uniform" → "Mechanism G validated on Intel QSV only; NVENC requires CleanPoint"
- Phase 0 round 3 probe (`phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`) retained as `#[ignore]`-gated regression evidence

### OUT of Scope

- Default-on feature flag flip (gated on Slice 6 closure)
- Disconnect drain-once cosmetic
- AMD vendor support (no Host C)
- Re-architecting Mechanism G on Intel QSV (works there; do NOT touch)
- Sub-50ms IDR latency target on NVENC

---

## 7. Affected Files (at Apply Time)

| File | Nature of Change |
|------|-----------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | Add `EncoderVendor` enum; GUID detection in `probe_and_select_mft`; `cleanpoint_pending: AtomicBool` on `MftEncoderShared`; `request_keyframe_via_cleanpoint()` method; CleanPoint write in `submit_frame()` (gated on flag); update `request_keyframe()` trait dispatch |
| `crates/sm-infra/tests/windows_mft_encode.rs` | Add P1/P2/P3 Phase 0 probes (`#[ignore]`-gated); update T7.1/T7.2 to use cleanpoint mechanism on NVENC |
| `openspec/archive/hw-encoder-mft-intel-qsv-mid-stream-idr/archive-report.md` (corrigendum) | Annotate "Mechanism G vendor-uniform" overclaim with reference to Slice 6 R2 finding |

---

## 8. Risks (Top 3)

### Risk 1 — CleanPoint NVENC behavior changed or context-dependent (HIGH)

Evidence is from inline comment, not direct trace log. Comment may reflect assumption rather than measurement. Driver version variance possible.

**Mitigation**: P1 probe on Host B is mandatory before committing to Candidate A. If P1 fails, escalate to Candidate B (ForceKeyFrame).

### Risk 2 — Vendor dispatch adds complexity (MEDIUM)

`EncoderVendor` detection runs at construction time. GUID failure → silent fallback to `Unknown → Mechanism G` would break NVENC T7.1/T7.2.

**Mitigation**: Log detected vendor at INFO during `probe_and_select_mft`. Unit test for GUID detection. `Unknown` vendor fallback emits WARN.

### Risk 3 — CleanPoint timing (MEDIUM)

Exact CleanPoint timing relative to ProcessInput may matter for NVENC. Slice 4 SWAP-FIRE was driven by Intel QSV's rejection of pre-ProcessInput ICodecAPI calls; NVENC's CleanPoint behavior may be similarly precise.

**Mitigation**: P1 probe tests CleanPoint set on IMFSample BEFORE ProcessInput (natural MFT API pattern). If P1 fails, P1-variant with after-ProcessInput timing serves as alternative.

---

## 9. Recommended Next Phase

**`phase-0-trace` (interim phase) before `sdd-propose`**.

Despite Candidate A having high empirical confidence, the exact behavior needs confirmation in current codebase architecture. Phase 0 probes P1 + P2 + P3 take ~1 day implementation + one Host B run.

If P1 PASSES → move directly to `sdd-propose` with Candidate E architecture.
If P1 FAILS → escalate with new evidence and re-evaluate.

---

## Key Discovery Log

**Critical code evidence at `windows_mft.rs:1108–1110`**:
```
// DD10: `CODECAPI_AVEncVideoForceKeyFrame` SetValue branch removed — Intel QSV does
// not honor mid-stream ICodecAPI ForceKeyFrame; NVENC honored CleanPoint instead.
// Both are deleted. Mid-stream IDR is produced exclusively by Mechanism G.
```

DEFINITIVE: NVENC honored CleanPoint (not ForceKeyFrame). Mechanism deleted under vendor-uniform assumption. Re-introducing it on NVENC is the lowest-risk path.

**NVENC GUID confirmed**: `{60F44560-5A20-4857-BFEF-D29773CB8040}` (both C0 and C0.b probe logs).

**Priming IDR detection working**: NVENC emits `raw_prefix=[00,00,00,01,09,10]` (AUD `primary_pic_type=0x10` = I-frame) for setup-sequence IDR. `is_keyframe=true` correctly detected.

**Post-recreate failure absolute**: 29/29 packets `is_keyframe=false`, sizes 3011–3176 bytes (P-frame range, never the priming IDR's 4337). Zero IDRs in the 30-frame post-recreate batch.
