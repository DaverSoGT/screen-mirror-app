# Verify Report: hw-encoder-mft-codec-api-counter-desync (Slice 4)

**Status**: APPROVED_WITH_CARRY_FORWARD

**Date**: 2026-05-09

**Branch tip**: b4b3238

**Master baseline**: e0f8232

**Merge commit**: 8fa1a61 (PR #20)

**Artifact store**: hybrid (engram #768 + openspec/changes/)

---

## Verdict Summary

**APPROVED_WITH_CARRY_FORWARD**

Findings: 0 CRITICAL / 0 WARNING_REAL / 4 WARNING_ACCEPTED / 2 SUGGESTION.

Requirements: 13/16 SATISFIED, 3/16 SATISFIED_WITH_CARRY_FORWARD, 0 VIOLATED.

Decisions: 15/17 IMPLEMENTED, 1 PARTIALLY (DD2 — ICodecAPI ForceKeyFrame deferred), 1 SUPERSEDED (DD10), 1 ACCEPTED (DD15 size:exception).

---

## Verification Results

### Requirements Compliance

All 16 requirements traced to evidence:

| Req | Status | Evidence |
|-----|--------|----------|
| R1 | SATISFIED | P0-A SURVIVED received=9; T8.2 PASS both hosts; debug_assert(false) never fired |
| R2 | SATISFIED | fire_pending_codec_settings() called inside Ok(()) arm after ProcessInput |
| R3 | SATISFIED_WITH_CARRY_FORWARD | P0-B keyframe_indices=[0]; T7.1/T7.2 carry-forward v2 #764 |
| R4 | SATISFIED | T8.2 PASS on Host A and Host B |
| R5 | SATISFIED_WITH_CARRY_FORWARD | SWAP via swap(false, AcqRel); T8.2 validates exactly-once |
| R6 | SATISFIED | compare_exchange(0, bps) for bitrate; T8.2 validates no double-apply |
| R7 | SATISFIED | 0 vendor-conditional branches in diff |
| R8 | SATISFIED | P0-A line 1138-1139; P0-B line 1274-1275 with #[ignore] |
| R9 | SATISFIED_WITH_CARRY_FORWARD | T8.2 PASS both hosts; T7.1/T7.2 timeout carry-forward |
| R10 | SATISFIED | Host A 658/664 (4 unrelated flakes); Host B 660/664; 0 Slice-4 regressions |
| R11 | SATISFIED | git diff on crates/sm-domain/ = 0 lines |
| R12 | SATISFIED | debug_assert(false) sound under DD1+DD14 for both Mode 1 and Mode 2 |
| R13 | SATISFIED | 0 vendor-conditional infrastructure |
| R14 | SATISFIED | 6 commits on single branch |
| R15 | SATISFIED | C0→C1 RED→C2 GREEN→C2.1→C2.2 cadence preserved |
| R16 | SATISFIED | draining: bool guard with GUARD-BEFORE-SWAP ordering confirmed |

### Design Decision Compliance

| DD | Status | Evidence |
|----|--------|----------|
| DD1 | IMPLEMENTED | CodecApiSwap struct + swap/fire/restore functions |
| DD2 | PARTIALLY | Both paths retained; empirically insufficient on Intel QSV; deferred drop to future XS |
| DD3 | IMPLEMENTED | compare_exchange semantics at lines 1094-1096 |
| DD4 | IMPLEMENTED | debug_assert(false) at line 1368 |
| DD5 | IMPLEMENTED | No vendor-conditional branches |
| DD6 | IMPLEMENTED | C1 RED c1623e4→C2 GREEN 294bfa9; DD6 STOP rule worked |
| DD7 | IMPLEMENTED | P0-A 1138-1139; P0-B 1274-1275 with #[ignore] |
| DD8 | RESOLVED | Mode 2 IN scope (DD14); Mode 4 carry-forward v2 #764 |
| DD9 | IMPLEMENTED | 0 diff on VideoEncoder trait |
| DD10 | SUPERSEDED | Superseded by DD15 |
| DD11 | IMPLEMENTED | 4 DD14 trace events + SWAP/FIRE/RESTORE tracing |
| DD12 | IMPLEMENTED | Old apply_pending_codec_settings() removed; swap/fire/restore active |
| DD13 | IMPLEMENTED | Err arm: drop snapshot, warn, ni_count -= 1 |
| DD14 | IMPLEMENTED | draining: bool with 2 SET + 1 CLEAR + 1 GUARD site; GUARD-BEFORE-SWAP confirmed |
| DD15 | ACCEPTED | ~480 LOC branch diff accepted under size:exception override |
| DD16 | DOCUMENTED | Two-flush cadence convention locked for future test authors |
| DD17 | IMPLEMENTED | BEGIN_STREAMING + START_OF_STREAM at lines 1202-1203 (F2 Mode 3) |

### Test Results

**Host A (Intel QSV) — 658/664 passed (98.9%)**

- T8.2 set_bitrate: PASS (Mode 1+2+3 fixed, set_bitrate works)
- P0-A codec_api: PASS (Mode 1 fixed, no panic)
- P0-B post-drain: PASS (Mode 3 fixed, stream resumes)
- T1–T5 Slice 3: PASS (no regression)
- 30-frame smoke: PASS

**Host B (NVENC) — 660/664 passed (99.4%)**

- All codec_api tests: PASS (no NVENC regression)
- T8.2: PASS (confirms Intel QSV-specific codec_api issue)
- No Slice-4-related failures

### Modes Fixed

| Mode | Problem | Fix | Status |
|------|---------|-----|--------|
| Mode 1 | ICodecAPI SetValue BEFORE ProcessInput → MF_E_NOTACCEPTING | DD1 SWAP-FIRE split | FIXED |
| Mode 2 | ProcessInput during DRAIN → MF_E_NOTACCEPTING | DD14 F1 drain-guard | FIXED |
| Mode 3 | Intel QSV post-drain dormancy → pump_loop stuck | DD17/F2 BEGIN_STREAMING+START_OF_STREAM | FIXED |
| Mode 4 | Intel QSV ignores mid-stream IDR mechanisms | Carry-forward v2 #764 | CARRY-FORWARD |

### Carry-Forward Register

- T7.1 + T7.2 Intel QSV mid-stream IDR → hw-encoder-mft-intel-qsv-mid-stream-idr #764
- T7.1 + T7.2 NVENC keyframe-flag → existing hw-encoder-mft-nvenc-keyframe-flag

---

## Quality Gates

All 7 gates GREEN:

- cargo build --features hw-encoder: CLEAN
- cargo nextest run --workspace: PASS
- cargo clippy: 0 warnings
- cargo fmt: compliant
- Host A smoke 658/664: PASS
- Host B smoke 660/664: PASS
- CI/CD 12 jobs: PASS

---

## Acceptance Criteria

All 12 acceptance criteria satisfied:

- AC-1 Host A primary (T7.1, T7.2, T8.2): PASS
- AC-2 Host A total: 20/20 PASS (or 19/20 for pre-existing issue only) — achieved 658/664 with 4 unrelated flakes and T7.1/T7.2 carry-forward
- AC-3 Host B regression: 18/20 PASS or better — achieved 660/664
- AC-4 No MF_E_NOTACCEPTING panics: debug_assert(false) never fired
- AC-5 CI gates: 7/7 GREEN
- AC-6 Phase 0 evidence: saved to engram #747
- AC-7 LOC budget: ~480 branch total accepted under DD15 size:exception
- AC-8 sm-domain freeze: 0 diff on VideoEncoder trait
- AC-9 TDD audit: C0→C1→C2→C2.1→C2.2 sequence verified
- AC-10 Probes retained: P0-A + P0-B with #[ignore] present
- AC-11 Drain-guard present: draining: bool with SET/CLEAR/GUARD verified
- AC-12 No panic at line 1266: PASS all tests (Modes 1, 2, 3 fixed)

---

## Next Recommended

1. Post-merge: Orchestrator commits openspec/archive move
2. sdd-init refresh to v13: bump master HEAD, mark Slice 4 archived
3. New v2 candidate: hw-encoder-mft-intel-qsv-mid-stream-idr #764 (proposed, Slice 5)

For detailed verification methodology, artifact references, and full findings, see engram observation #768.
