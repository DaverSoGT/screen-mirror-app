# Verify Report: hw-encoder-mft-single-frame-flush (Slice 3)

> Phase: SDD verify. Branch HEAD: b7cdb6f. Master HEAD: daa9522.
> Mode: Strict TDD ACTIVE. Artifact store: hybrid (engram + openspec).
> Date: 2026-05-09.

---

## Verdict: APPROVED_WITH_CARRY_FORWARD

## Summary

- 0 CRITICAL
- 0 WARNING (real)
- 3 WARNING (theoretical/accepted)
- 2 SUGGESTION

---

## Branch / Commit Anchors (verified)

| Slot | SHA | Subject | Status |
|------|-----|---------|--------|
| Master HEAD | daa9522 | Merge pull request #18 | baseline |
| C0 | ea7994f | test(infra): add Phase 0 trace probes | DONE |
| C1 | 0f33772 | test(infra): assert single-frame intel-qsv tests flush before recv | DONE (RED) |
| C2 | 2af01f7 | feat(infra): add flush() to WindowsMftH264Encoder for short-stream output | DONE (GREEN) |
| C3 | 8f1dfcb | style(infra): cargo fmt for flush handler | DONE (POLISH) |
| C4 | b7cdb6f | test(infra): revert T6/T7/T8 to master bodies - codec_api desync out of scope | DONE (DD6 FALLBACK) |

---

## Quality Gates (6/6 GREEN)

| Gate | Status | Evidence |
|------|--------|----------|
| cargo build --features hw-encoder | GREEN | 44.78s, 0 errors |
| cargo nextest run --workspace | GREEN | 611 passed, 19 skipped |
| cargo clippy --all-targets --all-features --locked -- -D warnings | GREEN | 0 warnings |
| cargo fmt --check --all | GREEN | No diff |
| Host A smoke (Intel QSV) 17/20 PASS | GREEN | Engram #719, 29.902s wall |
| Host B smoke (NVENC) 18/20 PASS | GREEN | Engram #721, 18.931s wall |

---

## Spec Coverage (R1-R15)

| Req | Status | Evidence |
|-----|--------|----------|
| R1 (flush() inherent) | SATISFIED | pub fn flush(&self) at windows_mft.rs:1597 |
| R2 (drain_pending: AtomicBool) | SATISFIED | Field line 96, init line 106, store(true,Release) line 1598 |
| R3 (pump_loop check post-NeedInput) | SATISFIED | Lines 1305-1314, swap(false,AcqRel) per DD4 |
| R4 (flush() callable while frame_tx alive) | SATISFIED | One atomic store, T1-T5 call flush() with frame_tx in scope |
| R5 (DrainComplete handler UNCHANGED) | SATISFIED | Disconnect arm lines 1291-1302 unchanged |
| R6 (STREAM_CHANGE UNCHANGED) | SATISFIED | renegotiate_output_type lines 639/1419, no diff |
| R7 (sm-domain UNCHANGED) | SATISFIED | git diff = 0 lines on encode.rs |
| R8 (flush() doc comment 4 clauses) | SATISFIED | 14-line doc with all 4 R8 clauses |
| R9 (8 tests PASS on Host A) | PARTIAL 5/8 | T1-T5 PASS. T6/T7/T8 FAIL Timeout ~3.7s. Codec_api desync Intel QSV-specific. Carry-forward. |
| R10 (no regression) | SATISFIED | 0 regressions on either host |
| R11 (Phase 0 evidence) | SATISFIED | Engram #710: OQ-1 LOCKED YES, 1F DRAIN GOOD 258ms |
| R12 (BLOCKED_ON_SMOKE transcripts) | SATISFIED | Host A #719, Host B #721 |
| R13 (quality gates) | SATISFIED | 6 CI gates GREEN, 17/20 Host A meets R10 exception |
| R14 (sm-domain + Cargo.toml FROZEN) | SATISFIED | git diff = 0, default = [] confirmed |
| R15 (Strict TDD RED before GREEN) | SATISFIED | C1 0f33772 RED before C2 2af01f7 GREEN |

