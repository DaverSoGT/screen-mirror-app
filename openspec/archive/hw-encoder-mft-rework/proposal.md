# Proposal: hw-encoder-mft-rework

> Phase: SDD propose. Inputs: explore #594, init #186, prior chain (hardware-accel-encoder #579, smoke-fixes #582–#592, PR #15 `f01f27f`).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-rework/proposal` + this file). Strict TDD: ACTIVE. Delivery strategy: auto-chain. Execution mode: interactive.

## 0. Honest framing — why this change exists

PR #15 (`fix/hw-encoder-smoke-fixes`, `f01f27f`, 2026-05-04) closed the immediately-shippable defects in `WindowsMftH264Encoder` (Drop AV, async-unlock ordering, probe corruption, EncoderConfig dimensions, test liveness, tracing). It did NOT close the two architectural Bucket A bugs that the in-cycle smokes revealed:

1. **Vendor MFT priming**: Intel QSV / NVIDIA NVENC / AMD AMF emit `METransformHaveOutput` BEFORE `METransformNeedInput` on startup; `pump_loop` is reactive-only and never calls `ProcessInput` first, deadlocking the pipeline.
2. **`pump_loop` stop-signal starvation**: `GetEvent(MF_EVENT_FLAG_NONE)` blocks indefinitely; `state.stop` is checked once per loop iteration AND `MFShutdown()` runs from `Drop` AFTER `join()`. Circular wait → process hang.

PR #15 mitigated production exposure by flipping `default = []`; the HW path is opt-in only via `--features hw-encoder` and currently does not pass smoke. This proposal locks the rework that earns the feature back to default-on AND closes the loop on the v2-candidate row in `sdd-init/screen-mirror-app` (#186).

Per discovery #592 / explore #594, the two bugs are NOT orthogonal — they share `pump_loop:743` (`GetEvent(MF_EVENT_FLAG_NONE)`) and the recommended fixes (Pattern B + Fix B) collapse into a single coherent loop redesign. This proposal locks that single redesign and the delivery mechanism.

## 1. Inputs Read

### Engram observations
- #594 (sdd/hw-encoder-mft-rework/explore) — primary input; locked patterns + recommendations
- #186 (sdd-init/screen-mirror-app) — project context, conventions, v2 candidate row
- #593 (session_summary) — accomplishments + carry-forward state from PR #15
- #584 (sdd/hardware-accel-encoder-smoke-fixes/proposal) — template/structure mirrored

### Files (NOT re-read in this phase — explore #594 already read them in full)
- `crates/sm-infra/src/encode/windows_mft.rs` (1141 lines)
- `crates/sm-infra/tests/windows_mft_encode.rs` (745 lines, 16 `#[ignore]` smoke tests)
- `crates/sm-infra/src/encode/factory.rs`
- `crates/sm-domain/src/encode.rs`
- `crates/sm-infra/Cargo.toml`

## 2. Intent

Redesign `pump_loop` in `crates/sm-infra/src/encode/windows_mft.rs` so that (a) vendor MFT priming sequences (HaveOutput-before-NeedInput) are correctly serviced via a counter-based dual-arm state machine that drains output before submitting input, and (b) the stop signal is honored within ≤ 1 ms on every loop iteration via `MF_EVENT_FLAG_NO_WAIT` polling. Both fixes land as a single redesign because they share the same loop body. Success: 16/16 smoke tests in `crates/sm-infra/tests/windows_mft_encode.rs` PASS on a HW-capable Windows host with manual transcript supplied (BLOCKED_ON_SMOKE rule honored), and `pump_loop` stops within 2 s on idle and active scenarios. The feature flag stays `default = []` for THIS change; default-on is a follow-up decision after the smoke transcript is reviewed.

## 3. Scope IN

