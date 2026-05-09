# Exploration: hw-encoder-mft-vendor-compat-rework

> Phase: SDD explore. Inputs: prompt head-start (engram #628/#629/#630/#605/#604/#600/#601/#602/#186 summarized in launch prompt) + direct file/web reads.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-vendor-compat-rework/explore` (#631) + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).

---

## 1. Inputs Read

| Source | How Read |
|--------|---------|
| Prompt HEAD START (observations #628, #629, #630 summarized) | Injected context |
| Previous explore.md | `openspec/archive/hw-encoder-mft-vendor-compat-slice1-nvenc/explore.md` — full file read |
| Slice 1 archive report | `openspec/archive/hw-encoder-mft-vendor-compat-slice1-nvenc/archive-report.md` — full file read |
| `windows_mft.rs` | Full read of lines 1–1460 |
| `windows_mft_encode.rs` | Full read — all 18 test definitions |
| Microsoft Learn — Handling Stream Changes | Web fetch |
| Microsoft Learn — `_MFT_PROCESS_OUTPUT_STATUS` | Web fetch |
| Microsoft Learn — `MFT_OUTPUT_DATA_BUFFER` | Web fetch |
| Microsoft Learn — Asynchronous MFTs | Web fetch |
| Microsoft Learn — Basic MFT Processing Model | Web fetch |

Engram tools (`mem_search`, `mem_get_observation`) were unavailable in this executor context. Required inputs were covered via prompt head-start + direct file/web reads.

---

## 2. Problem Statement

On Intel QSV Host A (commit `3c8bc48`, master), the `pump_loop` in `crates/sm-infra/src/encode/windows_mft.rs` stalls indefinitely after approximately 17 frames because `collect_output` (line 1317) silently returns `Ok(None)` when `ProcessOutput` fires `MF_E_TRANSFORM_STREAM_CHANGE`, without performing the Microsoft-mandated output-type renegotiation (`GetOutputAvailableType` + `SetOutputType`). Intel QSV's driver signals a format/GOP-boundary renegotiation at approximately frame 17; because the client never re-calls `SetOutputType`, the MFT's pipeline enters a permanent stall — it stops emitting `METransformHaveOutput` events and `ho_count` drains to zero. The producer thread fills the bounded channel and blocks on `frame_tx.send()`. This manifests as `mft_thirty_frame_smoke_emits_at_least_one_keyframe` hanging at `producer.join()` (after the 10-second deadline loop exits without receiving enough packets), and all 8 other full-encoding tests on Host A timing out with no packets received.

---

## 3. Microsoft-Documented Protocol for `MF_E_TRANSFORM_STREAM_CHANGE`

**Primary sources**:
- [Handling Stream Changes](https://learn.microsoft.com/en-us/windows/win32/medfound/handling-stream-changes)
- [Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts)
- [`_MFT_PROCESS_OUTPUT_STATUS`](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ne-mftransform-_mft_process_output_status)
- [`MFT_OUTPUT_DATA_BUFFER`](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ns-mftransform-mft_output_data_buffer)

**When an async MFT emits this error**: When it needs to change its output format during streaming (e.g., internal GOP/rate-control reconfiguration) or when it adds/removes output streams. Hardware H.264 encoders use this for internal format updates.

**What the host must do (4-step protocol)**:
1. `ProcessOutput` returns `MF_E_TRANSFORM_STREAM_CHANGE`. MFT does NOT produce an output sample. MFT MUST set `MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` in `output.dwStatus` (from `_MFT_OUTPUT_DATA_BUFFER_FLAGS` — a DIFFERENT enum than `_MFT_PROCESS_OUTPUT_STATUS`).
2. Client calls `GetOutputAvailableType(stream_id, 0)` to retrieve the updated type.
3. Client calls `SetOutputType(stream_id, &new_type, 0)`.
4. Client resumes calling `ProcessInput` / `ProcessOutput`. Until steps 2–3 complete, further `ProcessOutput` calls return `MF_E_TRANSFORM_STREAM_CHANGE` again.

**`MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` vs. `MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS`**:
- `output.dwStatus` (`_MFT_OUTPUT_DATA_BUFFER_FLAGS`) is the per-buffer status — `FORMAT_CHANGE` goes here.
- The call-level `&mut status` (`_MFT_PROCESS_OUTPUT_STATUS`) has only one defined value: `MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS = 0x100` for new stream creation.
- Current code declares `let mut status: u32 = 0` but never reads either field after the call. Both should be logged at `trace!` level.

**`BEGIN_STREAMING` / `START_OF_STREAM` after renegotiation?** NO. The MS docs for async MFTs are explicit: after a mid-stream format change, the client calls `SetOutputType` and resumes `ProcessOutput`. The `NOTIFY_BEGIN_STREAMING` and `NOTIFY_START_OF_STREAM` messages are initial-setup only.

**Sync vs. async difference**: Sync MFTs with `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = FALSE` require drain (`COMMAND_DRAIN` + `ProcessOutput` until `NEED_MORE_INPUT`) before renegotiation. Async MFTs (always `TRUE`) do NOT require drain. This codebase targets async hardware MFTs exclusively.

---

## 4. Code-Level Investigation

**The exact swallow** (`windows_mft.rs:1317`):
```rust
Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),
```
This is inside `collect_output`. The `MFT_OUTPUT_DATA_BUFFER` struct's `dwStatus` field and the call-level `status: u32` are BOTH unread after the `ProcessOutput` call.

**`try_setup_output_type` (lines 589–630)**: Performs exactly what renegotiation needs — `GetOutputAvailableType(0, 0)` clone + overlay of FRAME_SIZE, FRAME_RATE, AVG_BITRATE + `SetOutputType`. It exists because Slice 1 designed this as the general output-type negotiation strategy. Its errors map to `EncoderError::InitFailed`. For mid-stream renegotiation, errors should map to `EncoderError::EncodeFailed`. The cleanest approach: extract a thin `renegotiate_output_type(mft, w, h, framerate, bitrate_bps) -> Result<(), EncoderError>` helper that performs the same COM operations but with `EncodeFailed` error mapping. This avoids touching the init path.

**Where renegotiation should happen**: Inside `collect_output`, co-located with `ProcessOutput`. The function already has `mft` access. Renegotiation is semantically part of the `ProcessOutput` response protocol — keeping it here is natural. No new return type needed; `Ok(None)` still signals "output was attempted but no packet produced this round, retry on the next `HaveOutput` event."

**`output.pSample` on stream change**: Per spec, no sample is produced when `MF_E_TRANSFORM_STREAM_CHANGE` fires. `output.pSample` is null. The fix returns `Ok(None)` after renegotiation, before `output.pSample.take()` — no leak.

**`output_format_known` on stream change**: Should be reset to `None` to force re-sniff of Annex-B vs. AVCC on the next successful packet. Correctness guarantee: the vendor might change its encoding format across the renegotiation boundary.

**`dwStatus` flags**: Both `output.dwStatus` and call-level `status` are unread. Neither is required for correctness (the HRESULT alone suffices), but both should be logged at `trace!` level for diagnostics.

---

## 5. Cross-Vendor Impact Analysis

| Vendor | Host | Impact | Notes |
|--------|------|--------|-------|
| Intel QSV | Host A | **PRIMARY TARGET** — fix unblocks 9 tests | STREAM_CHANGE confirmed at ~frame 17 |
| NVIDIA NVENC | Host B | **ORTHOGONAL** — 16/18 PASS maintained | NVENC never reaches streaming phase where STREAM_CHANGE fires; if it ever does, fix handles it transparently |
| AMD AMF | Neither | **UNTESTED** — hypothesis: compliant | Async MFT spec mandates `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE`; fix is a no-op if not triggered |
| Microsoft SW Encoder | N/A | **NOT IN SCOPE** — not enumerated | `MFT_ENUM_FLAG_HARDWARE` excludes software encoder |

---

## 6. Test Design Considerations

**`mft_thirty_frame_smoke_emits_at_least_one_keyframe` deadlock**: The test calls `producer.join()` BEFORE `enc.stop()`. When the encoding stalls, the bounded `frame_tx` channel fills, the producer blocks, and `producer.join()` never returns. Fix: swap the order — call `enc.stop()` before `producer.join()`. This is a 1-line change in the test file. Recommended to include in this PR: the ordering is unconditionally wrong (latent bug independent of encoding correctness).

**Will the fix unblock all 8 timeouts?** Yes, with high confidence. The counter-stall pattern in trace #629 is identical across all 8 timeout tests — all show the pipeline freezing after approximately the same number of frames. Single root cause (STREAM_CHANGE at the same pipeline initialization point).

**Stream-change-specific RED test?** Not recommended in this change. There is no practical way to force Intel QSV's MFT to emit `MF_E_TRANSFORM_STREAM_CHANGE` from test code without a mock/shim architecture that doesn't exist in this codebase. The 8 existing timeout tests serve as the empirical RED→GREEN proof on real hardware.

---

## 7. Open Questions for the Proposal Phase

- **OQ-1**: Renegotiation function factoring — thin `renegotiate_output_type` wrapper (Approach C) vs. direct call to `try_setup_output_type` with inline error translation (Approach A)?
- **OQ-2**: Flush before renegotiation — does Intel QSV require `COMMAND_FLUSH` before `GetOutputAvailableType`/`SetOutputType` mid-stream? MS spec says no; empirical first-run answer needed.
- **OQ-3**: Behavior on renegotiation failure — exit encoder thread (`return` from `pump_loop`) vs. retry N times? Recommendation: exit.
- **OQ-4**: Read and log `output.dwStatus` and call-level `status` in `collect_output`? Recommendation: yes, `trace!` level only.
- **OQ-5**: Fix `producer.join()` / `enc.stop()` ordering bug in the 30-frame test in this PR? Recommendation: yes — 1-line, zero risk.
- **OQ-6**: Reset `output_format_known` to `None` on stream change? Recommendation: yes — correctness guarantee.
- **OQ-7**: PR sizing forecast — total diff ~25–50 lines in production code + 1 line in test. Single PR, well under 400-line budget. `Decision needed before apply: No`, `Chained PRs recommended: No`, `400-line budget risk: Low`.

---

## 8. Risks / Unknowns

- **MEDIUM** — Renegotiation success on Intel QSV mid-stream is not yet empirically confirmed. `try_setup_output_type` succeeds at init; behavior when called during `MF_E_TRANSFORM_STREAM_CHANGE` handling depends on Intel's driver. Mandatory gate: smoke-trace.ps1 re-run on Host A.
- **LOW** — `GetOutputAvailableType` at stream-change time may return a type with different attributes than at init time. Clone-and-overlay approach handles this gracefully for all attributes except FRAME_SIZE (if vendor changes it, our overlay restores the configured dims — may or may not be correct for the vendor).
- **LOW** — Multiple consecutive `MF_E_TRANSFORM_STREAM_CHANGE` events: handled correctly — each `collect_output` call triggers one renegotiation attempt.
- **LOW** — If renegotiation fails AND `producer.join()` ordering is not fixed, the 30-frame test continues to deadlock. This is why OQ-5 (fix the ordering) is doubly important.
- **LOW** — Host B verify run must confirm 16/18 PASS is maintained (no regression from the renegotiation code path on NVENC).

---

## 9. Recommended Next Phase

Proceed to propose with OQs 1–7 as inputs. OQ-1 and OQ-2 are the only design decisions requiring explicit proposal-phase resolution. OQs 3–7 have clear recommendations and can be rubber-stamped with brief rationale.

---

## Sources

- [Handling Stream Changes — Win32 apps | Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/medfound/handling-stream-changes)
- [`_MFT_PROCESS_OUTPUT_STATUS` (mftransform.h) — Win32 apps | Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ne-mftransform-_mft_process_output_status)
- [`MFT_OUTPUT_DATA_BUFFER` (mftransform.h) — Win32 apps | Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ns-mftransform-mft_output_data_buffer)
- [Asynchronous MFTs — Win32 apps | Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts)
- [Basic MFT Processing Model — Win32 apps | Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/medfound/basic-mft-processing-model)
