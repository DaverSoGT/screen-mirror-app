# Spec: hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Slice 6 R2)

> Phase: SDD spec.
> Branch: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` @ `efc0f36` (off master `c48ae46`).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/spec` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-10.
> Inputs: proposal #810 (D1–D14 + D-ENUM locked), P2 breakthrough #809, research #808, explore #803,
>         falsifications #800 #801 #807, Slice 5 spec #777 (structure template).

---

## 1. Domain

Governs:

- `crates/sm-infra/src/encode/windows_mft.rs` — `MftEncoderShared`, `pump_loop()`, `request_keyframe()` trait impl, `collect_output()`, `probe_and_select_mft()`.
- `crates/sm-infra/tests/windows_mft_encode.rs` — T7.1, T7.2, T8.2, Phase 0 probes.

Explicitly excluded:

- `crates/sm-domain/src/encode.rs` — FROZEN (no trait surface change).
- `crates/sm-infra/Cargo.toml` features — unchanged.
- Any crate outside `sm-infra`.
- Default-on feature flag flip (separate slice).
- AMD / Host C support.
- Disconnect drain-once cosmetic.

---

## 2. Empirical Evidence Anchors

| ID | Finding | Observation |
|----|---------|-------------|
| E1 | Mechanism G executes cleanly on NVENC but yields 29/29 P-frames — no IDR | #801 (C0.b) |
| E2 | NVENC priming IDR detected correctly (4-byte Annex-B, AUD 0x10) | #800 (C0) |
| E3 | CleanPoint=1 on input sample → NVENC yields 30/30 P-frames — no IDR | #807 (P1) |
| E4 | ForceKeyFrame BEFORE+VT_UI4 → NVENC IDR at idx 0; Intel QSV IDR at idx 1 | #809 (P2) |
| E5 | Chromium + FFmpeg use CODECAPI_AVEncVideoForceKeyFrame BEFORE ProcessInput, VT_UI4=1 | #808 |
| E6 | CODECAPI_AVEncVideoForceKeyFrame is HCK-mandated for Win8+ hardware encoder MFTs | #808 |

---

## 3. Functional Requirements (R1–R18)

### R1 — `request_keyframe()` MUST set `force_keyframe_icodecapi_pending` (D1, D4, #809)

The `VideoEncoder::request_keyframe()` trait impl on `WindowsMftH264Encoder` MUST atomically set `force_keyframe_icodecapi_pending: AtomicBool` via `store(true, Release)`. It MUST NOT perform any vendor dispatch and MUST NOT call any deleted path (Mechanism G, CleanPoint).

