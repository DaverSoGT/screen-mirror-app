# Verify Report - hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Slice 6 R2)

> Phase: SDD verify
> Branch: feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism @ c4b59a9 (pushed)
> Base: master @ c48ae46
> Date: 2026-05-10
> Strict TDD: ACTIVE - test runner cargo nextest run --workspace
> Artifact store: hybrid (this file + engram topic_key sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/verify-report)

## 1. Status

APPROVED_WITH_CARRY_FORWARD - 0 CRITICAL, 2 WARNING (documented carry-forward), 1 SUGGESTION.

## 2. Acceptance Criteria Result Table

| AC | Requirement | Status | Evidence |
|----|-------------|--------|----------|
| AC-1 | Host A T7.1+T7.2 PASS | VERIFIED | host_a_intel_qsv_smoke.log 27/27 PASS; T7.1 11.374s, T7.2 11.300s |
| AC-2 | Host B T7.1+T7.2 PASS | VERIFIED | host_b_nvenc_smoke.log.txt 27/27 PASS; T7.1 6.014s, T7.2 6.015s |
| AC-3 | T8.2 PASS on BOTH hosts | VERIFIED | Host A 11.181s; Host B 5.959s |
| AC-4 | force_keyframe_icodecapi_pending defaults to false | VERIFIED | windows_mft.rs:209 AtomicBool::new(false); unit test default_false PASS |
| AC-5 | request_keyframe arms flag; no vendor dispatch | VERIFIED | windows_mft.rs:380-384 single store(true, Release); grep match-vendor in request_keyframe = 0 |
| AC-6 | pump_loop consumes with swap BEFORE ProcessInput | VERIFIED | windows_mft.rs:1503-1505 swap(false, AcqRel) BEFORE submit_frame at line 1531 |
| AC-7 | SetValue uses VARIANT vt=VT_UI4 ulVal=1 | VERIFIED | windows_mft.rs:1507 make_variant_u32(1); helper at 1831-1842 sets vt=VT_UI4, ulVal=value |
| AC-8 | SetValue HRESULT failure is non-fatal | VERIFIED | windows_mft.rs:1511-1520 tracing::warn on Err, no propagation; submit_frame proceeds |
| AC-9 | Mechanism G code fully deleted | VERIFIED | grep keyframe_recreate_pending or request_keyframe_via_recreate in src/ = 0 matches |
| AC-10 | CleanPoint write code fully deleted | VERIFIED | grep cleanpoint_pending or request_keyframe_via_cleanpoint in src/ = 0 matches |
| AC-11 | CleanPoint READ in collect_output unchanged | VERIFIED | windows_mft.rs:1761 GetUINT32 MFSampleExtension_CleanPoint unwrap_or 0 not equal 0 preserved |
| AC-12 | DD10 comment replaced with P2 + research citation | VERIFIED | windows_mft.rs:1200-1211 cites #809, Chromium MFVEA, FFmpeg mfenc.c, HCK Win8+; no Intel-QSV-does-not-honor or NVENC-honored-CleanPoint string anywhere in source |
| AC-13 | EncoderVendor retained for logging only | VERIFIED | enum at 132-157; consumers at 706-721 are tracing info/warn; vendor field marked dead_code |
| AC-14 | request_keyframe doc documents latency contract | VERIFIED | windows_mft.rs:368-379 NVENC idx 0 ~0ms, Intel QSV idx 1 ~33ms, within 30-frame tolerance |
| AC-15 | 5 Phase 0 R2 probes retained; Slice 5 round-3 absent | VERIFIED | All 5 probes in tests/windows_mft_encode.rs P0 1652, P0.b 1831, P1 2120, P2-NVENC 2422, P2-Intel 2704; deleted probe grep = 0 |
| AC-16 | 3 CI-runnable unit tests PASS | VERIFIED | windows_mft.rs:2088-2128; nextest 611/611 includes these |
| AC-17 | cargo clippy --all-targets --all-features --locked -D warnings | VERIFIED | Re-run 2026-05-10: exit 0, zero warnings 35.27s |
| AC-18 | cargo nextest run --workspace GREEN | VERIFIED | Re-run 2026-05-10: 611 passed, 19 skipped, 0 failed 12.298s |
| AC-19 | sm-domain diff vs c48ae46 = 0 lines | VERIFIED | git diff --stat c48ae46..HEAD -- crates/sm-domain returns empty output |
| AC-20 | Slice 6 R2 archive corrigendum | PENDING | Phase G TG.2 deferred to archive phase expected |

Totals: 19 VERIFIED, 1 PENDING Phase G, 0 CRITICAL DEVIATION.


## 3. Findings

### CRITICAL blocks PR merge

None.

### WARNING must document

W1 - Spec R11/R12 letter deviation: ignore attribute retained on T7.1/T7.2

