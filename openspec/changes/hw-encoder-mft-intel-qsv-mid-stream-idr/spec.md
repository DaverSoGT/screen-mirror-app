# Spec v2: hw-encoder-mft-intel-qsv-mid-stream-idr (Slice 5 — Intel QSV mid-stream forced IDR)

> Phase: SDD spec (REVISION v2 — supersedes v1 #777).
> Inputs: proposal v2 #776 (10 D-* decisions, Mechanism G locked), Phase 0 round 3 #783 (G PASS), Phase 0 rounds 1+2 #779/#780 (C/C-prime/A FAIL), Slice 4 archive #773 (APPROVED_WITH_CARRY_FORWARD), Slice 4 spec #738 v2 (R1–R16/S1–S18 pattern reference), sdd-init #186 v13.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/spec` UPSERT + `openspec/changes/hw-encoder-mft-intel-qsv-mid-stream-idr/spec.md` overwrite).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-09.
> Branch: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` @ `918447a` (off master `5130e87`).
> v1 → v2 delta: D-MECHANISM revised C → G; D-OWNERSHIP-REFACTOR, D-IMFACTIVATE-CLONE, D-RECREATE-SEQUENCE, D-API-SURFACE, D-TRAIT-IMPL, D-SLICE-4-CARRY-FORWARD, D-SCOPE-LATENCY, D-DOCSTRING-FIX, D-CLEANPOINT-DEPRECATION (strengthened) added; D-CODECAPI-POST-RECREATE deferred to design.

---

## 1. Inputs

| Source | Observation ID | Role |
|--------|---------------|------|
| Proposal v2 | #776 (UPSERT) | 10+ locked decisions — PRIMARY CONTRACT |
| Phase 0 round 3 | #783 | G PASS empirical evidence — behavioral contract anchor |
| Phase 0 round 1 | #779 | Mechanism C INVALID (regression evidence) |
| Phase 0 round 2 | #780 | Mechanism C-prime ENCODER_DIED; A no IDR (regression evidence) |
| Slice 4 archive | #773 | Carry-forward register, baselines (658+/664 Host A, 660+/664 Host B) |
| Slice 4 spec v2 | #738 | R/S structure pattern reference |
| sdd-init v13 | #186 | Strict TDD, BLOCKED_ON_SMOKE rule, project conventions |

---

## 2. Domain

This spec governs:

- `crates/sm-infra/src/encode/windows_mft.rs` — `pump_loop()` (ownership refactor + Mechanism G handler), `MftEncoderShared` (new `mft_activate_factory: IMFActivate` + `keyframe_recreate_pending: AtomicBool` fields), `request_keyframe_via_recreate()` new public method, `VideoEncoder::request_keyframe()` trait impl, deletion of `MFSampleExtension_CleanPoint` calls from `submit_frame()`, deletion of `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` from `fire_pending_codec_settings()`, deletion of `CodecApiSwap.force_keyframe: bool`, `flush()` docstring update.
- `crates/sm-infra/tests/windows_mft_encode.rs` — Phase 0 round 1+2 probes (retained `#[ignore]`), Phase 0 round 3 probe (retained `#[ignore]`), GREEN bodies for T7.1 and T7.2 (Slice 4 carry-forward).

Explicitly excluded:

- `crates/sm-domain/src/encode.rs` — FROZEN (R12)
- `crates/sm-infra/Cargo.toml` — `default = []` unchanged
- Any other crate
- NVENC keyframe-flag detection bug (`hw-encoder-mft-nvenc-keyframe-flag`, Slice 6)
- DRAIN spam cleanup (`hw-encoder-mft-disconnect-drain-once`, deferred XS)
- D-CODECAPI-POST-RECREATE concrete implementation (deferred to design)

---

## 3. Frozen Surfaces

The following surfaces MUST remain at master `5130e87` baseline with no behavioral regression:

| Surface | Constraint |
|---------|-----------|
| `sm_domain::VideoEncoder` trait (`crates/sm-domain/src/encode.rs`) | FROZEN — Slice 2/3/4 DD9 inheritance. Zero diff against master. |
| `crates/sm-infra/Cargo.toml` features | `default = []` unchanged. `hw-encoder` remains opt-in. |
| NVENC behavior (Host B) | MUST remain at-least-as-correct-as-today. T8.2 set_bitrate, 30-frame smoke, T1–T5 MUST continue PASS on Host B. |
| Slice 4 DD14 F1 drain-state guard | `draining: bool` stack-local at pump_loop; GUARD-BEFORE-SWAP ordering; SET on COMMAND_DRAIN; CLEAR on METransformDrainComplete. MUST NOT be disturbed. |
| Slice 4 DD17 F2 BEGIN_STREAMING+START_OF_STREAM resume | DrainComplete handler preserved unchanged — G's `setup_mft` issues equivalent messages on the NEW handle, not the F2 path. |
| Slice 4 T8.2 SWAP-FIRE set_bitrate path | `mft_set_bitrate_updates_encoder_without_restart` MUST continue PASS on Intel QSV AND NVENC. No drain cycle for set_bitrate. |
| `debug_assert!` at `windows_mft.rs` approx. line 1266 | PRESERVED and sound under G — G recreate uses a separate `keyframe_recreate_pending` branch; NeedInput credits during the G drain window are discarded by the F1 guard. |
| Phase 0 round 1+2 probes | RETAINED as `#[ignore]`-gated regression evidence (Mechanism C/C-prime/A INVALID). MUST NOT be deleted. |

---

## 4. Functional Requirements (R1–R18)

### R1 — Public API: `request_keyframe_via_recreate()` (D-API-SURFACE)

**Statement**: `WindowsMftH264Encoder` MUST expose `pub fn request_keyframe_via_recreate(&self)` that atomically arms `keyframe_recreate_pending: AtomicBool` with `store(true, Release)`. The name is explicit about mechanism cost (~9ms tear-down + drain in-flight batch + ~50–300ms first-encode latency). Callers MUST NOT expect sub-frame-latency IDR.

**Rationale**: D-API-SURFACE (proposal v2 #776).

**Scenarios**: S1, S3

---

### R2 — Trait routing: `VideoEncoder::request_keyframe()` → G (D-TRAIT-IMPL)

**Statement**: The `VideoEncoder::request_keyframe()` trait impl on `WindowsMftH264Encoder` MUST call `self.request_keyframe_via_recreate()`. This eliminates the trait→production divergence flagged in Slice 4 carry-forward. No other production path MAY arm `keyframe_recreate_pending` except `request_keyframe_via_recreate()`.

**Rationale**: D-TRAIT-IMPL (proposal v2 #776).

**Scenarios**: S1, S2

---

### R3 — Idempotent atomic: multiple in-flight requests collapse to one recreate (D-SLICE-4-CARRY-FORWARD)

**Statement**: Multiple `request_keyframe_via_recreate()` calls arriving before the pump_loop observes `keyframe_recreate_pending` MUST coalesce into a single recreate cycle. The atomic MUST be consumed (`swap(false, AcqRel)`) exactly once per recreate cycle. After the cycle the flag MUST be `false`; subsequent calls start a new independent cycle.

**Rationale**: D-SLICE-4-CARRY-FORWARD — inherits Slice 4 R5 exactly-once swap semantics.

**Scenarios**: S2, S3

---

### R4 — Mechanism G recreate sequence — event order (D-RECREATE-SEQUENCE)

**Statement**: When pump_loop observes `keyframe_recreate_pending=true`, it MUST execute the following ordered sequence on the current `IMFTransform` handle, then replace it:

1. Send `MFT_MESSAGE_COMMAND_END_OF_STREAM`
2. Send `MFT_MESSAGE_COMMAND_DRAIN`
3. Wait for `METransformDrainComplete` (drains the in-flight batch; F1 guard discards NeedInput credits during this window)
4. Send `MFT_MESSAGE_NOTIFY_END_STREAMING`
5. Drop the current `IMFTransform` (and its COM-cast `ICodecAPI` + `IMFMediaEventGenerator`)
6. Call `mft_activate_factory.ActivateObject::<IMFTransform>()` (second call on same factory)
7. Execute `setup_mft()` on the new handle (this internally issues FLUSH + set media types + BEGIN_STREAMING + START_OF_STREAM)
8. Re-cast `ICodecAPI` and `IMFMediaEventGenerator` from the new `IMFTransform`
9. Reset counters + `keyframe_recreate_pending=false` + `draining=false`
10. Continue pump_loop on new handle

The first frame submitted after step 10 MUST be emitted as IDR (`is_keyframe=true`). Empirically validated: round 3 #783 L524→L531 tear-down 9ms; L642 post-recreate pkt 0 `is_keyframe=true`.

**Rationale**: D-RECREATE-SEQUENCE (proposal v2 #776). Empirical anchor: Phase 0 round 3 #783.

**Scenarios**: S4, S5, S9, S11

---

### R5 — pump_loop ownership refactor (D-OWNERSHIP-REFACTOR)

**Statement**: `pump_loop` MUST own `IMFTransform`, `ICodecAPI`, and `IMFMediaEventGenerator` by value (was: borrowed `&IMFTransform`). This is a required precondition for G drop+replace. Non-G code paths (normal encode, set_bitrate, flush) MUST have identical behavior to master `5130e87` — no correctness regression on non-G paths.

**Rationale**: D-OWNERSHIP-REFACTOR (proposal v2 #776). Structurally present at branch tip `918447a`.

**Scenarios**: S6, S7, S14

---

### R6 — IMFActivate clone strategy (D-IMFACTIVATE-CLONE)

**Statement**: `MftEncoderShared` MUST store `mft_activate_factory: IMFActivate`, obtained by AddRef-cloning (via windows-rs `.clone()`) the winning activate before transferring to the encoder thread. The encoder thread MUST call `ActivateObject` a second time on this same factory object for step 6 of the G recreate sequence. `winning_activate.take()` (consuming the Option) MUST be replaced by the clone approach. Empirically validated: 2nd `ActivateObject` on same factory returns `Ok(())` on Intel QSV (no `E_UNEXPECTED`). Round 3 #783 disproves the primary risk from re-explore #781.

**Rationale**: D-IMFACTIVATE-CLONE (proposal v2 #776). Empirical anchor: #783 L1179 `encoder_died=false`.

**Scenarios**: S10, S11

---

### R7 — First-frame-post-recreate IDR guarantee (D-RECREATE-SEQUENCE)

**Statement**: The first encoded packet produced after the G recreate sequence MUST have `is_keyframe=true`. This is guaranteed by the H.264 spec: the first frame of any newly initialized encoder MUST be IDR. `setup_mft`'s canonical setup sequence (FLUSH + media types + BEGIN_STREAMING + START_OF_STREAM) on the fresh handle produces this guarantee vendor-uniformly. No CleanPoint or ICodecAPI call is needed post-recreate to achieve the IDR.

**Rationale**: D-MECHANISM G (proposal v2 #776). Empirical confirmation: #783 L642 `is_keyframe=true len=8356`, `keyframe_indices=[0]`.

**Scenarios**: S4, S5, S9

---

### R8 — Encoder survives recreate; no ENCODER_DIED (D-RECREATE-SEQUENCE)

**Statement**: After the G recreate sequence, `encoder_died` MUST be `false`. The `pump_loop` MUST continue normally on the new handle. `STREAM_CHANGE` event MUST NOT be emitted post-recreate (types are identical; Intel QSV does not renegotiate when types match). `MFT_E_TRANSFORM_TYPE_NOT_SET` MUST NOT occur (`setup_mft` re-derives types from `EncoderConfig`).

**Rationale**: Empirical: #783 L1179 `encoder_died=false`; no STREAM_CHANGE post-recreate; no `MFT_E_TRANSFORM_TYPE_NOT_SET`.

**Scenarios**: S11

---

### R9 — Latency profile documented; eventually-style test assertions (D-SCOPE-LATENCY)

**Statement**: `request_keyframe_via_recreate()` MUST carry a doc comment documenting the latency model: (a) drain of in-flight batch (batch-size-dependent), (b) ~9ms tear-down + recreate (empirical round 3), (c) ~50–300ms first-encode latency. Tests for T7.1/T7.2 MUST use eventually-style assertions (recv within batch boundary, not next-packet-immediate). `recv_timeout` in these tests MUST be set to accommodate the full G latency window.

**Rationale**: D-SCOPE-LATENCY (proposal v2 #776).

**Scenarios**: S4, S5

---

### R10 — CleanPoint and ForceKeyFrame DELETED from production (D-CLEANPOINT-DEPRECATION)

**Statement**: The following call sites MUST be DELETED from production code in this slice:

- `MFSampleExtension_CleanPoint=1` from `submit_frame()` (approximately `windows_mft.rs:~1475`)
- `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` from `fire_pending_codec_settings()` (approximately `windows_mft.rs:~1072`)
- `CodecApiSwap.force_keyframe: bool` field

**Justification**: G is the ONLY mid-stream IDR path. CleanPoint is a no-op on Intel QSV (empirical: Slice 4 P0-B). The "defensive for NVENC" justification is removed because the `VideoEncoder::request_keyframe()` trait now uniformly routes through G. NVENC keyframe-flag detection is a separate slice (`hw-encoder-mft-nvenc-keyframe-flag`).

**Rationale**: D-CLEANPOINT-DEPRECATION strengthened (proposal v2 #776).

**Scenarios**: S13, S14

---

### R11 — T7.1 GREEN: `mft_request_keyframe_marks_next_packet_as_keyframe` (D-SLICE-4-CARRY-FORWARD)

**Statement**: `mft_request_keyframe_marks_next_packet_as_keyframe` (Slice 4 carry-forward, currently `#[ignore]` on Intel QSV branch) MUST PASS on Intel QSV (Host A) after C2 GREEN commit, with G semantics: batch-push priming → flush → drain → `request_keyframe()` → push 1 IDR-target → flush → recv within eventually-style window → assert `is_keyframe=true`. `#[ignore]` and CARRY-FORWARD comments MUST be removed from the Intel QSV branch in commit C1.

NVENC (`#[ignore]`) remains gated on `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6) — NVENC T7.1 MUST remain `#[ignore]` with a carry-forward note pointing to that slice.

**Rationale**: D-SLICE-4-CARRY-FORWARD (proposal v2 #776). Primary deliverable.

**Scenarios**: S4

---

### R12 — T7.2 GREEN: `mft_keyframe_flag_cleared_after_idr_emitted` (D-SLICE-4-CARRY-FORWARD)

**Statement**: `mft_keyframe_flag_cleared_after_idr_emitted` (Slice 4 carry-forward, currently `#[ignore]` on Intel QSV branch) MUST PASS on Intel QSV (Host A) after C2 GREEN commit: forced IDR has `is_keyframe=true`; next packet has `is_keyframe=false`. `#[ignore]` and CARRY-FORWARD comments MUST be removed from the Intel QSV branch in commit C1.

NVENC version MUST remain `#[ignore]` (separate slice).

**Rationale**: D-SLICE-4-CARRY-FORWARD (proposal v2 #776).

**Scenarios**: S5

---

### R13 — Phase 0 probes retained as `#[ignore]`-gated regression evidence (D-PHASE0 / DD7 convention)

**Statement**: ALL Phase 0 probes across all three rounds MUST be retained in `crates/sm-infra/tests/windows_mft_encode.rs`, gated `#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV)"]`:

**Round 1 (Mechanism C — INVALID):**
- `phase0_intel_qsv_idr_via_drain_resume_first_frame_is_idr`
- `phase0_intel_qsv_idr_via_drain_resume_latency_measure`

**Round 2 (Mechanisms C-prime/A — INVALID):**
- `phase0_intel_qsv_idr_via_flush_begin_start_first_frame_is_idr` (or equivalent C-prime probe)
- (A-mechanism probe if present)

**Round 3 (Mechanism G — PASS / load-bearing gate):**
- `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`

Round 3 probe is the LOAD-BEARING PASS gate. Rounds 1+2 probes are regression evidence (C/C-prime/A invalid). MUST NOT delete any round.

**Rationale**: D-PHASE0 / D-PROBES-RETENTION (proposal v2 #776). Slice 3 DD10 / Slice 4 DD7 convention.

**Scenarios**: S8, S9

---

### R14 — `flush()` docstring updated (D-DOCSTRING-FIX)

**Statement**: The `flush()` inherent docstring at approximately `windows_mft.rs:~1696` MUST be updated in commit C2 to reflect the post-Slice-4 DD17/F2 + Slice 5 G reality:

> `flush()` drains in-flight samples via `MFT_MESSAGE_COMMAND_DRAIN` and resumes the stream via Slice 4 DD17/F2 (BEGIN_STREAMING + START_OF_STREAM after DrainComplete). For forced mid-stream IDR, use `request_keyframe_via_recreate()` (Slice 5 D-MECHANISM Mechanism G), which performs a full tear-down + recreate of the `IMFTransform` handle.

**Rationale**: D-DOCSTRING-FIX (proposal v2 #776). Stale since Slice 4 DD17/F2; further stale under G.

**Scenarios**: S15

---

### R15 — Zero regression: full test suite baseline maintained

**Statement**: All tests passing at master `5130e87` MUST continue to pass after this slice:

| Test set | Hosts | Expectation |
|----------|-------|-------------|
| Slice 3 single-frame tests T1–T5 | Host A | PASS |
| `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | Host A + Host B | PASS on both vendors |
| Slice 3 Phase 0 probes | Host A | Runnable, valid trace output |
| Slice 4 Phase 0 probes (P0-A + P0-B) | Host A | Runnable, no panic |
| T8.2 `mft_set_bitrate_updates_encoder_without_restart` | Host A + Host B | PASS |
| Host A total | Host A | ≥ 658/664 (Slice 4 baseline maintained) |
| Host B total | Host B | ≥ 660/664 (Slice 4 baseline maintained) |

**Rationale**: Proposal AC-2 / AC-5. Slice 4 archive baselines.

**Scenarios**: S6, S7, S8, S9

---

### R16 — sm-domain FROZEN; no vendor detection introduced

**Statement**: `crates/sm-domain/src/encode.rs` SHALL have an empty diff relative to master `5130e87`. The `VideoEncoder` trait MUST NOT gain new methods, lose existing methods, or have signatures changed. This slice MUST NOT introduce `is_intel_qsv: bool`, CLSID/friendly-name retention for branching, or vendor-conditional production code paths. G is vendor-uniform.

**Rationale**: D-PRESERVE-DD1-DD17 DD9 / D-MECHANISM (vendor-uniform) (proposal v2 #776).

**Scenarios**: S14

---

### R17 — Strict TDD commit cadence (C0/C1/C2/C3)

**Statement**: The branch MUST be developed in this exact order:

- **C0 (PROBES)**: Round 3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` added `#[ignore]`-gated; plus structural ownership refactor scaffold if needed. No production G handler yet. Compile passes.
- **C1 (RED)**: Removes `#[ignore]` + CARRY-FORWARD from T7.1/T7.2 Intel QSV branch; installs G-semantics assertion bodies (eventually-style). T7.1/T7.2 FAIL (G handler not yet in pump_loop). Compile passes.
- **C2 (GREEN)**: Implements G handler in pump_loop + deletes CleanPoint + deletes ForceKeyFrame ICodecAPI + updates `flush()` docstring. T7.1/T7.2 PASS on Host A. No previously-passing test regresses.
- **C3 (POLISH — optional)**: `cargo fmt`, `cargo clippy` fixes only if needed.

No squash of C1+C2. C0 precedes C1 precedes C2.

**Rationale**: D-PROBES-RETENTION / Strict TDD per sdd-init #186 v13. Inherits Slice 4 R15.

**Scenarios**: S15, S16

---

### R18 — Single-PR cohesion; LOC forecast

**Statement**: All changes MUST land in a single PR on branch `feat/hw-encoder-mft-intel-qsv-mid-stream-idr`. LOC forecast: ~600–800 net new vs master `5130e87` (realistic); hard cap 1000 with pre-locked PR-A/PR-B split path (D-DELIVERY). If Review Workload Guard arbitrates against single PR, split: PR-A pure ownership refactor (~300 LOC) + PR-B mechanism+tests (~400–500 LOC).

**Rationale**: D-DELIVERY / D-LOC-FORECAST (proposal v2 #776).

**Scenarios**: S15

---

## 5. Acceptance Scenarios (S1–S16)

### S1 — `request_keyframe_via_recreate()` exists and arms atomic correctly

- GIVEN `WindowsMftH264Encoder` on any host with `hw-encoder` feature enabled
- WHEN `encoder.request_keyframe_via_recreate()` is called
- THEN `keyframe_recreate_pending` reads `true` (acquire) immediately after the call; no other method arms this atomic; compile check passes

**Mapped requirements**: R1, R2

---

### S2 — Multiple in-flight requests coalesce to one recreate

- GIVEN `pump_loop` is running normally (encoding frames)
- WHEN `request_keyframe_via_recreate()` is called three times before `pump_loop` observes the flag
- THEN exactly one G recreate cycle executes; `keyframe_recreate_pending` is `false` after the cycle; the post-recreate batch produces exactly one leading IDR packet (`keyframe_indices=[0]`), not three

**Mapped requirements**: R1, R3

---

### S3 — `request_keyframe_via_recreate()` before first encode is graceful (no-op or deferred)

- GIVEN encoder created but no frames submitted yet (pump_loop in initial idle state)
- WHEN `request_keyframe_via_recreate()` is called
- THEN either (a) the call is silently no-op'd (G handler sees no in-flight frames to drain), or (b) the recreate cycle executes harmlessly producing no error; subsequent frame encode produces IDR as normal first frame; encoder does NOT die

**Mapped requirements**: R1, R3, R8

---

### S4 — T7.1 GREEN on Intel QSV (Host A): post-recreate first packet is keyframe

- GIVEN C2 (GREEN) commit; Host A (Intel QSV); `#[ignore]` removed from T7.1 Intel QSV branch
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder -E 'test(mft_request_keyframe_marks_next_packet_as_keyframe)'`
- THEN Test PASSES; forced IDR packet `is_keyframe=true`; no timeout within eventually-style recv window; no pump-thread panic; G latency (drain + ~9ms recreate + first-encode) absorbed by test timeout

**Mapped requirements**: R7, R9, R11

---

### S5 — T7.2 GREEN on Intel QSV (Host A): keyframe flag cleared after IDR emitted

- GIVEN C2 (GREEN) commit; Host A; `#[ignore]` removed from T7.2 Intel QSV branch
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder -E 'test(mft_keyframe_flag_cleared_after_idr_emitted)'`
- THEN Test PASSES; first post-recreate packet `is_keyframe=true`; second post-recreate packet `is_keyframe=false`; exactly-once semantics confirmed

**Mapped requirements**: R3, R7, R12

---

### S6 — T8.2 still GREEN cross-vendor: set_bitrate unaffected by G refactor

- GIVEN C2 (GREEN) commit; Host A and Host B
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder -E 'test(mft_set_bitrate_updates_encoder_without_restart)'` on each host
- THEN Test PASSES on BOTH hosts; `set_bitrate(8_000_000)` returns `Ok(())`; all recv calls succeed; encoder alive; `debug_assert!` at approx. line 1266 does NOT fire

**Mapped requirements**: R5, R15

---

### S7 — Slice 3 single-frame tests T1–T5 still GREEN (Host A)

- GIVEN C2 (GREEN) commit; Host A
- WHEN Run 5 Slice-3 single-frame tests
- THEN All 5 PASS; no timeout; no regression from ownership refactor or G changes

**Mapped requirements**: R15

---

### S8 — 30-frame smoke GREEN cross-vendor

- GIVEN C2 (GREEN) commit; Host A and Host B
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder -E 'test(mft_thirty_frame_smoke_emits_at_least_one_keyframe)'` on each host
- THEN At least 1 IDR + at least 10 P-frames within deadline on BOTH hosts

**Mapped requirements**: R15

---

### S9 — Phase 0 round 3 probe passes as regression gate

- GIVEN C2 (GREEN) commit; Host A; `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` present `#[ignore]`-gated
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder --run-ignored=ignored-only -E 'test(phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr)' -- --test-threads=1`
- THEN Probe passes: `is_keyframe=true` at post-recreate pkt 0; `keyframe_indices=[0]`; `encoder_died=false`; tear-down time ≤ 20ms; no `E_UNEXPECTED` on 2nd ActivateObject

**Mapped requirements**: R4, R6, R7, R8, R13

---

### S10 — 2nd ActivateObject on same factory succeeds (no E_UNEXPECTED)

- GIVEN G handler executing the recreate sequence (step 6 in R4)
- WHEN `mft_activate_factory.ActivateObject::<IMFTransform>()` is called for the second time
- THEN Returns `Ok(new_mft)`; no `E_UNEXPECTED (0x8000FFFF)`; new `IMFTransform` is usable; `setup_mft()` succeeds without `MFT_E_TRANSFORM_TYPE_NOT_SET`

**Mapped requirements**: R6

---

### S11 — Encoder does not die through tear-down + recreate cycle

- GIVEN G handler completes the full sequence (steps 1–10 in R4)
- WHEN Encoding resumes on the new handle
- THEN `encoder_died=false`; `pump_loop` continues normally; no `STREAM_CHANGE` event emitted; frame channel remains live

**Mapped requirements**: R5, R6, R8

---

### S12 — NVENC T7.1/T7.2 remain `#[ignore]` (no behavioral change on NVENC)

- GIVEN C2 (GREEN) commit; Host B
- WHEN `cargo nextest run --workspace --features sm-infra/hw-encoder --run-ignored=ignored-only -E 'test(/^mft_(request_keyframe_marks|keyframe_flag_cleared)/)' -- --test-threads=1` (or: check that the NVENC-path versions remain gated)
- THEN NVENC versions of T7.1/T7.2 remain `#[ignore]` with carry-forward note pointing to `hw-encoder-mft-nvenc-keyframe-flag`; no NVENC regression from CleanPoint / ForceKeyFrame deletion (G vendor-uniform)

**Mapped requirements**: R11, R12, R16

---

### S13 — CleanPoint + ForceKeyFrame call sites deleted; `force_keyframe` field removed

- GIVEN C2 (GREEN) commit
- WHEN `grep -n "CleanPoint\|ForceKeyFrame\|force_keyframe" crates/sm-infra/src/encode/windows_mft.rs`
- THEN Zero production occurrences of `MFSampleExtension_CleanPoint=1` assignment in `submit_frame()`; zero `SetValue(CODECAPI_AVEncVideoForceKeyFrame)` in `fire_pending_codec_settings()`; `force_keyframe` field absent from `CodecApiSwap`

**Mapped requirements**: R10

---

### S14 — sm-domain diff empty; no vendor detection in production paths

- GIVEN Any commit on `feat/hw-encoder-mft-intel-qsv-mid-stream-idr`
- WHEN `git diff 5130e87 -- crates/sm-domain/` AND grep production sources for `is_intel_qsv`, CLSID runtime comparison, vendor-conditional branches
- THEN (a) diff = 0 lines; (b) zero matches for vendor-detection patterns in production code

**Mapped requirements**: R5, R16

---

### S15 — `flush()` docstring updated; commit sequence verifiable; single-PR delivered

- GIVEN C2 (GREEN) commit
- WHEN Read `windows_mft.rs` at approx. line 1696 AND `git log --oneline feat/hw-encoder-mft-intel-qsv-mid-stream-idr`
- THEN (a) Docstring references `request_keyframe_via_recreate()` and describes G; no stale "terminal per session" or "do not call mid-stream" language; (b) commit sequence: C0 → C1 → C2; no broken-build intermediates; all changes in single PR

**Mapped requirements**: R14, R17, R18

---

### S16 — Strict TDD cadence: RED at C1, GREEN at C2

- GIVEN C1 commit (G handler NOT in pump_loop; `#[ignore]` removed from T7.1/T7.2)
- WHEN `cargo build --workspace --features sm-infra/hw-encoder --locked` AND run T7.1/T7.2 on Host A
- THEN Compile succeeds; T7.1 and T7.2 FAIL (no IDR produced via G) — RED confirmed
- AND GIVEN C2 commit (G handler applied)
- WHEN Same run on Host A
- THEN T7.1 and T7.2 PASS — GREEN confirmed; all Phase 0 probes (rounds 1+2+3) still present with `#[ignore]`

**Mapped requirements**: R17

---

## 6. Test Mapping Table

| Scenario | Test name or action | File | BLOCKED_ON_SMOKE |
|----------|---------------------|------|-------------------|
| S1 | Compile check + API inspection | `windows_mft.rs` | N (structural) |
| S2 | Idempotency: 3x arm → 1x recreate | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A) |
| S3 | Pre-encode graceful call | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A) |
| S4 | `mft_request_keyframe_marks_next_packet_as_keyframe` (Intel QSV branch) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A) |
| S5 | `mft_keyframe_flag_cleared_after_idr_emitted` (Intel QSV branch) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A) |
| S6 | `mft_set_bitrate_updates_encoder_without_restart` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A + B) |
| S7 | 5 Slice-3 single-frame tests (T1–T5) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A) |
| S8 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A + B) |
| S9 | `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` (round 3) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y (Host A, `#[ignore]`) |
| S10 | ActivateObject 2nd call success — observable via S9 trace | `windows_mft_encode.rs` | Y (Host A, via S9) |
| S11 | `encoder_died=false` — observable via S9 trace | `windows_mft_encode.rs` | Y (Host A, via S9) |
| S12 | NVENC T7.1/T7.2 stay `#[ignore]` | `windows_mft_encode.rs` | N (structural + Host B smoke) |
| S13 | `grep` for deleted call sites | `windows_mft.rs` | N (structural) |
| S14 | `git diff` sm-domain + vendor-detection grep | git | N (structural) |
| S15 | Docstring read + `git log` sequence | `windows_mft.rs` + git | N (structural) |
| S16 | C1 RED compile+fail; C2 GREEN pass | cargo nextest + Host A | Y |

---

## 7. Out of Scope (explicit)

- NVENC mid-stream IDR mechanism — separate slice `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6)
- DRAIN spam cleanup — separate slice `hw-encoder-mft-disconnect-drain-once` (deferred XS)
- D-CODECAPI-POST-RECREATE concrete implementation (re-apply `pending_bitrate`/`pending_profile` vs. accept EncoderConfig defaults) — deferred to design DD
- pump_loop refactor for non-G code paths — ownership refactor ONLY; behavioral change required is zero on non-G paths
- MediaSDK / VPL integration (Mechanism F) — escalation path no longer needed (G PASS)
- Mechanisms A / B / C / C-prime / D / E / F — rejected per empirical rounds 1+2+3
- Sub-50ms IDR latency requirement — not a current product requirement
- `default = ["hw-encoder"]` feature flip — gated on Slice 5 + Slice 6 both landing

---

## 8. Open Questions (deferred to design)

| OQ | Description | Design action required |
|----|-------------|------------------------|
| OQ1 (D-CODECAPI-POST-RECREATE) | Re-apply `pending_bitrate` / `pending_profile` after G recreate, or accept reset to `EncoderConfig` defaults? ICodecAPI state is lost on `IMFTransform` drop. | Design DD must resolve with empirical or analytical justification. Critical for T8.2 / `set_bitrate` ↔ `request_keyframe` interleave correctness. |
| OQ2 (D-DRAIN-RACE) | `set_bitrate()` called concurrently with `request_keyframe_via_recreate()` — SWAP-FIRE pattern (Slice 4 DD1) still applies post-recreate? Does the G handler need to hold `draining=true` for its full duration to block SWAP? | Design DD must address concurrency invariants. |

---

## 9. Strict TDD Commit Cadence (C0/C1/C2/C3)

Each commit MUST produce a buildable state:

| Commit | Label | Contents | `cargo build` | T7.1/T7.2 (Host A) | Commit message pattern |
|--------|-------|----------|---------------|--------------------|------------------------|
| C0 | PROBES | Round 3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` retained `#[ignore]`; structural ownership refactor scaffold (pump_loop owns IMFTransform by value). | PASS | FAIL (unchanged) | `test(infra): Phase 0 round 3 — probe Mechanism G` |
| C1 | RED | Remove `#[ignore]` + CARRY-FORWARD from T7.1/T7.2 Intel QSV branch; install G-semantics eventually-style assertion bodies. No G handler in pump_loop yet. | PASS | FAIL (no IDR via G) | `test(infra): revert T7.1/T7.2 to master bodies — mid-stream IDR carry-forward` (aligned with branch tip `b4b3238`) |
| C2 | GREEN | G handler in pump_loop; delete CleanPoint; delete ForceKeyFrame ICodecAPI; `mft_activate_factory: IMFActivate` clone; `request_keyframe_via_recreate()` + trait impl; `flush()` docstring updated. T7.1/T7.2 PASS Host A. | PASS | PASS | `feat(infra): force IDR via IMFTransform recreate (Mechanism G)` |
| C3 | POLISH (opt) | `cargo fmt`, `cargo clippy` fixes if any. | PASS | PASS | `style(infra): cargo fmt for Mechanism G handler` |

**Invariants**: C0 precedes C1 precedes C2. No squash of C1+C2.

---

## 10. Acceptance Criteria Checklist

- [ ] AC-1: Host A T7.1 + T7.2 PASS on Intel QSV with G-semantics eventually-style assertions.
- [ ] AC-2: Host A ≥ 658/664 maintained (Slice 4 baseline).
- [ ] AC-3: Host B ≥ 660/664 maintained (Slice 4 baseline); NVENC T7.1/T7.2 remain `#[ignore]`.
- [ ] AC-4: T8.2 PASS on BOTH Host A and Host B.
- [ ] AC-5: `cargo nextest` GREEN; clippy `-D warnings` clean; fmt clean; build clean.
- [ ] AC-6: All 3 rounds of Phase 0 probes retained with `#[ignore]` post-fix.
- [ ] AC-7: ≤ 800 LOC realistic vs master `5130e87`; hard cap 1000 with split path.
- [ ] AC-8: sm-domain UNCHANGED — `git diff 5130e87 -- crates/sm-domain/` = 0 lines.
- [ ] AC-9: `default = []` unchanged in `crates/sm-infra/Cargo.toml`.
- [ ] AC-10: `flush()` docstring updated per R14.
- [ ] AC-11: `MFSampleExtension_CleanPoint=1` call DELETED from `submit_frame()`.
- [ ] AC-12: `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` call DELETED from `fire_pending_codec_settings()`.
- [ ] AC-13: `CodecApiSwap.force_keyframe: bool` field DELETED.
- [ ] AC-14: `request_keyframe_via_recreate()` exists; is the ONLY path arming `keyframe_recreate_pending`.
- [ ] AC-15: `VideoEncoder::request_keyframe()` trait impl routes to `request_keyframe_via_recreate()`.
- [ ] AC-16: `MftEncoderShared.mft_activate_factory: IMFActivate` present (AddRef-clone strategy, not `.take()`).

---

## 11. Risks

| Sev | Lik | Risk | Spec handling |
|-----|-----|------|---------------|
| HIGH | LOW | T7.1/T7.2 cadence mismatch with G latency | D-SCOPE-LATENCY (R9) mandates eventually-style; RED commit surfaces immediately. |
| HIGH | LOW | NVENC regression from CleanPoint + ForceKeyFrame deletion | Host B smoke AC-3; G vendor-uniform per setup-sequence. |
| MED | MED | LOC budget blow-up (~700–850 vs cap 800) | D-DELIVERY split path pre-locked (PR-A + PR-B). |
| MED | MED | D-CODECAPI-POST-RECREATE wrong choice regresses T8.2 | OQ1 must be resolved in design DD before apply. |
| MED | LOW | COM lifetime leak on multi-recreate cycles | Stress probe candidate at verify (10+ recreate cycles). |
| MED | LOW | Concurrent `set_bitrate()` during G recreate handler | OQ2; design DD must address `draining` scope for G. |
| MED | LOW | STREAM_CHANGE under different production cadence | Slice 2 handler absorbs on new handle (types identical). |
| LOW | LOW | docstring fix slips | R14 + AC-10 lock in same C2 commit. |

---

## 12. SDD Chain Anchors

- **Predecessor**: PR #20 / `8fa1a61` / Slice 4 archive #773. Master baseline: `5130e87`.
- **Branch baseline**: `918447a` (round 3 probe + structural ownership refactor).
- **This slice**: `hw-encoder-mft-intel-qsv-mid-stream-idr` (Slice 5). Branch: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` off `5130e87`.
- **Successors**: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6 candidate). `hw-encoder-default-on-flip` (gated on Slice 5 + Slice 6).
- **Optional XS follow-up**: `hw-encoder-mft-disconnect-drain-once`.
- **Engram chain**: explore #775 → re-explore #781 → proposal v1 #776 → Phase 0 rounds 1+2 #779/#780 → Phase 0 round 3 #783 → **proposal v2 #776 (UPSERT)** → **spec v2 (this)** → design (OQ1+OQ2 must resolve) → tasks → apply → verify → archive.
- **Supersedes**: spec v1 #777 (Mechanism C, now superseded by Mechanism G).
