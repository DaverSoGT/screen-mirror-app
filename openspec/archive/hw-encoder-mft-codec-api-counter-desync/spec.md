# Spec: hw-encoder-mft-codec-api-counter-desync (Slice 4 — Round 2 REVISED)

**Status**: LOCKED (R1–R16, S1–S18, AC-1–AC-12)

**Date**: 2026-05-09

**Branch**: feat/hw-encoder-mft-codec-api-counter-desync

**Artifact store**: hybrid (engram #738 v2 + openspec/changes/)

**Requirements**: 13 SATISFIED / 3 SATISFIED_WITH_CARRY_FORWARD / 0 VIOLATED

**Scenarios**: 18 total (S1–S18) covering Phase 0 probes, codec_api reorder validation, drain-state guard validation, smoke cross-vendor, carry-forward items

**Key decisions locked**:
- R1: Counter-desync invariant (no MF_E_NOTACCEPTING on serviced NeedInput credit)
- R2: ICodecAPI side effects AFTER ProcessInput (Approach B)
- R3: Frame-N keyframe via CleanPoint (deferred ICodecAPI ForceKeyFrame finalization to design)
- R4: Bitrate change within 2 frames
- R5/R6: Atomic exactly-once semantics (keyframe_pending, pending_bitrate)
- R7: Vendor-uniform code path (no vendor-conditional branching)
- R8: Phase 0 probes retained #[ignore]
- R9: T7.1, T7.2, T8.2 GREEN on Host A and Host B
- R10: Zero regressions across full suite
- R11: sm-domain FROZEN
- R12: debug_assert!(false) semantics sound under DD1+DD14
- R13: No vendor detection infrastructure
- R14: Single-PR cohesion
- R15: Strict TDD commit cadence (C0→C1 RED→C2 GREEN→C3 polish)
- **R16 (NEW)**: Drain-state ProcessInput guard (F1 stack-local draining: bool)

**Carry-forward items**:
- T7.1/T7.2 Intel QSV mid-stream IDR → v2 candidate #764
- T7.1/T7.2 NVENC keyframe-flag → existing v2 candidate

For full spec details including R1–R16 text, S1–S18 scenarios, and acceptance criteria, see engram observation #738 or the parallel markdown file in this archive.