- Spec text: R11/R12 require the ignore annotation MUST be removed from both Host A and Host B variants.
- Actual: T7.1 line 391 and T7.2 line 536 retain ignore = "Slice 6 R2 - requires hardware Host A or Host B; run with --run-ignored". Functional equivalence via cargo nextest run --run-ignored only is documented in apply-progress.
- Justification: CI is headless no GPU. Removing the ignore would cause T7.1/T7.2 to attempt MFT activation in CI and FAIL. Spec R17 cargo nextest run --workspace MUST be GREEN conflicts with literal R11/R12 if ignore is removed. The current design preserves R17 and equivalent CI behavior via cfg target_os=windows feature=hw-encoder gating combined with ignore-reason text now referencing Slice 6 R2 framing instead of Slice 5 Mechanism G.
- Evidence of equivalence: Both hosts ran 27/27 PASS using --run-ignored only; same end-state as ignore-removed-but-feature-gated would produce on hardware.
- Recommendation: ACCEPT as carry-forward; record in archive-report corrigendum.

W2 - Spec R14 probe inventory drift, 2 extra carry-forward probes present

- Spec text: R14 enumerates exactly 5 retained Phase 0 probes P0, P0.b, P1, P2-NVENC, P2-Intel.
- Actual: tests/windows_mft_encode.rs contains 7 phase0_* functions - the 5 R14 probes plus phase0_codec_api_before_processinput_triggers_notaccepting line 1355 and phase0_codec_api_after_processinput_no_notaccepting_and_idr_on_frame_4 line 1491.
- Status: These 2 extras are Slice 4/5 carry-forward regression evidence retained under prior-slice frozen-surface policy spec section 6 "Slice 3/4 Phase 0 probes prior slices MUST NOT be touched". They predate Slice 6 R2 and were not on the delete list. Both PASS on Host A in smoke logs.
- Recommendation: ACCEPT as carry-forward. R14 wording could be amended in archive corrigendum to read "exactly 5 Slice 6 R2 probes plus Slice 4/5 carry-forwards retained per section 6 frozen-surface rule".

### SUGGESTION

S1 - TA.7 deviation: source comment polish

- mft_activate_factory field is retained despite spec R5 listing it for deletion. Apply-progress documents thoroughly. Source already explains Drop semantics at windows_mft.rs:406-408. Consider one extra line in the field-declaration doc-comment noting "retained per Slice 6 R2 TA.7 deviation - used for initial ActivateObject, unrelated to deleted Mechanism G". Cosmetic; does not block merge.


## 4. Carry-forward Register

| ID | Spec | Deviation | Justification | Evidence | Resolution |
|----|------|-----------|---------------|----------|------------|
| W1 | R11, R12 | ignore kept on T7.1/T7.2 with text updated to Slice 6 R2 framing | Removing ignore breaks AC-18 / R17 on headless CI. --run-ignored only is functionally equivalent on HW hosts. | Smoke logs: Host A 11.374s/11.300s PASS, Host B 6.014s/6.015s PASS | Document in archive corrigendum |
| W2 | R14 | 7 phase0_* probes present vs 5 enumerated | The 2 extras are Slice 4/5 carry-forward probes covered by section 6 frozen-surface rule. | tests/windows_mft_encode.rs lines 1355 + 1491; smoke 27/27 PASS | Document in archive corrigendum; optionally amend R14 wording |
| TA.7 | R5 | mft_activate_factory field retained | DD2 over-scoped deletion. Field is required for initial ActivateObject in start and Drop-time release - unrelated to Mechanism G. | apply-progress.md Phase A TA.7 note; windows_mft.rs:406-409 Drop | Already documented; archive corrigendum should record design over-spec |

## 5. Test Evidence Summary

### Smoke already gathered

| Host | GPU | Total | Pass | Fail | Skip | Duration |
|------|-----|-------|------|------|------|----------|
| Host A | Intel QSV | 27 | 27 | 0 | 0 | 178.245s |
| Host B | NVENC | 27 | 27 | 0 | 0 | 162.223s |

Zero ERROR. Zero panic. Zero assertion failure. Benign non-fatal WARNs only: AMD MFT rejection on Host B = expected fallback when no AMD GPU; probe-end NO_MORE_PACKETS on Host A = informative carry-forward Slice 4/5 probe logs.

### Workspace re-run during verify 2026-05-10

- cargo clippy --all-targets --all-features --locked -- -D warnings -> exit 0, zero warnings 35.27s
- cargo nextest run --workspace -> 611 passed, 19 skipped HW carry-forward ignores, 0 failed 12.298s

## 6. Recommendation

READY FOR PR. WARNINGs are documentation-only and MUST be reflected in the archive corrigendum to close AC-20.

1. Phase F TF.1: open PR with conventional subject refactor infra: replace Mechanism G with vendor-uniform ForceKeyFrame for mid-stream IDR Slice 6 R2.
2. PR body MUST cross-reference engram observations #809 / #807 / #801 / #808, AC-1..AC-19 results table, and W1/W2 carry-forward notes.
3. Phase G TG.2: archive-report MUST contain Retroactive Corrections to Slice 5 and 4 section enumerating the 3 retracted overclaims plus W1, W2, and TA.7 notes.

## 7. Cross-References

- Spec: engram #811 sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/spec
- Design: engram #812 sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design
- Tasks: engram #813 sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/tasks
- Apply progress: engram #805 sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/apply-progress
- Empirical anchors: #800 P0 priming format, #801 P0.b Mechanism G falsified on NVENC, #807 P1 CleanPoint INPUT falsified on NVENC, #808 Chromium/FFmpeg/HCK research, #809 P2 vendor-uniform ForceKeyFrame success
- Slice 5 archive immutable historical record: #791
