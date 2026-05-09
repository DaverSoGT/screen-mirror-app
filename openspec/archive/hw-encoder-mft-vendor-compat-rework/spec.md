# Spec: hw-encoder-mft-vendor-compat-rework (Slice 2 — Intel QSV stream-change renegotiation)

> Phase: SDD spec. Inputs: proposal #632, explore #631, root-cause #630, DIAG trace #629, predecessor spec #596.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-vendor-compat-rework/spec` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`). Delivery: single PR, branch `feat/hw-encoder-mft-stream-change-handling`.
> Date: 2026-05-07.

---

## 1. Inputs

| Observation | Topic key | Role |
|-------------|-----------|------|
| #632 | `sdd/hw-encoder-mft-vendor-compat-rework/proposal` | 8 locked decisions D1–D8, 4 design-phase OQs, AC-1–AC-5, DoD |
| #631 | `sdd/hw-encoder-mft-vendor-compat-rework/explore` | Microsoft async-MFT protocol, code-level analysis, cross-vendor impact |
| #630 | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-root-cause` | Confirmed root cause: line 1317 silent swallow + fix sketch |
| #629 | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-trace-3c8bc48` | DIAG trace: ni_count/ho_count frozen at 0 after frame ~17 |
| #596 | `sdd/hw-encoder-mft-rework/spec` | Predecessor spec — format template, R/S notation, BLOCKED_ON_SMOKE convention |
| n/a | `openspec/changes/hw-encoder-mft-vendor-compat-rework/proposal.md` | File cross-reference (same content as #632) |
| n/a | `crates/sm-infra/tests/windows_mft_encode.rs` | All 18 test names and bodies (direct read) |
| n/a | `crates/sm-infra/src/encode/windows_mft.rs` | `collect_output` lines 1305–1378, `try_setup_output_type` lines 589–630 |

---

## 2. Domain Context

`WindowsMftH264Encoder` in `crates/sm-infra/src/encode/windows_mft.rs` runs a `pump_loop` on a dedicated encoder thread. On Intel QSV (Host A, commit `3c8bc48`), `ProcessOutput` returns `MF_E_TRANSFORM_STREAM_CHANGE` at approximately frame 17 — the vendor's signal that its output type has changed and the Microsoft-mandated renegotiation handshake must run. The current code at line 1317 swallows this HRESULT as `Ok(None)` without calling `GetOutputAvailableType` + `SetOutputType`. After that, the Intel QSV MFT never emits another `METransformHaveOutput` event, causing `pump_loop` to spin forever with `ho_count=0`, blocking the 30-frame smoke and causing all 8 other encoding-output tests to timeout.

NVENC (Host B) reaches 16/18 PASS post-PR #17 because NVENC's pipeline fails earlier in `setup_mft` (`SetOutputType: 0xC00D6D76`) and never reaches the streaming phase where `MF_E_TRANSFORM_STREAM_CHANGE` fires. The renegotiation fix is a no-op on NVENC's current code path.

---

## 3. Requirements

### R1 — `collect_output` calls `renegotiate_output_type` on `MF_E_TRANSFORM_STREAM_CHANGE`

**Statement**: WHEN `ProcessOutput` returns an `Err` whose HRESULT code is `MF_E_TRANSFORM_STREAM_CHANGE`, THEN `collect_output` SHALL call `renegotiate_output_type(mft, w, h, framerate, bitrate_bps)` before returning from the match arm. If `renegotiate_output_type` returns `Ok(())`, `collect_output` SHALL return `Ok(None)`. If it returns `Err(e)`, `collect_output` SHALL propagate `Err(e)` to its caller.

**BLOCKED_ON_SMOKE**: Y — the HRESULT path requires Intel QSV hardware to fire `MF_E_TRANSFORM_STREAM_CHANGE` mid-stream.
**Decision**: D1.

---

### R2 — `renegotiate_output_type` executes the same COM sequence as `try_setup_output_type`

**Statement**: WHEN `renegotiate_output_type(mft, w, h, framerate, bitrate_bps)` is called, THEN it SHALL execute, in order:
1. `mft.GetOutputAvailableType(0, 0)` — retrieve the vendor's current slot-0 output type.
2. On the returned `IMFMediaType`, call `SetUINT64(&MF_MT_FRAME_SIZE, ...)`, `SetUINT64(&MF_MT_FRAME_RATE, ...)`, `SetUINT32(&MF_MT_AVG_BITRATE, ...)` (overlay caller-controlled attributes, same as `try_setup_output_type`).
3. `mft.SetOutputType(0, &out_type, 0)` — install the updated type.

No additional MFT messages (flush, drain, begin-streaming, start-of-stream) are sent in this sequence.

**BLOCKED_ON_SMOKE**: Y.
**Decision**: D1, D2, D5 (no flush/notify messages).

---

### R3 — `renegotiate_output_type` maps COM errors to `EncoderError::EncodeFailed`

**Statement**: WHEN any COM call inside `renegotiate_output_type` returns an error HRESULT, THEN `renegotiate_output_type` SHALL return `Err(EncoderError::EncodeFailed(...))`. The error message SHALL include the name of the failing call and its HRESULT as a hex literal (e.g., `"renegotiate_output_type: GetOutputAvailableType: 0x{:08X}"`). `EncoderError::InitFailed` SHALL NOT be returned from this function.

**BLOCKED_ON_SMOKE**: N — error mapping is unit-verifiable at the type level; the actual COM failure path is smoke-only.
**Decision**: D1 (option C rationale: `InitFailed` is wrong post-streaming).

---

### R4 — No flush or drain before renegotiation

**Statement**: WHEN `collect_output` handles `MF_E_TRANSFORM_STREAM_CHANGE`, THEN neither `MFT_MESSAGE_COMMAND_FLUSH` nor `MFT_MESSAGE_COMMAND_DRAIN` SHALL be sent to the MFT before or during the `renegotiate_output_type` call sequence. This applies to all async hardware MFTs (`MFT_ENUM_FLAG_HARDWARE`).

**BLOCKED_ON_SMOKE**: Y — empirical confirmation requires Host A smoke showing renegotiation completes without flush.
**Decision**: D2 (load-bearing assumption: async MFT spec requires `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE`).

---

### R5 — No `NOTIFY_BEGIN_STREAMING` or `NOTIFY_START_OF_STREAM` after renegotiation

**Statement**: WHEN `collect_output` successfully completes renegotiation via `renegotiate_output_type`, THEN neither `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` nor `MFT_MESSAGE_NOTIFY_START_OF_STREAM` SHALL be sent to the MFT. Those messages are initial-setup-only and SHALL NOT be resent for a mid-stream format change.

**BLOCKED_ON_SMOKE**: Y — absence is confirmed by observing that streaming resumes normally on Host A smoke.
**Decision**: D2, D5 (see proposal OQ-B: no NOTIFY resend per MS async-MFT docs).

---

### R6 — Renegotiation failure propagates as fatal error out of `pump_loop`

**Statement**: WHEN `renegotiate_output_type` returns `Err(e)` inside `collect_output`, THEN `collect_output` SHALL return `Err(e)`. WHEN `pump_loop` receives that error from `collect_output`, THEN it SHALL log `tracing::error!("renegotiate_output_type failed: {e}")` and `return`, exiting the encoder thread. The encoder thread SHALL NOT retry renegotiation. Consumers of the packet channel SHALL observe a channel disconnect as the encoder thread exits.

**BLOCKED_ON_SMOKE**: N — the propagation path is statically verifiable; the triggering condition (COM failure on renegotiation) is smoke-only.
**Decision**: D3.

---

### R7 — `collect_output` logs `output.dwStatus` and call-level `status` at `trace!` level on every call

**Statement**: WHEN `ProcessOutput` returns (whether `Ok(())` or any `Err`), THEN `collect_output` SHALL emit a `tracing::trace!` log entry that includes both `output.dwStatus` (per-buffer `_MFT_OUTPUT_DATA_BUFFER_FLAGS`) and `status` (per-call `_MFT_PROCESS_OUTPUT_STATUS`). This logging SHALL occur on every `collect_output` invocation, including the success path. No behavioral coupling to these values is introduced by this requirement.

**BLOCKED_ON_SMOKE**: N — logging is a structural requirement; the log entry can be verified with `RUST_LOG=sm_infra::encode=trace`.
**Decision**: D4.

---

### R8 — `mft_thirty_frame_smoke_emits_at_least_one_keyframe` calls `enc.stop()` before `producer.join()`

**Statement**: In `crates/sm-infra/tests/windows_mft_encode.rs`, the test `mft_thirty_frame_smoke_emits_at_least_one_keyframe` SHALL call `enc.stop()` before `producer.join()`. The current order (line 241: `producer.join()`, line 242: `enc.stop()`) SHALL be swapped. After the swap, `enc.stop()` SHALL be called first; `producer.join()` SHALL be called immediately after.

**BLOCKED_ON_SMOKE**: Y — the effect (unblocking the producer via channel disconnect) is empirically confirmed by the test completing within deadline rather than hanging.
**Decision**: D5.

---

### R9 — `collect_output` resets `*output_format_known` to `None` on successful renegotiation

**Statement**: WHEN `renegotiate_output_type` returns `Ok(())` inside `collect_output`, THEN `collect_output` SHALL set `*output_format_known = None` before returning `Ok(None)`. This invalidates the Annex-B/AVCC detection cache and forces re-detection on the next successfully delivered packet.

**BLOCKED_ON_SMOKE**: N — the reset is a deterministic write verifiable at the code level; empirical format change across a renegotiation boundary requires smoke.
**Decision**: D6.

---

### R10 — T-NEW-1 and T-NEW-2 PASS cross-vendor with no regression

**Statement**: WHEN the change is applied, THEN `mft_stop_during_idle_returns_within_deadline` (T-NEW-1) and `mft_stop_during_active_encode_returns_within_deadline` (T-NEW-2) SHALL continue to PASS on both Host A (Intel QSV) and Host B (NVIDIA NVENC), completing within `STOP_DEADLINE_MS = 2000` ms. No change to the test body or deadline constant is permitted. These tests cover the Bug 2 fix from PR #16 (`pump_loop` NO_WAIT polling); the current change SHALL NOT regress that fix.

**BLOCKED_ON_SMOKE**: Y — cross-vendor pass requires both hardware hosts.
**Decision**: PR #16 carry-forward; R10 is a regression guard.

---

### R11 — The 8 currently-timing-out tests on Host A PASS post-fix

**Statement**: WHEN the change is applied, THEN the following 8 tests, which currently time out on Host A (`3c8bc48` master), SHALL PASS:

1. `mft_encoded_packet_starts_with_annex_b_start_code`
2. `mft_encoded_packet_timestamp_matches_capture_frame`
3. `mft_request_keyframe_marks_next_packet_as_keyframe`
4. `mft_keyframe_flag_cleared_after_idr_emitted`
5. `mft_set_bitrate_updates_encoder_without_restart`
6. `mft_first_real_packet_is_annex_b`
7. `mft_setup_uses_config_dimensions_when_nonzero`
8. `mft_setup_falls_back_when_config_dimensions_zero`

All 8 share the same root cause as the 30-frame smoke (pipeline stall at `MF_E_TRANSFORM_STREAM_CHANGE`). AC-2 allows at most 1 of these to fail with a different failure mode than TIMEOUT (prove separate bug). Target: ≥7/8 PASS.

**BLOCKED_ON_SMOKE**: Y — requires Intel QSV hardware on Host A.
**Decision**: D8 (existing tests serve as RED→GREEN proof, no new stream-change test added).

---

### R12 — Host B 16/18 PASS pattern maintained

**Statement**: WHEN the change is applied and a regression smoke is run on Host B (NVIDIA NVENC), the smoke SHALL produce ≥16/18 PASS. The 2 pre-existing failures (`mft_request_keyframe_marks_next_packet_as_keyframe`, `mft_keyframe_flag_cleared_after_idr_emitted` — force-IDR carry-forward, tracked in separate change `hw-encoder-mft-nvenc-setup-fix`) SHALL NOT change their failure mode. No test that currently PASSES on Host B SHALL FAIL after this change.

**BLOCKED_ON_SMOKE**: Y — requires NVIDIA NVENC hardware on Host B.
**Decision**: Regression guard; NVENC path is not exercised by stream-change fix.

---

### R13 — `cargo nextest run --workspace` GREEN on Linux, macOS, Windows CI

**Statement**: WHEN the change is merged, THEN `cargo nextest run --workspace` SHALL exit with code 0 on Linux, macOS, and Windows CI runners. All 18 `#[ignore]`-gated HW tests SHALL remain gated and SHALL NOT run on CI. The 7 quality gates SHALL all be GREEN: `cargo check --workspace`, `cargo clippy --all-targets --all-features`, `cargo fmt --check --all`, `cargo nextest run --workspace`, `cargo deny check`, `cargo check --no-default-features`, `cargo check --features hw-encoder`.

**BLOCKED_ON_SMOKE**: N — CI is fully automated; `#[ignore]` gate is already in place.
**Decision**: AC-5; standard project quality gate.

---

### R14 — `VideoEncoder` trait surface MUST NOT change

**Statement**: WHEN the change is applied, `crates/sm-domain/src/encode.rs` SHALL have an empty diff. `VideoEncoder` trait method signatures, `EncoderConfig` fields, `EncodedPacket` fields, and `EncoderError` variants SHALL remain identical to the master `3c8bc48` baseline. No new `EncoderError` variant is added for this change.

**BLOCKED_ON_SMOKE**: N — statically verifiable via diff.
**Decision**: Proposal §3 OUT-of-scope; domain layer FROZEN.

---

### R15 — `default = []` for `hw-encoder` feature MUST remain unchanged

**Statement**: WHEN the change is applied, `crates/sm-infra/Cargo.toml` SHALL still read `default = []`. The `hw-encoder` feature SHALL NOT appear in the default array. The Cargo default flip is a separate planned change (`hw-encoder-default-on-flip`).

**BLOCKED_ON_SMOKE**: N — statically verifiable via Cargo.toml diff.
**Decision**: Proposal §3 OUT-of-scope.

---

## 4. Scenarios

### S1.1 — Renegotiation triggers on `MF_E_TRANSFORM_STREAM_CHANGE`

```
GIVEN: pump_loop is processing frames, Intel QSV MFT has emitted >=1 METransformNeedInput
WHEN: ProcessOutput returns MF_E_TRANSFORM_STREAM_CHANGE at approximately frame 17
THEN: collect_output calls GetOutputAvailableType(0, 0) before returning
AND: collect_output calls SetOutputType(0, &updated_type, 0)
AND: collect_output returns Ok(None) signalling "retry on next HaveOutput"
AND: pump_loop decrements ho_count and continues polling
TEST EVIDENCE: Host A smoke transcript — mft_thirty_frame_smoke_emits_at_least_one_keyframe
progresses past frame 17 and eventually receives >=1 keyframe + >=10 P-frames within 10s
COVERAGE: BLOCKED_ON_SMOKE
```

### S1.2 — Non-stream-change errors propagate unchanged

```
GIVEN: pump_loop is processing frames
WHEN: ProcessOutput returns an Err whose code is NOT MF_E_TRANSFORM_STREAM_CHANGE
  AND is NOT MF_E_TRANSFORM_NEED_MORE_INPUT
THEN: collect_output returns Err(EncoderError::EncodeFailed("ProcessOutput: 0x{code}"))
AND: renegotiate_output_type is NOT called
AND: pump_loop propagates the error and exits
COVERAGE: NOT BLOCKED_ON_SMOKE (code path verification)
```

### S2.1 — `renegotiate_output_type` COM call sequence matches `try_setup_output_type`

```
GIVEN: collect_output is handling MF_E_TRANSFORM_STREAM_CHANGE
WHEN: renegotiate_output_type(mft, w, h, framerate, bitrate_bps) is called
THEN: GetOutputAvailableType(0, 0) is called on mft
AND: MF_MT_FRAME_SIZE is set on the returned type via SetUINT64
AND: MF_MT_FRAME_RATE is set on the returned type via SetUINT64
AND: MF_MT_AVG_BITRATE is set on the returned type via SetUINT32
AND: SetOutputType(0, &out_type, 0) is called on mft
AND: no MFT_MESSAGE_COMMAND_FLUSH is sent
AND: no MFT_MESSAGE_COMMAND_DRAIN is sent
AND: no MFT_MESSAGE_NOTIFY_BEGIN_STREAMING is sent
AND: no MFT_MESSAGE_NOTIFY_START_OF_STREAM is sent
AND: renegotiate_output_type returns Ok(())
COVERAGE: BLOCKED_ON_SMOKE (COM call ordering confirmed by Host A smoke progress)
```

### S3.1 — Error mapping: COM failure inside renegotiation yields `EncodeFailed`

```
GIVEN: renegotiate_output_type is executing
WHEN: GetOutputAvailableType returns a failing HRESULT (e.g. 0x80070001)
THEN: renegotiate_output_type returns Err(EncoderError::EncodeFailed(msg))
  WHERE msg contains "renegotiate_output_type: GetOutputAvailableType: 0x80070001"
AND: EncoderError::InitFailed is NOT returned

GIVEN: renegotiate_output_type is executing
WHEN: SetOutputType returns a failing HRESULT
THEN: renegotiate_output_type returns Err(EncoderError::EncodeFailed(msg))
  WHERE msg contains "renegotiate_output_type: SetOutputType: 0x{code}"
COVERAGE: NOT BLOCKED_ON_SMOKE (error variant is a compile-time structural property;
exact message content verified by code review or unit test if injection architecture exists)
```

### S4.1 — No flush message sent on `MF_E_TRANSFORM_STREAM_CHANGE`

```
GIVEN: pump_loop is in the HaveOutput service phase
WHEN: ProcessOutput returns MF_E_TRANSFORM_STREAM_CHANGE
THEN: no call to mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, ...) occurs
  before, during, or after renegotiate_output_type
COVERAGE: BLOCKED_ON_SMOKE (absence confirmed by Host A smoke completing without
pipeline reset artefacts)
```

### S5.1 — No `NOTIFY_BEGIN_STREAMING` or `NOTIFY_START_OF_STREAM` resent

```
GIVEN: renegotiate_output_type returns Ok(())
WHEN: collect_output prepares to return Ok(None)
THEN: no call to mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, ...) occurs
AND: no call to mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, ...) occurs
AND: streaming resumes with the next pump_loop iteration
COVERAGE: BLOCKED_ON_SMOKE
```

### S6.1 — Renegotiation failure exits `pump_loop` cleanly

```
GIVEN: ProcessOutput has returned MF_E_TRANSFORM_STREAM_CHANGE
  AND: renegotiate_output_type returns Err(EncoderError::EncodeFailed("..."))
WHEN: collect_output propagates the error to pump_loop
THEN: pump_loop emits tracing::error!("renegotiate_output_type failed: {e}")
AND: pump_loop returns (encoder thread exits)
AND: the packet SyncSender (tx) is dropped, causing consumers to see channel disconnect
AND: no retry of renegotiate_output_type occurs
COVERAGE: NOT BLOCKED_ON_SMOKE (propagation path is structural; triggering condition is smoke)
```

### S7.1 — `dwStatus` and call-level `status` logged on `ProcessOutput` Ok path

```
GIVEN: collect_output is called and ProcessOutput returns Ok(())
WHEN: RUST_LOG=sm_infra::encode=trace is set
THEN: a trace! log entry is emitted containing both output.dwStatus and status values
COVERAGE: NOT BLOCKED_ON_SMOKE (verifiable via smoke-trace.ps1 log inspection)
```

### S7.2 — `dwStatus` and call-level `status` logged on `ProcessOutput` Err path

```
GIVEN: collect_output is called and ProcessOutput returns any Err variant
  (MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, or other)
WHEN: RUST_LOG=sm_infra::encode=trace is set
THEN: a trace! log entry is emitted containing both output.dwStatus and status values
  before the Err arm executes
COVERAGE: NOT BLOCKED_ON_SMOKE (structural logging; Err variants are exercised by smoke)
```

### S8.1 — Test ordering: `enc.stop()` before `producer.join()`

```
GIVEN: mft_thirty_frame_smoke_emits_at_least_one_keyframe has completed the
  pkt_rx.recv_timeout loop (either received N_FRAMES or DEADLINE elapsed)
WHEN: the test teardown sequence executes
THEN: enc.stop() is called first
AND: producer.join() is called after enc.stop() returns
AND: producer.join() returns without deadlock even if the bounded channel
  was full at the time enc.stop() was called
COVERAGE: BLOCKED_ON_SMOKE (deadlock scenario requires the encoding pipeline to stall)
```

### S9.1 — `output_format_known` reset to `None` on stream change

```
GIVEN: output_format_known is Some(false) (Annex-B confirmed) or Some(true) (AVCC confirmed)
WHEN: collect_output encounters MF_E_TRANSFORM_STREAM_CHANGE
  AND: renegotiate_output_type returns Ok(())
THEN: *output_format_known is set to None before collect_output returns Ok(None)
AND: on the next packet after renegotiation, collect_output re-detects the format
  from the raw bytes and sets output_format_known to the appropriate Some(...)
COVERAGE: NOT BLOCKED_ON_SMOKE (the reset write is structural; re-detection is smoke)
```

### S10.1 — T-NEW-1 cross-vendor: idle-stop deadline not regressed

```
GIVEN: mft_stop_during_idle_returns_within_deadline is run on Host A (Intel QSV)
  AND: no frames have been sent to the encoder
WHEN: enc.stop() is called after 100ms idle sleep
THEN: stop() returns Ok(()) within STOP_DEADLINE_MS = 2000 ms
AND: elapsed.as_millis() < 2000

GIVEN: mft_stop_during_idle_returns_within_deadline is run on Host B (NVIDIA NVENC)
THEN: same pass criterion applies
COVERAGE: BLOCKED_ON_SMOKE (both hardware hosts)
```

### S10.2 — T-NEW-2 cross-vendor: active-encode-stop deadline not regressed

```
GIVEN: mft_stop_during_active_encode_returns_within_deadline is run on Host A or Host B
  AND: 5 frames have been sent with frame_tx still open
WHEN: enc.stop() is called
THEN: stop() returns Ok(()) within STOP_DEADLINE_MS = 2000 ms
AND: frame_tx dropped after stop() returns (no deadlock)
COVERAGE: BLOCKED_ON_SMOKE (both hardware hosts)
```

### S11.1 — `mft_encoded_packet_starts_with_annex_b_start_code` PASSES post-fix (Host A)

```
GIVEN: Intel QSV MFT renegotiates at frame ~17 and streaming resumes
WHEN: mft_encoded_packet_starts_with_annex_b_start_code sends 1 frame and waits 5s
THEN: an EncodedPacket arrives within the timeout
AND: pkt.data[0..4] == [0x00, 0x00, 0x00, 0x01]
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.2 — `mft_encoded_packet_timestamp_matches_capture_frame` PASSES post-fix (Host A)

```
GIVEN: streaming resumes after renegotiation
WHEN: 1 frame with timestamp 500ms is submitted
THEN: the returned EncodedPacket.timestamp == Duration::from_millis(500)
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.3 — `mft_request_keyframe_marks_next_packet_as_keyframe` PASSES post-fix (Host A)

```
GIVEN: streaming flows past the renegotiation boundary
WHEN: request_keyframe() is called then one more frame is submitted
THEN: the returned packet has is_keyframe == true
  AND: data[0..4] == [0x00, 0x00, 0x00, 0x01]
  AND: data[4] & 0x1F == 0x07 (SPS NAL)
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.4 — `mft_keyframe_flag_cleared_after_idr_emitted` PASSES post-fix (Host A)

```
GIVEN: streaming flows past the renegotiation boundary
WHEN: request_keyframe() forces an IDR, then the next frame is submitted
THEN: the forced IDR has is_keyframe == true
AND: the subsequent packet has is_keyframe == false
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.5 — `mft_set_bitrate_updates_encoder_without_restart` PASSES post-fix (Host A)

```
GIVEN: streaming is active; 3 frames encoded at 4 Mbps
WHEN: set_bitrate(8_000_000) is called
THEN: set_bitrate returns Ok(())
AND: 3 more frames can be encoded (encoder thread alive)
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.6 — `mft_first_real_packet_is_annex_b` PASSES post-fix (Host A)

```
GIVEN: streaming flows past renegotiation
WHEN: 1 frame is submitted and the first EncodedPacket received
THEN: pkt.data[0..4] == [0x00, 0x00, 0x00, 0x01]
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.7 — `mft_setup_uses_config_dimensions_when_nonzero` PASSES post-fix (Host A)

```
GIVEN: encoder configured at 640x480; streaming flows past renegotiation
WHEN: a 640x480 frame is submitted
THEN: an EncodedPacket is received within 5s
COVERAGE: BLOCKED_ON_SMOKE
```

### S11.8 — `mft_setup_falls_back_when_config_dimensions_zero` PASSES post-fix (Host A)

```
GIVEN: encoder configured at 0x0 (sentinel fallback to 1920x1080); streaming flows
WHEN: a 1920x1080 frame is submitted
THEN: an EncodedPacket is received within 5s
COVERAGE: BLOCKED_ON_SMOKE
```

### S12.1 — Host B regression: 16/18 PASS maintained

```
GIVEN: the change is applied to feat/hw-encoder-mft-stream-change-handling
WHEN: smoke is run on Host B (NVIDIA NVENC) with --run-ignored=all
THEN: at least 16 of 18 tests PASS
AND: mft_request_keyframe_marks_next_packet_as_keyframe may still FAIL (pre-existing)
AND: mft_keyframe_flag_cleared_after_idr_emitted may still FAIL (pre-existing)
AND: no test that passed on Host B at 3c8bc48 master regresses to FAIL
COVERAGE: BLOCKED_ON_SMOKE
```

---

## 5. Test Mapping

| Scenario | Test name | File | BLOCKED_ON_SMOKE |
|----------|-----------|------|-----------------|
| S1.1 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S1.2 | (code review / diff) | `crates/sm-infra/src/encode/windows_mft.rs` | N |
| S2.1 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S3.1 | (code review; error variant is structural) | `crates/sm-infra/src/encode/windows_mft.rs` | N |
| S4.1 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` (absence) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S5.1 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` (absence) | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S6.1 | (code review / tracing output inspection) | `crates/sm-infra/src/encode/windows_mft.rs` | N |
| S7.1 | smoke-trace.ps1 RUST_LOG=trace log inspection | `crates/sm-infra/tests/windows_mft_encode.rs` | N |
| S7.2 | smoke-trace.ps1 RUST_LOG=trace log inspection | `crates/sm-infra/tests/windows_mft_encode.rs` | N |
| S8.1 | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S9.1 | (code review; reset is a write; re-detection via smoke) | `crates/sm-infra/src/encode/windows_mft.rs` | N (partial) |
| S10.1 | `mft_stop_during_idle_returns_within_deadline` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S10.2 | `mft_stop_during_active_encode_returns_within_deadline` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.1 | `mft_encoded_packet_starts_with_annex_b_start_code` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.2 | `mft_encoded_packet_timestamp_matches_capture_frame` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.3 | `mft_request_keyframe_marks_next_packet_as_keyframe` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.4 | `mft_keyframe_flag_cleared_after_idr_emitted` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.5 | `mft_set_bitrate_updates_encoder_without_restart` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.6 | `mft_first_real_packet_is_annex_b` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.7 | `mft_setup_uses_config_dimensions_when_nonzero` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S11.8 | `mft_setup_falls_back_when_config_dimensions_zero` | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |
| S12.1 | full 18-test run on Host B | `crates/sm-infra/tests/windows_mft_encode.rs` | Y |

**Total scenarios**: 22
**BLOCKED_ON_SMOKE: Y**: 15 scenarios (S1.1, S2.1, S4.1, S5.1, S8.1, S10.1, S10.2, S11.1–S11.8, S12.1)
**CI-verifiable (no smoke)**: 7 scenarios (S1.2, S3.1, S6.1, S7.1, S7.2, S9.1 partial, R13–R15 direct)

---

## 6. Acceptance Criteria Checklist

Restated from proposal §8:

- [ ] **AC-1 (Host A primary)**: `mft_thirty_frame_smoke_emits_at_least_one_keyframe` PASSES on Host A on branch `feat/hw-encoder-mft-stream-change-handling`. Currently HANGS on `3c8bc48`.
- [ ] **AC-2 (Host A secondary)**: At least 7 of the 8 currently-timing-out tests listed in R11 PASS on Host A. If 1 fails with a different failure mode (not TIMEOUT), document as separate bug, do not block merge.
- [ ] **AC-3 (cross-vendor)**: T-NEW-1 and T-NEW-2 GREEN cross-vendor. No regression of PR #16's Bug 2 fix.
- [ ] **AC-4 (Host B)**: Host B smoke maintains ≥16/18 PASS. Pre-existing 2 failures (force-IDR carry-forward) are accepted.
- [ ] **AC-5 (CI)**: `cargo nextest run --workspace` GREEN on Linux + macOS + Windows. All 7 quality gates GREEN.

---

## 7. Out of Scope / Deferred

Per proposal §3:

- NVENC `SetOutputType: 0xC00D6D76` failure on Host B (Bug 1 Manifestation B) — separate future change `hw-encoder-mft-nvenc-setup-fix`.
- AMD AMF empirical verification — no hardware available on either test host.
- Flipping `default = ["hw-encoder"]` — separate planned change `hw-encoder-default-on-flip`.
- Adding a stream-change-specific unit/integration test that injects `MF_E_TRANSFORM_STREAM_CHANGE` — requires `trait MftLike` shim architecture that does not exist (proposal D8).
- Refactoring `try_setup_output_type` to be parameterized over error mapping — touches the init path from PR #17 (proposal D1 option B rejected).
- Domain-layer changes (`crates/sm-domain/src/encode.rs` FROZEN) — no new `EncoderError` variants, no `VideoEncoder` API changes.
- Triggering renegotiation on `output.dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE != 0` when `ProcessOutput` returns `Ok(())` — proposal OQ-C, deferred to design with recommendation to skip.

---

## 8. Open Questions for Design Phase

**OQ-A** — Inner HRESULT in `EncoderError::EncodeFailed`: R3 specifies that `renegotiate_output_type` carries the failing inner HRESULT (e.g., from `GetOutputAvailableType` or `SetOutputType`), not the triggering `MF_E_TRANSFORM_STREAM_CHANGE`. Design MUST confirm exact message format string and how to thread the inner error code through the match arms without shadowing the trigger context.

**OQ-B** — Should `renegotiate_output_type` be `pub(crate)` or `fn` (private)? Spec marks it as a private helper. Making it `pub(crate)` would allow future integration tests to call it directly if a shim architecture is ever added. Design should decide visibility; spec does not constrain it beyond "not public API".

**OQ-C** — `dwStatus`-only path (OQ-C from proposal): if `ProcessOutput` returns `Ok(())` but `output.dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE != 0`, the spec does NOT require triggering renegotiation (per MS spec they are mutually consistent). Design must confirm this skip is correct and document why in code comments, since future maintainers may see the unread flag and assume it is a bug.

**OQ-D** — Log level taxonomy: R6 requires `tracing::error!` on renegotiation failure. Design must verify this matches the existing log-level taxonomy in `windows_mft.rs` (all other `pump_loop` fatal errors already use `error!`; likely no divergence).

---

## 9. Strict TDD Constraints

ACTIVE. Commit sequence (proposal §7 commit chain):

1. **C1 (RED)** — `test(infra): assert thirty-frame smoke survives stop ordering`. 1-line swap: `enc.stop()` before `producer.join()`. RED on master because the test hangs (pipeline stalls before stop is reached, producer.join deadlocks). Demonstrates the ordering fix is necessary independently of the renegotiation fix.
2. **C2 (GREEN core)** — `feat(infra): renegotiate MFT output type on MF_E_TRANSFORM_STREAM_CHANGE`. `renegotiate_output_type` helper + `collect_output` STREAM_CHANGE arm rewrite + `*output_format_known = None` reset.
3. **C3 (GREEN observability)** — `feat(infra): trace dwStatus and ProcessOutput status flags on every collect_output call`. `tracing::trace!` additions only (~5–10 lines).
4. **C4 (optional)** — `style(infra): cargo fmt windows_mft.rs` if needed.

Every R that cannot be turned into a `#[test]` is flagged BLOCKED_ON_SMOKE in the requirements table. No requirement in this spec is untestable — those marked BLOCKED_ON_SMOKE are tested via the existing hardware smoke suite.

---

## 10. Smoke Transcript Requirements

BLOCKED_ON_SMOKE requirements (R1, R2, R4, R5, R8, R10, R11, R12) MUST be satisfied before the verify phase can emit a passing result. Verify will emit BLOCKED_ON_SMOKE status until the user supplies:

```
cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all
```

Required outcomes:
- Host A: smoke transcript at engram `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-a-branch` showing ≥17/18 PASS (target 18/18).
- Host B: regression transcript at engram `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-b-regression` showing ≥16/18 PASS.

---

## 11. Result Contract

- **status**: complete
- **executive_summary**: 15 requirements (R1–R15), 22 acceptance scenarios (S1.1–S12.1). 15 scenarios BLOCKED_ON_SMOKE (require Intel QSV on Host A or NVENC on Host B). 7 scenarios are CI/code-review-verifiable. Four design-phase OQs raised (OQ-A inner HRESULT format, OQ-B helper visibility, OQ-C dwStatus-only path, OQ-D log level taxonomy). No new test functions required; RED→GREEN evidence is the existing 8 timeout tests + 30-frame smoke on real hardware. Single PR, branch `feat/hw-encoder-mft-stream-change-handling`.
- **artifacts**: engram `sdd/hw-encoder-mft-vendor-compat-rework/spec` + `openspec/changes/hw-encoder-mft-vendor-compat-rework/spec.md`
- **next_recommended**: `sdd-design` (resolves OQ-A through OQ-D, then `sdd-tasks`)
- **risks**: MED — renegotiation success on Intel QSV mid-stream is an empirical assumption not yet confirmed (mandatory Host A smoke gate); LOW — D2 no-flush assumption load-bearing (if Host A smoke shows `SetOutputType` fails without flush, design phase must add `COMMAND_FLUSH` retry); LOW — ≤1 of 8 Host A tests may fail with a different root cause post-fix (AC-2 tolerates this).
- **skill_resolution**: injected
