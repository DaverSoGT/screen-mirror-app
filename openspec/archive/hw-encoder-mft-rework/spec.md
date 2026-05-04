# Spec: hw-encoder-mft-rework

> Phase: SDD spec. Inputs: proposal #595, explore #594, init #186, prior chain spec #586.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-rework/spec` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`). Delivery: single PR.

---

## 1. Inputs Read

| Observation | Topic | Role |
|-------------|-------|------|
| #595 | `sdd/hw-encoder-mft-rework/proposal` | 9 locked decisions, 5 OQs, smoke plan summary, BLOCKED_ON_SMOKE rule |
| #594 | `sdd/hw-encoder-mft-rework/explore` | pump_loop diagnostics, Pattern A/B/C analysis, Fix A/B analysis, smoke forecast T1.1–T13.2 |
| #186 | `sdd-init/screen-mirror-app` | Project conventions, Strict TDD mode, BLOCKED_ON_SMOKE rule encoding, test naming |
| #586 | `sdd/hardware-accel-encoder-smoke-fixes/spec` | Spec template, `Smoke required: yes/no` flag per requirement, 11 req / 21 scenario structure |

---

## 2. Domain Context

`WindowsMftH264Encoder` in `crates/sm-infra/src/encode/windows_mft.rs` (1141 lines) carries two Bucket A architectural bugs not closed by PR #15:

**Bug 1 — Vendor MFT priming deadlock** (`pump_loop` lines 712–847): The loop is purely reactive. It waits for an MFT event and then responds. Hardware vendor MFTs (Intel QSV, NVIDIA NVENC, AMD AMF) may emit `METransformHaveOutput` BEFORE any `METransformNeedInput` at startup. The current design cannot drain that priming output because it only calls `collect_output` in response to a `HaveOutput` event — and the MFT will not emit `NeedInput` until the priming output is drained. Deadlock.

**Bug 2 — Stop-signal starvation** (`windows_mft.rs:743`): `GetEvent(MF_EVENT_FLAG_NONE)` blocks indefinitely. `state.stop` is checked BEFORE the call but cannot interrupt it. `MFShutdown()` runs from `Drop` AFTER `join()`. This is a circular wait: the encoder thread is blocked in `GetEvent`; the caller thread is blocked in `join()`; neither can proceed. All 13 tests that call `stop()` hang indefinitely.

Both bugs share the same root call site: `GetEvent(MF_EVENT_FLAG_NONE)` at `pump_loop:743`. The proposed fix (Pattern B + Fix B — a single `NO_WAIT` polling loop with dual-arm counters) rewrites that site and resolves both bugs in one coherent change.

---

## 3. Requirements

Each requirement carries:
- **Smoke required**: `yes` if ANY acceptance scenario maps exclusively to `#[ignore]`-gated HW smoke tests; `no` if all scenarios are CI-runnable without a GPU host. Per #186 BLOCKED_ON_SMOKE rule: `yes` requirements cause `sdd-verify` to emit `BLOCKED_ON_SMOKE` until a manual smoke transcript is supplied.
- **Source decision**: anchors to proposal #595 Decision # or OQ resolution.

---

### R1 — pump_loop switches to MF_EVENT_FLAG_NO_WAIT polling

**Statement**: `pump_loop` in `crates/sm-infra/src/encode/windows_mft.rs` MUST replace every call to `GetEvent(MF_EVENT_FLAG_NONE)` with `GetEvent(MF_EVENT_FLAG_NO_WAIT)` within a polling loop. The polling loop MUST NOT call `GetEvent` more than once per iteration before servicing pending counters and checking the stop flag.

**Smoke required**: yes

**Source decision**: Proposal Decision #1 (Pattern B), Decision #2 (Fix B), Decision #3 (single PR — both fixes in one loop rewrite).

**Acceptance scenarios**:

- **S1.1** — No-wait flag used
  - Given: `pump_loop` is running in a production build
  - When: the MFT has no events queued
  - Then: `GetEvent` returns `MF_E_NO_EVENTS_AVAILABLE` (HRESULT `0x00040204`) and does NOT block indefinitely

- **S1.2** — Stop latency bounded
  - Given: `pump_loop` is blocked in the idle sleep (no pending events)
  - When: `state.stop.store(true, Ordering::Release)` is called from another thread
  - Then: `pump_loop` observes the stop flag and exits within 2 × PUMP_SLEEP_MS of the store (worst-case two loop iterations)

**Test mapping**:
- S1.1 — exercised implicitly by `mft_stop_during_idle_returns_within_deadline` (T-NEW-1); if the blocking call were still present, the test would hang
- S1.2 — `mft_stop_during_idle_returns_within_deadline` (T-NEW-1) directly

---

### R2 — Explicit counters track pending NeedInput and HaveOutput events

**Statement**: `pump_loop` MUST maintain two local `u32` counters: one tracking unserviced `METransformNeedInput` events (`pending_need_input`) and one tracking unserviced `METransformHaveOutput` events (`pending_have_output`). Each `METransformNeedInput` event received MUST increment `pending_need_input` by exactly 1. Each `METransformHaveOutput` event received MUST increment `pending_have_output` by exactly 1. Each `ProcessOutput` call (via `collect_output`) MUST be preceded by a nonzero `pending_have_output` and MUST decrement `pending_have_output` by 1 after it completes. Each `ProcessInput` call (via `submit_next_frame`) MUST be preceded by a nonzero `pending_need_input` and MUST decrement `pending_need_input` by 1 after it completes. Submitting a frame to the MFT without a preceding `NeedInput` event is FORBIDDEN (the Microsoft async-MFT spec requires `ProcessInput` only in response to `METransformNeedInput`).