1. Rewrite `pump_loop` event-handling body (`windows_mft.rs:712–847`) into a single polling loop using `GetEvent(MF_EVENT_FLAG_NO_WAIT)` with a 1 ms sleep on `MF_E_NO_EVENTS_AVAILABLE`.
2. Introduce explicit counters `pending_need_input: u32` and `pending_have_output: u32` in pump-loop local state.
3. Service order on every iteration: (i) drain ALL `pending_have_output` first (call `collect_output` per count), (ii) then service ALL `pending_need_input` (call `submit_next_frame` per count), (iii) then read next event.
4. Add an explicit arm for `METransformDrainComplete` (event reset on counter, signal end-of-stream).
5. Stop check via `state.stop.load(Ordering::Acquire)` at the top of each loop iteration (already polled, no change to stop API surface).
6. Add 2 new `#[ignore]` smoke tests (counters T-NEW-1, T-NEW-2 in §8).
7. Update tracing instrumentation in pump_loop to emit counter snapshots on every event arm (per discovery #592 tracing-before-explore convention; the next session if smoke fails should not need new instrumentation).
8. Update spec template requirement-by-requirement `Smoke required: yes/no` flag and ensure each requirement has the right value.
9. SDD chain link emitted in §10.

## 4. Scope OUT

- BeginGetEvent / EndGetEvent async COM callback path (Pattern A from explore §3 / Fix A) — explicitly rejected for this change because of high COM-callback unsafe surface; preserved as the contingency in Decision #7.
- Pattern A "proactive ProcessInput on startup" — rejected by explore §2 (does not actually solve HaveOutput-before-NeedInput).
- Pattern C "separate input/output threads" — rejected by explore §2 (high complexity, doubles thread count per encoder, fragile shutdown synchronization).
- Flipping `default = ["hw-encoder"]` in Cargo.toml — see Decision #8: this change ships with `default = []` unchanged; the flip back is a follow-up gated on a clean post-merge smoke transcript.
- Adding NVENC / AMF / QSV vendor-specific code paths (the MFT path multiplexes these transparently per #568).
- Touching the `VideoEncoder` port contract in `sm-domain` (no domain change).
- Reverting any of the 6 smoke-fixes locked items from #584 (Drop fix, probe removal, Annex-B sniff, EncoderConfig dimensions, test liveness, BLOCKED_ON_SMOKE rule).
- The 3rd new test from explore §6 (`mft_handles_have_output_before_need_input` vendor-priming test) — see Decision #6 (deferred; needs mock MFT or vendor-specific empirical validation that does not fit this change).
- Any change to the `factory.rs` HW-first / SW-fallback logic.
- Any tunable for the polling sleep duration via config (see Decision #5).
- v0.2.0 release artifacts.

## 5. Stakeholders / Affected surfaces

- **Crate `sm-infra`**: `src/encode/windows_mft.rs` — `pump_loop` body rewritten, helper functions for counter servicing, `METransformDrainComplete` arm.
- **Crate `sm-infra`**: `tests/windows_mft_encode.rs` — 2 new `#[ignore]` smoke tests added (T-NEW-1, T-NEW-2). Existing 16 tests continue to be the GREEN gate.
- **Crate `sm-domain`**: NO change. `EncoderConfig` already gained dimensions in PR #15.
- **Crate `src-tauri`**: NO change. Production call site already wires real screen dimensions through PR #15.
- **Cargo `default`**: stays `default = []` for this change. Decision #8 governs the flip.
- **Project conventions in `sdd-init/screen-mirror-app` #186**: row updated when this change archives — `Bucket A residual rework` moves from "v2 candidate" to "shipped".
- **Strict TDD test runner**: `cargo nextest run --workspace` continues; HW smoke invoked separately via `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all` on a HW host.
- **CI matrix**: 3-OS matrix runs `--no-default-features` path (HW feature off); cross-platform clippy/fmt validation per discoveries #580, #581. No matrix change required.

## 6. Decisions Locked

| # | Decision | Choice | Alternative considered | Rationale (anchored) |
|---|----------|--------|-------------------------|----------------------|
| 1 | Loop architecture | **Pattern B** (NO_WAIT polling + dual-arm counters with HaveOutput-drain-first ordering) | Pattern A (proactive ProcessInput); Pattern C (separate input/output threads) | Explore §2 ruled out A (does not fix HaveOutput-first vendor priming) and C (high COM-thread complexity, fragile shutdown). Pattern B preserves single-thread model, satisfies async-MFT spec 1:1 counter contract, drains output before input matching vendor expectation. |
| 2 | Stop-signal mechanism | **Fix B** (NO_WAIT polling, fold into Pattern B) | Fix A (BeginGetEvent + IMFShutdown) | Explore §3 — Fix A requires implementing `IMFAsyncCallback` outgoing COM interface in Rust (high unsafe surface, fragile lifetime). Fix B is a single flag change + sleep, naturally bundled with Pattern B (same loop body), stop latency ≤ 1 ms (well under any test deadline). |
| 3 | PR delivery | **SINGLE PR** (NOT chained) | Chained 2-slice (PR1 = Fix B stop-fix, PR2 = Pattern B priming-fix) | Explore §4 — both bugs share `pump_loop:743`, combined diff ~60–100 LOC, well under the 400-line budget. Splitting forces PR1 to ship a polling loop without the counters that justify it; PR1 alone would not improve smoke pass rate (counter-design is the priming fix). Auto-chain delivery_strategy is overruled here on technical grounds: the slices are not autonomously valuable. |
| 4 | METransformDrainComplete arm | **INCLUDE in this change** | Defer to follow-up | Explore §5 OQ4 — counter-based design makes a missing DrainComplete arm a correctness hole (NeedInput counter never resets after drain; subsequent `start()`/`stop()` cycles would accumulate phantom NeedInputs). Adding the arm now is ~5 LOC; deferring guarantees a follow-up PR. |
| 5 | Polling sleep duration | **Const 1 ms** (`std::time::Duration::from_millis(1)`) inline in pump_loop | Config field on `EncoderConfig` | Explore §3 / OQ5 — 1 ms gives ≤ 1 ms stop latency (well under 5 s test deadline) and ≤ 0.1% CPU at 30 fps. No production driver for tunability today. Adding a config field violates YAGNI and forces a `sm-domain` change for zero current value. If vendor-specific tuning is later needed, promote to config in a follow-up. |
| 6 | New smoke tests in scope | **2 of 3** from explore §6: T-NEW-1 (`mft_stop_during_idle_returns_within_deadline`) AND T-NEW-2 (`mft_stop_during_active_encode_returns_within_deadline`). **EXCLUDE** T-NEW-3 (`mft_handles_have_output_before_need_input`). | Add all 3 | Explore §6 — T-NEW-3 requires either a mock MFT (does not fit our smoke pattern; existing 16 tests use real MFT) or vendor-specific empirical scaffolding. T-NEW-1 / T-NEW-2 directly exercise Bug 2 (stop starvation) on idle and active paths; both currently hang. Per BLOCKED_ON_SMOKE rule (#186), these tests count toward the requirement coverage and the manual smoke transcript MUST include them. |
| 7 | NO_WAIT vendor verification fallback (R1 from explore §5) | **Hybrid contingency**: ship Pattern B with NO_WAIT as primary; if apply-phase smoke transcript shows MF_E_NO_EVENTS_AVAILABLE busy-loops or absent on Intel/NVIDIA/AMD on the user's GPU, **abort the smoke handoff at apply, do NOT merge, and escalate to a design-phase BeginGetEvent path** (Fix A) under a NEW change `hw-encoder-mft-async-callback`. Do NOT silently degrade or hide the failure. | Leave as open question; or auto-fallback at runtime to BeginGetEvent | Explore risk-1 calls out NO_WAIT vendor support is unverified empirically. Auto-fallback at runtime would require BOTH paths implemented in this change, defeating the simplicity rationale of Decision #1. The hybrid contingency keeps THIS change small AND preserves a clear contract: smoke must pass on the user's GPU, otherwise the change does not land — same dogfood discipline that the BLOCKED_ON_SMOKE rule (#186) encodes. |
| 8 | Cargo `default` post-change | **STAYS `default = []`** for the merge of THIS change. Flip to `default = ["hw-encoder"]` is a SEPARATE follow-up gated on (a) clean smoke transcript on user GPU, (b) at least one additional vendor confirmed (Intel + NVIDIA OR AMD), (c) 24-hour soak observation. | Flip back to `default = ["hw-encoder"]` in this PR; or leave default-off permanently | PR #15 flipped to `default = []` precisely because the HW path was broken on real hardware. Flipping back in the same PR that rewrites pump_loop would re-create the exposure window of PR #14 → smoke #582 (8/10 FAIL in production default). Decoupling the flip from the rewrite preserves blast-radius isolation: the rewrite is reviewable on its own merits, the flip is reviewable on its own evidence (smoke transcript). |
| 9 | SDD chain link | **Explicit cross-link to**: `sdd/hardware-accel-encoder-smoke-fixes/proposal` (#584), `sdd/hardware-accel-encoder-smoke-fixes/spec` (#586), `sdd-init/screen-mirror-app` (#186) v2 candidate row "HW encoder MFT rework". Archive of THIS change MUST update #186 to mark the row "shipped" and either remove it or relabel as "default-on follow-up pending". | Implicit lineage via topic_key naming only | #186 is the canonical roadmap; per the precedent in §"Roadmap State", every shipped change updates the table. The v2 candidate row exists explicitly for this rework and must be reconciled at archive time. |

## 7. Open Questions (small list, for spec/design)

- **OQ-1** (design): Exact counter-update points within `collect_output` / `submit_next_frame` — does the decrement happen before or after the COM call? Affects retry semantics on transient errors. (Recommendation: decrement AFTER successful return; on COM error, leave the counter and let the next iteration retry once before propagating Err.)
- **OQ-2** (design): How to log counter snapshots without spamming at 30 fps. Recommendation: emit `tracing::trace!` only when counter changes vs. previous iteration; emit `tracing::debug!` once on every 1000-iteration boundary as a heartbeat.
- **OQ-3** (spec): Per-requirement `Smoke required: yes/no` flag values for the new requirements introduced by this change (almost certainly all `yes` since they exercise pump_loop on real MFT, but spec phase confirms).
- **OQ-4** (design): METransformDrainComplete arm exact behavior — does it (a) only reset counters and continue, or (b) also break the loop if `state.stop` is set? Recommendation: (a) — reset counters and continue; let the top-of-iteration stop check be the single break point.
- **OQ-5** (design): Test deadline constants for T-NEW-1 / T-NEW-2 — explore suggests "2 seconds". Spec phase locks the exact constant and whether it lives in a shared `tests/common/timeouts.rs` or inline.

## 8. Smoke Plan Summary

### Existing 16 `#[ignore]` smoke tests in `crates/sm-infra/tests/windows_mft_encode.rs`
All 16 must transition from current state (3/16 PASS per #591) to **16/16 PASS** on the user's HW host after this change. Per explore §6 forecast, Pattern B + Fix B together unlock the 13 currently-failing tests (Bug 2 was the dominant blocker).

### 2 new `#[ignore]` smoke tests added by this change

| Test | Bug exercised | Pre-change behavior | Post-change pass criterion |
|------|---------------|---------------------|------------------------------|
| `mft_stop_during_idle_returns_within_deadline` | Bug 2 (stop starvation, idle path) | Hangs indefinitely → test never returns | `stop()` returns `Ok(())` within 2 s after `start()` with no frames sent |
| `mft_stop_during_active_encode_returns_within_deadline` | Bug 2 (stop starvation, active path) | Hangs / requires `frame_tx` drop | `stop()` returns `Ok(())` within 2 s after sending 5 frames mid-stream WITHOUT closing `frame_tx` first |

### BLOCKED_ON_SMOKE rule (per #186, introduced #586)

ALL requirements in the spec for this change that map to ANY of the 18 (16 + 2) `#[ignore]` smoke tests will be flagged `Smoke required: yes`. Per the rule, verify CANNOT issue `APPROVED_FOR_ARCHIVE` for those requirements without a manual smoke transcript supplied by the user from a HW-capable Windows host (`cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all`). Verify will emit `BLOCKED_ON_SMOKE` until that transcript is provided and reviewed.

This is non-negotiable — eat-our-own-dogfood per #584 §11. The same protocol that PR #15 codified applies retroactively to this change.

### Strict TDD discipline

Each new test (T-NEW-1, T-NEW-2) and each modified existing test gets a RED commit before any pump_loop code change. Per #186 Strict TDD Mode: `cargo nextest run --workspace` is the runner; the tests added here are `#[ignore]`-gated so they do not run in default `cargo nextest`, but RED evidence lives in the apply-progress engram observation as commits like `test(infra): add MFT idle-stop smoke test (RED)` followed by `feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling (GREEN)`.

## 9. Backout Plan

If smoke transcript on user GPU shows the rewrite is worse than current (i.e., post-rewrite smoke pass rate < 3/16, the current baseline):

1. **Immediate**: Do NOT merge the PR. The rewrite stays on the branch `feat/hw-encoder-mft-rework` (or chained branches if Decision #3 is overridden during apply).
2. **Triage**: Compare smoke transcript against explore §5 risk-1 (NO_WAIT vendor support). If MF_E_NO_EVENTS_AVAILABLE is busy-returning or absent, Decision #7 contingency activates: spawn new change `hw-encoder-mft-async-callback` for the BeginGetEvent path.
3. **Revert mechanism if already merged**: `git revert <merge-commit>` — the rewrite is contained to `crates/sm-infra/src/encode/windows_mft.rs` + `crates/sm-infra/tests/windows_mft_encode.rs`. No `sm-domain` changes mean the revert is local to one crate. The `default = []` Cargo flag stays unchanged through this entire change (Decision #8), so a revert restores the exact PR-#15 state.
4. **Communication**: Update `sdd-init/screen-mirror-app` (#186) v2 candidate row to note "rework attempted, vendor-NO_WAIT incompatibility found, escalated to async-callback design".
5. **Carry-forward**: The 2 new smoke tests (T-NEW-1, T-NEW-2) MAY survive the revert as new `#[ignore]` tests pinned to the next attempt; they remain valuable independent of the chosen loop architecture.

## 10. SDD Chain Links

- **Predecessor (carry-forward source)**: `sdd/hardware-accel-encoder-smoke-fixes/proposal` (#584) — 6 locked items, BLOCKED_ON_SMOKE rule, `Smoke required` flag policy. PR #15 (`f01f27f`).
- **Predecessor (root SDD change)**: `sdd/hardware-accel-encoder/proposal` (#570) — original MFT introduction. Archive #579, PR #14 (`80f0853`).
- **Roadmap anchor**: `sdd-init/screen-mirror-app` (#186) — v2 candidates table, row "HW encoder MFT rework | PR #15 carry-forward | L". This change consumes that row.
- **Direct predecessor in chain**: `sdd/hw-encoder-mft-rework/explore` (#594) — patterns A/B/C analysis, Fix A/B analysis, recommendation Pattern B + Fix B. Read in full for this proposal.
- **Discovery anchors**: #592 (MFT async-unlock + tracing-before-re-explore convention), #585 (drain-arm spin), #582 (HW encoder smoke FAIL pattern that earned the BLOCKED_ON_SMOKE rule).
- **Convention references**: #580 (`#[allow]` over `#[expect]` for cfg-gated consumers), #581 (`cargo fmt --check` Windows pipe trap).
- **Successor candidates** (out of scope here, captured for #186 update at archive time):
  - `hw-encoder-mft-async-callback` — IF Decision #7 contingency activates.
  - `hw-encoder-default-on-flip` — Decision #8 follow-up; gated on smoke transcript + soak.

## 11. Result Contract

- **status**: done
- **executive_summary**: Locked Pattern B + Fix B as a single `pump_loop` redesign in `crates/sm-infra/src/encode/windows_mft.rs` to fix vendor priming AND stop-signal starvation in one PR (~60–100 LOC), keeps `default = []` for blast-radius isolation, includes METransformDrainComplete arm and 2 new stop-deadline smoke tests, with explicit fallback to a future async-callback change if NO_WAIT proves vendor-incompatible at smoke time.
- **artifacts**:
  - `engram://sdd/hw-encoder-mft-rework/proposal` (this proposal, persisted)
  - `openspec/changes/hw-encoder-mft-rework/proposal.md` (this file)
- **next_recommended**: `sdd-spec` and `sdd-design` (can run in parallel — spec consumes Decisions #1–#9 and §8 for requirements/scenarios; design consumes Decisions #1, #2, #4, #5 plus OQ-1, OQ-2, OQ-4, OQ-5 for the loop body design).
- **risks**:
  - **R1**: NO_WAIT vendor-support unverified empirically on user GPU until apply-phase smoke. Decision #7 covers contingency but the round-trip cost of escalating to a new change is non-trivial.
  - **R2**: 18 (16 + 2) `#[ignore]` smoke tests means BLOCKED_ON_SMOKE will fire on verify; user must supply transcript before archive. Process risk only if user does not.
  - **R3**: Decision #3 (single PR) overrides the cached `auto-chain` delivery_strategy. The technical rationale is documented but the Review Workload Forecast in `sdd-tasks` will need to confirm the diff stays within the 400-line budget; if it slips, the orchestrator MUST re-trigger the Review Workload Guard rather than silently waving it through.
  - **R4**: METransformDrainComplete arm (Decision #4) is being added without prior empirical evidence that current code reaches `DrainComplete` cleanly — design phase must confirm the event is actually emitted by Microsoft / vendor MFTs after `COMMAND_DRAIN`, not just documented.
  - **R5**: Decision #8 (default stays `[]`) means this change does not, by itself, restore production HW acceleration — that requires a follow-up flip change. User expectations must be set correctly at archive time.
- **skill_resolution**: injected
