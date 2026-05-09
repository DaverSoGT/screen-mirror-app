# Proposal: hw-encoder-mft-single-frame-flush (Slice 3 — Intel QSV single-frame drain)

> Phase: SDD propose. Inputs: explore #701, sdd-init #186 v11, predecessors #632 (Slice 2 D1–D8 single-PR), #595 (Slice 1).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-single-frame-flush/proposal` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-09.
> Branch: `feat/hw-encoder-mft-single-frame-flush` (off master `daa9522`).

---

## 1. Inputs

| Source | Topic key / Path | Observation ID |
|--------|------------------|----------------|
| Exploration | `sdd/hw-encoder-mft-single-frame-flush/explore` | #701 |
| sdd-init project context (v11) | `sdd-init/screen-mirror-app` | #186 |
| Predecessor proposal (Slice 2) | `sdd/hw-encoder-mft-vendor-compat-rework/proposal` | #632 |
| Predecessor archive (Slice 2, PR #18) | `sdd/hw-encoder-mft-vendor-compat-rework/archive-report` | #699 |
| Engram-tracked convention | `.engram/ tracked` | #698 |
| Earlier predecessor (Slice 1) | `sdd/hw-encoder-mft-rework/proposal` | #595 |
| Tracing-before-explore convention | (project conv #592) | n/a |

---

## 2. Intent

After PR #18 closed Bug 1 Manifestation #3 (Intel QSV STREAM_CHANGE renegotiation), Host A
ships at **10/18 PASS** on the `windows_mft_encode` integration suite. The remaining 8 fails
are **single-frame timeouts** with a unifying root cause: Intel QSV does not emit
`METransformHaveOutput` until at least three frames are buffered in its pipeline, and the
`pump_loop`'s only DRAIN trigger fires on `frame_tx` channel-disconnect. Single-frame tests
keep `frame_tx` open while waiting on `recv_timeout`, so DRAIN is never sent and the vendor
sits silent waiting for more input that the test will never produce.

This slice closes those 8 tests by giving callers a way to **proactively flush** the encoder
without closing the channel. We add an inherent `pub fn flush(&self)` on
`WindowsMftH264Encoder` that arms an atomic `drain_pending` flag; the pump_loop checks the
flag after servicing NeedInput and fires `MFT_MESSAGE_COMMAND_DRAIN` — the same MS-documented
mechanism already validated in `mft_drain_after_channel_close_does_not_panic`. Success is
empirical: Host A reaches **18/18 PASS** (or 17/18 with one failure attributable to the
separately-tracked NVENC keyframe-flag issue, which lives on a different host) and Host B
shows **zero regressions** vs. the post-PR-#18 baseline of 16/18.

The change is intentionally narrow. sm-domain stays FROZEN (R14/R15 inherited from Slice 2);
`flush()` is **inherent only**, not added to the `VideoEncoder` trait. No `default` flip.
No production callers added — `flush()` is a test affordance for short streams; production
streaming usage is unchanged.

---

## 3. Scope

### IN scope

- New inherent method `pub fn flush(&self)` on `WindowsMftH264Encoder` in
  `crates/sm-infra/src/encode/windows_mft.rs`.
- New `drain_pending: AtomicBool` field on the encoder's shared state struct (`MftEncoderShared`
  per explore #701). `flush()` stores `true`; pump_loop checks and `compare_exchange`s back to
  `false` after firing `MFT_MESSAGE_COMMAND_DRAIN`.
- New pump_loop check site: AFTER the NeedInput inner servicing block, BEFORE returning to
  the top-of-loop `state.stop` poll. If the flag is armed, send `COMMAND_DRAIN` once.
- 8 specific test bodies in `crates/sm-infra/tests/windows_mft_encode.rs` get `enc.flush()`
  inserted between the last `frame_tx.send(...)` and the first `pkt_rx.recv_timeout(...)`
  for each per-expectation submit-recv pair.
- **Phase 0 empirical trace** on Host A (Intel QSV) recording 1-frame DRAIN behaviour. Saved
  to engram topic `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`. **REQUIRED before
  design lock** per `tracing-before-explore` convention #592.
- Cross-vendor regression check on Host B (NVENC) at smoke phase end.
- Strict TDD ordering: RED commit (8 tests gain `enc.flush()` calls — fails to compile on
  master because the method does not exist; in the same RED commit a stub `pub fn flush(&self) {}`
  is added so the tests COMPILE and FAIL the recv_timeout assertion); GREEN commit
  (drain-flag wiring + pump_loop check); optional fmt commit.

### OUT of scope

- **NVENC keyframe-flag detection** (Host B's 2 remaining fails: `mft_keyframe_flag_cleared_after_idr_emitted`,
  `mft_request_keyframe_marks_next_packet_as_keyframe`). Tracked as the separate v2 candidate
  `hw-encoder-mft-nvenc-keyframe-flag` (per init #186 v11). NOT included here.
- Adding `flush()` / `drain_and_wait()` to the `VideoEncoder` trait. sm-domain is FROZEN per
  R14/R15 (locked in D2 below).
- Flipping Cargo `default = ["hw-encoder"]`. Tracked separately as `hw-encoder-default-on-flip`.
- Production callers of `flush()`. The method is an end-of-burst affordance for short streams
  / tests. Streaming production paths continue to rely on continuous frame submission and the
  existing channel-disconnect DRAIN trigger.
- Refactoring the existing channel-disconnect DRAIN site to share code with the new
  flag-driven DRAIN site. Both call `mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)` — a
  one-liner. Extracting a helper for two callers adds risk for negative gain.
- A new RED test specifically for `flush()` semantics (e.g. `mft_flush_emits_pending_output`).
  The 8 currently-failing tests ARE the RED signal; once `flush()` exists they go GREEN.
  Adding a 9th flush-specific test is redundant.

---

## 4. Locked Decisions (D1–D8)

### D1 — Approach: Approach C (explicit inherent `flush()` method)

CHOSEN: **Approach C** from explore #701 §6 — `pub fn flush(&self)` inherent method on
`WindowsMftH264Encoder`, atomic flag mechanism, pump_loop checks after NeedInput servicing
and fires `MFT_MESSAGE_COMMAND_DRAIN`.

Rejected:
- **Approach A1** (pump_loop drains on RecvTimeoutError::Timeout): brittle — vendor cannot
  distinguish "test is slow producing next frame" from "test is done"; would drain mid-stream
  in production.
- **Approach A3** (tests close `frame_tx` early): structurally awkward for tests 6/7/8 which
  need multi-phase submit/recv. Forces every short-stream caller to drop the channel before
  waiting for output, which is surprising in production.
- **Approach B** (auto-pad with duplicate frames): rejected outright — produces wrong encoded
  bitstream (duplicate frames in output), breaks Annex-B / timestamp / keyframe-flag
  assertions. Treats symptom not cause.
- **Approach D** (vendor-conditional path): Slice 1 + Slice 2 already absorbed cross-vendor
  complexity; vendor-detection-by-CLSID is fragile and against the goal of a vendor-agnostic
  MFT wrapper.
- **Approach E** (A3 now, C later): requires a second slice to actually fix production
  ergonomics. C is the same LOC budget as A3 and ends up with the right API.

Rationale (cited from explore #701 §7):
1. Semantically correct — "I've submitted my last frame for this burst, please flush" is a
   well-defined request. DRAIN is the MS-documented mechanism.
2. No domain API change — inherent method, sm-domain FROZEN.
3. No bitstream corruption — only frames actually submitted are flushed.
4. Reuses already-validated DRAIN path (`mft_drain_after_channel_close_does_not_panic`).
5. Minimal test diff — one `enc.flush()` line per failing test.
6. Cross-vendor safe — DRAIN is part of the async MFT spec, NVENC honors it on
   channel-close already.

**Fallback (if Phase 0 reveals OQ-1 NEGATIVE — Intel QSV does NOT honor 1-frame DRAIN)**:
escalate to Approach E variant — keep `flush()` API surface but document that callers must
submit at least N frames before flushing where N is the empirically-determined minimum
priming count. Update the 8 failing tests to submit N-1 padding frames + 1 real frame +
`flush()`. If Phase 0 reveals N is unbounded (vendor never flushes <3 frames regardless of
DRAIN), escalate to a new exploration; this proposal becomes BLOCKED until vendor behaviour
is understood. **Design phase will not proceed without Phase 0 evidence.**

### D2 — sm-domain FROZEN (inherited R14/R15)

CHOSEN: `flush()` is an **inherent method on `WindowsMftH264Encoder` only**. The
`VideoEncoder` trait in `crates/sm-domain/src/encode.rs` is **not modified**. No new
`EncoderError` variants. No new trait methods.

Rejected: adding `fn flush(&mut self) -> Result<(), EncoderError>` to `VideoEncoder` —
breaks R14/R15 freeze, forces SW (`OpenH264Encoder`) and any future encoder adapter to
implement flush semantics that are MFT-specific.

Rationale: tests already `use sm_infra::encode::windows_mft::WindowsMftH264Encoder` directly
(per explore §6 Approach A2 note); calling `enc.flush()` on the concrete type is natural.
Production sm-domain callers do not need flush semantics — they stream continuously and rely
on channel-disconnect DRAIN. If a future production use case demands trait-level drain, that
is a separate RFC.

### D3 — Mechanism: `AtomicBool` flag, pump_loop checks after NeedInput

CHOSEN: single `drain_pending: AtomicBool` on `MftEncoderShared`. `flush()` calls
`drain_pending.store(true, Ordering::Release)`. The pump_loop, after the NeedInput inner
loop exits (whether via successful submission, RecvTimeoutError::Timeout, or
RecvTimeoutError::Disconnected), reads `drain_pending.compare_exchange(true, false,
Acquire, Relaxed)`; on success, fires `mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)`
exactly once.

Rejected:
- A bounded mpsc "command" channel (carrying a `DrainCommand` enum): adds a second channel
  the pump_loop must `select!` on; over-engineered for one boolean.
- A condvar / parking-lot signal: pump_loop already polls `state.stop` at top-of-loop; an
  atomic flag fits the same poll cadence with zero extra primitives.
- Storing in `EncodeState` rather than `MftEncoderShared`: the shared state is the existing
  Arc-shared struct; adding a field there is the minimal-surface change.

Rationale: matches the prevailing concurrency style in `windows_mft.rs` (top-of-loop atomic
poll for `stop`). `compare_exchange` ensures the flag is consumed exactly once per arm —
multiple `flush()` calls between pump-loop iterations collapse to one DRAIN. Memory ordering
follows the existing `state.stop` precedent (`Release`/`Acquire`).

**Channel-disconnect DRAIN path is NOT modified.** It remains the source of truth for
end-of-stream DRAIN. The new flag-driven DRAIN is additive — same `ProcessMessage` call,
different trigger.

### D4 — Test scope: 8 specific tests, no API consumers added

CHOSEN: exactly 8 test bodies modified in `crates/sm-infra/tests/windows_mft_encode.rs`:

1. `mft_encoded_packet_starts_with_annex_b_start_code`
2. `mft_first_real_packet_is_annex_b`
3. `mft_encoded_packet_timestamp_matches_capture_frame`
4. `mft_setup_uses_config_dimensions_when_nonzero`
5. `mft_setup_falls_back_when_config_dimensions_zero`
6. `mft_request_keyframe_marks_next_packet_as_keyframe` *
7. `mft_keyframe_flag_cleared_after_idr_emitted` *
8. `mft_set_bitrate_updates_encoder_without_restart` *

Each gets `enc.flush()` inserted **after the last `frame_tx.send(...)` for the relevant
expectation and before the corresponding `pkt_rx.recv_timeout(...)`**. For tests 6/7/8
(starred — multi-phase), the flush call site is design-deferred (see D6 / OQ-3) but lives
in the same edit budget.

No production callers of `flush()` are added. The `VideoEncoder` trait is unchanged. The
existing 10 passing tests are unchanged. Smoke tests (`mft_thirty_frame_smoke_*`,
`mft_drain_after_channel_close_does_not_panic`) are unchanged.

Rejected:
- Adding `enc.flush()` to all 18 tests defensively: pollutes passing tests, makes the
  intent unclear, and risks masking future regressions where the streaming path stops
  emitting output without explicit flush.
- Adding a new `mft_flush_emits_pending_output_after_one_frame` RED test: redundant — the
  8 existing failures ARE the RED signal.

Rationale: minimal, localised, reviewable. One line per test. The 8 tests align exactly
with the 8-fail forensic count from archive #699.

### D5 — Delivery: single PR (matches Slices 1, 2 precedent)

CHOSEN: **`delivery_strategy: single-pr`**. Branch `feat/hw-encoder-mft-single-frame-flush`.
Override session-cached `auto-chain` for this slice.

Forecast LOC:
- Production (`windows_mft.rs`): `AtomicBool` field (~1 LOC) + `pub fn flush()` (~5 LOC) +
  pump_loop drain-flag check site (~6 LOC) + imports / doc comment (~3 LOC) ≈ **~15 LOC**.
- Tests (`windows_mft_encode.rs`): ~1 LOC × 8 tests = **~8 LOC**.
- Total: **~23 LOC** (explore #701 §13 estimate ~35 LOC; this revision excludes a redundant
  helper extraction). **Well under the 400-line review budget.** No chained PRs needed.

Rejected:
- `auto-chain`: forecast LOC is 1/16 of the budget; chaining adds review overhead with no
  benefit.
- Two-PR split (production C2 in PR-A, tests C1/C3 in PR-B): RED-without-GREEN ships a
  broken main branch; GREEN-without-RED loses the TDD audit trail. Single PR with strict
  TDD commit ordering is the project's established pattern (Slice 1 #604, Slice 2 #699).

Rationale: matches Slice 1 (PR #16, ~70 LOC, single PR) and Slice 2 (PR #18, ~126 LOC,
single PR with explicit single-PR override per #632 D7) precedent. PR body will follow the
project's single-PR template (Summary / Commits / Gates / Test plan / SDD artifacts).

### D6 — Phase 0 gate before design lock

CHOSEN: an empirical 1-frame DRAIN trace on **Host A (Intel QSV)** is **REQUIRED before
the design phase begins**. Per `tracing-before-explore` convention #592, the design lock
cannot proceed without trace evidence saved to engram topic
`sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`.

Required Phase 0 experiment (per explore #701 §10):
1. On Host A, write or extend `smoke-trace.ps1` to invoke a minimal scenario: build a
   `WindowsMftH264Encoder`, `start()`, submit exactly 1 frame, drop `frame_tx`, then wait
   `recv_timeout(10s)` for output. Capture `RUST_LOG=sm_infra::encode=trace`.
2. Confirm the event sequence after the channel-disconnect DRAIN: does Intel QSV emit
   `METransformHaveOutput` (then a packet) before `METransformDrainComplete`, or does
   `DrainComplete` fire empty?
3. Repeat with 2-frame submission to bracket the threshold.
4. Confirm that `mft_drain_after_channel_close_does_not_panic` PASSES on master `daa9522`
   (resolves OQ-5 trivially).
5. Save the trace transcript + an interpretation summary (1-frame DRAIN works / partial /
   fails) to engram with the topic key above.

**Phase 0 outcomes determine design**:
- **GOOD** (1-frame DRAIN emits packet): Approach C goes ahead unchanged. Design phase
  proceeds.
- **PARTIAL** (1-frame DRAIN empty, 2-frame DRAIN emits): Approach C still ships; tests
  using single-frame-only patterns add 1 padding frame before `flush()`. Design phase
  documents the minimum-priming requirement.
- **BAD** (DRAIN never produces output regardless of frame count, or hangs): Approach C
  is BLOCKED. Re-explore needed. This proposal is not implemented as-is.

Spec phase **may run in parallel with Phase 0** (it locks "what" not "how"). Design phase
**cannot start without Phase 0 evidence**.

Rationale: the central empirical assumption (Intel QSV honors DRAIN with N=1) is unverified
in the explore phase. Locking design without evidence violates `tracing-before-explore`
convention #592 — which was reaffirmed in init #186 v11 specifically because Slice 1 and
Slice 2 both surfaced vendor surprises.

### D7 — Strict TDD: 3-commit sequence (RED → GREEN → polish)

CHOSEN: 3-commit chain on `feat/hw-encoder-mft-single-frame-flush`:

1. **C1 (RED)** — `test(infra): assert single-frame intel-qsv tests flush before recv`
   - Adds `enc.flush()` calls to the 8 failing tests.
   - Adds a stub `pub fn flush(&self) {}` (no-op) on `WindowsMftH264Encoder` so the tests
     COMPILE.
   - On master `daa9522`, the 8 tests STILL TIME OUT because the no-op flush does not
     actually fire DRAIN. RED signal preserved at the runtime level even though it has
     compiled.
   - This commit alone reproduces the 8 failures with the post-fix call shape.

2. **C2 (GREEN)** — `feat(infra): flush() drains MFT pipeline via COMMAND_DRAIN flag`
   - Replaces the no-op `flush()` body with `drain_pending.store(true, Release)`.
   - Adds `drain_pending: AtomicBool` field + `Default::default()` initialisation on
     `MftEncoderShared`.
   - Adds the pump_loop drain-flag check site after the NeedInput inner block, calling
     `mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)` once when armed.
   - Tests previously RED on C1 now GREEN.

3. **C3 (polish, optional)** — `style(infra): cargo fmt windows_mft.rs`
   - Only if `cargo fmt --check --all` reports diff after C2. Mirrors Slice 2's
     `0110da6` polish commit (#699 archive).

Rejected:
- Single GREEN commit (no separate RED): violates strict TDD; reviewer cannot see the
  RED→GREEN transition.
- Splitting C2 into "field + flush" and "pump_loop check": creates an intermediate state
  where `flush()` arms a flag that nothing reads, and pushes the GREEN signal to commit
  3 — confusing for reviewers and bisect.

Rationale: matches the Slice 2 pattern (C1 test ordering / C2 stream-change / C3 fmt per
#632 §7 and #699 archive). Strict TDD per init #186 v11. RED → GREEN is auditable in
`git log --oneline`.

### D8 — Carry-forward: NVENC keyframe-flag is OUT of scope

CHOSEN: this slice does NOT address the 2 NVENC keyframe-flag fails on Host B
(`mft_keyframe_flag_cleared_after_idr_emitted`, `mft_request_keyframe_marks_next_packet_as_keyframe`).
They are tracked as the separate v2 candidate `hw-encoder-mft-nvenc-keyframe-flag` (M)
per init #186 v11.

Rationale: the NVENC issue is a **different root cause** (NAL type 5 detection edge case
on forced IDR per init #186 v11). Bundling it into Slice 3 would expand the diff onto a
second host's adapter and double the smoke-cost. The `hw-encoder-default-on-flip` candidate
is already gated on BOTH this slice AND the NVENC slice (per init #186 v11 update). Closing
them sequentially is the correct order.

Boundary: Host B regression check at smoke-phase end MUST confirm that this slice does NOT
make the NVENC keyframe-flag tests fail in a *new* way. The expected baseline on Host B is
**16/18 PASS** (same as post-PR-#18 #696). 2 known fails remain → tracked elsewhere.

---

## 5. Open Questions Resolution

| OQ (from explore #701 §9) | Status | Resolution |
|---|---|---|
| **OQ-1**: Does Intel QSV honor `MFT_MESSAGE_COMMAND_DRAIN` when only 1 frame submitted? | **PHASE 0 GATE** | Empirical. Blocks design lock. See D6. Save evidence to `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`. |
| **OQ-2**: Inherent-only `flush()` vs. trait-level `drain_and_wait()`? | **LOCKED — inherent only** | D2. sm-domain FROZEN. Tests already `use ...WindowsMftH264Encoder` directly. |
| **OQ-3**: Where to call `flush()` in T6/T7/T8 multi-phase tests? | **DEFERRED to design** | Once Phase 0 reveals whether DRAIN is terminal (OQ-4), design picks: (a) one flush before final recv, (b) one flush per recv, or (c) reset between recvs. Cannot decide pre-Phase-0. |
| **OQ-4**: Is DRAIN terminal — can the encoder accept frames after `flush()`? | **DEFERRED to design (Phase 0 secondary)** | MS docs (per explore §5) state `ProcessInput` returns `MF_E_NOTACCEPTING` while draining and resumes after `DrainComplete`. The pump_loop's `DrainComplete` handler (lines 1110–1124, explore §4) ALREADY resets counters and continues; the comment says "Do NOT break". Design can lean on this. Phase 0 trace may corroborate empirically. If DRAIN turns out to be effectively terminal on Intel QSV (vendor refuses input post-`DrainComplete` until restart), tests 6/7/8 pattern requires re-design. |
| **OQ-5**: Does `mft_drain_after_channel_close_does_not_panic` currently pass on master `daa9522`? | **TRIVIAL — confirm in Phase 0** | Archive #699 says "10/18 PASS". The 8 named fails do not include this test → it passes. Phase 0 baseline run reconfirms. |

**Additional design-phase opens** (parallel to #632 §5 "OQ-A through OQ-D"):

- **OQ-A** — Doc comment on `flush()`: warn that DRAIN may be terminal until next
  `ProcessInput`-eligible state? Recommendation: yes, even if benign post-DrainComplete.
- **OQ-B** — `flush()` idempotence: with `compare_exchange(true, false)`, multiple
  `flush()` calls between pump iterations collapse to one DRAIN. After DRAIN fires, a
  subsequent `flush()` re-arms the flag and a subsequent pump cycle fires DRAIN again.
  Confirm this is desired (likely yes — supports multi-burst patterns).
- **OQ-C** — Pump-loop check site: after NeedInput inner block (proposed) vs. at top-of-loop
  (alongside `state.stop`). Both work. Recommendation: at the same site as the
  channel-disconnect DRAIN (post-NeedInput) for code locality.

---

## 6. Risks (with locked mitigation)

| Sev | Lik | Risk | Mitigation locked |
|---|---|---|---|
| **HIGH** | MED | Intel QSV does NOT honor 1-frame DRAIN — the load-bearing empirical assumption fails. | **D6 Phase 0 gate**. Design BLOCKED until trace evidence confirms. Fallback: padding-frame variant per D1. If vendor refuses any DRAIN-driven flush, escalate to re-explore. |
| MED | LOW | DRAIN is effectively terminal on Intel QSV — tests 6/7/8 cannot use mid-stream `flush()`. | OQ-3/OQ-4 deferred to design with Phase 0 evidence. If terminal, design restructures multi-phase tests (e.g. drop `frame_tx` for the final recv). |
| MED | LOW | sm-domain trait freeze pressure — reviewer or future requirement asks to add `flush()` to `VideoEncoder`. | D2 LOCKED inherent-only. Any trait change requires a separate proposal. |
| LOW | MED | NVENC regression on Host B — explicit DRAIN via `ProcessMessage(COMMAND_DRAIN, 0)` while the channel is still open is untested on NVENC (current channel-disconnect DRAIN is fine). | Cross-vendor regression smoke on Host B at smoke-phase end. Expected baseline 16/18 PASS. |
| LOW | LOW | `recv_timeout(3s)` / `recv_timeout(5s)` deadlines insufficient if post-DRAIN packet has added latency. | Phase 0 trace measures actual latency. If needed, design phase extends 1-2 deadlines to 10s. |
| LOW | LOW | `cargo fmt` reformats surrounding lines, inflating diff. | C3 polish commit; `cargo fmt --check --all` before merge. |
| LOW | LOW | Production caller accidentally calls `flush()` mid-stream. | Doc-comment warning (OQ-A). No production callers added in this slice. |
| LOW | LOW | Phase 0 gate delays the slice if Host A is unavailable. | User runs Phase 0 trace on Host A before sdd-design is launched. Documented in §7 below. |

---

## 7. Acceptance Criteria

1. **AC-1** (Host A primary): all 8 named single-frame tests PASS on `feat/hw-encoder-mft-single-frame-flush`.
   Currently 0/8 on master `daa9522`.
2. **AC-2** (Host A total): **18/18 PASS** target. **17/18 PASS** acceptable if the one
   remaining fail is attributable to a separately-tracked issue (NOT a regression introduced
   by this slice).
3. **AC-3** (Host B regression): **16/18 PASS** maintained or better. The 2 known NVENC
   keyframe-flag fails are tracked under `hw-encoder-mft-nvenc-keyframe-flag` and are NOT
   regressions.
4. **AC-4** (cross-vendor smoke): T-NEW-1 / T-NEW-2 (Slice 1 contract) GREEN cross-vendor.
   `mft_thirty_frame_smoke_emits_at_least_one_keyframe` PASSES on Host A and Host B.
5. **AC-5** (CI): `cargo nextest run --workspace` GREEN cross-platform. 7/7 quality gates
   GREEN on the merge SHA.
6. **AC-6** (Phase 0 evidence): trace transcript saved to engram topic
   `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace` BEFORE the design phase begins.
7. **AC-7** (LOC budget): final diff ≤ 50 lines (forecast ~23). Well under 400-line review
   budget; no chained PRs.

---

## 8. Definition of Done

- **Code**: `pub fn flush(&self)` on `WindowsMftH264Encoder` arms `drain_pending: AtomicBool`.
  pump_loop fires `MFT_MESSAGE_COMMAND_DRAIN` once per arm. `crates/sm-domain/src/encode.rs`
  UNCHANGED. `crates/sm-infra/Cargo.toml` UNCHANGED (`default = []` preserved).
- **Tests**: 8 test bodies have `enc.flush()` insertions per D4. Existing 10 passing tests
  unchanged. Smoke tests unchanged.
- **Phase 0**: trace transcript saved to engram BEFORE design lock.
- **Smoke**: Host A transcript saved at engram
  `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-a-branch`. Host B regression
  at `sdd/hw-encoder-mft-single-frame-flush/smoke-transcript-host-b-regression`.
- **CI**: 7/7 gates GREEN on merge SHA. `cargo clippy --all-targets --all-features --locked
  -- -D warnings` clean.
- **Docs**: `flush()` has a doc comment (`///`) explaining it fires a one-shot DRAIN, that
  DRAIN behaviour is vendor-dependent, that DRAIN may transiently make the encoder
  non-accepting, and that production streaming callers should NOT call `flush()`.
- **SDD chain**: archive-report updates init #186 v12 with row 19. SDD chain links section
  added.

---

## 9. Out-of-Band Coordination

- **User must run Phase 0 trace on Host A** (Intel QSV) BEFORE sdd-design is launched.
  Trace evidence saved to `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`. Design
  phase will refuse to proceed without it.
- **Smoke handoff at apply-phase end** — BLOCKED_ON_SMOKE rule per init #186 (project
  convention #582). The user runs smoke-trace.ps1 on Host A and Host B; results saved to
  engram before verify phase begins.
- **Branch hygiene**: branch off master `daa9522`. After merge, `gh pr merge --merge
  --delete-branch` followed by `git push origin --delete` if the remote retains the branch.

---

## 10. SDD Chain Anchors

- **Predecessor**: PR #18 (`hw-encoder-mft-vendor-compat-rework`, archive #699, master
  `daa9522`).
- **Earlier predecessors**: PR #16 (Slice 1, `hw-encoder-mft-rework`, archive #604), PR #14
  (`hardware-accel-encoder`, archive #579).
- **Successor**: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 4, M, separate slice). Both
  this slice AND the NVENC slice gate the `hw-encoder-default-on-flip` candidate per init
  #186 v11.
- **Engram topic_key chain**:
  - Explore #701: `sdd/hw-encoder-mft-single-frame-flush/explore`
  - Proposal (this): `sdd/hw-encoder-mft-single-frame-flush/proposal`
  - Phase 0 trace (PENDING, blocks design): `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`
  - Spec (next): `sdd/hw-encoder-mft-single-frame-flush/spec`
  - Design (next, BLOCKED on Phase 0): `sdd/hw-encoder-mft-single-frame-flush/design`
  - Tasks: `sdd/hw-encoder-mft-single-frame-flush/tasks`
  - Apply progress: `sdd/hw-encoder-mft-single-frame-flush/apply-progress`
  - Verify report: `sdd/hw-encoder-mft-single-frame-flush/verify-report`
  - Archive report: `sdd/hw-encoder-mft-single-frame-flush/archive-report`

---

## Result Contract

- **status**: `done` (PROPOSED — locked decisions complete)
- **executive_summary**: Locked 8 decisions (D1–D8). Approach C (inherent `flush()` on
  `WindowsMftH264Encoder` driving `MFT_MESSAGE_COMMAND_DRAIN` via an `AtomicBool` flag) is
  the recommended path; sm-domain stays FROZEN; 8 tests get one-line `enc.flush()`
  insertions; ~23 LOC forecast; single-PR delivery (overrides session `auto-chain`); strict
  TDD 3-commit chain. Phase 0 1-frame DRAIN trace on Host A is REQUIRED before design lock.
  NVENC keyframe-flag deferred to a separate slice.
- **artifacts**:
  - engram `sdd/hw-encoder-mft-single-frame-flush/proposal` (saved by mem_save)
  - file: `openspec/changes/hw-encoder-mft-single-frame-flush/proposal.md` (this file)
- **next_recommended**: `sdd-spec` (can run in parallel with Phase 0); `sdd-design` (BLOCKED
  on Phase 0 trace evidence at engram topic
  `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`).
- **locked_decisions**: D1 (Approach C), D2 (sm-domain FROZEN), D3 (AtomicBool flag), D4
  (8-test scope), D5 (single-PR), D6 (Phase 0 gate), D7 (3-commit TDD), D8 (NVENC
  keyframe-flag carry-forward).
- **open_phase_0_gate**: 1-frame DRAIN trace on Host A required, save to engram topic
  `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace`. Design CANNOT lock without it.
- **risks** (top 3):
  1. HIGH/MED — Intel QSV may not honor 1-frame DRAIN (Phase 0 gate; fallback path
     documented in D1).
  2. MED/LOW — DRAIN may be effectively terminal on Intel QSV (OQ-3/OQ-4 design-deferred).
  3. LOW/MED — NVENC regression risk on Host B with mid-stream DRAIN (cross-vendor smoke).
- **skill_resolution**: `injected`
