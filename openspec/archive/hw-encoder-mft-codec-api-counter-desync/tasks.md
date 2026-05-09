# Tasks: hw-encoder-mft-codec-api-counter-desync (Slice 4 — Round 2 REVISED)

**Status**: EXECUTED (Phase 0–2 DONE; Phase 3–5 complete post-merge)

**Date**: 2026-05-09

**Branch**: feat/hw-encoder-mft-codec-api-counter-desync

**Artifact store**: hybrid (engram #751 v2 + openspec/changes/)

**Total tasks**: 39

**Phase summary**:

- **Phase 0 (DONE)**: 3 tasks — C0 probes added (f41e7d0), cadence corrected (c6c7c8c), Phase 0 trace locked (#747)
- **Phase 1 (DONE)**: 8 tasks — C1 RED test bodies restored (c1623e4), RED smoke gate cleared (#754, Mode 2 confirmed)
- **Phase 2 (DONE)**: 13 tasks — C2 GREEN production fix applied (Approach B SWAP-FIRE + F1 drain-guard); T8.2 PASS both hosts; no regressions
- **Phase 3 (DONE)**: 4 tasks — C3 polish (fmt/clippy), optional doc updates
- **Phase 4 (DONE)**: 3 tasks — Smoke gates validated Host A (658/664) + Host B (660/664); P0-B re-run resolved OQ-2/OQ-3
- **Phase 5 (DONE)**: 8 tasks — Verify APPROVED_WITH_CARRY_FORWARD; PR #20 merged; archive moved; sdd-init refreshed

**Key production tasks completed**:

- T2.1: Split apply_pending_codec_settings() into swap/fire/restore (DD1 SWAP-FIRE)
- T2.2–T2.5: Add draining: bool stack-local guard + SET/CLEAR/GUARD sites (DD14 F1)
- T2.6: F1 GUARD at top of inner loop BEFORE SWAP (GUARD-BEFORE-SWAP ordering)
- T2.7: SWAP then FIRE post-ProcessInput (Mode 1 fix)
- T2.8: Update 3 restoration sites (DD3)
- T2.9: Remove old apply_pending_codec_settings function
- T2.10–T2.13: Build/compile, smoke, fmt/clippy, commit C2 GREEN

For full task list with completion status and individual step details, see engram observation #751 or the parallel markdown file in this archive.
