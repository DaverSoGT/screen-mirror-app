# Verify Report (post-merge): hw-encoder-mft-rework

> See engram observation #603 (sdd/hw-encoder-mft-rework/verify-report) for full report.
> Generated: 2026-05-04. Artifact store: hybrid.

## Verdict: APPROVED_WITH_CARRY_FORWARD

0 CRITICAL | 2 WARNING (theoretical/accepted) | 3 SUGGESTION

## Branch / Commit Anchors (verified)

- Master HEAD: ee32ff4 (PR #16 merge 2026-05-04 17:44:13 UTC)
- C1: 8d1b341 | C2: 97d4d81 | C3: b0bfeec
- CI: 23/23 SUCCESS
- PR: https://github.com/DaverSoGT/screen-mirror-app/pull/16 (MERGED)

## Quality Gates on ee32ff4 (all 7 GREEN)

| Gate | Exit |
|------|------|
| cargo check --workspace | 0 |
| cargo clippy --workspace --all-targets --all-features -- -D warnings | 0 |
| cargo fmt --check --all (direct per discovery #581) | 0 |
| cargo nextest run --workspace (611 passed, 19 skipped) | 0 |
| cargo deny check (all ok) | 0 |
| cargo check -p sm-infra --no-default-features | 0 |
| cargo check -p sm-infra --features hw-encoder | 0 |

## Spec Compliance Matrix (R1-R13)

| Req | Smoke req | Status | Evidence |
|-----|-----------|--------|----------|
| R1 - NO_WAIT polling | yes | COMPLIANT | windows_mft.rs:800 GetEvent(MF_EVENT_FLAG_NO_WAIT); T-NEW-1 PASS Host A 848ms, Host B 255ms |
| R2 - Explicit counters | yes | COMPLIANT | windows_mft.rs:782-783 ni_count/ho_count; decrements AFTER COM calls per DD2 |
| R3 - HaveOutput drain FIRST | yes | COMPLIANT | windows_mft.rs:864-904 while ho_count>0 before while ni_count>0 |
| R4 - Stop within deadline | yes | COMPLIANT | top-of-loop stop line 793; FRAME_RECV_TIMEOUT=50ms line 115; T-NEW-1 PASS both hosts |
| R5 - POLLING_SLEEP=1ms | no | COMPLIANT | windows_mft.rs:107 const; no EncoderConfig field |
| R6 - DrainComplete arm | yes | COMPLIANT | windows_mft.rs:815-829; resets ni_count=ho_count=0; INFO log; no break |
| R7 - Tracing cadence | no | COMPLIANT | windows_mft.rs:971-984 change-only + heartbeat every 1000 iters |
| R8 - T-NEW-1 added | yes | COMPLIANT | tests/windows_mft_encode.rs:722; STOP_DEADLINE_MS=2000 inline |
| R9 - T-NEW-2 added | yes | COMPLIANT | tests/windows_mft_encode.rs:769; STOP_DEADLINE_MS=2000 inline |
| R10 - 16 existing tests intact | yes | COMPLIANT (partial) | All 16 present; no regression; 18/18 NOT achieved (Bug 1 out-of-scope, accepted by maintainer) |
| R11 - default=[] unchanged | no | COMPLIANT | Cargo.toml:14 |
| R12 - No public API changes | no | COMPLIANT | VideoEncoder + EncoderConfig unchanged; 611 nextest passed |
| R13 - No new unsafe surface | no | COMPLIANT | No IMFAsyncCallback (grep=0); unsafe count 36==36 baseline |

## Decisions Compliance (Proposal #595 - 9 decisions)

1 Pattern B: YES | 2 Fix B, reject Fix A: YES | 3 Single PR: YES
4 DrainComplete: YES (windows_mft.rs:815-829) | 5 POLLING_SLEEP=1ms: YES
6 T-NEW-1+T-NEW-2 only: YES | 7 NO_WAIT contingency: YES (not triggered)
8 Cargo default []: YES | 9 SDD chain #186: PARTIAL (archive phase)

## Design Compliance (Design #597 DD1-DD10 - all YES)

DD1: windows_mft.rs:790-986 | DD2: decrement AFTER COM; Timeout/Disconnected break
DD3: windows_mft.rs:815-829 | DD4: E_UNEXPECTED string-prefix 891-897
DD5: MF_E_NOTACCEPTING debug_assert 929-937 | DD6: POLLING_SLEEP const 107
DD7: deadline inline T-NEW-1 724, T-NEW-2 771 | DD8: TRACE+DEBUG ON CHANGE 809/971-980/983
DD9: apply_pending_codec_settings 725-749 | DD10: Cargo.toml unchanged

## Smoke Validation (all claims CONFIRMED vs PR body, no divergence)

Host A: master 6/16 -> branch 9/18 (+3 Bug 2 fix). 5 ABORT Manifestation A pre-existing.
Host B: master 6/16 -> branch 7/18 (+1 T-NEW-1 Bug 2 cross-vendor). No regression.
SetOutputType 0xC00D6D76 confirmed pre-existing (#601 vs #602).

## Tasks Compliance (#598)

Phase 0: COMPLETE | Phase 1 C1 8d1b341: COMPLETE
Phase 2 C2 97d4d81: COMPLETE (sub-task ticks coarse; tracking gap; no functional skip)
Phase 3 C3 b0bfeec: COMPLETE | Phase 4: COMPLETE (7/7 GREEN, PR #16 MERGED)

## Structural Checks

default=[] Cargo.toml: PASS (line 14)
No impl IMFAsyncCallback: PASS (grep=0)
unsafe count vs PR #15 baseline: PASS (36==36, zero net addition)
EncoderConfig fields unchanged: PASS | VideoEncoder trait unchanged: PASS

## Carry-Forward Items

1. hw-encoder-mft-vendor-compat-rework: Manifestation A+B; Phase 0 MUST include multi-host smoke+tracing. Ref: #600, #601, #602.
2. hw-encoder-default-on-flip: after 18/18 on >=2 vendors + 24hr soak. Ref: Decision #8.
3. #186 v2 row update: mark shipped/Bug 2 closed at archive. Ref: Decision #9.
4. SUGGESTION: future long apply phases emit per-sub-task status lines.

## Out-of-Scope Deferrals (accepted by maintainer via PR #16 merge)

Manifestation A: 0xC0000005 AV inside vendor ProcessOutput (Host A). Pre-exists in master f01f27f.
Manifestation B: SetOutputType 0xC00D6D76 rejection in setup_mft (Host B). Pre-exists in master f01f27f.
BLOCKED_ON_SMOKE gate satisfied: transcripts in engram #601 (branch) and #602 (master baseline).

## CRITICAL: None

## WARNING

W1 (theoretical/accepted): 18/18 not achieved; Bug 1 pre-existing out-of-scope; zero regression on either host.
W2 (theoretical/accepted): nextest.toml retains prior-change slow-timeout for fake_video_sender; benign (hw-encoder override removed in C2).

## SUGGESTION

S1: expect(clippy::too_many_arguments) at windows_mft.rs:751 correct per discovery #580.
S2: apply_pending_codec_settings() always in pump_loop scope; no dead_code risk.
S3: Phase 2 apply-progress coarse; future long phases should emit per-sub-task ticks.

## Result Contract

status: done
executive_summary: APPROVED_WITH_CARRY_FORWARD. 0 CRITICAL, 2 WARNING (theoretical/accepted), 3 SUGGESTION. All 7 quality gates GREEN on ee32ff4 (611 tests, 19 skipped). T-NEW-1 Bug 2 stop-starvation fix cross-vendor PASS (Host A 848ms, Host B 255ms). Bug 1 family deferred to hw-encoder-mft-vendor-compat-rework with maintainer acceptance via PR #16 merge.
artifacts: engram #603 sdd/hw-encoder-mft-rework/verify-report + openspec/changes/hw-encoder-mft-rework/verify-report.md
next_recommended: sdd-archive
risks: (1) Bug 1 family - no automated tests for setup_mft vendor failures. (2) 9/18 Host A single-host; Host B confirms Bug 2 cross-vendor. (3) Production on SW encoder until hw-encoder-default-on-flip.
skill_resolution: injected
