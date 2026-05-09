# Proposal: hw-encoder-mft-codec-api-counter-desync (Slice 4)

[Full proposal content mirrored from engram #735 — see archive-report.md for details and observation IDs]

**Status**: PROPOSED (Locked decisions: D1, D-PHASE0, D-DELIVERY, D-VENDOR, D-CLEANPOINT, D-RESTORATION, D-GREEN-TESTS, D-LOC-FORECAST, D-PROBES-RETENTION)

**Branch**: feat/hw-encoder-mft-codec-api-counter-desync

**Base**: e0f8232 (master, PR #19 merged)

**Approach locked**: B (reorder ICodecAPI side effects to AFTER ProcessInput)

**Delivery**: Single PR override of auto-chain; ~160–230 LOC realistic forecast

**Phase 0 gate**: P0-A + P0-B empirical confirmation REQUIRED before design lock

**Artifact store**: hybrid (engram #735 + openspec/changes/)

For full proposal details, see engram observation #735 or the parallel markdown file in this archive.