**Smoke required**: yes

**Source decision**: Proposal Decision #1 (Pattern B counter design), explore #594 §2 ("Critical detail: the NeedInput counter MUST be maintained precisely").

**Acceptance scenarios**:

- **S2.1** — Counter increment on NeedInput
  - Given: `pump_loop` is running and `pending_need_input` is 0
  - When: the MFT emits one `METransformNeedInput` event
  - Then: `pending_need_input` becomes 1 before the next service step executes

- **S2.2** — Counter decrement on ProcessInput
  - Given: `pending_need_input` is 1 and a frame is available in the channel
  - When: `submit_next_frame` is called once
  - Then: `pending_need_input` becomes 0

- **S2.3** — Counter increment on HaveOutput
  - Given: `pump_loop` is running and `pending_have_output` is 0
  - When: the MFT emits one `METransformHaveOutput` event
  - Then: `pending_have_output` becomes 1 before the next service step executes

- **S2.4** — Counter decrement on ProcessOutput
  - Given: `pending_have_output` is 1
  - When: `collect_output` is called once
  - Then: `pending_have_output` becomes 0

- **S2.5** — No spurious ProcessInput (1:1 contract)
  - Given: `pending_need_input` is 0 and the frame channel is non-empty
  - When: the service step for NeedInput runs
  - Then: `submit_next_frame` is NOT called (counter zero gates the call)

**Test mapping**:
- S2.1–S2.5 — exercised end-to-end by the 16 existing smoke tests (counter correctness is an invariant visible as correct encode output) and by T-NEW-1 / T-NEW-2 for the stop path

---

### R3 — HaveOutput MUST be drained before NeedInput is serviced on every iteration

**Statement**: On every loop iteration after reading events, `pump_loop` MUST service ALL pending `HaveOutput` events (drain `pending_have_output` to 0 by calling `collect_output` once per count) BEFORE servicing ANY pending `NeedInput` events (calling `submit_next_frame`). This ordering MUST hold even when both counters are nonzero simultaneously. This invariant is the direct fix for Bug 1 (vendor priming): hardware vendor MFTs that emit `HaveOutput` before `NeedInput` at startup will have their priming output drained before the encoder attempts to submit input.

**Smoke required**: yes

**Source decision**: Proposal Decision #1 ("Service order on every iteration: (i) drain ALL pending_have_output first, (ii) then service ALL pending_need_input"), explore #594 §2 ("Drains output before submitting input, matching vendor priming expectations").

**Acceptance scenarios**:

- **S3.1** — Priming output drained before input submitted
  - Given: at loop iteration start, `pending_have_output` is 2 and `pending_need_input` is 1
  - When: the service phase executes
  - Then: `collect_output` is called twice before `submit_next_frame` is called once

- **S3.2** — Encode pipeline reaches steady state after startup
  - Given: the encoder has been `start()`-ed and `pending_have_output` was nonzero before any `NeedInput` arrived
  - When: frames are continuously sent via the channel
  - Then: encoded packets are received on the output side (the pipeline does not deadlock)

- **S3.3** — No NeedInput serviced when HaveOutput queue is nonempty
  - Given: `pending_have_output` is 1 and `pending_need_input` is 1
  - When: the service phase begins
  - Then: `collect_output` executes; `submit_next_frame` does NOT execute until `pending_have_output` reaches 0

**Test mapping**:
- S3.1, S3.3 — structural invariant verified by the 16 existing smoke tests (if ordering were wrong, the pipeline would deadlock and all 16 would fail even after Fix B)
- S3.2 — `mft_thirty_frame_smoke_emits_at_least_one_keyframe` and `mft_encoded_packet_starts_with_annex_b_start_code` (regression coverage confirming pipeline reaches steady state)

---

### R4 — Stop signal honored within a defined deadline from any pump_loop state

