# Design: hw-encoder-mft-vendor-compat-rework (Slice 2 — Intel QSV stream-change renegotiation)

> Phase: SDD design. Inputs: proposal #632 (8 LOCKED, 4 OQs), explore #631, root-cause #630, DIAG trace #629, predecessor design #597 (DD format precedent), spec (R1–R9).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-vendor-compat-rework/design` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`). Single PR. Branch `feat/hw-encoder-mft-stream-change-handling`.
> Date: 2026-05-07.

---

## 1. Inputs

| Source | Topic key / Path | Observation ID |
|--------|------------------|----------------|
| Proposal | `sdd/hw-encoder-mft-vendor-compat-rework/proposal` | #632 |
| Exploration | `sdd/hw-encoder-mft-vendor-compat-rework/explore` | #631 |
| Root-cause analysis | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-root-cause` | #630 |
| DIAG trace | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-trace-3c8bc48` | #629 |
| Predecessor design (PR #16, DD format precedent) | `sdd/hw-encoder-mft-rework/design` | #597 |
| sdd-init | `sdd-init/screen-mirror-app` | #186 |
| Code under change | `crates/sm-infra/src/encode/windows_mft.rs` | line 1317 swallow + 589–630 helper + 1108–1148 HO arm |
| Test under change | `crates/sm-infra/tests/windows_mft_encode.rs` | lines 241–242 ordering |
| MS docs | [Handling Stream Changes](https://learn.microsoft.com/en-us/windows/win32/medfound/handling-stream-changes), [Async MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts), [`MFT_OUTPUT_DATA_BUFFER`](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ns-mftransform-mft_output_data_buffer), [`_MFT_PROCESS_OUTPUT_STATUS`](https://learn.microsoft.com/en-us/windows/win32/api/mftransform/ne-mftransform-_mft_process_output_status) | n/a |

Line-number reconciliation: root-cause #630 cites `windows_mft.rs:1338`; proposal §3 and the actual current master (`3c8bc48`) confirm the swallow at **line 1317**. Design references line 1317 throughout.

---

## 2. Architecture Overview

The renegotiation flow is co-located with the COM call that triggers it (Approach A from explore §4.3). When `IMFTransform::ProcessOutput` returns `MF_E_TRANSFORM_STREAM_CHANGE`, `collect_output` invokes a new `renegotiate_output_type(mft, w, h, framerate, bitrate_bps)` helper that performs `GetOutputAvailableType(0, 0)` + clone + overlay (FRAME_SIZE / FRAME_RATE / AVG_BITRATE) + `SetOutputType(0, &out_type, 0)` — identical COM sequence to `try_setup_output_type` but mapping errors to `EncoderError::EncodeFailed("renegotiate: …")` instead of `InitFailed`. On success, `*output_format_known = None` (reset BEFORE the renegotiation call so a failure still leaves the cache invalid), `collect_output` returns `Ok(None)`; the next pump iteration's HO arm consumes the credit and the vendor MFT resumes emitting `METransformHaveOutput`. On failure, `Err(EncodeFailed)` propagates through `collect_output`'s caller in the HO arm of `pump_loop` (lines 1108–1148), where the existing `string-prefix` error-classification pattern (DR-NEW-2 from #597) recognises the `"renegotiate"` reason, logs `tracing::error!`, and `return`s — terminating the encoder thread cleanly. No new threads, no new sync primitives, no domain-layer changes, no `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` resend (per MS async-MFT spec).

---

## 3. Sequence Diagrams

### 3.1 Successful Renegotiation Flow (Intel QSV @ ~frame 17)

```
pump_loop                   collect_output            IMFTransform               renegotiate_output_type
   │                              │                         │                              │
   │  ho_count > 0                │                         │                              │
   ├─────────────────────────────▶│                         │                              │
   │  collect_output(...)         │                         │                              │
   │                              │  ProcessOutput(0,&buf,&status)                         │
   │                              ├────────────────────────▶│                              │
   │                              │                         │                              │
   │                              │  Err(MF_E_TRANSFORM_STREAM_CHANGE)                     │
   │                              │◀────────────────────────┤                              │
   │                              │                         │                              │
   │                              │  trace!(dwStatus=0x100  │                              │
   │                              │         status=0)       │                              │
   │                              │                         │                              │
   │                              │  *output_format_known = None  (DD6: BEFORE call)      │
   │                              │                         │                              │
   │                              │  renegotiate_output_type(mft, w, h, fps, br)           │
   │                              ├──────────────────────────────────────────────────────▶│
   │                              │                         │                              │
   │                              │                         │   GetOutputAvailableType(0,0)│
   │                              │                         │◀─────────────────────────────┤
   │                              │                         │   Ok(out_type)               │
   │                              │                         ├─────────────────────────────▶│
   │                              │                         │                              │
   │                              │                         │   SetUINT64(FRAME_SIZE)      │
   │                              │                         │   SetUINT64(FRAME_RATE)      │
   │                              │                         │   SetUINT32(AVG_BITRATE)     │
   │                              │                         │◀─────────────────────────────┤
   │                              │                         │                              │
   │                              │                         │   SetOutputType(0,&out,0)    │
   │                              │                         │◀─────────────────────────────┤
   │                              │                         │   Ok(())                     │
   │                              │                         ├─────────────────────────────▶│
   │                              │  Ok(())                 │                              │
   │                              │◀──────────────────────────────────────────────────────┤
   │                              │                         │                              │
   │                              │  return Ok(None)        │                              │
   │  Ok(None)                    │                         │                              │
   │◀─────────────────────────────┤                         │                              │
   │  ho_count -= 1 (existing)    │                         │                              │
   │                              │                         │                              │
   │  next iteration              │                         │                              │
   │  GetEvent(NO_WAIT) →         │                         │                              │
   │  METransformHaveOutput       │                         │                              │
   │  → ho_count += 1             │                         │                              │
   │                              │                         │                              │
   │  collect_output → Ok(Some(pkt))  ◀── pipeline resumed                                 │
```

### 3.2 Renegotiation Failure Flow

```
pump_loop                   collect_output            renegotiate_output_type
   │                              │                              │
   │  collect_output(...)         │                              │
   ├─────────────────────────────▶│                              │
   │                              │  ProcessOutput → Err(STREAM_CHANGE)
   │                              │  *output_format_known = None
   │                              │  renegotiate_output_type(...)
   │                              ├─────────────────────────────▶│
   │                              │                              │  GetOutputAvailableType
   │                              │                              │  → Err(0xC00D36BA)
   │                              │  Err(EncodeFailed(           │
   │                              │   "renegotiate: GetOutputAvailableType: 0xC00D36BA"))
   │                              │◀─────────────────────────────┤
   │                              │                              │
   │  Err(EncodeFailed("renegotiate: …"))
   │◀─────────────────────────────┤                              │
   │                              │                              │
   │  match arm (HO loop):        │                              │
   │    reason.contains("renegotiate") = true                   │
   │  tracing::error!(             │                              │
   │   "pump_loop: renegotiate_output_type failed: {e}")        │
   │  return  (encoder thread exits)                            │
   │                              │                              │
```

Test-side consequence: `enc.stop()` (called BEFORE `producer.join()` per DD7) drops the receiver, the producer's bounded-channel `send` returns `SendError`, producer exits, `producer.join()` returns. Test fails on the assertion (no IDR observed) instead of hanging at `producer.join()`.

---

## 4. Decision Details (DD1–DD10)

### DD1 — `renegotiate_output_type` signature and placement (R1, R2, R3, D1)

**Choice**: Add a private function adjacent to `try_setup_output_type` (around line 631 in `windows_mft.rs`, immediately after `try_setup_output_type`'s closing brace).

```rust
fn renegotiate_output_type(
    mft: &IMFTransform,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<(), EncoderError> { … }
```

**Visibility**: module-private `fn` (not `pub(crate)`). It is only invoked from `collect_output` in the same module; broader visibility offers no testing benefit because the function is not unit-testable without a real `IMFTransform` (no mock infrastructure, see explore §6.3).

**Rationale**: Matches `try_setup_output_type`'s signature exactly so a future cleanup can extract a shared inner helper without rippling call sites. Co-locating the two functions keeps the COM-protocol implementation in one region of the file.

**References**: spec R1, R2, R3; proposal D1; explore §4.2 Option A.

### DD2 — Renegotiation invocation site inside `collect_output` (R1)

**Choice**: Replace line 1317's `Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),` with a multi-line arm that:

1. Reads `output.dwStatus` and `status` (declared at lines 1311–1312, currently unread) into `trace!` log line.
2. Sets `*output_format_known = None` (DD6 ordering).
3. Calls `renegotiate_output_type(mft, …)` and propagates `Err(...)` via `?`.
4. On success, returns `Ok(None)` to drain the existing HO credit; the next iteration's GetEvent loop will receive a fresh `METransformHaveOutput` once the vendor resumes.

**Why not in the HO arm of `pump_loop`?** Approach B (pump_loop owns the renegotiation) would require adding a third `collect_output` return variant or a sentinel error, leaking output-type-management into the HO state machine. Co-locating in `collect_output` keeps `pump_loop`'s drain-first counter logic intact (DD1 from PR #16 design).

**Caller config plumbing**: `collect_output` does NOT currently receive `&EncoderConfig` or `(w, h, framerate, bitrate_bps)`. We MUST extend its signature. The invariant is that the renegotiation MUST use the same `effective_dimensions(config)` tuple and the same `config.framerate` / `config.bitrate_bps` that `setup_mft` originally used. See DD3 for the plumbing strategy.

**References**: spec R1; proposal D1; explore §4.3 Option 1.

### DD3 — Config plumbing into `collect_output` (R2)

**Choice**: Extend `collect_output` signature to accept four scalar args `(w, h, framerate, bitrate_bps)` rather than `&EncoderConfig`. `pump_loop` already has `&EncoderConfig` (`cfg`) available (line 1156 area); resolves `(w, h)` once via `effective_dimensions(cfg)` outside the loop and shadows `cfg.framerate` / `cfg.bitrate_bps` as locals. This avoids passing `&EncoderConfig` deep into per-packet code paths.

```rust
fn collect_output(
    mft: &IMFTransform,
    output_format_known: &mut Option<bool>,
    frame_timestamp: std::time::Duration,
    seq: &mut u64,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<Option<EncodedPacket>, EncoderError> { … }
```

**Why not pass `&EncoderConfig`?** Two reasons: (1) `EncoderConfig` lives in `sm-domain` (frozen), and threading it deeper widens the coupling surface; (2) the four scalars cleanly express the renegotiation contract — only these four fields are honoured by `try_setup_output_type` / `renegotiate_output_type`.

**Single call site**: `collect_output` is called once, from `pump_loop`'s HO arm at line 1112. One signature-update site.

**References**: spec R2; explore §4.2.

### DD4 — Error handling matrix (R3, R6)

| Failure point | EncoderError variant | Reason string |
|---------------|---------------------|---------------|
| `GetOutputAvailableType(0, 0)` returns Err | `EncoderError::EncodeFailed` | `"renegotiate: GetOutputAvailableType: 0x{:08X}"` |
| `SetUINT64(MF_MT_FRAME_SIZE)` returns Err | `EncoderError::EncodeFailed` | `"renegotiate: SetUINT64 FrameSize: 0x{:08X}"` |
| `SetUINT64(MF_MT_FRAME_RATE)` returns Err | `EncoderError::EncodeFailed` | `"renegotiate: SetUINT64 FrameRate: 0x{:08X}"` |
| `SetUINT32(MF_MT_AVG_BITRATE)` returns Err | `EncoderError::EncodeFailed` | `"renegotiate: SetUINT32 Bitrate: 0x{:08X}"` |
| `SetOutputType(0, &out_type, 0)` returns Err | `EncoderError::EncodeFailed` | `"renegotiate: SetOutputType: 0x{:08X}"` |

**Propagation contract**: `collect_output` returns `Err(EncoderError::EncodeFailed("renegotiate: …"))`. The HO arm in `pump_loop` (lines 1131–1146) currently classifies via `reason.contains("ProcessOutput: 0x80004005")` (DR-NEW-2 in #597). We extend the same string-classification pattern: `if reason.contains("renegotiate") { tracing::error!(...); return; }`.

**Why string-match instead of a typed variant?** Adding a typed `EncoderError::Renegotiation` variant requires touching `crates/sm-domain/src/encode.rs`, which is FROZEN (proposal §3 OUT scope). The string-prefix pattern is already established for vendor-priming `E_UNEXPECTED` recognition (#597 DD4), so we extend, not invent. Brittleness mitigation: the prefix `"renegotiate"` is unique within the file's error reasons (verified — no other call site uses it).

**HO arm placement**: The `renegotiate` branch goes BEFORE the existing `ProcessOutput: 0x80004005` branch — both are mutually exclusive (different prefixes) but ordering by frequency (renegotiation is rarer than priming) is irrelevant to correctness; placement is cosmetic. Pick first-match-by-source-order to keep the diff minimal.

**References**: spec R3, R6; proposal D3; #597 DR-NEW-2 precedent.

### DD5 — Logging contract (R7, OQ-D resolution)

**Choice**: Two log levels, two call sites:

1. **Always-on `trace!` after every `ProcessOutput`** (success and error): `tracing::trace!(dwStatus = output.dwStatus, status, "collect_output: ProcessOutput status flags");`. This consumes the previously-unread `output.dwStatus` / `status` fields. `trace!` is filtered out by default `RUST_LOG=info`, so production builds are unaffected. Smoke runs (`smoke-trace.ps1`) use `RUST_LOG=sm_infra::encode=trace` and capture this.

2. **`error!` on renegotiation failure** (in `pump_loop`'s HO arm): `tracing::error!("pump_loop: renegotiate_output_type failed: {e}");`. Distinct from the existing `tracing::error!("pump_loop: collect_output failed: {e}")` line so smoke transcripts can distinguish renegotiation failure from generic ProcessOutput failure.

**Example log lines**:

```
TRACE collect_output: ProcessOutput status flags dwStatus=0x00000100 status=0x00000000
ERROR pump_loop: renegotiate_output_type failed: encoder error: renegotiate: SetOutputType: 0xC00D6D76
```

**OQ-D resolved**: D4 says trace-level for diagnostics (dwStatus/status); D3 says error-level for renegotiation failure. These are different code paths and different audiences (diagnostic vs. fatal). The split is consistent with the logging taxonomy in #597 §7.

**References**: spec R7; proposal D3, D4; #597 DD8.

### DD6 — `output_format_known` cache reset ordering (R9)

**Choice**: `*output_format_known = None` MUST happen BEFORE the call to `renegotiate_output_type`. Concretely the order in the `MF_E_TRANSFORM_STREAM_CHANGE` arm is:

1. `tracing::trace!(...)` for dwStatus/status.
2. `*output_format_known = None;` (cache invalidated unconditionally).
3. `renegotiate_output_type(mft, w, h, framerate, bitrate_bps)?;` (propagates on failure).
4. `return Ok(None);`.

**Rationale**: If renegotiation fails (step 3 returns `Err`), the encoder thread is about to exit anyway, so the cache state is moot — but if we ever recover (future change), an Annex-B/AVCC cache that survived a stream-change boundary is a correctness hazard. Resetting BEFORE the call is the strictly safer ordering. Cost: zero (a `None` write before a fallible call vs. after).

**References**: spec R9; proposal D6.

### DD7 — Test ordering fix in `mft_thirty_frame_smoke_emits_at_least_one_keyframe` (R8)

**Choice**: Single-line swap in `crates/sm-infra/tests/windows_mft_encode.rs` at lines 241–242.

**Before**:
```rust
producer.join().expect("producer thread should not panic");
enc.stop().expect("stop should succeed");
```

**After**:
```rust
enc.stop().expect("stop should succeed");
producer.join().expect("producer thread should not panic");
```

**Rationale**: `producer.join()` first deadlocks when the bounded `frame_tx` channel saturates (encoder pump stalled, channel full, producer blocks on `send`). `enc.stop()` first sets the stop flag, pump_loop exits, the channel's `Receiver` drops, producer's `send` returns `SendError`, producer exits cleanly, `join()` returns. Latent ordering bug — independent of stream-change correctness — fixed unconditionally. RED→GREEN evidence: BEFORE the swap, this test HANGS on `producer.join()`. AFTER the swap (with stream-change still broken), the test fails the IDR assertion at line 249ish with a clear assertion message instead of hanging. AFTER both fixes (DD1–DD6 + DD7), the test PASSES.

**Why not also patch the other 8 tests' similar ordering?** Grep at lines 177, 298, 381, 440, 499, 545, 576, 609, 639, 686, 718, 721, 761, 813 shows all OTHER tests already call `enc.stop()` before `producer.join()` — only the 30-frame test has the inverted order. Single-line scope confirmed.

**References**: spec R8; proposal D5; explore §6.1 Option A.

### DD8 — No `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` resend (R5, OQ-B resolution)

**Choice**: Do NOT resend `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` after `SetOutputType` succeeds.

**Rationale**: Per [Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts) and [Handling Stream Changes](https://learn.microsoft.com/en-us/windows/win32/medfound/handling-stream-changes), async MFTs (which all advertise `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE`) keep their streaming state across a mid-stream `SetOutputType`. The `NOTIFY_BEGIN_STREAMING` and `NOTIFY_START_OF_STREAM` messages are the initial-setup handshake and are NOT part of the stream-change protocol. Resending would be redundant at best, error-inducing at worst (some vendors return `MF_E_INVALIDREQUEST` on duplicate begin-streaming). The Microsoft documented sequence after `MF_E_TRANSFORM_STREAM_CHANGE` is exactly `GetOutputAvailableType` + `SetOutputType` + resume polling — nothing else.

**OQ-B resolved**: NO resend. Locked.

**References**: spec R5; proposal D2; explore §3 "Does streaming need NOTIFY_BEGIN_STREAMING after renegotiation? No."

### DD9 — HRESULT propagation choice (OQ-A resolution)

**Choice**: `EncoderError::EncodeFailed` carries the **inner** failure HRESULT (e.g., `SetOutputType: 0xC00D6D76`), NOT the trigger HRESULT (`MF_E_TRANSFORM_STREAM_CHANGE`).

**Rationale**: `MF_E_TRANSFORM_STREAM_CHANGE` is the EXPECTED trigger of renegotiation — its HRESULT carries no diagnostic value at the point of failure (we already know stream change occurred; that is why we are renegotiating). The actionable signal is which step of the renegotiation handshake failed and with what HRESULT — that is what an engineer reading the smoke transcript needs to triage. Trigger-vs-cause distinction matches Rust's `std::error::Error::source()` chain semantics: the proximate cause is the most useful for diagnosis.

**Concrete example**: If Intel QSV emits stream-change at frame 17 and then `SetOutputType` returns `MF_E_INVALIDMEDIATYPE` (`0xC00D36B4`), the error string is `"renegotiate: SetOutputType: 0xC00D36B4"`, NOT `"renegotiate: 0xC00D9C40"` (the stream-change HRESULT itself).

**OQ-A resolved**: inner HRESULT. Locked.

**References**: proposal §5 OQ-A; explore §3 "What the host must do".

### DD10 — `dwStatus` Ok-path renegotiation skipped (OQ-C resolution)

**Choice**: Do NOT trigger renegotiation when `output.dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE != 0` on the `Ok` path of `ProcessOutput`. Only trigger on the `Err(MF_E_TRANSFORM_STREAM_CHANGE)` path.

**Rationale**: The Microsoft spec (`_MFT_OUTPUT_DATA_BUFFER_FLAGS`) does allow `MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` to be advertised on a successful `ProcessOutput` call inline with a sample. In that case the next `ProcessOutput` returns `MF_E_TRANSFORM_STREAM_CHANGE` and the Err-path handling kicks in — meaning the Ok-path detection would only buy us at most one frame of earlier renegotiation. Empirically, the DIAG trace (#629) shows Intel QSV uses the canonical Err-path; we have no evidence the Ok-path is exercised.

**Risk if we are wrong**: If a vendor advertises FORMAT_CHANGE on Ok and then never returns `MF_E_TRANSFORM_STREAM_CHANGE` (degenerate spec interpretation), we would silently emit one stale-format packet before stalling — but the trace logging (DD5) captures `dwStatus` on every call, so post-mortem analysis of any future stall would reveal this within minutes.

**OQ-C resolved**: SKIP for this change. Marked as future work. If smoke transcripts on Host A or Host B ever show `dwStatus & 0x100 != 0` on an Ok call WITHOUT a subsequent `MF_E_TRANSFORM_STREAM_CHANGE`, escalate to a follow-up change `hw-encoder-mft-ok-path-format-change`.

**References**: proposal §5 OQ-C; explore §3 "MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE vs. NEW_STREAMS".

---

## 5. Implementation Order (Commit Sequence)

Strict TDD. Single PR. Branch `feat/hw-encoder-mft-stream-change-handling`.

### C1 (RED-discipline test infrastructure fix) — `test(infra): swap stop()/join() order in thirty-frame smoke (RED-clean failure)`

**Scope**: 1-line swap at `windows_mft_encode.rs:241–242` (DD7).

**Why RED-discipline, not RED-correctness**: This is NOT a test that locks new behaviour; it is a TEST INFRASTRUCTURE fix that converts a hang (no signal) into a clean assertion failure (clear signal). The test still fails on master after this commit (no IDR observed because Intel QSV stalls), but the failure mode changes from `producer.join()` deadlock to a deterministic assertion at line ~249. RED in the strict-TDD sense: the test now correctly REPORTS the bug exists.

**Verification**: `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode mft_thirty_frame_smoke_emits_at_least_one_keyframe --run-ignored=all --no-capture` on Host A → assertion failure within ≤2s of `enc.stop()`. NOT a hang.

**Line forecast**: 2 lines changed (1 swap = -1/+1, plus surrounding comment update if needed). Net diff ≤ 3 lines.

### C2 (GREEN core) — `feat(infra): renegotiate MFT output type on MF_E_TRANSFORM_STREAM_CHANGE`

**Scope**:
1. Add `fn renegotiate_output_type(...)` adjacent to `try_setup_output_type` (DD1) — ~25 LOC including doc comment + COM call sequence + error mapping.
2. Extend `collect_output` signature with `(w, h, framerate, bitrate_bps)` (DD3) — 4 new params.
3. Replace the `MF_E_TRANSFORM_STREAM_CHANGE` arm at line 1317 with the multi-statement block (DD2 + DD5 trace + DD6 cache reset + DD9 inner HRESULT propagation) — ~6 LOC.
4. Update the single call site in `pump_loop` to compute `(w, h)` via `effective_dimensions(cfg)` and pass `cfg.framerate`, `cfg.bitrate_bps` (DD3) — ~3 LOC pre-loop + 4 args at the call site.
5. Extend the HO-arm error classifier in `pump_loop` (lines 1131–1146) with the `reason.contains("renegotiate")` branch (DD4) — ~5 LOC.
6. Add the always-on `tracing::trace!(dwStatus, status, ...)` line after `ProcessOutput` matches (DD5 first call site) — ~3 LOC.

**Line forecast**: ~45 lines added in `windows_mft.rs` (helper ~25 + collect_output edits ~10 + pump_loop edits ~10), 1 line removed (the old swallow). Net diff ~45 LOC.

**Verification**: After this commit, on Host A, `mft_thirty_frame_smoke_emits_at_least_one_keyframe` should PASS (≥1 IDR observed within 30 frames). All 8 timing-out tests should PASS (one per AC-2).

### C3 (style cleanup, conditional) — `style(infra): cargo fmt windows_mft.rs`

**Scope**: `cargo fmt` may reformat surrounding lines if rustfmt's wrapping changes. ONLY commit this if `cargo fmt --check --all` reports differences after C2.

**Line forecast**: ≤10 lines (formatting only).

### Smoke gate (NOT a commit) — Manual on Host A and Host B

User-driven, per BLOCKED_ON_SMOKE rule (sdd-init #186):

1. **Host A (Intel QSV)**: `pwsh ./smoke-trace.ps1` (or equivalent invocation). Save transcript at engram `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-a-branch`. Required to PASS: 30-frame smoke + ≥7 of the 8 currently-timing-out tests.
2. **Host B (NVIDIA NVENC)**: regression smoke. Save at `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-b-regression`. Required: 16/18 PASS maintained (or better).

**Total LOC forecast**: C1 (≤3) + C2 (~45) + C3 (≤10) = **≤58 lines**. Well under the 400-line single-PR budget. Auto-chain not required (per proposal D7 single-PR override).

---

## 6. Code Skeletons (signatures + sketches; NOT implementations)

### 6.1 `renegotiate_output_type` skeleton (DD1)

```rust
/// Re-perform the GetOutputAvailableType + clone + overlay + SetOutputType
/// sequence in response to MF_E_TRANSFORM_STREAM_CHANGE during streaming.
/// Mirrors `try_setup_output_type` except errors map to `EncodeFailed`
/// (post-streaming semantics, not init).
fn renegotiate_output_type(
    mft: &IMFTransform,
    w: u32,
    h: u32,
    framerate: u32,
    bitrate_bps: u32,
) -> Result<(), EncoderError> {
    // 1. GetOutputAvailableType(0, 0) → IMFMediaType
    // 2. SetUINT64(MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
    // 3. SetUINT64(MF_MT_FRAME_RATE, ((framerate as u64) << 32) | 1)
    // 4. SetUINT32(MF_MT_AVG_BITRATE, bitrate_bps)
    // 5. mft.SetOutputType(0, &out_type, 0)
    // Each step maps Err → EncoderError::EncodeFailed("renegotiate: <step>: 0x{HRESULT}")
    Ok(())
}
```

### 6.2 `collect_output` STREAM_CHANGE arm sketch (DD2 + DD5 + DD6 + DD9)

```rust
// Inside collect_output, replacing line 1317:
match unsafe { mft.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status) } {
    Ok(()) => {}
    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
        tracing::trace!(
            dwStatus = output.dwStatus,
            status,
            "collect_output: STREAM_CHANGE — renegotiating output type"
        );
        *output_format_known = None;                              // DD6: BEFORE call
        renegotiate_output_type(mft, w, h, framerate, bitrate_bps)?;  // DD9: inner HRESULT
        return Ok(None);
    }
    Err(e) => {
        return Err(EncoderError::EncodeFailed(format!(
            "ProcessOutput: 0x{:08X}",
            e.code().0
        )));
    }
}

// Always-on trace AFTER the match (covers Ok path):
tracing::trace!(
    dwStatus = output.dwStatus,
    status,
    "collect_output: ProcessOutput status flags"
);
```

### 6.3 `pump_loop` HO-arm error classifier extension (DD4)

```rust
// Inside pump_loop's HO drain loop, replacing the existing classifier ~lines 1131-1146:
Err(e) => {
    let reason = e.to_string();
    if reason.contains("renegotiate") {
        tracing::error!("pump_loop: renegotiate_output_type failed: {e}");
        return;
    }
    if reason.contains("ProcessOutput: 0x80004005") {
        tracing::warn!(
            "pump_loop: ProcessOutput E_UNEXPECTED (vendor priming) — consuming credit"
        );
        ho_count -= 1;
    } else {
        tracing::error!("pump_loop: collect_output failed: {e}");
        return;
    }
}
```

### 6.4 `pump_loop` call-site update (DD3)

```rust
// Pre-loop, after let cfg_w, cfg_h = ... already exists ~line area where effective_dimensions is used:
let (cfg_w, cfg_h) = effective_dimensions(cfg);
let cfg_fps = cfg.framerate;
let cfg_br  = cfg.bitrate_bps;

// Inside the HO drain loop, line 1112 area:
match collect_output(
    mft,
    output_format_known,
    current_ts,
    &mut seq,
    cfg_w,
    cfg_h,
    cfg_fps,
    cfg_br,
) { … }
```

### 6.5 Test ordering fix sketch (DD7)

```rust
// crates/sm-infra/tests/windows_mft_encode.rs lines 241-242:
//   producer.join().expect("producer thread should not panic");  ← was here
//   enc.stop().expect("stop should succeed");                    ← was here
// Becomes:
enc.stop().expect("stop should succeed");
producer.join().expect("producer thread should not panic");
```

---

## 7. Cross-Vendor Verification Matrix

| Vendor | Host | Pre-change behaviour (master `3c8bc48`) | Expected post-change | Smoke required |
|--------|------|----------------------------------------|----------------------|----------------|
| Intel Quick Sync H.264 MFT | Host A | 30-frame smoke HANGS at `producer.join()`; 8 other tests TIMEOUT after 5–10s; 9/18 PASS | 30-frame smoke PASSES; ≥7 of 8 timing-out tests PASS; target 17/18 or 18/18; renegotiation observed at ~frame 17 in trace | YES — BLOCKED_ON_SMOKE |
| NVIDIA NVENC | Host B | 16/18 PASS (orthogonal SetOutputType failure prevents reaching streaming); 2 fail (T7.1, T7.2 force-IDR carry-forward, separate change) | 16/18 PASS maintained — new code unreachable from NVENC's pre-streaming failure path | YES — regression check |
| AMD AMF | — | Untested (no hardware); per PR #17, AMF honours CleanPoint | No-op (renegotiation is spec-compliant for any async MFT; not exercised here) | Future work |
| Microsoft SW H.264 (sync MFT) | — | Not enumerated (`MFT_ENUM_FLAG_HARDWARE` only) | N/A | N/A |

---

## 8. Risks → Mitigation Table

| Sev | Lik | Description | Mitigation | Gate |
|-----|-----|-------------|------------|------|
| MED | MED | Renegotiation success on Intel QSV mid-stream not yet empirically confirmed (carry-forward proposal §6) | Mandatory `smoke-trace.ps1` re-run on Host A. Trace logging from DD5 captures dwStatus + per-step HRESULT for post-mortem. If `SetOutputType` returns a new HRESULT mid-stream, escalate to add `MFT_MESSAGE_COMMAND_FLUSH` retry. | Host A smoke transcript |
| MED | LOW | `GetOutputAvailableType(0, 0)` at stream-change time may return a type with vendor-changed `MF_MT_FRAME_SIZE` (vendor decides to normalise padding) — our overlay would write our original dims back, possibly conflicting | Trace log (DD5) captures both dwStatus and per-call status. If `SetOutputType` returns `MF_E_INVALIDMEDIATYPE`, smoke transcript surfaces it; design phase of follow-up change can decide honour-vs-override. Out of scope here. | Host A smoke transcript |
| LOW | LOW | DD3 signature change to `collect_output` ripples through any future call site (currently single call site) | One call site only (verified via grep). Future expansions must respect the contract. | Code review at PR time |
| LOW | LOW | DD4 string-match `reason.contains("renegotiate")` is brittle to error-string format changes | Inline contract comment on `renegotiate_output_type` doc + on the HO-arm classifier. Pattern already established in #597 DR-NEW-2 (E_UNEXPECTED string match). Future cleanup (typed `EncoderError::Renegotiation` variant) requires unfreezing `sm-domain`, out of scope. | Code review |
| LOW | LOW | Multiple consecutive `MF_E_TRANSFORM_STREAM_CHANGE` events per spec | Each `collect_output` call handles one renegotiation independently; no state across calls. Per MS spec, supported. | None |
| LOW | LOW | NVENC regression on Host B from `collect_output` signature change | The new params `(w, h, framerate, bitrate_bps)` are pure data; do not affect the Ok path. NVENC fails earlier in `setup_mft` before reaching streaming. | Host B smoke transcript |
| LOW | MED | `trace!` logging (DD5) verbose in debug runs | Filtered by default `RUST_LOG=info`. Production unaffected. Smoke-trace.ps1 explicitly opts in via `RUST_LOG=sm_infra::encode=trace`. | None |
| LOW | LOW | If renegotiation fails AND DD7 ordering fix omitted, 30-frame test deadlocks again | DD7 IS included. Belt-and-braces. | C1 commit lands first |
| LOW | LOW | `cargo fmt` may reformat surrounding lines, inflating diff | C3 commit isolates formatting. `cargo fmt --check --all` in CI. Diff stays ≤58 lines. | CI quality gate |
| LOW | LOW | Smoke transcript may show 1 of 8 tests still failing post-fix (different root cause) | AC-2 allows ≤2 failures with NEW failure mode. Verify-phase distinguishes regression from new bug via transcript triage. | Verify phase |
| LOW | LOW | OQ-C deferred (Ok-path FORMAT_CHANGE) might bite if a vendor exercises that path | DD5 trace logging captures `dwStatus` on every call. Future smoke transcripts will reveal if any vendor sets `dwStatus & 0x100 != 0` on Ok without a subsequent STREAM_CHANGE Err. Escalation criteria documented. | Future smoke triage |

---

## 9. Open Questions Remaining (post-design)

None. All four proposal-phase OQs (OQ-A, OQ-B, OQ-C, OQ-D) are resolved here:

- **OQ-A** (HRESULT propagation): inner HRESULT — DD9. Locked.
- **OQ-B** (`BEGIN_STREAMING` resend): NO — DD8. Locked per MS async-MFT spec.
- **OQ-C** (Ok-path `FORMAT_CHANGE`): SKIP for this change — DD10. Future work conditional on smoke evidence.
- **OQ-D** (renegotiation-failure log level): `error!` — DD5. Distinct from DD5's `trace!` for diagnostics.

Apply phase has zero deferred design questions.

---

## 10. Executive Summary

10 design decisions (DD1–DD10) translate proposal D1–D8 into an implementation blueprint. New private helper `renegotiate_output_type` mirrors `try_setup_output_type`'s clone-and-overlay sequence with `EncodeFailed` error mapping. Invocation co-located in `collect_output`'s `MF_E_TRANSFORM_STREAM_CHANGE` arm; cache reset BEFORE call (DD6); inner-HRESULT propagation (DD9). Caller signature extended to plumb `(w, h, framerate, bitrate_bps)` from `pump_loop`'s `&EncoderConfig` (DD3). HO-arm classifier in `pump_loop` extended with `reason.contains("renegotiate")` branch → `tracing::error!` + `return` (DD4). Always-on `trace!` of `dwStatus` / `status` consumes the previously-unread fields (DD5). Single-line test ordering swap (DD7). No `BEGIN_STREAMING` resend (DD8 per spec). Ok-path `dwStatus & FORMAT_CHANGE` deferred (DD10). Implementation order: C1 (RED-discipline test fix, ≤3 LOC) → C2 (GREEN core, ~45 LOC) → optional C3 (`cargo fmt`, ≤10 LOC). Total ≤58 LOC, single PR, branch `feat/hw-encoder-mft-stream-change-handling`. Mandatory smoke gates on Host A (Intel QSV) and Host B (NVENC regression) post-merge to PR. All four proposal OQs (OQ-A through OQ-D) resolved in this design.
