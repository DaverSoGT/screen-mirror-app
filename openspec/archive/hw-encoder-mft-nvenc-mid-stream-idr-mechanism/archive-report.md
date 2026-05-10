# Archive Report — hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Slice 6 R2)

> **Slice**: Slice 6 R2 — Hardware Encoder Mid-Stream IDR Mechanism (AGGRESSIVE Replacement)
> **Branch lifecycle**: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` opened at `c48ae46`, merged via PR #22 at `966e5ee`, branch deleted
> **Date archived**: 2026-05-10
> **Artifact store**: hybrid (engram + openspec)
> **Status**: COMPLETED AND CLOSED — Ready for next slice (`hw-encoder-default-on-flip`)

---

## 1. Outcome Summary

Slice 6 R2 replaced Mechanism G (Slice 5 drop+recreate IMFTransform) + CleanPoint INPUT-write + `EncoderVendor` IDR dispatch with a single vendor-uniform mid-stream IDR mechanism: `CODECAPI_AVEncVideoForceKeyFrame` via `ICodecAPI::SetValue()` with `VARIANT { vt: VT_UI4, ulVal: 1 }` invoked BEFORE `ProcessInput`. Cross-vendor smoke validation: Intel QSV (Host A) 27/27 PASS in 178.245s; NVIDIA NVENC (Host B) 27/27 PASS in 162.223s. Workspace test suite: 611 passed, 19 skipped (hardware carry-forward), 0 failed in 12.298s. Clippy: 0 warnings (35.27s). Net production code delta: +50 ADD / −300 DELETE = **−250 LOC simplification**. Bug 1 (mid-stream IDR) CLOSED. Default-on feature flip unblocked.

---

## 2. Retroactive Corrections to Slice 5 (and 4) — THE CORRIGENDUM

This section is **mandatory** (closes AC-20) and is the reason Slice 6 R2 archive is substantively different from Slice 5 or other prior slices. Three architectural overclaims from prior slices were empirically falsified by Slice 6 R2 Phase 0 probes and are hereby corrected.

### Overclaim 1: Slice 4 — "Intel QSV does not honor mid-stream `CODECAPI_AVEncVideoForceKeyFrame`"

**Original claim**: Slice 4 (during exploratory SWAP-FIRE Mechanism A attempt) concluded that Intel QSV rejected mid-stream `CODECAPI_AVEncVideoForceKeyFrame` SetValue calls.

**Reality**: The claim was WRONG due to two implementation errors in Slice 4's test:
1. **Wrong variant type**: Slice 4 used `VT_BOOL=TRUE` instead of the HCK-mandated `VT_UI4=1`.
2. **Wrong timing**: Slice 4 called `SetValue` AFTER `ProcessInput` (SWAP-FIRE position) instead of BEFORE.

**Falsifying evidence**: 
- Engram **#809** (P2 Intel QSV probe, `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput`): Validates `VT_UI4=1` BEFORE `ProcessInput` on Intel QSV. Result: IDR at packet idx 1 (8.356 KB, ~33ms latency at 30fps), well within the 30-frame test tolerance.
- Engram **#808** (Chromium/FFmpeg/HCK research): Confirms `VT_UI4` + BEFORE timing as canonical Windows MFT keyframe mechanism; HCK Win8+ certification mandates `CODECAPI_AVEncVideoForceKeyFrame` with `VT_UI4`.

**Corrected understanding**: Intel QSV DOES honor mid-stream `CODECAPI_AVEncVideoForceKeyFrame` when invoked correctly (VT_UI4, BEFORE ProcessInput). The mechanism is vendor-uniform and is the canonical choice for Windows MFT mid-stream IDR across both Intel QSV and NVIDIA NVENC.

**Archive implication**: Slice 4's negative conclusion was wrong; future architects should not defer to it. The correctness evidence is now locked in Slice 6 R2 archive + Phase 0 probes (D13, R14).

---

### Overclaim 2: Slice 5 — "Mechanism G is vendor-uniform"

**Original claim**: Slice 5 (after successfully implementing Mechanism G on Intel QSV via drop+ActivateObject+setup_mft+resume) believed the mechanism was vendor-uniform and would work on both Intel QSV and NVIDIA NVENC.

**Reality**: The claim was WRONG on NVENC. Mechanism G executes cleanly on NVENC (no exception, no hung encoder) but yields 29/29 post-recreate packets with `is_keyframe=false` — zero IDR output despite successful handler execution.

**Falsifying evidence**:
- Engram **#801** (C0.b probe, `phase0_nvenc_post_recreate_idr_format_dump`): Commits the full Mechanism G handler path on NVENC, collects 30 post-request packets, and observes all 30 packets are P-frames with no IDR. The pump_loop sequence (END_OF_STREAM → COMMAND_DRAIN → NOTIFY_END_STREAMING → drop → ActivateObject → setup_mft → COMMAND_FLUSH → NOTIFY_BEGIN_STREAMING → NOTIFY_START_OF_STREAM → resume) produces no encoder state reset that causes IDR emission on NVENC.

**Corrected understanding**: Mechanism G is Intel-QSV-specific, NOT vendor-uniform. It does not work on NVIDIA NVENC in production drivers.

**Archive implication**: Slice 5 archive (#791) retains the historical record that Mechanism G was implemented and tested on Intel QSV successfully. This corrigendum documents that the "vendor-uniform" claim was wrong. Future Slice 7+ slices should NOT re-attempt vendor-uniform mechanisms without both Host A and Host B cross-vendor smoke pre-merge validation (see D14 gate).

---

### Overclaim 3: Slice 5 DD10 — "NVENC honored CleanPoint instead"

**Original claim**: Slice 5 inline comment at `windows_mft.rs:1108-1110` stated: "Intel QSV does not honor mid-stream ICodecAPI ForceKeyFrame; NVENC honored CleanPoint instead. Both are deleted. Mid-stream IDR is produced exclusively by Mechanism G."

**Reality**: The claim was WRONG. NVENC does NOT honor `MFSampleExtension_CleanPoint` written to input samples. Setting `CleanPoint=1` on an input sample to NVENC produces 30/30 post-request packets with `is_keyframe=false` — zero IDR output.

**Falsifying evidence**:
- Engram **#807** (P1 probe, `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute`): Implements CleanPoint INPUT-write mechanism (Candidate A), invokes it on NVENC, and observes 30/30 P-frame output with no IDR. The write succeeds (no exception) but is ignored by the encoder.

**Corrected understanding**: `MFSampleExtension_CleanPoint` is an output-side attribute (the encoder writes it to signal clean points to downstream consumers), NOT an input-side control for requesting IDRs. The Slice 5 DD10 comment conflated output-side semantics with an input-side request, leading to a failed architecture.

**Archive implication**: The DD10 comment in Slice 5 was the load-bearing misdirection that caused Slice 5 to delete `CODECAPI_AVEncVideoForceKeyFrame` infrastructure and pursue CleanPoint instead. This corrigendum definitively corrects it. Slice 5 archive (#791) is immutable; this Slice 6 R2 archive now serves as the authoritative record.

---

## 3. Carry-forward Deviations (Accepted by sdd-verify #815)

### Deviation W1: R11/R12 `#[ignore]` Annotation Retention

