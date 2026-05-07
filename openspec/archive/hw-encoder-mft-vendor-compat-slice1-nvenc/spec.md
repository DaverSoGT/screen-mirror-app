# Spec: hw-encoder-mft-vendor-compat-slice1-nvenc

> Delta spec over `hw-encoder-mft-rework` (PR #16, `ee32ff4`).
> Artifact store: hybrid — engram canonical, openspec for diff review.
> Phase 0 status at spec-write time: NOT_STARTED. Phase 0 transcripts are a MANDATORY gate before design ratifies fix shape.

---

## 1. Scope summary

Fix Manifestation B of Bug 1: NVIDIA NVENC rejects `SetOutputType` with `MF_E_INVALIDMEDIATYPE` (`0xC00D6D76`) when `setup_mft` presents the current output `IMFMediaType`. On Host B (JDNHS) this causes 11 of 18 smoke tests to fail. Slice 1 changes only:

- The output `IMFMediaType` attribute set in `setup_mft` (lines 531–580 of `crates/sm-infra/src/encode/windows_mft.rs`).
- Optionally `apply_pending_codec_settings` if H-B1 or H-B2 shape requires ICodecAPI timing (lines 725–749).
- Optionally `enumerate_and_activate` / `init_mft_sync` for enumeration fallback (lines 294–395), conditional on PQ-4 budget.
- Phase 0 instrumentation commit (debug-only, reverted before production fix).

All predecessor invariants (NO_WAIT polling, dual-arm counters, `METransformDrainComplete` reset, `apply_pending_codec_settings` pattern) MUST be preserved unchanged.

---

## 2. Requirements

### R1 — Output type negotiation: NVENC acceptance

| Field | Value |
|-------|-------|
| ID | R1 |
| Smoke required | YES |
| Files | `windows_mft.rs` lines 531–580 |
| Gate | gates 1, 2, 3, 7 + Host B 18/18 |

**R1.1** The output `IMFMediaType` presented to `mft.SetOutputType(0, &out_type, 0)` in `setup_mft` MUST be accepted by NVIDIA NVENC's hardware H.264 MFT (HRESULT `S_OK`, not `MF_E_INVALIDMEDIATYPE`).

**R1.2** The output type MUST always contain `MF_MT_MAJOR_TYPE = MFMediaType_Video` and `MF_MT_SUBTYPE = MFVideoFormat_H264`.

**R1.3** The output type MUST always contain `MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE`, and `MF_MT_PIXEL_ASPECT_RATIO` encoded as packed u64 ratios.

**R1.4** The presence or absence of `MF_MT_MPEG2_PROFILE`, `MF_MT_AVG_BITRATE`, and `MF_MT_INTERLACE_MODE` in the output type is a DESIGN-TIME decision (resolved by Phase 0 transcripts). The spec requires that whatever attribute set is chosen, `SetOutputType` returns `S_OK` on Host B.

**R1.5** If `MF_MT_MPEG2_PROFILE` is removed from the output type (H-B1 shape), the H.264 profile MUST be applied via `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` post-`START_OF_STREAM`, pre-first-`ProcessInput`, using the `apply_pending_codec_settings` extension pattern (R3). Driver rejection of the `ICodecAPI` call MUST be logged at `warn!` level and MUST NOT crash the encoder thread.

**R1.6** If `MF_MT_AVG_BITRATE` is removed from the output type (H-B2 shape), bitrate MUST continue to be driven exclusively via `ICodecAPI::SetValue(CODECAPI_AVEncCommonMeanBitRate)` inside `apply_pending_codec_settings` — the existing path at line 740 satisfies this requirement without change.

#### Scenario S1.1 — NVENC `setup_mft` succeeds on Host B

- GIVEN a Host B machine (NVIDIA NVENC) with `--features hw-encoder`
- WHEN `WindowsMftH264Encoder::new(EncoderConfig { width: 640, height: 480, .. })` is called
- THEN `setup_mft` returns `Ok(())` (no `MF_E_INVALIDMEDIATYPE`)
- AND `mft_new_on_hw_capable_machine_returns_ok` (T3.2) PASSES

#### Scenario S1.2 — NVENC setup succeeds at fallback 1920×1080

- GIVEN a Host B machine with `EncoderConfig::default()` (sentinel 0×0 → 1920×1080)
- WHEN `start()` is called and the encoder thread reaches `setup_mft`
- THEN `SetOutputType` returns `S_OK`
- AND encoded packets are produced within 5 s

#### Scenario S1.3 — `GetOutputAvailableType` probe handles `E_NOTIMPL`

- GIVEN H-B3 fix shape is chosen by design (conditional on Phase 0 transcripts)
- WHEN `mft.GetOutputAvailableType(0, 0)` returns `E_NOTIMPL`
- THEN `setup_mft` MUST NOT propagate `E_NOTIMPL` as an error; it MUST fall back to the attribute-construction path (H-B1/H-B2)
- AND `setup_mft` MUST eventually call `SetOutputType` with a valid constructed type

---

### R2 — Predecessor pump-loop invariants preserved

| Field | Value |
|-------|-------|
| ID | R2 |
| Smoke required | YES |
| Files | `windows_mft.rs` lines 780–905 |
| Gate | gates 1, 2, 4 + T-NEW-1 + T-NEW-2 |

**R2.1** `GetEvent` MUST continue to be called with `MF_EVENT_FLAG_NO_WAIT`. Blocking `GetEvent` is PROHIBITED.

**R2.2** `ni_count` and `ho_count` dual-arm counters MUST remain stack-local to `pump_loop`. No new atomics may be introduced for these counters.

**R2.3** On `METransformDrainComplete`, both counters MUST be reset to zero and the event logged at `info!` level with old values. The loop MUST NOT exit on this event.

**R2.4** `METransformHaveOutput` credits MUST be drained before `METransformNeedInput` credits are serviced (HaveOutput-first ordering).

**R2.5** The sole loop-exit condition remains the `state.stop` atomic flag checked at top-of-loop.

#### Scenario S2.1 — T-NEW-1 still PASS after Slice 1 changes

- GIVEN Host B with Slice 1 applied
- WHEN `mft_stop_during_idle_returns_within_deadline` runs
- THEN `stop()` returns in < 2000 ms
- AND the test PASSES

#### Scenario S2.2 — T-NEW-2 still PASS after Slice 1 changes

- GIVEN Host B with Slice 1 applied
- WHEN `mft_stop_during_active_encode_returns_within_deadline` runs
- THEN `stop()` returns in < 2000 ms with `frame_tx` still open
- AND the test PASSES

---

### R3 — `apply_pending_codec_settings` extension for H-B1/H-B2 (conditional)

| Field | Value |
|-------|-------|
| ID | R3 |
| Smoke required | YES (indirectly via encoding tests) |
| Files | `windows_mft.rs` lines 725–749 |
| Gate | gates 1, 2, 7 + encoding smoke tests |

**R3.1** If H-B1 shape is chosen: a new `session_init_pending` one-shot flag MUST be added to `MftEncoderShared` (same pattern as `keyframe_pending`, `AcqRel` ordering). It is consumed exactly once inside `apply_pending_codec_settings` at the first `ProcessInput` opportunity, setting `CODECAPI_AVEncH264VProfile`.

**R3.2** If H-B2 shape is chosen: no new flag is needed; the existing `pending_bitrate` path and `CODECAPI_AVEncCommonMeanBitRate` path satisfies bitrate delivery. This requirement is vacuous for H-B2 but MUST be confirmed by design.

**R3.3** `ICodecAPI::SetValue` failure for profile or bitrate MUST be logged at `warn!` and MUST NOT cause `EncoderError` to propagate (non-fatal, same policy as existing bitrate warn at line 742).

**R3.4** `apply_pending_codec_settings` MUST remain callable on every `METransformNeedInput` credit service — the extension MUST NOT change the calling contract.

#### Scenario S3.1 — Profile applied post-START_OF_STREAM (H-B1 conditional)

- GIVEN H-B1 fix shape is active and `session_init_pending` is true
- WHEN `pump_loop` receives the first `METransformNeedInput` and calls `apply_pending_codec_settings`
- THEN `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile, Main)` is called exactly once
- AND `session_init_pending` is set to false (one-shot consumed)
- AND subsequent `ProcessInput` calls do NOT repeat the ICodecAPI call

#### Scenario S3.2 — ICodecAPI profile rejection is non-fatal

- GIVEN `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` returns an error
- WHEN `apply_pending_codec_settings` processes the rejection
- THEN a `warn!` log line is emitted with the HRESULT
- AND the encoder thread does NOT exit
- AND `ProcessInput` proceeds normally

---

### R4 — Phase 0 instrumentation commit (debug-only)

| Field | Value |
|-------|-------|
| ID | R4 |
| Smoke required | NO |
| Files | `windows_mft.rs` `setup_mft` |
| Gate | gate 6 (no-default-features must stay GREEN) |

**R4.1** The Phase 0 instrumentation MUST be gated by BOTH `#[cfg(debug_assertions)]` AND `feature = "hw-encoder"`. It MUST NOT appear in release builds or in `cargo check --no-default-features` (gate 6).

**R4.2** The instrumentation MUST include: (a) a `GetOutputAvailableType(0, 0)` probe that logs the HRESULT and, if `S_OK`, logs each attribute GUID+value; (b) a binary-search `SetOutputType` sequence that attempts progressively larger attribute sets and logs the HRESULT for each attempt.

**R4.3** The instrumentation commit MUST be a standalone commit with message `feat(infra): add hw-encoder setup_mft attribute-walk trace (debug-only)`. It MUST be reverted / removed (trace block deleted) before the production-fix commit lands. The production-fix PR diff MUST NOT contain any Phase 0 trace code.

**R4.4** All Phase 0 trace calls MUST use `tracing::trace!` at the `sm_infra::encode` target so they are controlled by `RUST_LOG=sm_infra::encode=trace`.

#### Scenario S4.1 — Gate 6 clean with Phase 0 trace present

- GIVEN the Phase 0 instrumentation commit is applied (debug_assertions + hw-encoder gated)
- WHEN `cargo check --no-default-features` is run
- THEN the command exits with code 0 (gate 6 GREEN)

#### Scenario S4.2 — Phase 0 trace produces per-prefix HRESULT table on Host B

- GIVEN `RUST_LOG=sm_infra::encode=trace` is set and `--features hw-encoder` is active
- WHEN any smoke test that calls `setup_mft` runs on Host B
- THEN the log contains one `trace!` line per attribute prefix attempt, each with the HRESULT value
- AND the log contains the result of `GetOutputAvailableType(0, 0)` (HRESULT + attribute dump or `E_NOTIMPL`)

---

### R5 — Enumeration fallback in `init_mft_sync` (conditional on PQ-4 budget)

| Field | Value |
|-------|-------|
| ID | R5 |
| Smoke required | NO (cannot smoke on single-GPU hosts) |
| Files | `windows_mft.rs` lines 294–395 |
| Gate | gates 1, 2, 3, 6, 7 |
| Conditional | Included only if Slice 1 line budget allows; otherwise deferred to `hw-encoder-mft-enumeration-fallback` |

**R5.1** If included: `enumerate_and_activate` MUST be refactored to return all activated MFTs (or their `IMFActivate` tokens) so that `init_mft_sync` can iterate them.

**R5.2** If `setup_mft(pactivates[i])` fails for `i < count`, `init_mft_sync` MUST attempt `pactivates[i+1]` before returning `Err`. This MUST log the failure at `warn!` level with the failed index and HRESULT.

**R5.3** If all `pactivates[0..count]` fail `setup_mft`, `init_mft_sync` MUST return the error from the LAST attempt.

**R5.4** If `count == 1`, behavior MUST be identical to the current single-MFT path (no regression for single-GPU hosts).

**R5.5** No new public API or feature flag is introduced for this fallback.

#### Scenario S5.1 — Fallback skips failing MFT (dual-GPU machine)

- GIVEN a machine with two hardware H.264 MFTs registered (e.g., Intel + NVIDIA)
- AND `setup_mft` fails for `pactivates[0]` with `MF_E_INVALIDMEDIATYPE`
- WHEN `init_mft_sync` runs
- THEN `pactivates[1]` is attempted
- AND if it succeeds, `init_mft_sync` returns `Ok((mft, codec_api))`
- AND a `warn!` line is emitted for the skipped index

#### Scenario S5.2 — Single-GPU machine unaffected

- GIVEN `MFTEnumEx` returns `count == 1`
- WHEN `init_mft_sync` runs
- THEN behavior is identical to the current implementation (no extra attempt, no regression)

---

### R6 — 18/18 smoke tests PASS on Host B

| Field | Value |
|-------|-------|
| ID | R6 |
| Smoke required | YES |
| Files | `crates/sm-infra/tests/windows_mft_encode.rs` |
| Gate | Host B evidence transcript |

**R6.1** All 11 currently-failing NVENC tests on Host B MUST move from FAIL to PASS without any test-side workaround (no `#[ignore]` removal tricks, no test modification to mask errors).

The 11 failing tests (all fail at `setup_mft` → `SetOutputType` → `MF_E_INVALIDMEDIATYPE`):
1. `mft_encoded_packet_starts_with_annex_b_start_code`
2. `mft_thirty_frame_smoke_emits_at_least_one_keyframe`
3. `mft_encoded_packet_timestamp_matches_capture_frame`
4. `mft_request_keyframe_marks_next_packet_as_keyframe`
5. `mft_keyframe_flag_cleared_after_idr_emitted`
6. `mft_set_bitrate_updates_encoder_without_restart`
7. `mft_first_real_packet_is_annex_b`
8. `mft_setup_uses_config_dimensions_when_nonzero`
9. `mft_setup_falls_back_when_config_dimensions_zero`
10. `mft_drain_after_channel_close_does_not_panic`
11. `mft_stop_is_idempotent`

**R6.2** All 7 currently-passing Host B tests MUST remain PASS (no regression):
1. `mft_new_then_drop_does_not_av`
2. `mft_new_on_hw_capable_machine_returns_ok`
3. `mft_new_returns_init_failed_when_no_hardware_mft`
4. `mft_new_does_not_submit_frames_to_mft_during_init`
5. `mft_stop_during_idle_returns_within_deadline` (T-NEW-1)
6. `mft_stop_during_active_encode_returns_within_deadline` (T-NEW-2)
7. `mft_drop_without_stop_does_not_leak_thread`

**R6.3** The verify gate requires an attached Host B run transcript (`phase0-host-b-nvenc.log` or equivalent) showing 18/18 PASS with `--run-ignored=ignored-only --features hw-encoder`.

#### Scenario S6.1 — 11 NVENC tests move from FAIL to PASS

- GIVEN Slice 1 production fix is applied on Host B
- WHEN `cargo nextest run -p sm-infra --features hw-encoder --run-ignored=ignored-only` is executed
- THEN all 18 tests report PASS
- AND no test reports FAIL or ABORT or TIMEOUT

#### Scenario S6.2 — 7 passing tests do not regress

- GIVEN Slice 1 production fix is applied on Host B
- WHEN the 7 previously-passing tests run (subset of the 18)
- THEN each reports PASS with the same pass criterion as before (e.g., T-NEW-1 < 2000 ms, T-NEW-2 < 2000 ms)

---

### R7 — 7 quality gates GREEN

| Field | Value |
|-------|-------|
| ID | R7 |
| Smoke required | NO (CI gates; no HW required) |
| Gate | all 7 |

**R7.1** All seven quality gates MUST be GREEN before archive is approved:

| # | Command |
|---|---------|
| 1 | `cargo check --workspace` |
| 2 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| 3 | `cargo fmt --check --all` |
| 4 | `cargo nextest run --workspace` (no `--features hw-encoder`; HW smoke tests are `#[ignore]`) |
| 5 | `cargo deny check` |
| 6 | `cargo check --no-default-features` |
| 7 | `cargo check --features hw-encoder` |

**R7.2** `cargo check --no-default-features` (gate 6) MUST pass with Phase 0 instrumentation present AND with the production fix in place.

**R7.3** `cargo nextest run --workspace` (gate 4, no HW flag) MUST remain GREEN. No HW-gated `#[ignore]` test is expected to run; no new non-ignored test is expected to fail.

#### Scenario S7.1 — Gate 6 clean after production fix

- GIVEN the production-fix commit is applied (Phase 0 trace removed)
- WHEN `cargo check --no-default-features` runs
- THEN exit code 0

#### Scenario S7.2 — Gate 4 does not regress

- GIVEN the production-fix commit is applied
- WHEN `cargo nextest run --workspace` runs (default feature set, CI-compatible)
- THEN all non-ignored tests PASS

---

### R8 — Public API frozen

| Field | Value |
|-------|-------|
| ID | R8 |
| Smoke required | NO |
| Files | `crates/sm-domain/src/encode.rs` |
| Gate | gates 1, 2, `no_platform_deps.rs` invariant |

**R8.1** `VideoEncoder`, `EncoderConfig`, `EncodedPacket`, and `EncoderError` public shapes MUST be identical after Slice 1 to what was archived in `hw-encoder-mft-rework` (PR #16).

**R8.2** No new public fields, variants, or methods MAY be added to these types. Internal fields on `MftEncoderShared` (e.g., `session_init_pending`) are exempt — they are `pub(super)` or private.

**R8.3** `crates/sm-domain` MUST remain free of platform-specific dependencies (`no_platform_deps.rs` invariant).

#### Scenario S8.1 — Frozen API verified by verify phase

- GIVEN Slice 1 is fully applied
- WHEN `sdd-verify` inspects `crates/sm-domain/src/encode.rs`
- THEN the public surface is byte-identical to the predecessor archived shape
- AND no new `pub` items appear in the diff

---

## 3. Non-requirements (explicit out-of-scope)

The following are EXPLICITLY excluded from Slice 1. Including them is a spec violation.

| Item | Owner |
|------|-------|
| NV12 stride / `MF_MT_DEFAULT_STRIDE` on input type | Slice 2 (PQ-5) |
| `bgra_to_nv12.rs` stride padding or `Nv12` layout changes | Slice 2 |
| Intel QSV diagnostics / Manifestation A | Slice 2 |
| `MF_MT_VIDEO_NOMINAL_RANGE` on input or output type | Slice 2 |
| `default = ["hw-encoder"]` feature flip | `hw-encoder-default-on-flip` (gated) |
| T-NEW-3 (`mft_handles_have_output_before_need_input`) | Deferred (PQ-3) |
| New feature flags beyond `hw-encoder` | D-6 frozen |
| New public API on `VideoEncoder` / `EncoderConfig` | FROZEN |
| Host A (Intel QSV) evidence / verify gate | Slice 2 |
| SEH wrapper for driver AV | Slice 2 |

---

## 4. Coverage matrix

| Requirement | Anchoring tests | Quality gates |
|-------------|----------------|---------------|
| R1 — Output type NVENC acceptance | S1.1/S1.2 → T3.2 + all 11 previously-failing | gates 1, 2, 7 + Host B 18/18 |
| R1.3 — `GetOutputAvailableType` E_NOTIMPL | S1.3 (conditional) | gate 1, 7 |
| R2 — Pump-loop invariants | S2.1 (T-NEW-1) + S2.2 (T-NEW-2) | gates 1, 2, 4 |
| R3 — `apply_pending_codec_settings` extension | S3.1/S3.2 → encoding tests (T4.1, T7.1, T8.2) | gates 1, 2, 7 |
| R4 — Phase 0 instrumentation | S4.1/S4.2 | gate 6 (no-default-features) |
| R5 — Enumeration fallback (conditional) | S5.1/S5.2 | gates 1, 2, 3, 6, 7 |
| R6 — 18/18 Host B PASS | S6.1/S6.2 | Host B transcript |
| R7 — 7 quality gates | S7.1/S7.2 | all 7 |
| R8 — Public API frozen | S8.1 | gates 1, 2 + verify |

---

## 5. Assumptions and open anchors

**A1** — Phase 0 transcripts are the load-bearing input for design. Spec R1.4 intentionally leaves the exact attribute delta as a design-time decision. Design MUST cite Phase 0 per-prefix HRESULT table when ratifying the fix shape.

**A2** — `GetOutputAvailableType(0, 0)` returning `E_NOTIMPL` on NVENC is assessed as Medium likelihood (explore #147, Risk 5). R1.3/S1.3 specifies graceful fallback behavior so the spec is complete regardless of the outcome.

**A3** — The 7 currently-passing Host B tests that succeed despite `setup_mft` failure (T-NEW-1, T-NEW-2, lifecycle tests) succeed because the encoder thread exits early and `stop()` joins cleanly. This analysis from explore #147 is treated as confirmed. Slice 1 MUST NOT break this behavior.

**A4** — R5 (enumeration fallback) is marked Conditional. If design determines the fix + fallback exceeds the 400-line PR budget, R5 migrates to `hw-encoder-mft-enumeration-fallback` as a standalone change (D-8). Spec rows remain present to preserve the requirement; `sdd-tasks` marks them as deferred if budget is exceeded.

**A5** — Host A (Intel QSV) behavior is NOT a Slice 1 verify gate. Manifestation A's AV signature is recorded as "carry-forward, owned by Slice 2". If Slice 1 inadvertently changes Host A behavior, this is noted in the archive report but does NOT block Slice 1 archive.

---

## Part 2 — Predecessor invariant mapping

The following predecessor spec requirements (from `hw-encoder-mft-rework`) are in MAINTAINED state — Slice 1 must not regress them. They are not re-specified here; they are cross-referenced for verify completeness.

| Predecessor req | Description | Verify check |
|-----------------|-------------|--------------|
| Pred-R1 (NO_WAIT polling) | `GetEvent(MF_EVENT_FLAG_NO_WAIT)` only | Confirmed by R2.1 |
| Pred-R2 (dual-arm counters) | `ni_count` / `ho_count` stack-local | Confirmed by R2.2 |
| Pred-R3 (HaveOutput-first drain) | ho_count drained before ni_count serviced | Confirmed by R2.4 |
| Pred-R4 (DrainComplete reset) | Both counters reset to 0 on DrainComplete | Confirmed by R2.3 |
| Pred-R5 (stop flag sole exit) | Loop exits only via `state.stop` | Confirmed by R2.5 |
| Pred-R6 (apply_pending pattern) | `apply_pending_codec_settings` called per NeedInput | Confirmed by R3.4 |
| Pred-T-NEW-1 (stop idle ≤ 2s) | | S2.1 |
| Pred-T-NEW-2 (stop active ≤ 2s) | | S2.2 |