---

## Design Adherence (DD1-DD10)

| DD | Status | Evidence |
|----|--------|----------|
| DD1 (flush() inherent, &self) | SATISFIED | pub fn flush(&self) line 1597 |
| DD2 (drain_pending field) | SATISFIED | Field line 96 with #[allow(dead_code)] per convention #188 |
| DD3 (pump_loop check site) | SATISFIED | Lines 1305-1314, tracing::info! present |
| DD4 (swap once per flag-set) | SATISFIED | swap(false, Ordering::AcqRel) line 1308 |
| DD5 (14-line doc comment) | SATISFIED | All R8 clauses including Phase 0 latency anchor ~250ms |
| DD6 (T6/T7/T8 restructure) | FALLBACK APPLIED | Codec_api desync timeout Host A. Reverted via b7cdb6f. Carry-forward to hw-encoder-mft-codec-api-counter-desync |
| DD7 (3-commit TDD) | SATISFIED | C0+C1 RED + C2 GREEN + C3 fmt + C4 fallback |
| DD8 (disconnect DRAIN spam deferred) | SATISFIED | Disconnect arm UNCHANGED, pre-existing per Phase 0 #710 |
| DD9 (flush() always-pub) | SATISFIED | pub fn flush(&self) unconditional |
| DD10 (Phase 0 probes retained) | SATISFIED | Lines 924/970, PASS on both hosts |

---

## Smoke Validation

| Host | Tests | PASS | FAIL | Net delta vs baseline | Gate |
|------|-------|------|------|----------------------|------|
| Host A (Intel QSV) | 20 | 17 | 3 | 10/18 to 17/20 (+5 single-frame + 2 probes) | PASS |
| Host B (NVENC, JDNHS) | 20 | 18 | 2 | 16/18 to 18/20 (+T8 on NVENC + 2 probes) | PASS |

Host A FAILs (3): mft_request_keyframe_marks_next_packet_as_keyframe (Timeout 3.760s), mft_keyframe_flag_cleared_after_idr_emitted (Timeout 3.798s), mft_set_bitrate_updates_encoder_without_restart (Timeout 3.716s). All carry-forward, codec_api desync Intel QSV-specific. Encoder thread alive throughout, clean timeout failure mode.

Host B FAILs (2): mft_request_keyframe_marks_next_packet_as_keyframe, mft_keyframe_flag_cleared_after_idr_emitted. Pre-existing NVENC NAL-type-5 detection bug per Slice 2 archive #699/#604.

Bonus: mft_set_bitrate_updates_encoder_without_restart PASSES on NVENC (Host B). Confirms codec_api desync is Intel QSV-specific.

---

## Findings

### CRITICAL
(none)

### WARNING (real)
(none)

### WARNING (theoretical/accepted)

W1 - DD6 FALLBACK applied: T6/T7/T8 carry-forward (Intel QSV codec_api desync)