**Spec literal**: R11 and R12 required removing the `#[ignore]` annotation from T7.1 and T7.2 on both Host A and Host B variants.

**Actual state**: T7.1 (line 391) and T7.2 (line 536) retain `#[ignore = "Slice 6 R2 — requires hardware (Host A or Host B); run with --run-ignored"]`.

**Justification**: CI is headless (no GPU). Removing `#[ignore]` would cause T7.1 and T7.2 to execute in CI (post-merge), where they would attempt MFT activation and FAIL. Spec R17 (`cargo nextest run --workspace` MUST be GREEN) conflicts with literal R11/R12 if `#[ignore]` is removed on a headless CI runner.

The fix: both hosts ran the tests with `cargo nextest run --run-ignored only` during Phase D smoke validation. Cross-vendor results: Host A (Intel QSV) T7.1 11.374s PASS + T7.2 11.300s PASS; Host B (NVENC) T7.1 6.014s PASS + T7.2 6.015s PASS. Functional equivalence is proven.

**Resolution**: CARRY-FORWARD ACCEPTED. Documented as carry-forward in verify-report (#815, §4 "Carry-forward Register"). The `#[ignore]` annotation text was updated to reference Slice 6 R2 framing instead of Slice 5 Mechanism G context.

### Deviation W2: Spec R14 Probe Inventory Drift (2 Extra Carry-forward Probes)

**Spec literal**: R14 enumerated exactly 5 retained Phase 0 probes (P0, P0.b, P1, P2-NVENC, P2-Intel).

**Actual state**: `tests/windows_mft_encode.rs` contains 7 phase0_* function declarations:
- The 5 R14 probes (as specified)
- `phase0_codec_api_before_processinput_triggers_notaccepting` (line 1355)
- `phase0_codec_api_after_processinput_no_notaccepting_and_idr_on_frame_4` (line 1491)

**Justification**: The 2 extra probes predate Slice 6 R2. They are Slice 4/5 carry-forward regression evidence retained under the frozen-surface policy (spec §6: "Slice 3/4 Phase 0 probes (prior slices) MUST NOT be touched"). Both probes PASS on Host A and are not on the apply-phase delete list.

**Resolution**: CARRY-FORWARD ACCEPTED. Documented in verify-report (#815, §4). R14 wording could be amended in future archive-reports to read: "5 Slice 6 R2 Phase 0 probes PLUS Slice 4/5 carry-forward probes retained per §6 frozen-surface rule."

### Deviation TA.7: Design DD2 Over-scoped Deletion — `mft_activate_factory` Field Retained

**Design literal**: DD2 instructed deleting `mft_activate_factory: Option<IMFActivate>` field along with Mechanism G code deletion (R5).

**Actual state**: The field `mft_activate_factory: Option<IMFActivate>` was RETAINED in `MftEncoderShared` (line ~281).

**Root cause**: During apply Phase A (TA.7), an audit of the field's read sites revealed:
- `start()` at line ~363: `self.winning_activate = self.mft_activate_factory.take()` — required for initial `ActivateObject()` call during encoder thread initialization.
- `Drop` impl at line ~430: field release via COM interface release.

The field IS required for encoder lifecycle management and is **NOT** related to Mechanism G's recreate path. DD2 over-scoped the deletion.

**Resolution**: Field KEPT. Doc-comment updated (lines 406–408) to explain the field's purpose: "Retained for initial ActivateObject during encoder start; unrelated to deleted Mechanism G recreate path." Documented in apply-progress.md Phase A (TA.7 deviation note) and this archive-report.

---

## 4. Design Deviation Note

See TA.7 above. DD2 in design (#812) instructed deletion of `mft_activate_factory`, but apply-phase analysis revealed the field is load-bearing for encoder initialization (the ActivateObject call during `start()`, unrelated to Mechanism G). The field was retained with updated doc-comment. This is the ONLY design deviation; all other DDs (DD1–DD14) executed as locked.

---

## 5. Label Drift (Minor)

**Tasks TF.3** specified PR label `type:refactor`. 

**Actual state**: The label does not exist in the repo. Available labels: `bug`, `documentation`, `enhancement`, `ci`. None matched the intended semantics.

**Resolution**: PR #22 shipped without a label. Cosmetic; no functional impact. Document for completeness.

---

## 6. Final Test Evidence Summary

### Cross-Vendor Smoke (Phase D)

| Host | GPU | Total packets | Pass | Fail | Skip | Duration | Notes |
|------|-----|---------------|------|------|------|----------|-------|
| Host A | Intel QSV | 27 | 27 | 0 | 0 | 178.245s | T7.1/T7.2/T8.2 suite; no timeouts |
| Host B | NVIDIA NVENC | 27 | 27 | 0 | 0 | 162.223s | T7.1/T7.2/T8.2 suite; no timeouts |

**Defects discovered**: None. Zero ERROR, zero panic, zero assertion failure. Benign non-fatal WARNs: AMD MFT rejection on Host B (expected fallback behavior); probe-end NO_MORE_PACKETS on Host A (informative carry-forward). Both are historical and unrelated to Slice 6 R2 changes.

### Workspace Validation (2026-05-10, Phase E verify-report #815)

- **Clippy**: `cargo clippy --all-targets --all-features --locked -- -D warnings` → exit 0, **zero warnings** (35.27s)
- **Nextest**: `cargo nextest run --workspace` → **611 passed**, 19 skipped (HW carry-forward ignores), 0 failed (12.298s)
- **Production code footprint**: Zero changes to `crates/sm-domain/` (trait frozen); all changes in `crates/sm-infra/` (windows_mft encoder + integration tests)

### New CI-runnable Unit Tests (Slice 6 R2, Phase B)

Three new unit tests (no hardware required):

| Test | Scenario | Status | Evidence |
|------|----------|--------|----------|
| `force_keyframe_icodecapi_pending_defaults_to_false_on_construction` (S19) | Flag is `false` on `MftEncoderShared` construction | PASS | windows_mft.rs:2088–2098 |
| `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true` (S20) | Calling `request_keyframe()` sets flag to `true` | PASS | windows_mft.rs:2099–2113 |
| `force_keyframe_icodecapi_pending_swap_consumes_to_false` (S21) | `swap(false, AcqRel)` returns `true` and leaves flag `false` | PASS | windows_mft.rs:2114–2128 |

All 3 PASS in nextest without hardware.

### Phase 0 Probes (Regression Evidence, #[ignore]-gated)

All 5 retained Phase 0 probes compile and run cleanly with `--run-ignored only`:

| Probe | Engram ID | Falsification/Success | Notes |
|-------|-----------|----------------------|-------|
| `phase0_nvenc_idr_packet_format_dump` | #800 (P0) | Success — priming format confirmed | 4-byte Annex-B AUD `primary_pic_type=0x10` |
| `phase0_nvenc_post_recreate_idr_format_dump` | #801 (P0.b) | Mechanism G FALSIFIED on NVENC | 29/29 post-recreate packets are P-frames |
| `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute` | #807 (P1) | CleanPoint INPUT FALSIFIED on NVENC | 30/30 post-request packets are P-frames |
| `phase0_nvenc_force_keyframe_via_codecapi_before_processinput` | #809 (P2-NVENC) | ForceKeyFrame SUCCESS on NVENC | IDR at idx 0, len ~49KB, ~0ms latency |
| `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput` | #809 (P2-Intel) | ForceKeyFrame SUCCESS on Intel QSV | IDR at idx 1, len ~8KB, ~33ms latency (1-frame in-flight) |

---

## 7. Acceptance Criteria Final Status

| AC | Requirement | Status | Evidence |
|----|-------------|--------|----------|
| AC-1 | Host A (Intel QSV) T7.1 + T7.2 PASS | **VERIFIED** | Smoke: T7.1 11.374s, T7.2 11.300s — both PASS |
| AC-2 | Host B (NVENC) T7.1 + T7.2 PASS | **VERIFIED** | Smoke: T7.1 6.014s, T7.2 6.015s — both PASS |
| AC-3 | T8.2 (bitrate) PASS on BOTH hosts | **VERIFIED** | Host A 11.181s, Host B 5.959s — both PASS |
| AC-4 | `force_keyframe_icodecapi_pending` defaults to `false` | **VERIFIED** | windows_mft.rs:209; unit test default_false PASS |
| AC-5 | `request_keyframe()` arms flag; no vendor dispatch | **VERIFIED** | windows_mft.rs:380–384; `grep match.*vendor` in request_keyframe = 0 |
| AC-6 | pump_loop consumes with `swap(false, AcqRel)` BEFORE ProcessInput | **VERIFIED** | windows_mft.rs:1503–1505; swap BEFORE submit_frame (line 1531) |
| AC-7 | SetValue uses `VARIANT { vt: VT_UI4, ulVal: 1 }` | **VERIFIED** | windows_mft.rs:1507; helper at 1831–1842 sets vt=VT_UI4 |
| AC-8 | SetValue HRESULT failure is non-fatal | **VERIFIED** | windows_mft.rs:1511–1520; `tracing::warn!` on Err; proceed |
| AC-9 | Mechanism G code fully deleted | **VERIFIED** | grep `keyframe_recreate_pending\|request_keyframe_via_recreate` in src/ = 0 |
| AC-10 | CleanPoint write code fully deleted | **VERIFIED** | grep `cleanpoint_pending\|request_keyframe_via_cleanpoint` in src/ = 0 |
| AC-11 | CleanPoint READ in collect_output unchanged | **VERIFIED** | windows_mft.rs:1761; READ path preserved |
| AC-12 | DD10 comment replaced with P2 + research citation | **VERIFIED** | windows_mft.rs:1200–1211; cites #809, Chromium, FFmpeg, HCK |
| AC-13 | EncoderVendor retained for logging only | **VERIFIED** | enum 132–157; consumers are `info!`/`warn!` only |
| AC-14 | `request_keyframe()` doc documents latency contract | **VERIFIED** | windows_mft.rs:368–379 — NVENC idx 0 (~0ms), Intel idx 1 (~33ms) |
| AC-15 | 5 Phase 0 R2 probes retained; Slice 5 round-3 absent | **VERIFIED** | All 5 present; deleted probe grep = 0 |
| AC-16 | 3 CI-runnable unit tests PASS | **VERIFIED** | windows_mft.rs:2088–2128; all 3 PASS in nextest |
| AC-17 | clippy clean (0 warnings) | **VERIFIED** | `cargo clippy --all-targets --all-features --locked -- -D warnings` → exit 0 |
| AC-18 | nextest GREEN (no regressions) | **VERIFIED** | 611 passed, 19 skipped, 0 failed; no previously-PASS test FAILED |
| AC-19 | sm-domain diff vs c48ae46 = 0 lines | **VERIFIED** | `git diff --stat c48ae46..HEAD -- crates/sm-domain` empty |
| AC-20 | Slice 6 R2 archive corrigendum | **VERIFIED** | This section (§2 Retroactive Corrections) + W1/W2/TA.7 deviations |

**Overall**: 20/20 AC items VERIFIED. Status: **APPROVED_WITH_CARRY_FORWARD** (0 CRITICAL, 2 WARNING carry-forward documented, 1 design deviation documented).

---

## 8. Commits Register

The branch accumulated 13 commits (including the merge commit):

### Round 1 Lineage (Phase 0 experimental, later reset)
- `b048b36` — Phase 0 round 1 start
- `ae36499` — Phase 0 round 1 continuation
- `aa67af0` — Phase 0 round 1 continuation
- `f66c670` — Phase 0 round 1 continuation (soft-reset after; most content re-authored in R2 batches)

### Round 2 Batch 1 (Phase 0 + Slices 1–2)
- `6efd4c6` — `feat(infra): add EncoderVendor dispatch + cleanpoint_pending mechanism for NVENC mid-stream IDR (Slice 6 R2 candidate A)`
- `aee5750` — `test(infra): C0 P1 — phase0_nvenc_cleanpoint_idr_via_input_sample_attribute (#[ignore]-gated)`
- `beda9ed` — `feat(infra): add CODECAPI_AVEncVideoForceKeyFrame mechanism (Candidate B, before ProcessInput, VT_UI4)`
- `3bd5b95` — `test(infra): C0 P2 — cross-vendor force_keyframe_via_codecapi_before_processinput probes (NVENC + Intel QSV)`

### Round 2 Batch 2 (Phase A apply + Phase B unit tests)
- `efc0f36` — Commits at this point mark the start of Batch 2 apply work

### Round 2 Batch 3 (Phase A cleanup + Phase C documentation)
- `8aac4f6` — `refactor(infra): replace Mechanism G + CleanPoint with vendor-uniform ForceKeyFrame ICodecAPI mid-stream IDR (Slice 6 R2)` (Phase A cleanup commit, ~−250 LOC net)
- `735eb18` — `test(infra): add CI-runnable unit tests for ForceKeyFrame mechanism (Slice 6 R2)` (Phase B unit tests)
- `c4b59a9` — `docs(test): document retained Phase 0 probes for Slice 6 R2 (regression evidence)` (Phase C inventory)

### Pre-Merge Polish
- `fdd38d1` — `style(infra): apply cargo fmt to Slice 6 R2 sources (C3 POLISH)` (rustfmt issue fix)

### Merge Commit
- `966e5ee` — Merge commit on master (PR #22 merged; base `c48ae46` → `966e5ee`); branch deleted

---

## 9. Downstream Unblocks

### Bug 1: Mid-Stream IDR Mechanism — CLOSED

NVENC mid-stream IDR mechanism was the last open gate before flipping the hardware encoder feature default-on. Slice 6 R2 closes Bug 1 by establishing a vendor-uniform mid-stream IDR mechanism proven on both Intel QSV (Host A) and NVIDIA NVENC (Host B).

**Next slice readiness**: `hw-encoder-default-on-flip` was blocked on Bug 1. With Bug 1 closed and documented in this archive, the default-on flip is now **UNBLOCKED** and ready for planning.

---

## 10. Engram Chain Anchors (Observation IDs for Traceability)

The complete SDD artifact chain for this change:

| Phase | Artifact | Engram ID | Topic Key |
|-------|----------|-----------|-----------|
| Explore | Round 2 investigation | #803 | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/explore` |
| Phase 0 P0 | Priming format (NVENC) | #800 | (embedded in explore) |
| Phase 0 P0.b | Mechanism G falsified (NVENC) | #801 | (embedded in explore) |
| Phase 0 P1 | CleanPoint falsified (NVENC) | #807 | (embedded in explore) |
| Research | Chromium/FFmpeg/HCK precedent | #808 | (embedded in explore) |
| Phase 0 P2 | ForceKeyFrame vendor-uniform (breakthrough) | #809 | (embedded in explore) |
| **Proposal** | (This artifact) | **#810** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/proposal` |
| **Spec** | (This artifact) | **#811** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/spec` |
| **Design** | (This artifact) | **#812** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design` |
| **Tasks** | (This artifact) | **#813** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/tasks` |
| **Apply-Progress** | (This artifact) | **#805** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/apply-progress` |
| **Verify-Report** | (This artifact) | **#815** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/verify-report` |
| **Archive-Report** | (This artifact) | **(pending save)** | `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/archive-report` |

---

## 11. Prior Slice Archive References

- **Slice 5 Archive**: `openspec/archive/hw-encoder-mft-intel-qsv-mid-stream-idr/` (#791 engram entry). Immutable historical record; this Slice 6 R2 archive serves as corrigendum for the "Mechanism G vendor-uniform" overclaim.
- **Slice 4/3 Archives**: Referenced in frozen-surface policy (spec §6); no changes required.

---

## 12. Next Slice: `hw-encoder-default-on-flip`

**Status**: UNBLOCKED by Slice 6 R2 closure.

**Preconditions** (satisfied):
- [x] NVENC mid-stream IDR mechanism working (Bug 1 closed)
- [x] Cross-vendor smoke validation gate established (D14)
- [x] Phase 0 regression probes locked in (D13)
- [x] Three architectural overclaims corrected via corrigendum

**Ready for**: SDD exploration of feature-flag flip strategy (conditional compilation, feature-gating, rollout scope, risk mitigation).

---

## Appendix: File Manifest

This archive contains:

| File | Purpose |
|------|---------|
| `proposal.md` | SDD proposal (#810) — scope, decisions D1–D14, risks, test inventory |
| `spec.md` | SDD spec (#811) — 20 acceptance requirements (R1–R18 + R19=frozen) + 21 scenarios (S1–S21) |
| `design.md` | SDD design (#812) — 14 design decisions (DD1–DD14) + risks + open questions |
| `tasks.md` | SDD tasks (#813) — 39 work items (Phase 0–G) with status, review workload forecast |
| `apply-progress.md` | Phase A/B/C progress + Phase D/E/F results — commits, cross-vendor smoke evidence |
| `verify-report.md` | Phase E results — AC-1..AC-20 verification, W1/W2 carry-forward, TA.7 design deviation |
| `explore.md` | Phase 0 exploration (investigative, pre-proposal) — 6 mechanism candidates analyzed |
| `archive-report.md` | This document — final corrigendum, outcome summary, acceptance criteria, engram chain |

All artifacts saved to engram topic_keys under `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/`.

---

**Archive status**: COMPLETE AND CLOSED. Ready for orchestrator to proceed with commit/push and next-slice planning.
