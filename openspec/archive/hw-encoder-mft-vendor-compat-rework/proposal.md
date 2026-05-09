# Proposal: hw-encoder-mft-vendor-compat-rework (Slice 2 — Intel QSV stream-change renegotiation)

> Phase: SDD propose. Inputs: engram observations #631 (explore), #630 (root cause), #629 (DIAG trace), #628 (smoke transcript), #604 (predecessor archive), #600 (Bug 1 multi-host evidence), #186 (sdd-init).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-vendor-compat-rework/proposal` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-07.

---

## 1. Inputs

| Source | Topic key / Path | Observation ID |
|--------|------------------|----------------|
| Exploration artifact | `sdd/hw-encoder-mft-vendor-compat-rework/explore` | #631 |
| Root cause analysis | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-root-cause` | #630 |
| DIAG trace transcript | `sdd/hw-encoder-mft-vendor-compat-rework/host-a-trace-3c8bc48` | #629 |
| Host A smoke summary (master 3c8bc48) | `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-a-master-3c8bc48` | #628 |
| Predecessor archive (PR #16) | `sdd/hw-encoder-mft-rework/archive-report` | #604 |
| Bug 1 family multi-host evidence | `sdd/hw-encoder-mft-rework/bug-1-deeper` | #600 |
| sdd-init project context | `sdd-init/screen-mirror-app` | #186 |
| Local explore file | `openspec/changes/hw-encoder-mft-vendor-compat-rework/explore.md` | n/a |

---

## 2. Intent

Intel QSV (Host A) currently stalls indefinitely after approximately 17 frames because `collect_output` in `crates/sm-infra/src/encode/windows_mft.rs` swallows `MF_E_TRANSFORM_STREAM_CHANGE` as `Ok(None)` instead of performing the Microsoft-mandated `GetOutputAvailableType` + `SetOutputType` renegotiation. This is the third confirmed manifestation of the Bug 1 family: Slice 1 (archived as `hw-encoder-mft-vendor-compat-slice1-nvenc`, PR #17) addressed cross-thread COM transfer and force-IDR carry-forward for NVIDIA NVENC, but uncovered this latent stream-change protocol gap on Intel QSV. We need to ship vendor-compat parity for Intel QSV by implementing the documented post-streaming format-renegotiation handshake. Success is empirical: 9/18 → ~18/18 PASS on Host A smoke, with no regression of NVENC's existing 16/18 PASS on Host B and no regression of T-NEW-1 / T-NEW-2 (Bug 2 fix from PR #16) cross-vendor. The fix is narrow and self-contained — it touches only `collect_output` and one test ordering line.

---

## 3. Scope

### IN scope

- `MF_E_TRANSFORM_STREAM_CHANGE` handling in `collect_output` (`crates/sm-infra/src/encode/windows_mft.rs:1317`): replace silent `Ok(None)` swallow with `GetOutputAvailableType` + `SetOutputType` renegotiation per Microsoft async-MFT protocol.
- New thin `renegotiate_output_type(mft, w, h, framerate, bitrate_bps) -> Result<(), EncoderError>` helper that performs the same clone-and-overlay COM operations as `try_setup_output_type` but maps errors to `EncoderError::EncodeFailed`.
- Reset `output_format_known` cache to `None` on successful renegotiation to force re-sniff of Annex-B vs. AVCC on the next packet.
- `tracing::trace!` logging of `output.dwStatus` (per-buffer flags) and the call-level `status` (`_MFT_PROCESS_OUTPUT_STATUS`) on every `collect_output` call — diagnostic only, no behavioral change.
- 1-line ordering fix in `mft_thirty_frame_smoke_emits_at_least_one_keyframe`: call `enc.stop()` before `producer.join()` to prevent producer-deadlock on bounded channel saturation when encoding stalls.
- Smoke verification on Host A (mandatory empirical gate per BLOCKED_ON_SMOKE rule #586) and regression check on Host B.

### OUT of scope

- **NVENC `SetOutputType: 0xC00D6D76` failure on Host B** (Bug 1 Manifestation B). Host B currently passes 16/18 with `force-IDR` from PR #17; the remaining 2 failures (T7.1, T7.2) and the `0xC00D6D76` rejection on the older smoke transcripts belong to a separate future change `hw-encoder-mft-nvenc-setup-fix`. Not addressed here.
- **AMD AMF empirical verification**. Neither test host has AMD hardware. AMF is an async MFT and per spec must support `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE`; the renegotiation logic should apply transparently if AMF ever fires `STREAM_CHANGE`. UNTESTED.
- **Flipping `default = ["hw-encoder"]`**. Cargo default stays `[]`. The default-on flip is a separate planned change `hw-encoder-default-on-flip`, gated on clean 18/18 smoke on ≥2 vendors + 24h soak.
- **Adding a stream-change-specific unit/integration test that injects `MF_E_TRANSFORM_STREAM_CHANGE`**. Forcing this HRESULT from test code requires a mock/shim architecture that does not exist in the codebase. The 8 currently-timing-out tests + the 30-frame smoke serve as the empirical RED→GREEN proof on real hardware.
- **Refactoring `try_setup_output_type`** to be parameterized over error mapping. Touching the init path adds risk; we prefer a dedicated helper for the streaming path.
- **Domain-layer changes** (`crates/sm-domain/src/encode.rs` is FROZEN): no new `EncoderError` variants, no `VideoEncoder` API changes.

---

## 4. Locked Decisions (D1–D8)

### D1 — Resolves OQ-1: Renegotiation function factoring

**Title**: Extract a thin `renegotiate_output_type` helper.

**Options considered**:
- **Option A**: Call `try_setup_output_type` directly from `collect_output`. Map `EncoderError::InitFailed` → `EncoderError::EncodeFailed` at the call site.
- **Option B**: Refactor `try_setup_output_type` to accept an error-tag parameter or closure for error mapping. Single function for both init and streaming.
- **Option C** (CHOSEN): Add a thin private helper `renegotiate_output_type(mft, w, h, framerate, bitrate_bps) -> Result<(), EncoderError>` next to `try_setup_output_type`. It performs the same `GetOutputAvailableType(0, 0)` + clone + overlay (FRAME_SIZE, FRAME_RATE, AVG_BITRATE) + `SetOutputType` operations, but maps errors to `EncoderError::EncodeFailed("renegotiate_output_type: ...")`.

**Rationale**: Option C is the lowest-risk path. The semantic distinction matters: `InitFailed` is wrong post-streaming because the encoder is already running; `EncodeFailed` correctly reflects a streaming-phase fault. Option A's call-site translation works but couples `collect_output` to the init error variant, which is misleading when reading the code. Option B touches the init path — the same `try_setup_output_type` code that ships in Slice 1's working NVENC fix on master `3c8bc48` — and we do NOT want to perturb that. Option C duplicates ~15 lines of clone-and-overlay logic; that duplication is acceptable given the divergent error semantics. If a future change wants to deduplicate, it can extract a private inner `do_set_output_type(mft, w, h, framerate, bitrate_bps) -> Result<(), windows::core::Error>` that both helpers call — that refactor is a future cleanup, NOT in scope here.

### D2 — Resolves OQ-2: Flush before renegotiation

**Title**: Do NOT flush before `GetOutputAvailableType` / `SetOutputType` during stream change.

**Options considered**:
- **Option A**: Send `MFT_MESSAGE_COMMAND_FLUSH` before renegotiation, then resume.
- **Option B** (CHOSEN): No flush. Call `GetOutputAvailableType` + `SetOutputType` directly and resume.

**Rationale**: Microsoft's async-MFT specification ([Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts)) is explicit: async MFTs are required to set `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE`, which means the client does NOT need to drain or flush before a mid-stream type change. Streaming continues immediately after `SetOutputType` returns `S_OK`. Sync MFTs require drain; we exclusively target async hardware MFTs (`MFT_ENUM_FLAG_HARDWARE`). This is a load-bearing assumption: if the Host A smoke transcript shows Intel QSV's MFT empirically misbehaves without a flush (e.g., `SetOutputType` returns `MF_E_INVALIDREQUEST` or the pipeline still stalls after renegotiation), the design phase must add `COMMAND_FLUSH` as a retry step. The smoke-trace re-run on Host A is the mandatory gate that confirms this assumption.

### D3 — Resolves OQ-3: Behavior on renegotiation failure

**Title**: On `renegotiate_output_type` failure, log error and `return` from `pump_loop`.

**Options considered**:
- **Option A**: Retry N times with exponential backoff before giving up.
- **Option B**: Demote to `tracing::warn!` and continue spinning, hoping vendor recovers.
- **Option C** (CHOSEN): `tracing::error!("renegotiate_output_type failed: {e}")`, then propagate the error so `pump_loop` exits cleanly via its existing fatal-error handling. Encoder thread terminates; consumer sees channel disconnect.

**Rationale**: Vendor protocol violation is fatal — the MFT pipeline is in an undefined state if `GetOutputAvailableType` or `SetOutputType` fails during stream change. Retrying risks spinning indefinitely on a permanently stuck MFT (which is the exact failure mode we are fixing). Continuing without renegotiation reproduces the original bug. Clean exit is consistent with how `pump_loop` already handles all other fatal errors (e.g., `ProcessOutput` returning an unexpected HRESULT — it returns `EncoderError::EncodeFailed` and the thread exits). The producer-side `enc.stop()` ordering fix (D5) ensures consumers detect the failure cleanly.

### D4 — Resolves OQ-4: Log `output.dwStatus` and call-level `status`

**Title**: Add `tracing::trace!` logging of both fields on every `collect_output` call.

**Options considered**:
- **Option A** (CHOSEN): Always log both fields at `trace!` level after every `ProcessOutput` call (including the success path). Diagnostic only, no behavioral coupling.
- **Option B**: Only log when `MF_E_TRANSFORM_STREAM_CHANGE` fires.
- **Option C**: Skip — not required for correctness.

**Rationale**: Currently both fields (`output.dwStatus` from `_MFT_OUTPUT_DATA_BUFFER_FLAGS` and the call-level `status` from `_MFT_PROCESS_OUTPUT_STATUS`) are declared but never read. The `MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` flag in `output.dwStatus` is the buffer-level signal of the same condition that the HRESULT advertises; the call-level `status` carries `MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS` for new-stream creation (irrelevant for our single-stream H.264 encoder, but worth seeing in logs). `trace!` keeps the cost zero in production builds (filtered out by default `RUST_LOG`) while giving us empirical evidence for future Bug 1 manifestations or vendor-specific quirks. Option A is preferred over Option B because non-stream-change calls also benefit from observability — we want to see the `dwStatus` flags on every output round, especially `MFT_OUTPUT_DATA_BUFFER_INCOMPLETE` if it ever fires.

### D5 — Resolves OQ-5: 30-frame test ordering fix

**Title**: Include the `enc.stop()` before `producer.join()` ordering fix in this PR.

**Options considered**:
- **Option A** (CHOSEN): Fix in this PR. 1-line swap.
- **Option B**: Defer to a separate test-hardening change.

**Rationale**: The current `producer.join()` before `enc.stop()` is unconditionally wrong. The producer thread sends 30 frames into a bounded `SyncSender` channel of capacity `ENCODE_CHANNEL_CAPACITY`. If the encoder pipeline stalls (which, even after the stream-change fix, could happen via any future fault), the channel fills and the producer blocks on `frame_tx.send(frame)`. `producer.join()` then deadlocks indefinitely. `enc.stop()` first signals the encoder thread to exit, which drops the receiver, which causes the producer's `send` to return `SendError`, which lets the producer exit, which lets `join()` return. This is a latent ordering bug that exists independently of the stream-change fix. Including it here costs 1 line, has zero risk, and prevents the test from masking future encoding faults as deadlocks. Deferring it would leave a ticking time bomb that re-surfaces on every future encoding regression.

### D6 — Resolves OQ-6: Reset `output_format_known` on stream change

**Title**: Reset `output_format_known = None` after successful renegotiation.

**Options considered**:
- **Option A** (CHOSEN): Set `*output_format_known = None` after `renegotiate_output_type` returns `Ok(())`, before returning `Ok(None)` from `collect_output`.
- **Option B**: Trust the cached format value across the renegotiation boundary.

**Rationale**: `output_format_known: &mut Option<bool>` caches the Annex-B vs. AVCC detection result. The vendor MFT may, in principle, change its output encoding format across a stream-change boundary (e.g., switch from Annex-B to AVCC, though Intel QSV is empirically Annex-B only). The safest correctness guarantee is to invalidate the cache on stream change and force re-detection on the next successful packet. Cost: at most one extra format-sniff pass on the post-renegotiation packet. The existing comment "Sniffing every packet while the cache is `None` self-corrects against partial first-packet (R-NEW-6)" already documents this self-healing behavior. Aligning with that documented contract.

### D7 — Resolves OQ-7: PR sizing — single PR

**Title**: Single PR. Override session-cached `auto-chain` delivery strategy.

**Options considered**:
- **Option A** (CHOSEN): Single PR.
- **Option B**: Chain into 2 PRs (renegotiation handler / test fix).
- **Option C**: Stack PRs (one per logical commit).

**Rationale**: Forecast diff is approximately 25–50 lines in production code (`renegotiate_output_type` helper + `collect_output` STREAM_CHANGE arm + `dwStatus` trace logging) plus 1 line in the test file (ordering swap) plus comments and `cargo fmt`. Total ≤80 lines. Well under the 400-line single-PR budget. The session-cached delivery strategy is `auto-chain`, but that exists as a guardrail for changes that risk exceeding the budget; this change does not. Locking single-PR here matches the established Slice 1 / PR #17 pattern (also single-PR with 3-commit chain) and keeps the smoke verification atomic — splitting the test fix from the renegotiation fix would force two smoke runs to demonstrate correctness, which is wasteful. Branch name: `feat/hw-encoder-mft-stream-change-handling`.

### D8 — Test discipline: NO new stream-change-specific RED test

**Title**: Do NOT add a new test that explicitly exercises `MF_E_TRANSFORM_STREAM_CHANGE` handling. Use the 8 existing timing-out tests as the empirical RED→GREEN proof.

**Options considered**:
- **Option A**: Add an integration test that mocks `IMFTransform::ProcessOutput` to return `MF_E_TRANSFORM_STREAM_CHANGE` on a configurable frame index. Requires shim architecture.
- **Option B** (CHOSEN): Skip. Use the 8 existing tests + the 30-frame smoke as integration-level RED→GREEN evidence on real hardware.

**Rationale**: There is no practical way to force Intel QSV's MFT to emit `MF_E_TRANSFORM_STREAM_CHANGE` from test code without introducing a `trait MftLike` shim that abstracts `ProcessOutput`. That refactor is large (touches all of `pump_loop`, `collect_output`, `setup_mft`) and adds an indirection that doesn't pay back outside this single test. Per Strict TDD discipline, the existing test suite ALREADY provides RED signal: `mft_thirty_frame_smoke_emits_at_least_one_keyframe` HANGS today on Host A and 8 other tests TIMEOUT, all due to this single root cause (verified by trace #629). After the fix, those same tests are expected to PASS. RED → GREEN is empirically demonstrable on real hardware. The smoke transcript before/after is the test evidence. No mock-based unit test would add catch-coverage for vendor-specific divergence anyway — that requires the real driver.

---

## 5. Open Questions for Design Phase

- **OQ-A** — Which HRESULT should `EncoderError::EncodeFailed` carry on renegotiation failure: the original `MF_E_TRANSFORM_STREAM_CHANGE` HRESULT (the trigger), or the failing inner HRESULT from `GetOutputAvailableType` / `SetOutputType` (the cause)? **Recommendation (to lock in design)**: the inner failing HRESULT, formatted as `"renegotiate_output_type: GetOutputAvailableType: 0xXXXXXXXX"` or `"renegotiate_output_type: SetOutputType: 0xXXXXXXXX"`. Carrying the trigger discards the diagnostic value of the actual failure point.
- **OQ-B** — On stream change, should renegotiation also resend `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING`? Per Microsoft async-MFT docs (explore §3): NO — those are initial-setup messages. Flagged here in case the design phase wants to confirm via a 1-line test on Intel QSV. Default: do NOT resend.
- **OQ-C** — If `ProcessOutput` returns success but `output.dwStatus` carries `MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE` (per spec, the buffer-level signal that mirrors the HRESULT), should we ALSO trigger renegotiation in addition to the HRESULT path? Per spec, the HRESULT and the flag fire together; if the HRESULT fires we already renegotiate. Defensive check: trigger renegotiation if `output.dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE != 0` even on `Ok(())`. **Recommendation (design)**: skip — spec says they are mutually consistent, and adding a flag-only path complicates the state machine. Trace logging (D4) gives us visibility if reality diverges.
- **OQ-D** — Stream-change-only logs at `error!` vs. `warn!` level on renegotiation failure: D3 chose `error!`. Confirm this matches the existing log-level taxonomy in `windows_mft.rs` (other fatal errors use `error!`). Likely no change needed.

---

## 6. Risks (Sev × Likelihood)

| Sev | Lik | Description | Mitigation |
|-----|-----|-------------|------------|
| MED | MED | Renegotiation success on Intel QSV mid-stream is not yet empirically confirmed. `try_setup_output_type` / `renegotiate_output_type` works at init; behavior when called during `MF_E_TRANSFORM_STREAM_CHANGE` depends on Intel's driver. | Mandatory smoke-trace.ps1 re-run on Host A as the empirical gate before merge. If `SetOutputType` returns a new HRESULT mid-stream, design phase escalation: add `COMMAND_FLUSH` retry step (Option A from D2). |
| LOW | MED | `GetOutputAvailableType` at stream-change time may return a type with different attributes than at init time — particularly if Intel changes `MF_MT_FRAME_SIZE`. Our overlay restores configured dims, which may or may not be vendor-correct. | Trace logging (D4) captures the actual `output.dwStatus` flags for diagnosis. If empirical divergence appears, design phase decides whether to honor vendor's frame size or override. |
| LOW | LOW | Multiple consecutive `MF_E_TRANSFORM_STREAM_CHANGE` events. | Handled correctly per spec — each `collect_output` call triggers one renegotiation attempt. No state needed across calls. |
| LOW | LOW | If renegotiation fails AND D5 ordering fix is omitted, the 30-frame test deadlocks rather than reporting the error. | D5 IS included in this PR (locked). Ordering fix is unconditional. |
| LOW | LOW | NVENC regression on Host B. NVENC currently passes 16/18 PASS post-PR #17; it does not exercise the streaming phase where `MF_E_TRANSFORM_STREAM_CHANGE` would fire (Bug 1 Manifestation B blocks it earlier in setup). The new code path is reachable only when the streaming phase runs successfully — which on NVENC currently it does not. So the new code is a no-op on NVENC's failing tests. | Mandatory Host B regression smoke run to confirm 16/18 PASS maintained. New code paths are gated on a specific HRESULT that NVENC is not currently observed to emit. |
| LOW | LOW | AMD AMF behavior is untested (no hardware). | Renegotiation is spec-compliant for any async MFT; AMF MUST set `MFT_SUPPORT_DYNAMIC_FORMAT_CHANGE = TRUE` per the async-MFT contract. Treated as a no-op until empirically tested. Documented in scope as OUT-of-scope. |
| LOW | MED | Trace logging (D4) at `trace!` level may produce verbose logs in debug builds. | `trace!` is filtered out by default `RUST_LOG=info`. Production builds unaffected. Smoke runs use `RUST_LOG=sm_infra::encode=trace` explicitly per existing `smoke-trace.ps1`. |
| LOW | LOW | The smoke transcript on Host A may show one of the 8 tests still failing post-fix (different root cause than STREAM_CHANGE). | Verify report distinguishes "failing for same reason as before" (regression) vs. "failing for a different reason" (separate bug, document and defer). Acceptance criteria allow ≤2 tests to fail with a NEW mode without blocking merge. |
| LOW | LOW | `cargo fmt` may reformat surrounding lines, inflating the diff. | Run `cargo fmt --check --all` before commit. Direct invocation per project convention #581. Diff stays ≤80 lines. |

---

## 7. Delivery Strategy

**Single PR**. Branch: `feat/hw-encoder-mft-stream-change-handling`.

**Commit chain** (3–4 commits):

1. **C1 (RED)** — `test(infra): assert thirty-frame smoke survives stop ordering`. Includes the 1-line ordering swap (`enc.stop()` before `producer.join()`) in `mft_thirty_frame_smoke_emits_at_least_one_keyframe`. RED on master because the test still hangs (encoder stalls before stop is reached). Confirms the test mechanism, not the fix.
2. **C2 (GREEN core)** — `feat(infra): renegotiate MFT output type on MF_E_TRANSFORM_STREAM_CHANGE`. Adds `renegotiate_output_type` helper + `collect_output` STREAM_CHANGE arm rewrite + `output_format_known = None` reset. ~30–40 production lines.
3. **C3 (GREEN observability)** — `feat(infra): trace dwStatus and ProcessOutput status flags on every collect_output call`. ~5–10 production lines, `tracing::trace!` only.
4. **C4 (optional)** — `style(infra): cargo fmt windows_mft.rs` if needed.

**Forecast diff**: ≤80 lines total (production + test + comments). Well under 400-line budget. No chained PRs.

**PR body sections** (per project convention from PR #15/#16/#17):
- Summary
- Commits (with anchor hashes)
- Gates (7 quality gates GREEN)
- Test plan (Host A smoke transcript before/after, Host B regression check)
- SDD artifacts (engram topic_keys + openspec file paths)

**Merge mode**: `gh pr merge --merge --delete-branch` (NO squash). Manual `git push origin --delete feat/hw-encoder-mft-stream-change-handling` if needed (the `--delete-branch` flag is not always honored).

---

## 8. Acceptance Criteria

1. **AC-1 (Host A primary)**: `mft_thirty_frame_smoke_emits_at_least_one_keyframe` PASSES on Host A on `master` + this branch. Currently HANGS on `3c8bc48`.
2. **AC-2 (Host A secondary)**: At least 7 of the 8 currently-timing-out tests on Host A PASS on this branch. Acceptable if 1 of them fails with a DIFFERENT mode than the current TIMEOUT (proves a separate bug, not stream-change). Target: 17/18 or 18/18 PASS on Host A.
3. **AC-3 (cross-vendor preservation)**: T-NEW-1 (`mft_stop_during_idle_returns_within_deadline`) and T-NEW-2 (`mft_stop_during_active_encode_returns_within_deadline`) remain GREEN cross-vendor. No regression of PR #16's Bug 2 fix.
4. **AC-4 (Host B regression)**: Host B smoke maintains 16/18 PASS (or better — improvements welcome but not required). The 2 remaining failures (T7.1, T7.2 force-IDR carry-forward) are pre-existing and tracked in a separate change.
5. **AC-5 (CI)**: `cargo nextest run --workspace` GREEN cross-platform (Linux + macOS + Windows). 7 quality gates GREEN: `cargo check --workspace`, `cargo clippy --all-targets --all-features`, `cargo fmt --check --all`, `cargo nextest run --workspace`, `cargo deny check`, `cargo check --no-default-features`, `cargo check --features hw-encoder`.

---

## 9. Definition of Done

- **Code**:
  - `collect_output` handles `MF_E_TRANSFORM_STREAM_CHANGE` by calling `renegotiate_output_type` and resetting `*output_format_known = None`.
  - On renegotiation failure, `pump_loop` exits cleanly via `EncoderError::EncodeFailed`.
  - `output.dwStatus` and call-level `status` logged at `trace!` level on every `collect_output` call.
  - No changes to `crates/sm-domain/src/encode.rs` (FROZEN).
  - `default = []` in `crates/sm-infra/Cargo.toml` (UNCHANGED).
- **Tests**:
  - `mft_thirty_frame_smoke_emits_at_least_one_keyframe` ordering fix: `enc.stop()` before `producer.join()`.
  - All 18 existing test names preserved; no deletions.
  - T-NEW-1 / T-NEW-2 unchanged.
- **Smoke**:
  - Host A transcript captured (master + branch) showing 9/18 → ≥17/18 PASS. Saved to engram with topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-a-branch`.
  - Host B regression transcript saved to engram with topic_key `sdd/hw-encoder-mft-vendor-compat-rework/smoke-transcript-host-b-regression`.
