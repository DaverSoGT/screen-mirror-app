# Archive Report: hw-encoder-mft-vendor-compat-rework (Slice 2 — Intel QSV stream-change renegotiation)

**Status**: APPROVED_WITH_CARRY_FORWARD (per verify #697)

**Date archived**: 2026-05-08

**Branch**: feat/hw-encoder-mft-stream-change-handling

**Base**: 3c8bc48 (master, PR #17 merged)

**Branch tip (origin)**: 0110da6 (3 SDD commits)

**Branch tip (local, ahead +1)**: a4e4d54 (engram sync — TRACKED per convention #698)

---

## Executive Summary

Intel QSV (Host A) was stalling indefinitely after ~17 frames during the 30-frame smoke test because `collect_output` in windows_mft.rs was silently swallowing `MF_E_TRANSFORM_STREAM_CHANGE` errors as `Ok(None)` instead of performing the Microsoft-mandated async-MFT renegotiation. This is the third confirmed manifestation of the Bug 1 family: Slice 1 (PR #17, archived as #604) addressed cross-thread COM transfer and force-IDR carry-forward for NVENC; Slice 2 closes the latent stream-change protocol gap on Intel QSV. The fix is narrow and self-contained: a new private helper `renegotiate_output_type()` that mirrors the `try_setup_output_type()` COM sequence (GetOutputAvailableType → SetOutputType with frame-size/rate/bitrate overlay), invoked when STREAM_CHANGE fires, with proper error mapping to `EncoderError::EncodeFailed` and `output_format_known` cache invalidation. Empirically validated: 30-frame smoke on Host A PASS in 3.74s (was HANG >720s on master 3c8bc48). Host B (NVENC) unaffected — new code path unreachable on NVENC (doesn't emit STREAM_CHANGE) — maintains 16/18 PASS baseline. T-NEW-1 and T-NEW-2 cross-vendor stop-deadline tests GREEN both hosts, confirming Bug 2 fix from PR #16 not regressed. 7/7 quality gates GREEN on merge SHA. Carry-forward: 8 single-frame timeouts on Host A (Intel QSV doesn't emit STREAM_CHANGE until ≥3 frames; separate future change `hw-encoder-mft-single-frame-flush`) + 2 NVENC keyframe-flag failures on Host B (pre-existing per baseline #601, separate Bug).

---

## Slice Scope (Bug 1 family — Manifestation #3)

### IN scope (this Slice 2)

- `MF_E_TRANSFORM_STREAM_CHANGE` handling in `collect_output` (windows_mft.rs line 1317 baseline): replace silent swallow with renegotiation
- New thin `renegotiate_output_type(mft, w, h, framerate, bitrate_bps) → Result<(), EncoderError>` helper (mirrors try_setup_output_type, maps errors to EncodeFailed)
- Reset `output_format_known` cache to `None` on successful renegotiation to force re-detection
- `tracing::trace!()` of `output.dwStatus` and `status` on every ProcessOutput call (diagnostic)
- 1-line test ordering fix: `enc.stop()` before `producer.join()` in mft_thirty_frame_smoke_emits_at_least_one_keyframe
- Smoke verification on Host A (BLOCKED_ON_SMOKE gate) + regression check on Host B

### OUT of scope (other Manifestations, deferral)

- NVENC SetOutputType 0xC00D6D76 failure on Host B (Bug 1 Manifestation B) — separate change `hw-encoder-mft-nvenc-setup-fix`
- AMD AMF empirical verification (no hardware available)
- Flipping `default = ["hw-encoder"]` — separate change `hw-encoder-default-on-flip`
- Stream-change-specific mock/shim test (no practical way to force STREAM_CHANGE without large refactor)
- Refactoring `try_setup_output_type` for error parameterization (out-of-scope)
- Domain-layer changes (sm-domain FROZEN)

---

## Commits

| SHA | Subject | Role | Lines |
|-----|---------|------|-------|
| ba36bba | test(infra): swap stop/join order in 30-frame smoke to expose stall as failure | C1 RED | 2 |
| e44330b | feat(infra): handle MF_E_TRANSFORM_STREAM_CHANGE via renegotiate_output_type | C2 GREEN | 100 |
| 0110da6 | style(infra): cargo fmt for stream-change handling | C3 polish | 22 |

**Total changed lines**: ~126 LOC (well under 400-line single-PR budget)

---

## Quality Gates (7/7 GREEN)

| Gate | Status | Evidence |
|------|--------|----------|
| cargo nextest run --workspace | GREEN | 611 passed, 19 skipped (verified live on 0110da6) |
| cargo clippy --features hw-encoder --tests -- -D warnings | GREEN | Zero warnings (verified live) |
| cargo build --features hw-encoder | GREEN | Finished without error |
| Host A smoke: 30-frame smoke PASS | GREEN | #637 — 30 packets (1 IDR + 29 P) in 3.74s, was HANG on 3c8bc48 |
| Host A smoke: T-NEW-1/T-NEW-2 no regression | GREEN | #637 — mft_stop_during_idle: 0.923s, mft_stop_during_active_encode: 0.931s |
| Host B smoke: ≥16/18 PASS | GREEN | #696 — 16 PASS / 2 FAIL (pre-existing per #601 baseline) |
| Host B smoke: T-NEW-1/T-NEW-2 no regression | GREEN | #696 — both PASS on NVENC, Bug 2 fix preserved |

---

## Smoke Validation (Host A + Host B)

### Host A (Intel QSV) — Engram #637

**30-frame smoke PASS** (3.74s, was HANG >720s). Trace evidence confirms STREAM_CHANGE renegotiation at frame ~4:
```
ProcessOutput STREAM_CHANGE — renegotiating dw_status=256 status=0 hr=0xC00D6D61
[T4.2] pkt 1 is_keyframe=true len=29478 elapsed=602ms
... steady stream ...
[T4.2] pkt 30 is_keyframe=false len=11859 elapsed=3010ms
done: 30 packets (1 IDR + 29 P) in 3.0143977s
```

**10/18 PASS** (9/18 pre-fix). T-NEW-1 (idle stop) + T-NEW-2 (active stop) GREEN. 8 single-frame timeout tests remain (pre-existing vendor behavior: STREAM_CHANGE doesn't fire until ≥3 frames).

### Host B (NVIDIA NVENC) — Engram #696

**16/18 PASS** (no regression vs baseline #601 b0bfeec). STREAM_CHANGE renegotiation code path unreachable on NVENC (doesn't emit STREAM_CHANGE). T-NEW-1 and T-NEW-2 GREEN. 2 keyframe-flag failures pre-existing.

---

## Carry-forward Items

### 1. Intel QSV Single-Frame Timeout (8 tests on Host A)

Root cause: vendor requires ≥3 frames before emitting STREAM_CHANGE; single-frame tests expire before event fires. Pre-existing on master. Future slice: `hw-encoder-mft-single-frame-flush`.

### 2. NVENC Keyframe-Flag Detection Failure (2 tests on Host B)

Root cause: NAL type 5 detection edge case on NVENC (pre-existing per baseline #601). Future slice: `hw-encoder-mft-nvenc-keyframe-flag`.

---

## SDD Chain Links (traceability)

| Artifact | Topic key | Observation ID |
|----------|-----------|-----------------|
| Exploration | `sdd/hw-encoder-mft-vendor-compat-rework/explore` | #631 |
| Proposal | `sdd/hw-encoder-mft-vendor-compat-rework/proposal` | #632 |
| Spec | `sdd/hw-encoder-mft-vendor-compat-rework/spec` | #633 |
| Design | `sdd/hw-encoder-mft-vendor-compat-rework/design` | #634 |
| Tasks | `sdd/hw-encoder-mft-vendor-compat-rework/tasks` | #635 |
| Apply-progress | `sdd/hw-encoder-mft-vendor-compat-rework/apply-progress` | #636 |
| Host A smoke | `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-a-postfix` | #637 |
| Host B smoke | `sdd/hw-encoder-mft-vendor-compat-rework/smoke-host-b-postfix-regression` | #696 |
| Verify report | `sdd/hw-encoder-mft-vendor-compat-rework/verify-report` | #697 |
| Archive report | `sdd/hw-encoder-mft-vendor-compat-rework/archive-report` | #699 |

---

## Post-Merge Checklist

1. [ ] `gh pr merge --merge --delete-branch` on feat/hw-encoder-mft-stream-change-handling
2. [ ] Update sdd-init #186: bump master HEAD, mark rows 17–18 archived, add rows 19–21 carry-forward candidates
3. [ ] Confirm master CI 100% GREEN on merge commit
4. [ ] `git branch -D feat/hw-encoder-mft-stream-change-handling` (local cleanup)

---

**Ready for orchestrator: PR creation and merge.**