**Evidence**: D4 (vendor dispatch eliminated, #809); D1 (ForceKeyFrame locked).

**Scenarios**: S1, S2

---

### R2 — pump_loop NeedInput path MUST consume flag with `swap(false, AcqRel)` BEFORE `submit_frame()` (D1, D3, #809)

When `pump_loop` enters the NeedInput service path and `force_keyframe_icodecapi_pending` is `true`, it MUST:
1. Call `force_keyframe_icodecapi_pending.swap(false, AcqRel)` (one-shot consume, clears the flag).
2. Call `codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &make_variant_u32(1))` before any `submit_frame()` call for that iteration.

MUST NOT call SetValue after `ProcessInput` for the same frame (timing is BEFORE — D3).

**Evidence**: D3 (BEFORE ProcessInput); P2 #809 idx-0 NVENC / idx-1 Intel QSV; Chromium/FFmpeg #808.

**Scenarios**: S3, S4

---

### R3 — SetValue MUST use `VARIANT { vt: VT_UI4, ulVal: 1 }` (D2, #809)

`ICodecAPI::SetValue(&CODECAPI_AVEncVideoForceKeyFrame, ...)` MUST pass a `VARIANT` with `vt = VT_UI4` and `ulVal = 1`. `VT_BOOL` MUST NOT be used.

**Evidence**: D2; Slice 4 used VT_BOOL and was falsified; Chromium, FFmpeg, HCK all specify VT_UI4 (#808, #809).

**Scenarios**: S3

---

### R4 — SetValue rejection MUST be non-fatal; warn and continue (D13 retained from Slice 4)

If `codec_api.SetValue(...)` returns an `HRESULT` failure, the pump_loop MUST log a `WARN` and continue encoding. The error MUST NOT propagate as a fatal encoder failure. The frame MUST still be submitted.

**Evidence**: D13 (Slice 4 DD13 retained); hardware drivers may decline individual property sets without being in a broken state.

**Scenarios**: S5

---

### R5 — Mechanism G code path MUST be DELETED (D5, #801)

The following MUST be deleted:
- `request_keyframe_via_recreate()` method.
- `keyframe_recreate_pending: AtomicBool` field on `MftEncoderShared`.
- pump_loop Mechanism G handler block (drop+ActivateObject+setup_mft+resume).
- `mft_activate_factory: Option<IMFActivate>` field on `MftEncoderShared`.

**Evidence**: D5; C0.b probe #801 (G yields 0 IDR on NVENC); ForceKeyFrame replaces G vendor-uniformly.

**Scenarios**: S6, S7

---

### R6 — CleanPoint write path MUST be DELETED (D6, #807)

The following MUST be deleted:
- `request_keyframe_via_cleanpoint()` method.
- `cleanpoint_pending: AtomicBool` field on `MftEncoderShared`.
- pump_loop CleanPoint write block (the INPUT-write path in `submit_frame()` or its caller).

**Evidence**: D6; P1 probe #807 (CleanPoint yields 0 IDR on NVENC); CleanPoint write mechanism never validated on either vendor.

**Scenarios**: S6, S7

---

### R7 — `MFSampleExtension_CleanPoint` READ path in `collect_output` MUST remain unchanged (D8)

The output-side read of `MFSampleExtension_CleanPoint` in `collect_output` (used for IDR detection) MUST NOT be modified or deleted. This is a defense-in-depth attribute read, not a write path.

**Evidence**: D8; the READ path was always correct — only the INPUT WRITE path was wrong.

**Scenarios**: S8

---

### R8 — DD10 inline comment block MUST be DELETED and replaced (D7, #807, #808)

The comment block at `windows_mft.rs:1108-1110` (containing "Intel QSV does not honor mid-stream ICodecAPI ForceKeyFrame; NVENC honored CleanPoint instead") MUST be deleted. It MUST be replaced with a comment that:
- Cites P2 evidence (#809, observation ID).
- Cites Chromium/FFmpeg canonical sequence (#808).
- Documents the three architectural overclaims being corrected (Slice 4 VT_BOOL/AFTER; Slice 5 Mechanism G; Slice 5 DD10 CleanPoint claim).

**Evidence**: D7; #807 (P1 falsified the comment empirically); #809 (P2 established the correct claim).

**Scenarios**: S9

---

### R9 — `EncoderVendor` enum MUST be retained for INFO logging only; MUST NOT drive IDR mechanism dispatch (D-ENUM)

The `EncoderVendor` enum and GUID-based detection in `probe_and_select_mft` MUST be retained. It MUST be used for INFO/WARN logging only. It MUST NOT be used to select or dispatch the IDR mechanism. No `match vendor { ... }` branch MAY determine which `request_keyframe_*` path is called.

**Evidence**: D-ENUM (option b locked); diagnostic value retained; vendor dispatch eliminated (D4).

**Scenarios**: S10

---

### R10 — `request_keyframe()` doc-comment MUST document latency contract (D11)

The `///` doc-comment on `request_keyframe()` (or `request_keyframe_via_force_keyframe_icodecapi()` if exposed) MUST document:
- NVENC: IDR appears at idx 0 (~0ms latency, immediate).
- Intel QSV: IDR appears at idx 1 (~33ms at 30fps, 1 in-flight frame latency).
- Both are within `assert_keyframe_within_next_n_frames(30)` tolerance.

**Evidence**: D11; P2 traces #809 (NVENC idx 0, Intel QSV idx 1); replaces obsolete Slice 5 "60-310ms" Mechanism G figure.

**Scenarios**: S11

---

### R11 — T7.1 MUST PASS on BOTH Host A (Intel QSV) AND Host B (NVENC) (D10, D14)

`mft_request_keyframe_marks_next_packet_as_keyframe` MUST pass on both hosts with the ForceKeyFrame mechanism. The `#[ignore]` annotation MUST be removed from both Host A and Host B variants. The test tolerance MUST use `assert_keyframe_within_next_n_frames(30)`.

**Evidence**: D10 (no signature changes, internal routing); D14 (cross-vendor gate mandatory); #809 P2 evidence on both hosts.

**Scenarios**: S12, S13

---

### R12 — T7.2 MUST PASS on BOTH Host A AND Host B (D10, D14)

`mft_keyframe_flag_cleared_after_idr_emitted` MUST pass on both hosts. IDR packet `is_keyframe=true`; immediately following packet `is_keyframe=false` (flag consumed exactly once). `#[ignore]` MUST be removed from both variants.

**Evidence**: D10; D14; atomic one-shot consume guarantees exactly-once semantics.

**Scenarios**: S14, S15

---

### R13 — T8.2 MUST remain PASS cross-vendor (D14, Slice 4 carry-forward)

`mft_set_bitrate_updates_encoder_without_restart` MUST pass on both Host A and Host B. The SWAP-FIRE bitrate path MUST NOT be disturbed by any Slice 6 R2 change. No recreate cycle MUST occur on `set_bitrate()`.

**Evidence**: D14; SWAP-FIRE bitrate path is independent of the IDR mechanism; Slice 4 carry-forward.

**Scenarios**: S16

---

### R14 — Slice 6 R2 Phase 0 probes MUST be retained `#[ignore]`-gated (D13 probe retention)

All five Phase 0 probes MUST remain in `windows_mft_encode.rs` with `#[ignore]` after cleanup:
- `phase0_nvenc_idr_packet_format_dump` (P0 — C0)
- `phase0_nvenc_post_recreate_idr_format_dump` (P0.b — C0.b)
- `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute` (P1)
- `phase0_nvenc_force_keyframe_via_codecapi_before_processinput` (P2-NVENC)
- `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput` (P2-Intel)

They MUST compile and be `#[ignore]`-gated cleanly after Mechanism G and CleanPoint code is deleted.

**Evidence**: D13; #809 P2 probes are the empirical foundation for D1–D14.

**Scenarios**: S17

---

### R15 — Slice 5 round-3 probe MUST be DELETED (D9)

`phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` MUST be deleted. It directly invokes `request_keyframe_via_recreate()` which will no longer exist.

**Evidence**: D9; #810 §DELETE list; Slice 5 archive #791 retains historical record; P2-Intel probe is the regression-evidence successor.

**Scenarios**: S17

---

### R16 — `cargo clippy --all-targets --all-features --locked -- -D warnings` MUST be clean (D14)

After all deletions and additions, the workspace MUST produce zero clippy warnings. No unused imports, dead code, or unresolved items from the deleted paths MUST remain.

**Evidence**: D14 (pre-merge gate).

**Scenarios**: S18

---

### R17 — `cargo nextest run --workspace` MUST be GREEN (D14)

Full test suite MUST pass on both Host A and Host B (modulo pre-existing ignores documented as carry-forward). No test that was PASS before this slice MUST become FAIL.

**Evidence**: D14.

**Scenarios**: S18

---

### R18 — New CI-runnable unit test MUST verify `request_keyframe()` sets the atomic flag (proposal §Test inventory)

A new unit test (no hardware required, CI-runnable) MUST verify that calling `request_keyframe()` on a constructed `WindowsMftH264Encoder` sets `force_keyframe_icodecapi_pending` to `true`. A companion test MUST verify the flag defaults to `false` on construction. A third MUST verify `swap(false, AcqRel)` clears the flag (one-shot consume semantics).

**Evidence**: Proposal §4 "NEW unit tests"; these are CI-runnable and do not require BLOCKED_ON_SMOKE gating.

**Scenarios**: S19, S20, S21

---

## 4. Acceptance Scenarios (S1–S21)

### S1 — `request_keyframe()` sets flag; no vendor dispatch
- GIVEN `WindowsMftH264Encoder` constructed (hw-encoder feature)
- WHEN `encoder.request_keyframe()` is called
- THEN `force_keyframe_icodecapi_pending` reads `true` immediately
- AND no `match vendor` branch is taken in the production path

**Mapped**: R1, R9

---

### S2 — Multiple in-flight requests collapse to one SetValue
- GIVEN pump_loop running normally, not yet at NeedInput
- WHEN `request_keyframe()` is called 3x before pump_loop observes the flag
- THEN exactly one `SetValue(CODECAPI_AVEncVideoForceKeyFrame)` call is issued for those 3 requests (flag consumed once; subsequent calls re-arm)

**Mapped**: R1, R2

---

### S3 — SetValue uses VT_UI4 and fires BEFORE ProcessInput
- GIVEN `force_keyframe_icodecapi_pending = true` when NeedInput is next serviced
- WHEN pump_loop processes the frame
- THEN `SetValue(&CODECAPI_AVEncVideoForceKeyFrame, VARIANT { vt: VT_UI4, ulVal: 1 })` is called BEFORE `ProcessInput`
- AND flag is `false` after that iteration

**Mapped**: R2, R3

---

### S4 — IDR emitted within 30 frames post-request on both vendors
- GIVEN encoder encoding normally, priming IDR already emitted
- WHEN `request_keyframe()` is called and 30 subsequent frames are collected
- THEN NVENC: IDR at idx 0 (`is_keyframe=true`, AUD primary_pic_type=0x10)
- AND Intel QSV: IDR at idx 1 (`is_keyframe=true`, AUD primary_pic_type=0x10)

**Mapped**: R2, R10, R11, R12

---

### S5 — SetValue HRESULT failure is non-fatal
- GIVEN pump_loop about to fire SetValue
- WHEN `codec_api.SetValue(...)` returns a non-OK HRESULT (simulated or driver-level)
- THEN pump_loop logs WARN and continues; `submit_frame()` is still called; encoder remains alive; `encoder_died = false`

**Mapped**: R4

---

### S6 — Mechanism G code artifacts are absent from production code
- GIVEN the branch after all deletions
- WHEN `grep` for `keyframe_recreate_pending`, `request_keyframe_via_recreate`, `mft_activate_factory` in production sources
- THEN zero matches in non-test files

**Mapped**: R5

---

### S7 — CleanPoint write artifacts are absent from production code
- GIVEN the branch after all deletions
- WHEN `grep` for `cleanpoint_pending`, `request_keyframe_via_cleanpoint`, `MFSampleExtension_CleanPoint` (write context) in production sources
- THEN zero matches for write-path call sites in non-test files

**Mapped**: R6

---

### S8 — `MFSampleExtension_CleanPoint` READ in `collect_output` is unchanged
- GIVEN the branch after all deletions
- WHEN `collect_output` is called on a packet where the encoder set CleanPoint on output
- THEN `is_keyframe=true` is correctly detected via the output-side attribute read
- AND the read code path compiles and is unmodified relative to Slice 5 output

**Mapped**: R7

---

### S9 — DD10 comment replaced with empirical citation
- GIVEN `windows_mft.rs` at `1108-1110` region after fix
- WHEN the comment is read
- THEN no reference to "Intel QSV does not honor" or "NVENC honored CleanPoint instead"
- AND comment cites P2 (#809), Chromium/FFmpeg (#808), and the three corrected overclaims

**Mapped**: R8

---

### S10 — `EncoderVendor` enum used only for logging; no IDR dispatch
- GIVEN production code after cleanup
- WHEN `grep` for `EncoderVendor` in non-test production paths
- THEN all matches are `info!`, `warn!`, or `debug!` log call sites; zero matches in IDR request-keyframe dispatch

**Mapped**: R9

---

### S11 — `request_keyframe()` doc-comment documents latency contract
- GIVEN `windows_mft.rs` after fix
- WHEN the `///` doc-comment for `request_keyframe()` (or the ICodecAPI method) is read
- THEN it mentions NVENC idx 0 (~0ms), Intel QSV idx 1 (~33ms), and 30-frame tolerance

**Mapped**: R10

---

### S12 — T7.1 PASS on Host A (Intel QSV)
- GIVEN the branch tip; Host A; `#[ignore]` removed from T7.1 Intel QSV variant
- WHEN `cargo nextest run -E 'test(mft_request_keyframe_marks_next_packet_as_keyframe)'` on Host A
- THEN PASSES; `is_keyframe=true` within 30 post-request packets; no timeout

**Mapped**: R11

---

### S13 — T7.1 PASS on Host B (NVENC)
- GIVEN the branch tip; Host B; `#[ignore]` removed from T7.1 NVENC variant
- WHEN `cargo nextest run -E 'test(mft_request_keyframe_marks_next_packet_as_keyframe)'` on Host B
- THEN PASSES; NVENC IDR at idx 0; `is_keyframe=true`; no timeout

**Mapped**: R11

---

### S14 — T7.2 PASS on Host A (Intel QSV)
- GIVEN the branch tip; Host A; `#[ignore]` removed from T7.2 Intel QSV variant
- WHEN `cargo nextest run -E 'test(mft_keyframe_flag_cleared_after_idr_emitted)'` on Host A
- THEN PASSES; IDR `is_keyframe=true`; immediately following packet `is_keyframe=false`

**Mapped**: R12

---

### S15 — T7.2 PASS on Host B (NVENC)
- GIVEN the branch tip; Host B; `#[ignore]` removed from T7.2 NVENC variant
- WHEN `cargo nextest run -E 'test(mft_keyframe_flag_cleared_after_idr_emitted)'` on Host B
- THEN PASSES; IDR `is_keyframe=true`; next packet `is_keyframe=false`; flag consumed exactly once

**Mapped**: R12

---

### S16 — T8.2 PASS cross-vendor (bitrate path unaffected)
- GIVEN the branch tip; Host A and Host B
- WHEN `cargo nextest run -E 'test(mft_set_bitrate_updates_encoder_without_restart)'` on each host
- THEN PASSES on BOTH; encoder alive; no recreate cycle triggered by `set_bitrate()`

**Mapped**: R13

---

### S17 — Phase 0 probes compile and gate cleanly after cleanup
- GIVEN the branch after deleting Mechanism G, CleanPoint, and Slice 5 round-3 probe
- WHEN `cargo build --tests --features sm-infra/hw-encoder` compiles
- THEN all 5 retained Phase 0 probes compile with `#[ignore]`
- AND `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` is absent (deleted)
- AND running `cargo nextest run --run-ignored=ignored-only` for each retained probe PASSES (hardware present)

**Mapped**: R14, R15

---

### S18 — Clippy clean and nextest GREEN
- GIVEN the branch after all changes
- WHEN `cargo clippy --all-targets --all-features --locked -- -D warnings`
- THEN zero warnings
- AND WHEN `cargo nextest run --workspace`
- THEN no previously-passing test becomes FAIL; all pre-existing ignores documented

**Mapped**: R16, R17

---

### S19 — Unit test: flag defaults to `false` on construction (CI-runnable)
- GIVEN `WindowsMftH264Encoder` is constructed (no hardware required)
- WHEN `force_keyframe_icodecapi_pending` is read immediately after construction
- THEN value is `false`

**Mapped**: R18

---

### S20 — Unit test: `request_keyframe()` sets flag to `true` (CI-runnable)
- GIVEN `force_keyframe_icodecapi_pending = false` (initial state)
- WHEN `encoder.request_keyframe()` is called
- THEN `force_keyframe_icodecapi_pending.load(Acquire)` returns `true`

**Mapped**: R1, R18

---

### S21 — Unit test: `swap(false, AcqRel)` clears flag — one-shot consume (CI-runnable)
- GIVEN `force_keyframe_icodecapi_pending = true` (armed via `request_keyframe()`)
- WHEN `force_keyframe_icodecapi_pending.swap(false, AcqRel)` is called (simulating pump_loop NeedInput consume)
- THEN return value is `true` (was set)
- AND subsequent `load(Acquire)` returns `false` (consumed)

**Mapped**: R2, R18

---

## 5. Test Mapping Table

| Scenario | Test / Action | Host | CI-runnable | BLOCKED_ON_SMOKE |
|----------|---------------|------|-------------|-----------------|
| S1 | Structural: compile + API inspection | — | Y | N |
| S2 | Idempotency: 3x arm → 1x SetValue | — | Y | N |
| S3 | VT_UI4 + BEFORE ProcessInput: code inspection | — | Y | N |
| S4 | P2 empirical traces (#809) | A + B | N | Y |
| S5 | Non-fatal HRESULT: code inspection / mock | — | Y | N |
| S6 | `grep` Mechanism G artifacts absent | — | Y | N |
| S7 | `grep` CleanPoint write artifacts absent | — | Y | N |
| S8 | collect_output CleanPoint READ unchanged | A or B | N | Y |
| S9 | DD10 comment replaced: code inspection | — | Y | N |
| S10 | EncoderVendor logging-only: code inspection | — | Y | N |
| S11 | Doc-comment latency: code inspection | — | Y | N |
| S12 | T7.1 `mft_request_keyframe_...` | A | N | Y |
| S13 | T7.1 `mft_request_keyframe_...` | B | N | Y |
| S14 | T7.2 `mft_keyframe_flag_cleared_...` | A | N | Y |
| S15 | T7.2 `mft_keyframe_flag_cleared_...` | B | N | Y |
| S16 | T8.2 `mft_set_bitrate_...` | A + B | N | Y |
| S17 | Phase 0 probes compile + gate | A + B | Partial | Y |
| S18 | clippy + nextest GREEN | A + B | Partial | Y |
| S19 | Unit: flag default false | — | Y | N |
| S20 | Unit: `request_keyframe()` sets true | — | Y | N |
| S21 | Unit: swap consume one-shot | — | Y | N |

---

## 6. Frozen Surfaces

| Surface | Constraint |
|---------|-----------|
| `sm_domain::VideoEncoder` trait | FROZEN — zero diff vs master `c48ae46`. |
| `crates/sm-infra/Cargo.toml` features | `default = []` unchanged; `hw-encoder` remains opt-in. |
| `collect_output` CleanPoint READ | MUST NOT change (R7). |
| T8.2 SWAP-FIRE bitrate path | MUST continue PASS on Host A and Host B (R13). |
| Slice 3/4 Phase 0 probes (prior slices) | MUST NOT be touched. |

---

## 7. Deleted Code Register

| Symbol | Type | Reason | Proposal ref |
|--------|------|--------|-------------|
| `request_keyframe_via_recreate()` | method | Mechanism G replaced by ForceKeyFrame | D5 |
| `keyframe_recreate_pending: AtomicBool` | field | Mechanism G flag | D5 |
| pump_loop G handler block | code block | Mechanism G pump_loop handler | D5 |
| `mft_activate_factory: Option<IMFActivate>` | field | Only used by G recreate | D5 |
| `request_keyframe_via_cleanpoint()` | method | CleanPoint INPUT write falsified (P1, #807) | D6 |
| `cleanpoint_pending: AtomicBool` | field | CleanPoint flag | D6 |
| CleanPoint INPUT write in pump/submit | code block | CleanPoint write path | D6 |
| DD10 inline comment block (lines 1108-1110) | comment | Empirically wrong; replaced with P2+#808 citation | D7 |
| `EncoderVendor` dispatch arm in `request_keyframe()` | code | Vendor dispatch eliminated | D4 |
| Slice 5 DD4 post-recreate bitrate re-apply | code | Tied to G recreate (deleted) | D5 |
| Slice 5 DD9 trait routing comment | comment | G routing comment obsolete | D5 |
| `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` | test | Tests deleted code | D9 |

---

## 8. Risks

| Sev | Lik | Risk | Spec handling |
|-----|-----|------|--------------|
| MED | LOW | Driver variance on ForceKeyFrame — future driver breaks HCK mandate | R14 retains P2 probes as regression suite; `EncoderVendor` enum retained for surgical reintroduction if needed (R9). |
| MED | LOW | Mechanism G deletion blast radius — T8.2 or Slice 3/4 tests regress | R13 + R17 mandate cross-vendor nextest gate pre-merge (D14). |
| LOW | LOW | Intel QSV idx-1 latency surprise in production | R10 doc-comment + 30-frame test tolerance; production callers consume in arrival order. |

---

## 9. Slice 5 Archive Corrigendum (Slice 6 R2 archive phase)

The following three overclaims MUST be documented in the Slice 6 R2 archive corrigendum (NOT modifying Slice 5 archive #791 itself, which stays immutable):

| Overclaim | Slice | Reality |
|-----------|-------|---------|
| "Intel QSV does not honor ForceKeyFrame mid-stream" | Slice 4 | Wrong timing (AFTER) + wrong variant (VT_BOOL). BEFORE+VT_UI4 works (P2 #809). |
| "Mechanism G is vendor-uniform" | Slice 5 | G executes on NVENC but yields 0 IDR (C0.b #801). |
| "NVENC honored CleanPoint instead" | Slice 5 DD10 | Falsified by P1 #807 (30/30 P-frames). |

---

## 10. Strict TDD Commit Cadence

| Commit | Label | Contents | `cargo nextest` T7.1/T7.2 |
|--------|-------|----------|--------------------------|
| C0 | PROBES | ForceKeyFrame infrastructure present; retained Phase 0 probes compile; Slice 5 round-3 probe deleted | FAIL (test bodies not yet using ForceKeyFrame path) |
| C1 | RED | Remove `#[ignore]` from T7.1/T7.2 on both Host A + B variants; ForceKeyFrame atomic in `request_keyframe()` | FAIL (pump_loop NeedInput path not yet wired) |
| C2 | GREEN | pump_loop NeedInput wires SetValue BEFORE submit_frame; Mechanism G deleted; CleanPoint write deleted; DD10 comment replaced; doc-comment updated | PASS on both hosts |
| C3 | POLISH | `cargo fmt`, `cargo clippy` fixes only | PASS |

Invariant: C0 precedes C1 precedes C2. No squash of C1+C2.

---

## 11. Acceptance Criteria Checklist

- [ ] AC-1: Host A T7.1 + T7.2 PASS with ForceKeyFrame mechanism.
- [ ] AC-2: Host B T7.1 + T7.2 PASS with ForceKeyFrame mechanism.
- [ ] AC-3: T8.2 PASS on BOTH Host A and Host B.
- [ ] AC-4: `force_keyframe_icodecapi_pending` defaults to `false` on construction.
- [ ] AC-5: `request_keyframe()` sets `force_keyframe_icodecapi_pending=true`; no vendor dispatch.
- [ ] AC-6: pump_loop NeedInput consumes with `swap(false, AcqRel)` BEFORE `ProcessInput`.
- [ ] AC-7: SetValue uses `VARIANT { vt: VT_UI4, ulVal: 1 }`.
- [ ] AC-8: SetValue HRESULT failure is non-fatal (WARN + continue).
- [ ] AC-9: Mechanism G code (`request_keyframe_via_recreate`, `keyframe_recreate_pending`, G pump_loop block, `mft_activate_factory`) fully deleted.
- [ ] AC-10: CleanPoint write code (`request_keyframe_via_cleanpoint`, `cleanpoint_pending`, INPUT write block) fully deleted.
- [ ] AC-11: `MFSampleExtension_CleanPoint` READ in `collect_output` unchanged.
- [ ] AC-12: DD10 comment block deleted; replaced with P2 (#809) + #808 citation.
- [ ] AC-13: `EncoderVendor` enum retained for logging; no IDR dispatch.
- [ ] AC-14: `request_keyframe()` doc-comment documents NVENC idx-0, Intel QSV idx-1, 30-frame tolerance.
- [ ] AC-15: All 5 Phase 0 R2 probes retained `#[ignore]`-gated; Slice 5 round-3 probe deleted.
- [ ] AC-16: 3 new CI-runnable unit tests PASS (default=false, set=true, swap-consume).
- [ ] AC-17: `cargo clippy --all-targets --all-features --locked -- -D warnings` = zero warnings.
- [ ] AC-18: `cargo nextest run --workspace` GREEN (no previously-passing test regresses).
- [ ] AC-19: `sm-domain` diff vs `c48ae46` = 0 lines.
- [ ] AC-20: Slice 6 R2 archive corrigendum documents 3 corrected overclaims.

---

## 12. SDD Chain Anchors

- **Predecessor**: Slice 5 PR #21 / `c48ae46` / archive #791.
- **Branch baseline**: `efc0f36` (Phase 0 Batch 2 `beda9ed` — ForceKeyFrame infrastructure present).
- **Successor**: `hw-encoder-default-on-flip` (gated on Slice 5 + Slice 6 R2).
- **Engram chain**: explore #803 → falsifications #800 #801 #807 → research #808 → P2 #809 → proposal #810 → **spec (this)** → design → tasks → apply → verify → archive.
- **Artifact store**: hybrid — engram `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/spec` + `openspec/changes/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/spec.md`.
