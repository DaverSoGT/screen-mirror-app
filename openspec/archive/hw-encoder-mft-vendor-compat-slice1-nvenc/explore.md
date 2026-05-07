# Exploration: hw-encoder-mft-vendor-compat-rework

> **Persistence mirror**: this file is a verbatim mirror of engram observation
> #147 (`topic_key: sdd/hw-encoder-mft-vendor-compat-rework/explore`,
> revisions=2, last update 2026-05-04 23:40:44 UTC). Hybrid artifact store
> mode — both backends authoritative.

## Background

`hw-encoder-mft-rework` (PR #16, ee32ff4) fixed Bug 2 (stop-signal starvation via NO_WAIT polling + dual-arm counters) and was archived APPROVED_WITH_CARRY_FORWARD. Bug 1 — the vendor MFT priming/setup failure family — was confirmed multi-manifestation on two distinct hosts and hardware vendors, explicitly deferred. This new change `hw-encoder-mft-vendor-compat-rework` is the dedicated follow-up to fix Bug 1 across both manifestations. The tracing-before-explore convention (discovery #592, archive #604) is the MANDATORY anchor: empirical instrumented runs on both hosts must validate any proposed fix BEFORE spec/design begin.

---

## Bug Landscape

### Manifestation A — Intel QSV: Access Violation in `ProcessOutput` (Host A: Usuario\Desktop)

**Evidence** (from archive-report and apply-progress):
- Host A master baseline (f01f27f): 6/16 PASS, 5 ABORT (0xC0000005), 5 HANG (Bug 2)
- Host A branch (b0bfeec post-fix): 9/18 PASS; 5 ABORT (Manifestation A driver crash), 3 TIMEOUT, 1 HANG
- The 5 ABORT tests: `mft_encoded_packet_starts_with_annex_b_start_code`, `mft_encoded_packet_timestamp_matches_capture_frame`, `mft_keyframe_flag_cleared_after_idr_emitted`, `mft_request_keyframe_marks_next_packet_as_keyframe`, `mft_set_bitrate_updates_encoder_without_restart`
- The 3 TIMEOUT tests (packet never arrives, 5s timeout): `mft_first_real_packet_is_annex_b`, `mft_setup_falls_back_when_config_dimensions_zero`, `mft_setup_uses_config_dimensions_when_nonzero`
- The 1 HANG test (636s): `mft_thirty_frame_smoke_emits_at_least_one_keyframe`

**Crash profile**: The AV fires inside vendor driver code approximately 0.4s after init when `collect_output` first calls `mft.ProcessOutput(...)`. No HRESULT is returned — this is an OS-level structured exception (`0xC0000005`) inside the Intel QSV driver binary. Rust's error handling cannot intercept it.

**CRITICAL DISCOVERY from `bgra_to_nv12.rs` code read**: The `Nv12` struct uses tight NV12 layout: Y stride = `width` (no padding), UV stride = `chroma_w * 2` (no padding). Intel QSV (and most HW MFTs per Microsoft docs) require the NV12 buffer to have Y stride aligned to 16 or 64 bytes. A 640×480 frame has Y stride = 640 bytes (already 64-byte aligned). A 1280×720 frame has Y stride = 1280 (64-byte aligned). BUT the `MFT_OUTPUT_DATA_BUFFER` in `collect_output` is initialized as `MFT_OUTPUT_DATA_BUFFER::default()` (pSample = null) — this is correct for MFTs that allocate their own output samples (which ASYNC MFTs typically do). HOWEVER, the INPUT sample built by `build_imfsample` does NOT set `MF_MT_DEFAULT_STRIDE` on the sample, and the INPUT type we set via `SetInputType` does NOT include `MF_MT_DEFAULT_STRIDE`. Intel QSV may REQUIRE this attribute to know the actual stride of the input buffer.

**Timeout tests**: The 3 timeout tests (`mft_first_real_packet_is_annex_b`, etc.) wait 5s for the first encoded packet. These tests call `enc.stop()` after timeout. The pump_loop is running but NEVER receives a HaveOutput event (because ProcessOutput would crash if called, but actually the issue is the MFT never emits HaveOutput in the first place when the input NV12 is mis-strided). The 1 HANG test sends 30 frames over 30s+ — the producer thread eventually fills the channel and blocks.

### Manifestation B — NVIDIA NVENC: `SetOutputType` HRESULT `0xC00D6D76` (Host B: JDNHS)

**Evidence** (from verify-report and archive-report):
- Host B master baseline (f01f27f): 6/16 PASS, 10 FAIL (SetOutputType 0xC00D6D76)
- Host B branch (b0bfeec): 7/18 PASS, 11 FAIL
- ALL 11 failures share the same root: `setup_mft` fails at `mft.SetOutputType(0, &out_type, 0)` with HRESULT `0xC00D6D76` = `MF_E_INVALIDMEDIATYPE`
- 7 passing tests: the 3 lifecycle-only tests (new+drop, no-hardware, new+ok), `mft_new_does_not_submit_frames_to_mft_during_init`, `mft_stop_during_idle_returns_within_deadline` (T-NEW-1), `mft_stop_is_idempotent`, `mft_drop_without_stop_does_not_leak_thread` — all tests that call `setup_mft` AND proceed to encoding FAIL

**MF_E_INVALIDMEDIATYPE** means the *content* of the output media type we build is rejected by NVENC. Our output type attributes (from `setup_mft` lines 532–580):
- `MF_MT_MAJOR_TYPE` = `MFMediaType_Video` — standard, accepted
- `MF_MT_SUBTYPE` = `MFVideoFormat_H264` — standard
- `MF_MT_AVG_BITRATE` = `config.bitrate_bps` (4,000,000)
- `MF_MT_FRAME_SIZE` = `((w as u64) << 32) | h` — 640×480 or 1920×1080
- `MF_MT_FRAME_RATE` = `((30u64) << 32) | 1`
- `MF_MT_PIXEL_ASPECT_RATIO` = `(1u64 << 32) | 1`
- `MF_MT_INTERLACE_MODE` = `MFVideoInterlace_Progressive.0 as u32` = 2
- `MF_MT_MPEG2_PROFILE` = `eAVEncH264VProfile_Main.0 as u32` = 77

**Bottom line for Manifestation B**: NVENC rejects `SetOutputType` with our current output type. The rejection happens every time `setup_mft` is called on Host B for tests that use the full encoding path. The exact attribute causing rejection is UNKNOWN without Phase 0 trace instrumentation.

---

## MFT Enumeration — Current Behavior

`enumerate_and_activate()` (lines 347–395) calls `MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER`, input=NV12, output=H264, activates `pactivates[0]` — the FIRST hardware MFT returned. There is NO vendor-probe or selection logic. There is NO fallback to `pactivates[1]` if setup_mft fails. On a dual-GPU machine (Intel + NVIDIA), the enumeration winner is OS-dependent.

---

## Phase 0 Verdict: NOT sufficient — fresh instrumented runs required

Existing transcripts establish WHAT fails (HRESULT codes, test names, crash timing) but NOT the precise cause:

**Manifestation B gap**: Which specific attribute in `SetOutputType` causes NVENC to return `MF_E_INVALIDMEDIATYPE`? This is unknown. Requires Phase 0 attribute binary-search runs on Host B.

**Manifestation A gap**: Why does Intel QSV crash in `ProcessOutput`? Is it H-A1 (NV12 stride/padding), H-A2 (missing input type attribute), H-A5 (output buffer allocation), or H-A4 (driver-specific sequence requirement)? Requires Phase 0 trace runs on Host A.

**Transcripts taken WITHOUT `RUST_LOG=sm_infra::encode=trace`**: counter and event-type logs are absent. The `tracing::trace!` calls in pump_loop (event_type, counter snapshots) would reveal the exact event sequence before failures.

### Required Phase 0 Runs

#### For Manifestation B (Host B = JDNHS, NVIDIA NVENC)
Add attribute-walk trace in `setup_mft` before `SetOutputType`:
```rust
tracing::trace!("setup_mft: about to SetOutputType (w={}, h={}, fps={}, bps={}, profile={})", w, h, config.framerate, config.bitrate_bps, H264_PROFILE_MAIN);
```
After failure, attempt `SetOutputType` with progressively stripped types — binary search:
- Round 1: MAJOR_TYPE + SUBTYPE only → if PASS, add attributes one by one
- Round 2: add FRAME_SIZE (likely required)
- Round 3: add FRAME_RATE
- Round 4: add AVG_BITRATE
- Round 5: add INTERLACE_MODE
- Round 6: add MPEG2_PROFILE (this is the most likely culprit per H-B1)
- Round 7: add PAR

Also try: call `mft.GetOutputAvailableType(0, 0)` first, log its attributes.

#### For Manifestation A (Host A = Usuario\Desktop, Intel QSV)
Add `MF_MT_DEFAULT_STRIDE` to the input type set in `setup_mft`:
```rust
in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, w).map_err(...)?;
```
Also add trace in `build_imfsample` logging sample size, and in `collect_output` before `ProcessOutput`.

---

## Candidate Root-Cause Hypotheses

### Manifestation A (Intel QSV driver AV in `ProcessOutput`)

**H-A1 (HIGHEST CONFIDENCE) — Missing `MF_MT_DEFAULT_STRIDE` on input type**: The `Nv12` struct uses tight stride (width bytes, no padding). The MFT INPUT type we set via `SetInputType` does NOT include `MF_MT_DEFAULT_STRIDE`. Without this, Intel QSV may internally assume an aligned stride (e.g., `ALIGN(width, 64)`) and read out-of-bounds when accessing the input sample's buffer, causing the AV.
*Falsification*: Add `in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, w)` to `setup_mft`. Run Host A smoke tests.

**H-A2 — Missing `MF_MT_VIDEO_NOMINAL_RANGE`**: Intel QSV may require explicit specification of the video nominal range (limited vs. full range).
*Falsification*: Add `MF_MT_VIDEO_NOMINAL_RANGE = MFNominalRange_16_235` to input type.

**H-A3 — `MFT_OUTPUT_DATA_BUFFER` allocation requirement for Intel QSV**: Some MFT implementations require the caller to pre-allocate the output sample buffer.
*Falsification*: Query `IMFTransform::GetOutputStreamInfo()` for the `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` flag.

**H-A4 — Sequence violation: `ProcessOutput` called before MFT pipeline is fully primed**: If Intel QSV emits a spurious `HaveOutput` before the encode pipeline is fully initialized, calling `ProcessOutput` may crash.
*Falsification*: Confirmed pattern per apply-progress: AV fires inside vendor `ProcessOutput` ~0.4s after init.

**H-A5 — Known Intel QSV driver bug, requires `MFT_MESSAGE_COMMAND_DRAIN` before first output**: Some Intel QSV driver versions require an explicit `COMMAND_DRAIN` after the first `NeedInput` sequence.
*Falsification*: Add a `COMMAND_DRAIN` call after the first `ProcessInput` and before `ProcessOutput`.

### Manifestation B (NVIDIA NVENC `SetOutputType` HRESULT `0xC00D6D76`)

**H-B1 (HIGHEST CONFIDENCE) — `MF_MT_MPEG2_PROFILE = Main(77)` rejected**: NVENC's H.264 MFT may require `eAVEncH264VProfile_High` (100) or refuse `Main` profile in the output type.
*Falsification*: Remove `MF_MT_MPEG2_PROFILE` from `SetOutputType` call.

**H-B2 — `MF_MT_AVG_BITRATE` rejected in output type**: NVENC may control bitrate exclusively via `ICodecAPI`.
*Falsification*: Remove `MF_MT_AVG_BITRATE` from `SetOutputType` call. Set bitrate only via `codec_api.SetValue(CODECAPI_AVEncCommonMeanBitRate)`.

**H-B3 — `GetOutputAvailableType`-guided approach**: NVENC may require the caller to base the output type on what `GetOutputAvailableType` returns.
*Falsification*: Call `mft.GetOutputAvailableType(0, 0)`, clone the returned type, set FRAME_SIZE and FRAME_RATE on top, then `SetOutputType`.

**H-B4 — `MF_MT_INTERLACE_MODE` encoding issue**: Some NVENC versions may require this attribute to be absent.
*Falsification*: Remove `MF_MT_INTERLACE_MODE` from `SetOutputType`.

**H-B5 — Resolution-specific rejection**: 640×480 may be outside NVENC's supported resolution range at certain frame rates/bitrates.
*Falsification*: Try 1920×1080 instead of 640×480.

---

## NV12 Stride Analysis (H-A1 Clarification)

From `bgra_to_nv12.rs` code read:
- `Nv12::new(width, height)`: Y plane = `width * height` bytes (tight, no padding). UV plane = `chroma_w * chroma_h * 2` bytes (tight).
- `build_imfsample`: creates `MFCreateMemoryBuffer(total)` where `total = nv12.buf.len()`. The buffer is FLAT with no stride padding. The MFT must infer stride from the media type.
- In `SetInputType`, we do NOT set `MF_MT_DEFAULT_STRIDE`.
- Intel QSV may internally compute stride as `ALIGN(width, 64)` regardless of what we submit, and then read `ALIGN(width, N) * height` bytes from the buffer — but we only allocated `width * height` bytes. This would be an out-of-bounds read → AV. HIGH CONFIDENCE for H-A1.
- FIX: Set `MF_MT_DEFAULT_STRIDE = width` on the input media type AND potentially pad the Y plane to `ALIGN(width, 64) * height` in `Nv12::new`.

---

## Approach Comparison Table

| Approach | Description | Pros | Cons | Effort |
|----------|-------------|------|------|--------|
| A. One change, fix both manifestations simultaneously | Single PR | One PR cycle; unified | Larger diff; cross-vendor interaction risk | High |
| B. Two sequential slices: B first then A | Slice 1 = NVENC setup_mft. Slice 2 = Intel stride | Faster partial improvement; isolates risk | Two PR cycles | Medium+Medium |
| C. `GetOutputAvailableType`-guided type negotiation (applies to B) | Ask the MFT what types it supports | Vendor-agnostic | May return E_NOTIMPL on some MFTs | Medium |
| D. Enumeration fallback loop (PQ-4) | Try each activated MFT in order | Resilient to dual-GPU machines | Does not fix setup_mft per se | Low (20 LOC) |

**Recommended path**: Approach B. Slice 1 targets Manifestation B (NVENC `setup_mft`) via Phase 0 attribute binary-search on Host B. Slice 2 targets Manifestation A (Intel QSV) via Phase 0 stride investigation on Host A.

---

## Pending Questions (PQs — require user decision before proposal)

### PQ-1 (BLOCKER) — Phase 0 fresh runs: timing and responsibility
Existing transcripts are NOT sufficient for design-level root-cause analysis. Fresh runs with added trace points are required on both hosts BEFORE proposal can be written.

- **Option A**: User runs Phase 0 now (adds trace points per this exploration, runs on Host B + Host A, saves transcripts). Proposal proceeds after transcripts are reviewed.
- **Option B**: Proposal is written against hypotheses; Phase 0 runs are the FIRST task in sdd-apply; verify is BLOCKED_ON_PHASE0 until transcripts confirm or refute.
- **Option C**: Only Phase 0 for Manifestation B (Host B) is needed immediately. Manifestation A investigation is deferred to Slice 2.

**Recommendation**: Option C — run Manifestation B (Host B) Phase 0 now (attribute binary-search is fast); Manifestation A deferred to Slice 2 with its own Phase 0.

### PQ-2 (BLOCKER) — One change or two sequential slices?
- **Option A**: Single change covering both manifestations
- **Option B**: Two sequential changes (Slice 1 = NVENC `setup_mft`, Slice 2 = Intel QSV crash) — RECOMMENDED
- **Option C**: Two parallel changes (not recommended)

### PQ-3 (SCOPE) — Should T-NEW-3 be added in this change?
T-NEW-3 (`mft_handles_have_output_before_need_input`) was deferred from predecessor.
**Recommendation**: keep deferred — existing 18 tests are sufficient regression coverage.

### PQ-4 (SCOPE) — Enumeration fallback loop: in scope for this change?
If `setup_mft` fails on the first activated MFT, should we try the next one?
**Recommendation**: add if scope allows — low-risk improvement that makes Manifestation B self-healing on dual-GPU machines.

### PQ-5 (CODE TASK) — NV12 stride alignment: add `MF_MT_DEFAULT_STRIDE` to input type?
This is not a user preference question — it is a technical task for Phase 0. HIGHEST CONFIDENCE hypothesis for Manifestation A.

---

## Affected Areas

- `crates/sm-infra/src/encode/windows_mft.rs` — `setup_mft` (lines 511–637), `enumerate_and_activate` (347–395), `collect_output` (1011–1081). Phase 0 will add trace points here.
- `crates/sm-infra/src/encode/bgra_to_nv12.rs` — `Nv12::new` stride/padding, `convert` function. May need stride alignment for H-A1 fix. Note: this file is always compiled (no hw-encoder gate), so changes must be backward-compatible with SW path.
- `crates/sm-infra/tests/windows_mft_encode.rs` — 18 existing `#[ignore]` smoke tests. Target: 18/18 PASS on both hosts after fix.
- `crates/sm-domain/src/encode.rs` — `VideoEncoder`/`EncoderConfig`: FROZEN, MUST NOT change.

---

## Risks and Unknowns

1. **Manifestation A may be a driver bug unrecoverable in Rust**: A `0xC0000005` AV inside driver code is an OS-level exception. SEH from Rust requires unsafe code via `windows-sys` or a C shim.
2. **NVENC attribute rejection is unconfirmed until Phase 0**: H-B1 (MPEG2_PROFILE) and H-B2 (AVG_BITRATE) are the most likely causes.
3. **`MF_MT_DEFAULT_STRIDE` availability in `windows = "0.62.2"`**: Must be verified as present in the `Win32_Media_MediaFoundation` feature. May require manually-defined GUID constant.
4. **NV12 stride fix may affect SW path**: `bgra_to_nv12.rs` is not gated behind `hw-encoder`. Changes must pass existing unit tests and not break the SW path.
5. **`GetOutputAvailableType` may return `E_NOTIMPL`**: If it fails, approach C (H-B3) is not viable.
6. **Multi-vendor machine enumeration is untested**: Enumeration fallback (PQ-4) cannot be smoke-tested on either current host.
7. **Scope creep risk**: Both manifestations differ in nature. Combining them in one change risks entangling incompatible fixes.

---

## Result Contract

- **status**: done
- **executive_summary**: Both manifestations of Bug 1 are clearly classified: Manifestation B (NVIDIA NVENC `SetOutputType` rejection, `0xC00D6D76`) is in the `setup_mft` initialization path and is likely caused by NVENC rejecting `MF_MT_MPEG2_PROFILE = Main(77)` or `MF_MT_AVG_BITRATE` in the output type (H-B1/H-B2 highest confidence); Manifestation A (Intel QSV driver AV in `ProcessOutput`, `0xC0000005`) is likely caused by missing `MF_MT_DEFAULT_STRIDE` on the input type (H-A1 high confidence). Existing transcripts are NOT sufficient for design — Phase 0 attribute binary-search runs on Host B are required before Slice 1 proposal. Five PQs require user confirmation, with PQ-1 and PQ-2 being blockers. Recommended path: two sequential slices with Phase 0 runs on Host B first.
- **artifacts**: engram topic_key sdd/hw-encoder-mft-vendor-compat-rework/explore + openspec/changes/hw-encoder-mft-vendor-compat-rework/explore.md
- **next_recommended**: sdd-propose (with PQ-1 + PQ-2 confirmation)
- **risks**: see Risks and Unknowns
- **pq_count**: 5