- **CI**: 7/7 quality gates GREEN on the merge SHA. CI matrix (windows/macos/ubuntu × Check/Test/Clippy + Rustfmt + MSRV + JS Tests) all pass.
- **Docs**: SDD chain anchors updated in `archive-report` (chain link from this change to predecessor `hw-encoder-mft-vendor-compat-slice1-nvenc` archive #604). sdd-init #186 row updated in archive phase.

---

## Result Contract

- **status**: complete
- **executive_summary**: Locked 8 decisions (D1–D8) for Slice 2 of the Bug 1 family vendor-compat rework, addressing Intel QSV's silent-stall on `MF_E_TRANSFORM_STREAM_CHANGE` per Microsoft async-MFT protocol. OQ-1 and OQ-2 (the two open decisions from explore) resolved with explicit tradeoff analysis: Option C thin `renegotiate_output_type` helper for factoring, no-flush per spec for renegotiation. OQ-3 through OQ-7 rubber-stamped per explore recommendations. OQ-8 added: NO new stream-change-specific RED test (existing 8 timing-out tests serve as integration-level RED→GREEN proof). 4 design-phase open questions raised (OQ-A inner HRESULT propagation, OQ-B BEGIN_STREAMING resend, OQ-C dwStatus-only path, OQ-D log level). Forecast ≤80 LOC total diff, single PR override of session `auto-chain` strategy locked in D7. Branch `feat/hw-encoder-mft-stream-change-handling`, 3–4 commits. Mandatory smoke gate on Host A (Intel QSV) + regression smoke on Host B (NVIDIA NVENC).
- **artifacts**:
  - engram topic_key `sdd/hw-encoder-mft-vendor-compat-rework/proposal`
  - file `openspec/changes/hw-encoder-mft-vendor-compat-rework/proposal.md`
- **next_recommended**: `sdd-spec` and `sdd-design` (can run in parallel — both consume the proposal).
- **risks**:
  - MED: Renegotiation success on Intel QSV mid-stream not yet empirically confirmed; mandatory Host A smoke gate.
  - LOW: `GetOutputAvailableType` may return a type with vendor-changed `MF_MT_FRAME_SIZE`; clone-and-overlay restores configured dims (may or may not be vendor-correct).
  - LOW: Host B regression risk (NVENC); new code path is unreachable from NVENC's current setup-failure path, but mandatory regression smoke confirms 16/18 PASS maintained.
  - LOW: D2's no-flush assumption is load-bearing; if Intel QSV empirically requires flush, design phase escalation needed.
- **skill_resolution**: injected (Project Standards block was provided in the launch prompt with stack/Strict TDD/PR conventions/multi-host context).