**Statement**: After `state.stop` is set to `true` (via `Ordering::Release` store), `pump_loop` MUST exit within `STOP_DEADLINE_MS` milliseconds regardless of whether the loop is in the idle-sleep state, actively servicing HaveOutput events, or actively servicing NeedInput events. The stop check MUST occur at the top of every loop iteration (before event reads and counter servicing). `STOP_DEADLINE_MS` is defined as 2000 ms for acceptance test purposes. This is the externally-observable contract; the internal stop-check frequency (bounded by `PUMP_SLEEP_MS` = 1 ms per Decision #5) guarantees the deadline is met with margin.

**Smoke required**: yes

**Source decision**: Proposal Decision #2 (Fix B, stop check per iteration), Decision #5 (1 ms sleep constant), OQ-5 resolved here: deadline = 2000 ms for T-NEW-1 and T-NEW-2; constant lives inline in each test (not in a shared module, to keep the two new tests self-contained and avoid adding a `tests/common/` module in this change).

**Acceptance scenarios**:

- **S4.1** — Idle-path stop returns within deadline
  - Given: `start()` has been called and no frames have been sent via the channel
  - When: `stop()` is called from another thread
  - Then: `stop()` returns `Ok(())` within 2000 ms (STOP_DEADLINE_MS)

- **S4.2** — Active-path stop returns within deadline
  - Given: `start()` has been called and 5 frames have been sent mid-stream WITHOUT closing `frame_tx`
  - When: `stop()` is called from another thread while `frame_tx` remains open
  - Then: `stop()` returns `Ok(())` within 2000 ms without requiring `frame_tx` to be dropped first

- **S4.3** — Idempotent stop still returns within deadline
  - Given: `stop()` has already been called once and returned `Ok(())`
  - When: `stop()` is called a second time
  - Then: it returns `Ok(())` within 2000 ms (idempotency preserved by prior spec R13 from #572)

**Test mapping**:
- S4.1 — `mft_stop_during_idle_returns_within_deadline` (T-NEW-1)
- S4.2 — `mft_stop_during_active_encode_returns_within_deadline` (T-NEW-2)
- S4.3 — `mft_stop_is_idempotent` (existing, regression)

---

### R5 — Polling sleep duration locked at 1 ms

**Statement**: When `GetEvent(MF_EVENT_FLAG_NO_WAIT)` returns `MF_E_NO_EVENTS_AVAILABLE`, `pump_loop` MUST call `std::thread::sleep(Duration::from_millis(PUMP_SLEEP_MS))` where `PUMP_SLEEP_MS` is a `const u64 = 1` defined locally in `pump_loop` or at module scope within `windows_mft.rs`. The sleep MUST occur only when no counter servicing was done in the current iteration (no new events, no pending work). This value MUST NOT be configurable via `EncoderConfig` or any public API in this change. YAGNI applies: if vendor-specific tuning is needed, it is a follow-up.

**Smoke required**: no

**Source decision**: Proposal Decision #5 ("Const 1 ms — `std::time::Duration::from_millis(1)` inline in pump_loop"), OQ-5 partially resolved here (1 ms sleep confirmed; the `const` scope is implementation detail for design, but the value is locked in spec).

**Acceptance scenarios**:

- **S5.1** — Sleep is 1 ms
  - Given: `pump_loop` is running and no MFT events arrive for 100 ms
  - When: the idle-sleep path executes
  - Then: the thread sleeps approximately 1 ms per iteration (observable via stop-latency: worst-case stop detection ≤ 2 ms, well within 2000 ms deadline)

- **S5.2** — No config field for sleep duration
  - Given: the public `EncoderConfig` struct in `sm-domain`
  - When: the hw-encoder-mft-rework change is applied
  - Then: `EncoderConfig` does NOT gain a `pump_sleep_ms` or equivalent field

**Test mapping**:
- S5.1 — indirectly verified by T-NEW-1 (if sleep were 0, busy-wait would consume 100% CPU and likely cause test flakiness; if sleep were >> 1 ms, stop latency could exceed deadline)
- S5.2 — `encoder_config_no_pump_sleep_field` (CI compile-time check: verify `EncoderConfig` has no new sleep-related field)

---

### R6 — METransformDrainComplete arm added to pump_loop

**Statement**: `pump_loop` MUST include an explicit arm for `METransformDrainComplete` events. When this event is received, `pump_loop` MUST reset `pending_need_input` to 0 (the drain operation has consumed all pending input slots) and MUST reset `pending_have_output` to 0 (all output has been collected as part of the drain sequence). The `METransformDrainComplete` event MUST NOT by itself cause `pump_loop` to exit; the top-of-iteration stop check remains the single exit point for the stop signal. The existing catch-all warn arm (`tracing::warn!("unhandled event_type=...")`) MUST NOT fire for `METransformDrainComplete` after this change.

**Smoke required**: yes

**Source decision**: Proposal Decision #4 ("Include METransformDrainComplete in this change — counter-based design makes a missing DrainComplete arm a correctness hole"), OQ-4 resolved as "reset counters and continue; let the top-of-iteration stop check be the single break point".

**Acceptance scenarios**:

- **S6.1** — DrainComplete resets counters
  - Given: `pump_loop` has received `COMMAND_DRAIN`, `pending_need_input` is 2 and `pending_have_output` is 1
  - When: `METransformDrainComplete` is received
  - Then: `pending_need_input` becomes 0 and `pending_have_output` becomes 0

- **S6.2** — DrainComplete does not exit the loop
  - Given: `state.stop` is false when `METransformDrainComplete` is received
  - When: the DrainComplete arm executes
  - Then: `pump_loop` does NOT exit; it continues to the next iteration

- **S6.3** — No unhandled-event warn for DrainComplete
  - Given: `pump_loop` receives a `METransformDrainComplete` event
  - When: the arm matches
  - Then: `tracing::warn!("unhandled event_type=...")` is NOT emitted (observable in RUST_LOG output during smoke)

- **S6.4** — Drain-then-stop completes cleanly
  - Given: channel is closed (`frame_tx` dropped), drain has completed, `METransformDrainComplete` has been received
  - When: `stop()` is called
  - Then: `stop()` returns `Ok(())` within STOP_DEADLINE_MS (drain completion does not create phantom counter state that blocks shutdown)

**Test mapping**:
- S6.1–S6.3 — `mft_drain_after_channel_close_does_not_panic` (existing smoke, regression; covers the COMMAND_DRAIN → HaveOutput → DrainComplete sequence)
- S6.4 — `mft_stop_is_idempotent` and `mft_drain_after_channel_close_does_not_panic` (regression pair)

---

### R7 — Tracing emits counter snapshots without log spam at steady-state

**Statement**: `pump_loop` MUST emit `tracing::trace!` with `pending_need_input` and `pending_have_output` values ONLY when either counter changes value from its value in the previous iteration. `tracing::debug!` MUST be emitted once every 1000 loop iterations as a heartbeat (e.g., iteration number + current counter values). `tracing::trace!` or `tracing::debug!` MUST NOT be emitted on every iteration unconditionally. This ensures that a smoke run with `RUST_LOG=sm_infra::encode=trace` does not produce megabytes of log output per second at 30 fps, while still providing actionable counter diagnostics when events arrive out-of-order.

**Smoke required**: no

**Source decision**: OQ-2 resolved here: "emit `tracing::trace!` only when counter changes; emit `tracing::debug!` once per 1000-iteration boundary as heartbeat."

**Acceptance scenarios**:

- **S7.1** — Trace only on counter change
  - Given: 1000 consecutive iterations with no MFT events (counters unchanged at 0/0)
  - When: the loop runs at idle for 1 second
  - Then: `trace!` is emitted 0 times for counter-change events; `debug!` is emitted exactly 1 time (the 1000-iteration heartbeat)

- **S7.2** — Trace on NeedInput receipt
  - Given: `pending_need_input` was 0 and becomes 1 upon receiving an event
  - When: the event arm executes
  - Then: `trace!` is emitted with the new counter value

**Test mapping**:
- S7.1, S7.2 — Not a `cargo nextest` test; verified by inspection of `RUST_LOG=sm_infra::encode=trace` output during smoke transcript review. Verify phase checks smoke transcript for absence of per-iteration spam.

---

### R8 — T-NEW-1: mft_stop_during_idle_returns_within_deadline smoke test added

**Statement**: A new `#[test] #[ignore]` test named `mft_stop_during_idle_returns_within_deadline` MUST be added to `crates/sm-infra/tests/windows_mft_encode.rs`. The test MUST: (1) construct a `WindowsMftH264Encoder` with a valid config on a HW-capable host, (2) call `start()` and assert it returns `Ok(())`, (3) NOT send any frames via the frame channel, (4) call `stop()` and record elapsed time, (5) assert `stop()` returns `Ok(())` within 2000 ms. The test MUST be annotated `#[ignore]` (HW gate) and MUST call `init_tracing()` as the first line per project convention (#186). This test directly exercises Bug 2 on the idle path; before the fix, it hangs indefinitely.

**Smoke required**: yes

**Source decision**: Proposal Decision #6 (T-NEW-1 included), OQ-5 resolved here (deadline = 2000 ms, constant inline in test).

**Acceptance scenarios**:

- **S8.1** — Test exists and is properly gated
  - Given: `crates/sm-infra/tests/windows_mft_encode.rs` after the change
  - When: `cargo nextest run --workspace` runs (default, no `--run-ignored`)
  - Then: `mft_stop_during_idle_returns_within_deadline` is listed as IGNORED (not run, not failed)

- **S8.2** — Test passes on HW host after fix
  - Given: a HW-capable Windows host with Intel/NVIDIA/AMD GPU
  - When: `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all`
  - Then: `mft_stop_during_idle_returns_within_deadline` returns PASS within 2000 ms wall-clock from `stop()` call

- **S8.3** — Test hangs before fix (RED evidence)
  - Given: the test added BEFORE pump_loop is modified (Strict TDD RED commit)
  - When: the test runs under the current blocking `GetEvent(MF_EVENT_FLAG_NONE)` implementation
  - Then: the test hangs (test runner timeout, not a PASS) — this is the RED state

**Test mapping**: The requirement IS the test (T-NEW-1). RED commit: `test(infra): add MFT idle-stop smoke test RED`; GREEN commit: after pump_loop rewrite.

---

### R9 — T-NEW-2: mft_stop_during_active_encode_returns_within_deadline smoke test added

**Statement**: A new `#[test] #[ignore]` test named `mft_stop_during_active_encode_returns_within_deadline` MUST be added to `crates/sm-infra/tests/windows_mft_encode.rs`. The test MUST: (1) construct a `WindowsMftH264Encoder` with a valid config, (2) call `start()` and assert `Ok(())`, (3) send exactly 5 frames via the channel WITHOUT closing `frame_tx`, (4) call `stop()` while `frame_tx` is still open and record elapsed time, (5) assert `stop()` returns `Ok(())` within 2000 ms. The distinction from T-NEW-1 is that `frame_tx` remains open — this covers the scenario where the encoder is mid-stream when stop is called. The test MUST call `init_tracing()` as the first line.

**Smoke required**: yes

**Source decision**: Proposal Decision #6 (T-NEW-2 included), OQ-5 resolved here (deadline = 2000 ms, constant inline in test).

**Acceptance scenarios**:

- **S9.1** — Test exists and is properly gated
  - Given: `crates/sm-infra/tests/windows_mft_encode.rs` after the change
  - When: `cargo nextest run --workspace` (default)
  - Then: `mft_stop_during_active_encode_returns_within_deadline` is IGNORED

- **S9.2** — Test passes on HW host after fix
  - Given: a HW-capable Windows host
  - When: `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all`
  - Then: `mft_stop_during_active_encode_returns_within_deadline` returns PASS within 2000 ms from `stop()` call, with `frame_tx` still open at the time `stop()` is called

- **S9.3** — Test hangs before fix (RED evidence)
  - Given: the test added BEFORE pump_loop is modified (Strict TDD RED commit)
  - When: run under the current blocking GetEvent implementation
  - Then: the test hangs (test runner timeout)

**Test mapping**: The requirement IS the test (T-NEW-2). RED commit: `test(infra): add MFT active-stop smoke test RED`; GREEN commit: after pump_loop rewrite.

---

### R10 — All 16 existing smoke tests continue to PASS (regression coverage)

**Statement**: All 16 `#[ignore]`-gated tests already present in `crates/sm-infra/tests/windows_mft_encode.rs` before this change MUST continue to PASS on a HW-capable Windows host after the pump_loop rewrite is applied. No existing test MUST be deleted, renamed, or have its pass criterion loosened. The combined pass count on `--run-ignored=all` after this change MUST be 18/18 (16 existing + 2 new). These 16 tests serve as the regression gate: a pump_loop rewrite that breaks any of them introduces a regression worse than the current state.

**Smoke required**: yes

**Source decision**: Proposal §8 ("All 16 must transition from current state (3/16 PASS per #591) to 16/16 PASS"), Decision #1 (Pattern B rationale includes not regressing the existing test suite).

**Acceptance scenarios**:

- **S10.1** — Existing tests not modified structurally
  - Given: `crates/sm-infra/tests/windows_mft_encode.rs` before and after the change
  - When: a diff is applied
  - Then: none of the 16 existing test function bodies are deleted; test names are unchanged; `#[ignore]` annotations are preserved

- **S10.2** — All 18 tests PASS on HW host
  - Given: a HW-capable Windows host after the pump_loop rewrite
  - When: `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all`
  - Then: output shows 18 tests PASSED, 0 FAILED, 0 IGNORED (all run, all green)

- **S10.3** — Smoke forecast table realized: Bug 2 dominant unlocker
  - Given: explore #594 §6 forecast table predicts "Both fixes" column = PASS for all 16 tests
  - When: smoke transcript is supplied after this change
  - Then: the transcript shows ≥ 16/16 PASS for the pre-existing tests (the forecast was correct) — if fewer pass, the verify-report MUST flag the discrepancy as CRITICAL

**Test mapping**: All 16 existing tests by name (as listed in explore #594 §6 forecast table).

---

### R11 — Cargo default features unchanged (default = [])

**Statement**: `crates/sm-infra/Cargo.toml` MUST NOT change `default = []` in this change. The `hw-encoder` feature MUST remain opt-in for the duration of this change. The Cargo default flip (to `default = ["hw-encoder"]`) is a SEPARATE follow-up change gated on: (a) a clean 18/18 smoke transcript on the user's GPU, (b) at least one additional vendor confirmed, (c) 24-hour soak observation. This requirement MUST NOT be relaxed during apply phase even if smoke passes cleanly.

**Smoke required**: no

**Source decision**: Proposal Decision #8 (explicit, non-negotiable — "STAYS `default = []` for the merge of THIS change").

**Acceptance scenarios**:

- **S11.1** — default remains []
  - Given: `crates/sm-infra/Cargo.toml` after the change
  - When: the file is read
  - Then: `default = []` (or `default = [""]` if the array syntax varies); the `hw-encoder` feature key is NOT listed under `default`

- **S11.2** — CI matrix passes without hw-encoder
  - Given: the 3-OS CI matrix (`windows-latest`, `ubuntu-latest`, `macos-latest`)
  - When: `cargo nextest run --workspace` (no `--features hw-encoder`) runs on all three OSes
  - Then: all gates pass; no HW-dependent code is exercised in the default matrix

**Test mapping**:
- S11.1 — `cargo_default_has_no_hw_encoder` (CI compile-time check: `cargo metadata --no-deps | grep default` in Cargo.toml validation)
- S11.2 — existing CI matrix (no change required)

---

### R12 — No public API changes to VideoEncoder port

**Statement**: The `VideoEncoder` trait in `crates/sm-domain/src/encode.rs` MUST NOT gain, lose, or change any method signatures in this change. `EncoderConfig` MUST NOT gain or lose any fields in this change (all `EncoderConfig` field changes were shipped in PR #15). The `factory.rs` HW-first / SW-fallback logic MUST NOT be modified. The `src-tauri` call site for encoder construction MUST NOT require changes (dimensions are already threaded through from PR #15).

**Smoke required**: no

**Source decision**: Proposal §4 Scope OUT ("Touching the VideoEncoder port contract in sm-domain — no domain change"), Decision #8 (Cargo default unchanged), proposal §5 ("sm-domain: NO change").

**Acceptance scenarios**:

- **S12.1** — VideoEncoder trait diff is empty
  - Given: `crates/sm-domain/src/encode.rs` before and after the change
  - When: a diff is applied
  - Then: no additions or removals in the `VideoEncoder` trait definition block

- **S12.2** — no_platform_deps invariant still passes
  - Given: `cargo nextest run -p sm-domain`
  - When: run after the change
  - Then: the `no_platform_deps` test passes (sm-domain remains platform-free)

- **S12.3** — EncoderConfig field set unchanged
  - Given: `EncoderConfig` struct before and after the change
  - When: a diff is applied
  - Then: no new fields added, no fields removed, no field types changed

**Test mapping**:
- S12.1–S12.3 — `cargo nextest run -p sm-domain` (CI-runnable, no GPU required)

---

### R13 — No new unsafe COM surface introduced

**Statement**: This change MUST NOT introduce any new COM interface implementations (outgoing COM callbacks, COM vtables, or `IMFAsyncCallback` implementations) in `windows_mft.rs` or anywhere in `sm-infra`. The `unsafe` block count in `windows_mft.rs` MUST NOT increase from its PR #15 baseline. Pattern A (BeginGetEvent) is explicitly out of scope per Decision #2; no fallback to Pattern A is permitted in this change even if NO_WAIT proves problematic on a specific vendor.

**Smoke required**: no

**Source decision**: Proposal Decision #2 (Fix A rejected — "high unsafe surface, fragile lifetime"), Decision #7 (if NO_WAIT is vendor-incompatible, escalate to `hw-encoder-mft-async-callback` change rather than implement Fix A here).

**Acceptance scenarios**:

- **S13.1** — No new IMFAsyncCallback implementation
  - Given: `crates/sm-infra/src/encode/windows_mft.rs` after the change
  - When: searched for `impl IMFAsyncCallback` or `IMFAsyncCallback` as an outgoing impl
  - Then: no such implementation exists in the file

- **S13.2** — unsafe block count does not increase
  - Given: the PR #15 baseline unsafe block count in `windows_mft.rs`
  - When: the change is applied
  - Then: `grep -c 'unsafe {' crates/sm-infra/src/encode/windows_mft.rs` does not produce a larger number than the baseline

**Test mapping**:
- S13.1, S13.2 — `cargo clippy --workspace --all-targets --all-features -- -D warnings` (no new unsafe warnings) + diff review in verify phase

---

## 4. Scenarios Index (cross-reference)

| Scenario | Requirement | Test name | File | Smoke? |
|----------|-------------|-----------|------|--------|
| S1.1 | R1 — NO_WAIT polling | `mft_stop_during_idle_returns_within_deadline` (T-NEW-1) | `tests/windows_mft_encode.rs` | yes |
| S1.2 | R1 — stop latency bounded | `mft_stop_during_idle_returns_within_deadline` (T-NEW-1) | `tests/windows_mft_encode.rs` | yes |
| S2.1 | R2 — NeedInput counter++ | (16 existing + T-NEW-1/2 end-to-end coverage) | `tests/windows_mft_encode.rs` | yes |
| S2.2 | R2 — NeedInput counter-- | (16 existing + T-NEW-1/2) | `tests/windows_mft_encode.rs` | yes |
| S2.3 | R2 — HaveOutput counter++ | (16 existing + T-NEW-1/2) | `tests/windows_mft_encode.rs` | yes |
| S2.4 | R2 — HaveOutput counter-- | (16 existing + T-NEW-1/2) | `tests/windows_mft_encode.rs` | yes |
| S2.5 | R2 — no spurious ProcessInput | (16 existing regression) | `tests/windows_mft_encode.rs` | yes |
| S3.1 | R3 — HaveOutput drained first | (16 existing regression) | `tests/windows_mft_encode.rs` | yes |
| S3.2 | R3 — pipeline reaches steady state | `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | `tests/windows_mft_encode.rs` | yes |
| S3.3 | R3 — no NeedInput while HaveOutput pending | (16 existing regression) | `tests/windows_mft_encode.rs` | yes |
| S4.1 | R4 — idle-path stop deadline | `mft_stop_during_idle_returns_within_deadline` (T-NEW-1) | `tests/windows_mft_encode.rs` | yes |
| S4.2 | R4 — active-path stop deadline | `mft_stop_during_active_encode_returns_within_deadline` (T-NEW-2) | `tests/windows_mft_encode.rs` | yes |
| S4.3 | R4 — idempotent stop deadline | `mft_stop_is_idempotent` | `tests/windows_mft_encode.rs` | yes |
| S5.1 | R5 — sleep is 1 ms | T-NEW-1 (indirect: latency floor) | `tests/windows_mft_encode.rs` | yes |
| S5.2 | R5 — no config field | `encoder_config_no_pump_sleep_field` | `crates/sm-domain/src/encode.rs` (unit) | no |
| S6.1 | R6 — DrainComplete resets counters | `mft_drain_after_channel_close_does_not_panic` | `tests/windows_mft_encode.rs` | yes |
| S6.2 | R6 — DrainComplete no loop exit | `mft_drain_after_channel_close_does_not_panic` | `tests/windows_mft_encode.rs` | yes |
| S6.3 | R6 — no warn for DrainComplete | smoke transcript RUST_LOG inspection | `tests/windows_mft_encode.rs` | yes |
| S6.4 | R6 — drain-then-stop completes | `mft_stop_is_idempotent` + `mft_drain_after_channel_close_does_not_panic` | `tests/windows_mft_encode.rs` | yes |
| S7.1 | R7 — trace only on counter change | smoke transcript inspection | (process) | no |
| S7.2 | R7 — trace on NeedInput | smoke transcript inspection | (process) | no |
| S8.1 | R8 — T-NEW-1 gated by #[ignore] | `mft_stop_during_idle_returns_within_deadline` | `tests/windows_mft_encode.rs` | no |
| S8.2 | R8 — T-NEW-1 passes on HW host | `mft_stop_during_idle_returns_within_deadline` | `tests/windows_mft_encode.rs` | yes |
| S8.3 | R8 — T-NEW-1 RED before fix | `mft_stop_during_idle_returns_within_deadline` | `tests/windows_mft_encode.rs` | yes |
| S9.1 | R9 — T-NEW-2 gated by #[ignore] | `mft_stop_during_active_encode_returns_within_deadline` | `tests/windows_mft_encode.rs` | no |
| S9.2 | R9 — T-NEW-2 passes on HW host | `mft_stop_during_active_encode_returns_within_deadline` | `tests/windows_mft_encode.rs` | yes |
| S9.3 | R9 — T-NEW-2 RED before fix | `mft_stop_during_active_encode_returns_within_deadline` | `tests/windows_mft_encode.rs` | yes |
| S10.1 | R10 — existing tests not modified | diff review | `tests/windows_mft_encode.rs` | no |
| S10.2 | R10 — 18/18 PASS on HW host | all 18 tests | `tests/windows_mft_encode.rs` | yes |
| S10.3 | R10 — forecast table realized | smoke transcript vs. explore §6 | (process) | yes |
| S11.1 | R11 — default stays [] | Cargo.toml diff | `crates/sm-infra/Cargo.toml` | no |
| S11.2 | R11 — CI matrix without hw-encoder | existing CI matrix | (CI) | no |
| S12.1 | R12 — VideoEncoder trait unchanged | diff review | `crates/sm-domain/src/encode.rs` | no |
| S12.2 | R12 — no_platform_deps passes | `cargo nextest run -p sm-domain` | (CI) | no |
| S12.3 | R12 — EncoderConfig fields unchanged | diff review | `crates/sm-domain/src/encode.rs` | no |
| S13.1 | R13 — no IMFAsyncCallback impl | grep + diff review | `crates/sm-infra/src/encode/windows_mft.rs` | no |
| S13.2 | R13 — unsafe count stable | grep count + diff review | `crates/sm-infra/src/encode/windows_mft.rs` | no |

**Total scenarios**: 36
**Smoke-required scenarios**: 21
**CI-only scenarios**: 15

---

## 5. Non-Functional Requirements

### NF1 — Polling sleep duration

`PUMP_SLEEP_MS` MUST be `1` (millisecond). This value is locked by Decision #5 and is NOT tunable. It gives:
- Stop-detection latency: ≤ 2 ms worst-case (two iterations)
- CPU overhead at 30 fps idle: < 0.1% (1 ms sleep >> encode time per event)
- Stop deadline margin: 2000 ms deadline / 2 ms per iteration = 1000× safety factor

### NF2 — No regression on 16 currently-passing or previously-passing smoke tests

All 16 existing `#[ignore]` tests MUST be in the 18/18 PASS set. The pre-fix baseline (3/16 PASS per #591) is the floor; the target is 16/16 pre-existing + 2/2 new = 18/18.

### NF3 — No new unsafe COM surface (Fix A out of scope)

Confirmed by R13. The `unsafe` block count in `windows_mft.rs` is frozen at the PR #15 baseline. Any future COM callback work lives in `hw-encoder-mft-async-callback` (Decision #7 contingency change).

### NF4 — Cargo default = [] unchanged

Confirmed by R11. Production default is `default = []`; the HW path remains opt-in for this change.

### NF5 — No public API changes

Confirmed by R12. `VideoEncoder` trait, `EncoderConfig` struct shape, and `factory.rs` logic are frozen for this change.

### NF6 — All 5 quality gates remain GREEN

Both under `--features hw-encoder` and under `--no-default-features`:
1. `cargo check --workspace`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo fmt --check --all`
4. `cargo nextest run --workspace`
5. `cargo deny check`

---

## 6. Open Questions: Resolved vs. Deferred

### OQ-1 (counter-update timing) — DEFERRED to design

The proposal recommended: "decrement AFTER successful return; on COM error, leave the counter and let the next iteration retry once before propagating Err." This is correct behavior but the exact retry semantics (how many times, what HRESULT codes trigger retry vs. immediate Err) involve low-level state machine detail. Design phase locks the exact counter-mutation point within `collect_output` / `submit_next_frame`.

**Spec constraint enforced here**: counters MUST be decremented after the COM call completes (success or failure), not before. Pre-decrement that could cause under-counting on error is FORBIDDEN.

### OQ-2 (log spam strategy) — RESOLVED HERE

Resolved in R7:
- `tracing::trace!` emitted ONLY when either counter changes from previous-iteration value
- `tracing::debug!` emitted once per 1000 iterations as a heartbeat
- No per-iteration unconditional emit

### OQ-3 (Smoke required flag per requirement) — RESOLVED HERE

This IS the spec phase; all flags are assigned above. Summary:
- `Smoke required: yes` — R1, R2, R3, R4, R6, R8, R9, R10
- `Smoke required: no` — R5, R7, R11, R12, R13

### OQ-4 (DrainComplete arm exact behavior) — PARTIALLY RESOLVED HERE, detail to design

Resolved at the spec level: DrainComplete resets both counters and does NOT exit the loop (R6). The design phase specifies the exact state machine transition (whether it also sends any signal, whether it interacts with EOS detection).

### OQ-5 (test deadline constants) — RESOLVED HERE

- STOP_DEADLINE_MS = 2000 ms for both T-NEW-1 and T-NEW-2
- Constant lives INLINE in each test (not in a shared `tests/common/timeouts.rs` module — no new module added in this change)
- The 2000 ms value provides a 1000× safety margin over the 2 ms worst-case stop latency

---

## 7. Out of Scope

- **IMFAsyncCallback / BeginGetEvent path** (Fix A, Pattern A) — explicitly deferred to potential `hw-encoder-mft-async-callback` change per Decision #7 contingency. NOT permitted even as a runtime fallback in this change.
- **T-NEW-3** (`mft_handles_have_output_before_need_input` vendor-priming test) — deferred per Decision #6; requires a mock MFT or vendor-specific empirical scaffolding that does not fit the existing smoke pattern.
- **Cargo default flip** to `default = ["hw-encoder"]` — deferred per Decision #8 to a follow-up change gated on smoke transcript + soak.
- **Pattern C** (separate input/output threads) — rejected by explore §2; out of scope.
- **EncoderConfig tunability** for `pump_sleep_ms` — rejected by Decision #5 (YAGNI); `sm-domain` MUST NOT change.
- **factory.rs changes** — out of scope; HW-first/SW-fallback logic unchanged.
- **v0.2.0 release artifacts** — separate candidate per #186.
- **Additional vendor-specific code paths** (NVENC / AMF / QSV specific) — MFT path multiplexes transparently.

---

## 8. Strict TDD Constraints

ACTIVE per #186. Every `(impl)` task MUST be preceded by an `(test)` task producing a RED state. No implementation commit may land without a corresponding RED commit evidenced in the apply-progress log.

Suggested commit sequence (5 commits minimum):

1. `test(infra): add MFT idle-stop smoke test RED` — T-NEW-1 added, hangs under current code (S8.3)
2. `test(infra): add MFT active-stop smoke test RED` — T-NEW-2 added, hangs under current code (S9.3)
3. `feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling GREEN` — all R1–R6 implemented; T-NEW-1, T-NEW-2, and all 16 existing tests go GREEN
4. `refactor(infra): add DrainComplete arm and counter-change tracing` — R6, R7 refined (if not folded into commit 3)
5. `test(infra): add encoder_config_no_pump_sleep_field unit test` — S5.2 CI check

HW smoke tests (`#[ignore]`): added in commits 1–2, validated via manual smoke transcript supplied after commit 3.

---

## 9. Smoke Transcript Requirements

The verify phase MUST emit `BLOCKED_ON_SMOKE` until the user supplies a manual smoke transcript from a HW-capable Windows host:

```
cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all
```

Required result: **18/18 PASS** (16 pre-existing + 2 new).

The transcript MUST be saved as an engram observation under topic_key `sdd/hw-encoder-mft-rework/smoke-transcript` OR committed as `docs/smoke/hw-encoder-mft-rework-<date>.txt` before verify can issue `APPROVED_FOR_ARCHIVE`.

This requirement is non-negotiable per the BLOCKED_ON_SMOKE rule encoded in #186 and introduced in spec #586.

---

## 10. Decision #7 Contingency Encoding

If the apply-phase smoke transcript shows `MF_E_NO_EVENTS_AVAILABLE` is NOT returned correctly by the user's GPU vendor (i.e., `GetEvent(MF_EVENT_FLAG_NO_WAIT)` behaves as a blocking call or returns a different HRESULT), the following MUST happen:

1. Do NOT merge the PR to master.
2. The verify-report MUST emit `status: BLOCKED_ON_VENDOR_COMPAT` with the specific HRESULT observed.
3. A new SDD change `hw-encoder-mft-async-callback` MUST be spawned via `/sdd-new`.
4. The 2 new smoke tests (T-NEW-1, T-NEW-2) MAY be kept on the branch as diagnostic anchors for the follow-up change.

This contingency is separate from the BLOCKED_ON_SMOKE gate. Both gates must clear independently.

---

## 11. SDD Chain Links

- **Predecessor spec**: `sdd/hardware-accel-encoder-smoke-fixes/spec` (#586) — template source; introduced `Smoke required` flag
- **Predecessor proposal**: `sdd/hw-encoder-mft-rework/proposal` (#595) — 9 locked decisions, source of truth for this spec
- **Exploration**: `sdd/hw-encoder-mft-rework/explore` (#594) — Pattern B analysis, smoke forecast table
- **Project context**: `sdd-init/screen-mirror-app` (#186) — BLOCKED_ON_SMOKE rule, TDD mode, naming conventions
- **Next phases**: `sdd-design` (state machine detail for OQ-1, OQ-4) and `sdd-tasks` (after design); design and tasks can start from this spec

---

## 12. Result Contract

- **status**: done
- **executive_summary**: 13 requirements (R1–R13) and 36 acceptance scenarios translate proposal Decision #1–#9 into testable contracts for the dual-arm NO_WAIT polling redesign of `pump_loop`; every requirement carries a `Smoke required` flag; OQs 2, 3, 5 are resolved here; OQs 1 and 4 are deferred to design with spec-level constraints; the 18-test smoke gate (16 existing + 2 new) and the BLOCKED_ON_SMOKE rule are fully encoded.
- **artifacts**:
  - `engram://sdd/hw-encoder-mft-rework/spec` (topic_key, saved separately)
  - `openspec/changes/hw-encoder-mft-rework/spec.md` (this file)
- **next_recommended**: `sdd-design` (locks OQ-1 counter-mutation exact points, OQ-4 DrainComplete state machine, helper function signatures); `sdd-tasks` after design completes
- **risks**:
  - R1: NO_WAIT vendor compatibility unverified until apply-phase smoke; Decision #7 contingency covers escalation but adds round-trip cost
  - R2: DrainComplete arm (R6) is specified without empirical evidence that the MFT actually emits the event after COMMAND_DRAIN on the user's GPU — design must verify or the spec assertion could be untestable
  - R3: The 2000 ms STOP_DEADLINE_MS is conservative but not measured against actual vendor MFT shutdown timing; if a vendor MFT blocks `ProcessOutput` or `ProcessInput` for > 2000 ms (pathological case), T-NEW-2 could be flaky
- **skill_resolution**: injected
