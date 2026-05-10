# Verify Report: hw-encoder-mft-intel-qsv-mid-stream-idr (Slice 5)

> Phase: SDD verify. Branch tip: `d65c15c`. Master baseline: `5130e87`.
> Artifact store: hybrid (engram #790 + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-10.

## Verdict: APPROVED_WITH_CARRY_FORWARD

**CRITICAL**: 0 | **WARNING (real)**: 1 | **WARNING (theoretical)**: 2 | **SUGGESTION**: 2

---

## Build / Static Analysis Gates

| Gate | Result |
|------|--------|
| `cargo build --features hw-encoder` | PASS (57.77s) |
| `cargo nextest run --workspace` | PASS — 611/611, 19 skipped |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | PASS — 0 warnings |
| `cargo fmt --check --all` | PASS |
| `git diff 5130e87 -- crates/sm-domain/` | PASS — 0 lines |
| `git diff 5130e87 -- crates/sm-infra/Cargo.toml` | PASS — 0 lines |

---

## R-Set: 18/18 PASS (1 WARNING-real on R6 naming)

All R1–R18 verified. See engram #790 for full matrix.

Key verifications:
- R1: `request_keyframe_via_recreate()` at `windows_mft.rs:1979` — confirmed
- R2: trait routing at line 302 — confirmed
- R4: G 9-step sequence at lines 1490–1685 — confirmed
- R10: All DD10 deletions — confirmed; CodecApiSwap has only `new_bitrate`
- R14: `flush()` docstring at ~1927 — updated, stale language removed
- R15: nextest 611/611 + Host A 361/365 + Host B 359/365 (0 new regressions)
- R16: sm-domain diff = 0 lines

---

## S-Set: 16/16 PASS

All S1–S16 verified. Key:
- S4/S5: T7.1+T7.2 PASS on Host A at `d65c15c` (#788)
- S9: Round 3 probe PASS on Host A (#789)
- S13: grep confirms 0 production write-path CleanPoint/ForceKeyFrame matches

---

## DD Compliance: 11/12 PASS (1 WARNING-real on DD2)

All DD1–DD12 verified. DD2 naming deviation: field remains `winning_activate: Option<IMFActivate>` (not renamed to `mft_activate_factory`); functionally equivalent — factory available for G's 2nd ActivateObject call via borrow in pump_loop.

---

## Tasks: 36/41 done

Phase 0–4: complete. Phase 5 (T5.1–T5.4): deferred to PR creation. Phase 6 (T6.1–T6.10): all PASS except T6.10 structural naming deviation.

---

## Findings Summary

### WARNING (real) — 1
**W1: DD2-FIELD-RENAME** — `winning_activate` not renamed to `mft_activate_factory`; functionally equivalent (factory available for G's ActivateObject calls). No code change required.

### WARNING (theoretical) — 2
**W2: T5-PENDING** — PR body documentation tasks deferred to PR creation.
**W3: DD2-STRESS** — Multi-recreate stress probe (5–10 cycles) not run. Design explicitly allows carry-forward.

### SUGGESTION — 2
**SG1: DD7-ROUND2-GAPS** — Round 2 probes removed per DD7 design decision; R13 wording creates apparent gap. Archive report should document the resolution.
**SG2: LOC-FORECAST-OVERAGE** — Actual +1285 LOC vs 600–800 forecast; `size:exception` pre-approved. PR body should explain breakdown.

---

## Cross-Vendor Smoke (#789)

| Host | Pass/Run | New Fails |
|------|----------|-----------|
| Host A Intel QSV | 361/365 | 0 |
| Host B NVIDIA NVENC | 359/365 | 0 |

NVENC fails: 3 NAL-type-5 detection (T7.1, T7.2, round 3 probe) = pre-existing Slice 4 carry-forward → `hw-encoder-mft-nvenc-keyframe-flag` Slice 6.

---

## Next Steps

1. Open PR with `size:exception` label
2. Complete Phase 5 PR body documentation (T5.1–T5.4)
3. sdd-archive after PR merge
