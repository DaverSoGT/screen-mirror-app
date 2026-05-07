# Verify Report: hw-encoder-mft-vendor-compat-slice1-nvenc

**Change**: hw-encoder-mft-vendor-compat-slice1-nvenc
**Branch**: feat/hw-encoder-mft-nvenc-h-b3 (HEAD cd45065 + working-tree B-KF-FALLBACK + B-CLEANUP)
**Mode**: Strict TDD (ACTIVE — test runner: cargo nextest)
**Date**: 2026-05-05
**Verdict**: APPROVED_WITH_CARRY_FORWARD

---

## Quality Gates

| Gate | Command | Result |
|------|---------|--------|
| 1 | cargo check --workspace | PASS |
| 2 | cargo clippy --workspace --all-targets --all-features -- -D warnings | **FAIL** |
| 3 | cargo fmt --check --all | PASS |
| 4 | cargo nextest run --workspace (611 tests) | PASS |
| 5 | cargo deny check | PASS |
| 6 | cargo check --no-default-features | PASS |
| 7 | cargo check --features hw-encoder | PASS |

Gate 2 failure: `doc_lazy_continuation` lint at `windows_mft.rs:173`. Fix: add 2-space indent to that line.

---

## Host B Smoke

```
Summary [17.503s] 18 tests run: 16 passed, 2 failed, 0 skipped, 0 aborted.
  FAIL mft_keyframe_flag_cleared_after_idr_emitted        (T7.2)
  FAIL mft_request_keyframe_marks_next_packet_as_keyframe (T7.1)
```

0 AVs, 0 MF_E_INVALIDMEDIATYPE.

---

## Issues

### CRITICAL
None.

### WARNING
- W1: Gate 2 fails — `doc_lazy_continuation` lint at `windows_mft.rs:173`. Trivial 1-line fix.
- W2: DD2 enumeration loop (n=0..15) not implemented; only n=0 called. Works on NVENC; low regression risk.
- W3: T7.1 + T7.2 FAIL — NVENC vendor limitation (ignores both CODECAPI_AVEncVideoForceKeyFrame and MFSampleExtension_CleanPoint for runtime-forced IDRs). Carry-forward to Slice 2/3.

### SUGGESTION
- S1: Fix W1 before archive to restore 7/7 green gates.
- S2: Document DD4 retry absence in PR description.
- S3: Add comment at annex_b_contains_idr call site noting NVENC natural GOP IDR vs forced IDR distinction.

---

## Compliance Summary

| Requirement | Status |
|-------------|--------|
| R1 — NVENC SetOutputType acceptance | COMPLIANT |
| R2 — Pump-loop invariants | COMPLIANT |
| R3 — apply_pending_codec_settings | COMPLIANT (H-B1 refuted; no-op) |
| R4 — Phase 0 trace removed | COMPLIANT |
| R5 — Enumeration fallback | COMPLIANT (code review) |
| R6 — 18/18 smoke | PARTIAL (16/18; T7.1+T7.2 carry-forward) |
| R7 — 7 quality gates | WARNING (6/7; Gate 2 fails) |
| R8 — Public API frozen | COMPLIANT |

**Recommendation**: APPROVED_WITH_CARRY_FORWARD — fix W1 (clippy) before merging PR.
