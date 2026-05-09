# Design: hw-encoder-mft-codec-api-counter-desync (Slice 4 — Round 2 REVISED)

**Status**: LOCKED (DD1–DD16)

**Date**: 2026-05-09

**Branch**: feat/hw-encoder-mft-codec-api-counter-desync

**Artifact store**: hybrid (engram #749 v2 + openspec/changes/)

**Decisions locked**: 15 IMPLEMENTED / 1 PARTIALLY (DD2) / 1 SUPERSEDED (DD10) / 1 ACCEPTED (DD15)

**Key design decisions**:

- **DD1**: SWAP-FIRE split of `apply_pending_codec_settings()` — reads atomics BEFORE ProcessInput (force_keyframe for CleanPoint), fires ICodecAPI AFTER ProcessInput (Mode 1 fix)
- **DD2**: CleanPoint AND ICodecAPI ForceKeyFrame BOTH retained (conservative; drop deferred to future XS slice)
- **DD3**: Restoration semantics via `compare_exchange` for bitrate (last-write-wins), `store(true)` for keyframe
- **DD4**: `debug_assert!(false)` RETAINED and sound under DD1+DD14 for both Mode 1 and Mode 2
- **DD5**: Vendor-uniform fix (no vendor-conditional branching; NVENC strictly safer under reorder)
- **DD6**: TDD cadence + STOP rule (C0→C1 RED→C2 GREEN; RED smoke at c1623e4 exposed Mode 2, DD6 STOP triggered correctly)
- **DD7**: Probes retained #[ignore] (P0-A, P0-B permanent regression guards)
- **DD8**: Out-of-scope revised (Mode 2 now IN scope via DD14; Mode 4 carry-forward v2 #764)
- **DD9**: sm-domain FROZEN (draining is stack-local, no struct field, no trait change)
- **DD11**: Tracing instrumentation (SWAP/FIRE/RESTORE + DD14 SET/CLEAR/GUARD sites)
- **DD12**: Function naming (swap/fire/restore; old apply_pending_codec_settings REMOVED)
- **DD13**: Err(_) on submit_frame (drop snapshot, no restore)
- **DD14 (NEW)**: F1 drain-state ProcessInput guard (stack-local draining: bool; SET on COMMAND_DRAIN, CLEAR on DrainComplete, GUARD before SWAP at top of inner loop — Mode 2 fix)
- **DD15 (NEW)**: Revised LOC budget (~528–573 branch total accepted under size:exception override; production diff well-contained ~80–115 LOC)
- **DD16 (NEW)**: Test cadence convention (match production scenario; preserve two-flush for realistic mid-stream tests)

**Modes fixed**:
- Mode 1 (ICodecAPI race before ProcessInput): Fixed via DD1 SWAP-FIRE split
- Mode 2 (ProcessInput during DRAIN): Fixed via DD14 F1 drain-state guard
- Mode 3 (Intel QSV post-drain dormancy): Fixed via DD17/F2 BEGIN_STREAMING+START_OF_STREAM
- Mode 4 (Intel QSV mid-stream IDR): Carry-forward to v2 #764

For full design details including component map, data flow, risk register, and implementation contracts, see engram observation #749 or the parallel markdown file in this archive.