Slice 3 attempted to restructure T6/T7/T8 per DD6. Host A smoke: still timeout ~3.7s. Root cause: apply_pending_codec_settings() at windows_mft.rs:1221 causes Intel QSV non-accepting state; ProcessInput() returns MF_E_NOTACCEPTING with ni_count > 0. Pre-existing latent bug; DD6 restructure exposed it. Reverted to master bodies (b7cdb6f). New v2 candidate: hw-encoder-mft-codec-api-counter-desync (M scope, Intel QSV-specific per #721).

W2 - ~12x COMMAND_DRAIN spam on channel-disconnect (pre-existing, benign)

Phase 0 trace #710: ~12 duplicate COMMAND_DRAINs over ~7ms after disconnect. Vendor ignores duplicates. Pre-existing. Explicitly deferred DD8. The flush() path uses swap(false) and does NOT inherit this.

W3 - R9 partial: T6/T7/T8 timeout on Host A (carry-forward)

R9 required 8/8 PASS. 5/8 now PASS. T6/T7/T8 fail with Timeout same as master baseline. R10 exception clause applies.

### SUGGESTION

S1 - Extend T6/T7/T8 recv_timeout from 3s to 5s when hw-encoder-mft-codec-api-counter-desync lands. Accounts for DRAIN latency ~250ms per Phase 0.

S2 - Consider graduating Phase 0 probes from #[ignore] to annotated smoke. Both pass consistently ~1s on both hosts.

---

## Task Completion

| Phase | Status |
|-------|--------|
| Phase 0 (T0.1-T0.6) | All DONE |
| Phase 1 RED (T1.1-T1.7) | All DONE |
| Phase 2 GREEN (T2.1-T2.10) | All DONE |
| Phase 3 Polish (T3.1-T3.3) | All DONE |
| Phase 4 Smoke (T4.1-T4.2) | DONE - transcripts #719 + #721 saved |
| Phase 5 Verify (T5.1) | DONE (this report) |
| Phase 6 Archive (T6.1-T6.5) | PENDING |

---

## Strict TDD Audit

| Checkpoint | Status |
|------------|--------|
| C1 RED exists before C2 GREEN | SATISFIED -- 0f33772 before 2af01f7 |
| RED compiles on C1 | SATISFIED -- cargo build clean per T1.4 |
| GREEN makes CI pass on C2 | SATISFIED -- 611 nextest GREEN per T2.7 |
| C3 is fmt-only | SATISFIED -- no logic changes |
| C4 revert is test-only | SATISFIED -- only windows_mft_encode.rs touched |

---

## LOC Budget

| File | Insertions | Deletions | Net |
|------|-----------|-----------|-----|
| crates/sm-infra/src/encode/windows_mft.rs | +42 | 0 | +42 |
| crates/sm-infra/tests/windows_mft_encode.rs | +147 | 0 | +147 |
| Total | +189 | 0 | +189 |

Production code (windows_mft.rs): +42 ins -- within AC-8 budget 50 LOC. Test growth (+147) = Phase 0 probes (+121 C0) + 5 flush() call sites + carry-forward comments on T6/T7/T8.

---

## Carry-Forward Items

1. T6/T7/T8 Intel QSV multi-phase tests - codec_api desync at windows_mft.rs:1221/1266. apply_pending_codec_settings() triggers non-accepting state; ProcessInput() returns MF_E_NOTACCEPTING with ni_count > 0. Intel QSV-specific (T8 PASSES on NVENC per #721). New v2 candidate: hw-encoder-mft-codec-api-counter-desync (M scope).

2. NVENC keyframe-flag detection (2 tests Host B) - pre-existing Slice 2. NAL-type-5 parsing issue. Already tracked as hw-encoder-mft-nvenc-keyframe-flag (Slice 4, M scope).

3. Channel-disconnect DRAIN spam (~12x COMMAND_DRAIN at shutdown) - pre-existing, benign. Optional cleanup: hw-encoder-mft-disconnect-drain-once (XS scope, ~3 LOC).

---

## Recommendation

Proceed to sdd-archive.

PR title: feat(infra): add flush() to WindowsMftH264Encoder for short-stream output

Post-archive: Open new v2 candidate hw-encoder-mft-codec-api-counter-desync (Intel QSV codec_api desync, M scope, Intel QSV-specific).

---

## SDD Chain Anchors
- Predecessor: PR #18 (hw-encoder-mft-vendor-compat-rework, Slice 2, archive #699, master daa9522)
- This slice: hw-encoder-mft-single-frame-flush (Slice 3), branch feat/hw-encoder-mft-single-frame-flush
- Successor (blocked on this): hw-encoder-mft-nvenc-keyframe-flag (Slice 4)
- Next new candidate post-archive: hw-encoder-mft-codec-api-counter-desync
