# Design: hw-encoder-mft-single-frame-flush (Slice 3 — Intel QSV single-frame drain)

> Phase: SDD design. Inputs: proposal #707, spec #708, explore #701, **Phase 0 trace #710**, predecessor design #634.
> Artifact store: hybrid (engram `sdd/hw-encoder-mft-single-frame-flush/design` + this file).
> Strict TDD: ACTIVE. Single PR. Branch `feat/hw-encoder-mft-single-frame-flush` (off master `daa9522`).
> Date: 2026-05-09.

---

## 1. Inputs

| Source | Topic key / Path | ID |
|--------|------------------|----|
| Proposal (D1–D8 LOCKED) | `sdd/hw-encoder-mft-single-frame-flush/proposal` | #707 |
| Spec (R1–R15, S1–S14) | `sdd/hw-encoder-mft-single-frame-flush/spec` | #708 |
| Exploration | `sdd/hw-encoder-mft-single-frame-flush/explore` | #701 |
| **Phase 0 trace (OQ-1 LOCKED YES, OQ-2 LIKELY-TERMINAL)** | `sdd/hw-encoder-mft-single-frame-flush/phase-0-trace` | **#710** |
| Predecessor design (DD format) | `sdd/hw-encoder-mft-vendor-compat-rework/design` | #634 |
| Predecessor apply | `sdd/hw-encoder-mft-vendor-compat-rework/apply-progress` | #636 |
| Project context (v11) | `sdd-init/screen-mirror-app` | #186 |
| Code | `crates/sm-infra/src/encode/windows_mft.rs` (struct line 83, `WindowsMftH264Encoder` line 124, `pump_loop` line 1044, channel-disconnect arm line 1285, `renegotiate_output_type` line 633, inherent `impl` block line 1565) | n/a |
| Tests | `crates/sm-infra/tests/windows_mft_encode.rs` (8 single-frame tests + 2 Phase 0 probes line 898+, 944+) | n/a |

**Phase 0 evidence summary (from #710)**:
- 1-frame DRAIN: GOOD. Latency drop→packet = 258 ms. Packet len=4425, is_keyframe=true.
- 2-frame DRAIN: GOOD. Latency = 238 ms.
- Event sequence: NeedInput → submit → drop(frame_tx) → ~12× duplicate `COMMAND_DRAIN` (pre-existing channel-disconnect spam, BENIGN per #710) → STREAM_CHANGE (Slice 2 handler renegotiates) → packet → DrainComplete×2 → stop signal exits.
- OQ-1 LOCKED YES; OQ-2 LIKELY-TERMINAL (no further packets observed post-DrainComplete in the probes — but probes immediately stopped after first packet, so terminality is *empirically inconclusive* beyond "single-shot is sufficient for the 8 tests").

---

## 2. Architecture Overview

Single-shot caller-driven flush via atomic flag, consumed by `pump_loop` after the NeedInput service block, firing exactly one `MFT_MESSAGE_COMMAND_DRAIN` per flag-set transition. Reuses the entire existing post-DRAIN flow: STREAM_CHANGE renegotiation (Slice 2 handler at `windows_mft.rs:1395`) → vendor emits buffered packet via `METransformHaveOutput` → `METransformDrainComplete` resets counters (existing handler at `windows_mft.rs:1110-1124`). No new threads, no condvars, no command channels, no `sm-domain` changes, no new `EncoderError` variants.

### Sequence diagram (Intel QSV single-frame, post-Phase-0 trace)

```
caller test                pump_loop                   IMFTransform (vendor)
─────────────              ───────────                 ──────────────────────
enc.start(rx, tx) ─────────▶ entering pump_loop
                             ◀──────── METransformNeedInput
frame_tx.send(f0) ─────────▶ ProcessInput(f0) Ok
                             ◀──────── METransformNeedInput   (vendor wants more)
enc.flush() ──────► drain_pending = true
                             [post-NeedInput inner-loop check]
                             swap(false) succeeds   (DD4)
                             ProcessMessage(COMMAND_DRAIN, 0) ──────────▶
                                                                          (~250 ms)
                             ◀──────── METransformHaveOutput
                             collect_output → STREAM_CHANGE (Slice 2 handler)
                             *output_format_known = None
                             renegotiate_output_type(...) Ok
                             ◀──────── METransformHaveOutput
                             collect_output → ProcessOutput Ok ──▶ pkt
                                                                  ──────▶ tx.try_send(pkt)
pkt_rx.recv_timeout ───────▶ packet arrives ✅
                             ◀──────── METransformDrainComplete
                             ni_count = 0; ho_count = 0   (existing handler)
enc.stop() ────────────────▶ state.stop = true → break → MFShutdown teardown
```

The flag-driven DRAIN is **additive** — the existing channel-disconnect DRAIN at line 1285-1294 stays untouched. Both paths converge on the SAME post-DRAIN flow that PR #18 (Slice 2) made bulletproof.

---

## 3. Design Decisions (DD1–DD10)

### DD1 — `flush()` API surface

**Decision**: Add `pub fn flush(&self)` as an INHERENT method on `WindowsMftH264Encoder`, in the existing `impl WindowsMftH264Encoder { … }` block at `windows_mft.rs:1565` (where `new_for_validation_test` already lives). Returns `()` (no `Result`). Takes `&self` (NOT `&mut self`) because the operation is a single atomic store on `Arc<MftEncoderShared>`.

**Rationale**:
- Spec R1 mandates inherent-only; `VideoEncoder` trait FROZEN (R7 / proposal D2).
- `&self` allows the test to call `enc.flush()` without holding a mutable borrow that would conflict with later `enc.stop()` (which needs `&mut`). Tests already cast through `WindowsMftH264Encoder` directly (`use sm_infra::encode::windows_mft::WindowsMftH264Encoder`).
- Returning `()` reflects the asynchronous nature: the actual DRAIN happens in `pump_loop`; failure to flush surfaces as `recv_timeout` outcome, not as a synchronous error. This matches `request_keyframe(&self)` precedent (line 248), which also returns `()` and stores into `Arc<MftEncoderShared>`.

**Alternative considered**: `pub fn flush(&self) -> Result<(), EncoderError>` propagating internal state errors. **Rejected**: there is no synchronous failure mode — the atomic store cannot fail, and any DRAIN failure happens asynchronously in `pump_loop`. Adding `Result` would be misleading API design.

**Risk**: callers may forget to handle the asynchronous nature. Mitigated by DD5 doc comment.

---

### DD2 — `drain_pending: AtomicBool` field on `MftEncoderShared`

**Decision**: Add a new field `drain_pending: AtomicBool` to the existing `MftEncoderShared` struct at `windows_mft.rs:83`. Initialised to `false` in `MftEncoderShared::default()` (line 94). `flush()` issues `state.drain_pending.store(true, Ordering::Release)`; `pump_loop` consumes via `state.drain_pending.swap(false, Ordering::AcqRel)` (DD4). No new synchronization primitives.

**Rationale**:
- Matches existing pattern: `keyframe_pending: AtomicBool`, `pending_bitrate: AtomicU32`, `stop: AtomicBool` all live on `MftEncoderShared` and are shared between `WindowsMftH264Encoder` (caller side) and `pump_loop` (encoder thread) via `Arc`.
- Spec R2 explicitly mandates this field name and location.
- `Release/Acquire` pairing is the project default (see `state.stop.store(_, Release)` at line 241; loaded with `Acquire` at line 1088).

**Alternative considered**: `AtomicU32` flush-counter for richer semantics (e.g. "how many flush requests issued"). **Rejected**: spec S2 only requires "one DRAIN per flag-set transition between pump iterations"; a counter adds complexity without buying anything for the 8 tests.

**Risk**: NONE. Atomic flag is the simplest possible mechanism and matches three existing precedents on the same struct.

---

### DD3 — Pump-loop drain-flag check site

**Decision**: Insert the drain-flag check **AFTER** the NeedInput inner `while ni_count > 0 { … }` loop (currently ends at `windows_mft.rs:1297`) and **BEFORE** the idle-sleep block at line 1300. Rationale-anchored placement: `pump_loop` event-poll → HaveOutput service (line 1162) → NeedInput service (line 1214) → **NEW: drain-flag check** → idle sleep → heartbeat → top-of-loop.

The check uses `swap(false, Ordering::AcqRel)` (DD4). On `swap` returning `true` (flag was set), call `unsafe { let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0); }` exactly once and emit `tracing::info!("pump_loop: explicit flush() — sending COMMAND_DRAIN")`. **Do NOT break** out of the outer loop — control falls through to the existing idle sleep + top-of-loop, where the next iteration polls `GetEvent(NO_WAIT)` and picks up the resulting `METransformHaveOutput` / `METransformDrainComplete` events.

**Rationale**:
- Code locality with the existing channel-disconnect DRAIN site at line 1287-1294 (both fire `ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)`).
- Placing AFTER NeedInput servicing avoids interrupting active input flow: any frames still in `frame_tx` get submitted before the DRAIN. This is critical for spec S5 (multi-burst pattern) and for T6/T7/T8 where the test pattern is "submit frame, recv packet" with the DRAIN coming at the end.
- Placing BEFORE the idle sleep means the DRAIN fires immediately when the test calls `enc.flush()` after `frame_tx.send(...)` — no ≥1 ms idle-sleep latency added. Phase 0 #710 measured 250 ms total drop→packet; we don't want to add even 1 ms of avoidable overhead.
- Falls into the existing post-DRAIN flow validated by Phase 0 trace #710 and Slice 2 (PR #18) STREAM_CHANGE handler.

**Alternative considered**: Place at TOP of loop (before event poll). **Rejected**: would fire DRAIN before any pending NeedInput credit is serviced, producing partial flushes when the test calls `flush()` rapidly after `send()`. Phase 0 #710 trace shows the natural ordering is `submit → drain → vendor emits` — placing the check post-NeedInput preserves this.

**Risk**: LOW. The placement is deterministic (1 location, well-bounded by the surrounding service blocks).

---

### DD4 — DRAIN once-per-flag-set via `swap(false, AcqRel)`

**Decision**: Consume the flag with `state.drain_pending.swap(false, Ordering::AcqRel)`. Fire DRAIN only when `swap` returns `true`. This guarantees:
- Multiple `flush()` calls between two `pump_loop` iterations collapse to ONE DRAIN (the last `store(true)` wins; first `swap` fires DRAIN; subsequent `swap` returns `false` no-op).
- Subsequent `flush()` AFTER a DRAIN cycle re-arms the flag for the next cycle (spec S5 idempotent re-arm).

Use `swap`, NOT `compare_exchange(true, false)`: spec R3 mentions `compare_exchange` but `swap` is functionally equivalent here (both atomically read-then-write-false) and is one-line simpler. Document this minor deviation in the commit message body.

**Rationale**:
- `swap` returns the previous value atomically; if it was `false`, no work to do (no-op); if `true`, fire DRAIN.
- `AcqRel` ordering: `Acquire` half synchronises with the caller's `Release` store in `flush()`; `Release` half ensures any subsequent observations (DrainComplete handler reads of `ni_count`/`ho_count`) happen-after the `swap`.
- Idempotence is structurally guaranteed: after `swap` returns `true`, the flag is `false` until another `flush()` arms it.

**Alternative considered**: `compare_exchange(true, false, Acquire, Relaxed)` per spec R3 wording. **Resolution**: behaviourally identical for this 2-state flag; design adopts `swap` as the more idiomatic primitive. Spec R3 wording is non-binding for this micro-decision.

**Risk**: NONE. Atomic semantics guaranteed by the standard.

---

### DD5 — `flush()` doc comment (R8 wording, OQ-A locked)

**Decision**: The `flush()` method receives a 14-line `///` doc comment with these clauses:

```rust
/// Signal end-of-burst to the encoder pipeline. Triggers a single
/// `MFT_MESSAGE_COMMAND_DRAIN` from `pump_loop` on the next iteration after
/// the NeedInput service block, forcing the vendor to emit any output buffered
/// for frames already submitted.
///
/// **Single-shot semantics.** Once DRAIN completes the vendor enters a
/// transient non-accepting state until `METransformDrainComplete` fires; on
/// some vendors (Intel QSV per Phase 0 trace) the encoder is effectively
/// terminal for further output. Callers MUST treat `flush()` as the LAST
/// signal of an encoding burst and call `stop()` afterward.
///
/// **Async.** Returns immediately. The actual DRAIN happens on the encoder
/// thread; failure surfaces via `recv_timeout` on the packet channel.
///
/// **Concurrency-safe.** Implemented via `Arc<AtomicBool>`; safe to call
/// from any thread. Multiple calls between pump iterations collapse to one
/// DRAIN.
///
/// **Latency.** Empirically ~250 ms drop→packet on Intel QSV (Phase 0 trace
/// 2026-05-09 Host A `daa9522`).
///
/// **Production callers MUST NOT call this method.** It is a test affordance
/// for short-stream patterns. Production streaming code relies on continuous
/// frame submission + channel-disconnect DRAIN at shutdown.
```

**Rationale**: Spec R8 mandates four clauses — one-shot, vendor-dependent, transient non-accepting, production-streaming-MUST-NOT-call. This wording covers all four plus the empirical latency anchor for future debugging.

**Alternative considered**: Shorter 4-line doc comment. **Rejected**: the four clauses are load-bearing for preventing accidental production misuse; brevity is misuse-prone.

**Risk**: LOW (cosmetic).

---

### DD6 — Test placement strategy for T1–T5 (single-frame) and T6/T7/T8 (multi-phase)

This is the most architecturally consequential decision in the slice. Phase 0 #710 LOCKED OQ-2 as **LIKELY-TERMINAL** ("after METransformDrainComplete the encoder appears to be in a one-shot state — no further packets emitted"). That means a mid-test `flush()` cannot be reused for a second burst on the same encoder lifecycle.

#### T1–T5 (single-frame, single-phase) — TRIVIAL

Each test submits exactly 1 frame and waits for exactly 1 packet. Insert `enc.flush();` between the last `frame_tx.send(...)` and the `pkt_rx.recv_timeout(...)`. Exact line refs (master `daa9522`, will shift +1 line each):

| # | Test | Insert AFTER line | Insert BEFORE line | Diff |
|---|------|-------------------|-------------------|------|
| T1 | `mft_encoded_packet_starts_with_annex_b_start_code` | 159 (`.expect("frame_tx should be open")`) | 161 (`let pkt = pkt_rx`) | `+        enc.flush();` |
| T2 | `mft_first_real_packet_is_annex_b` | 527 | 529 | same |
| T3 | `mft_encoded_packet_timestamp_matches_capture_frame` | 285 | 287 | same |
| T4 | `mft_setup_uses_config_dimensions_when_nonzero` | 602 | 604 | same |
| T5 | `mft_setup_falls_back_when_config_dimensions_zero` | 632 | 634 | same |

5 single-line insertions = 5 LOC.

#### T6 / T7 / T8 (multi-phase) — RESTRUCTURED to single-shot

Per Phase 0 #710 directive ("place flush() AT END of multi-phase sequences"), each test is restructured from "`send` / `recv` / `send` / `recv` / …" to "`send all frames` / `flush()` / `recv all packets in order`". This works because the existing `pkt_rx` is a `mpsc::sync_channel(16)` (line 317 `(frame_tx, frame_rx) = mpsc::sync_channel(16)` — capacity 16 ≫ 5 frames), so the encoder thread can buffer all post-DRAIN packets without backpressure.

**T6 — `mft_request_keyframe_marks_next_packet_as_keyframe`** (current lines 306–382):
- Original: send(0) / recv (initial IDR) / send(1)/recv / send(2)/recv / send(3)/recv / `request_keyframe()` / send(4) / recv (forced IDR with assertions).
- New: `request_keyframe()` BEFORE send(4) (already the case, line 347) → submit ALL 5 frames in order: send(0), send(1), send(2), send(3), `enc.request_keyframe()`, send(4) → `enc.flush();` → drain `pkt_rx` 5 times, applying assertions to packet #5 (the forced IDR).
- **Semantic preservation**: `request_keyframe()` sets `state.keyframe_pending` BEFORE submit_frame(4) is called by `pump_loop`; the IDR-marking happens at `submit_frame()` not at recv. So packet #5 is still the forced IDR. ✅
- **Diff size**: ~10 LOC (reorder + drain loop).

**T7 — `mft_keyframe_flag_cleared_after_idr_emitted`** (current lines 388–441):
- Original: send(0) / recv / send(1)/recv / send(2)/recv / `request_keyframe()` / send(3) / recv (forced IDR) / send(4) / recv (after_idr, assert NOT keyframe).
- New: send(0), send(1), send(2), `enc.request_keyframe()`, send(3), send(4) → `enc.flush();` → drain 5 packets → assertions on packet #4 (forced IDR) and packet #5 (after_idr).
- **Semantic preservation**: same argument as T6.
- **Diff size**: ~10 LOC.

**T8 — `mft_set_bitrate_updates_encoder_without_restart`** (current lines 448–500):
- Original: send/recv 3× at 4 Mbps → `set_bitrate(8_000_000)` → send/recv 3× at 8 Mbps. **CRITICAL**: this test asserts `set_bitrate` succeeds AND that the encoder thread is alive AFTER the bitrate change. With a single-shot `flush()` placed at the END, the bitrate-change occurs DURING active encoding (mid-stream), which is the exact production-relevant invariant T8 was written to verify.
- New: send(0), send(1), send(2) → `enc.set_bitrate(8_000_000)` (mid-stream bitrate change, NO flush yet) → send(3), send(4), send(5) → `enc.flush();` → drain 6 packets. Assertions: 6 packets received; `set_bitrate` returns `Ok(())`; channel not disconnected (implicit — sends after `set_bitrate` succeed).
- **Semantic preservation**: the test's invariant is "set_bitrate works without restart while the encoder is live". The new pattern still exercises this — sends 4–6 happen AFTER `set_bitrate`, and they all enter the same encoder. The only thing lost is "recv between bitrate phases" assertions, which T8 didn't actually have anyway (only the post-bitrate sends had per-iteration recvs).
- **Diff size**: ~12 LOC.

**Total T6/T7/T8 diff**: ~32 LOC (restructure) + 3 × `enc.flush();` insertions.

#### SCOPE WARNING (documented for verify phase)

T6/T7/T8 restructure changes the **submission cadence** but preserves the **assertion contract** (packet #N has property P). If verify-phase manual smoke on Host A reveals that Intel QSV emits OUT-OF-ORDER packets after a single late DRAIN (e.g. the forced-IDR packet doesn't arrive at position #5), the FALLBACK is to split each multi-phase test into separate `#[test]` functions, one per logical phase, each with its own `enc.flush()` and `enc.stop()` cycle. This fallback is explicit out-of-scope for this slice unless smoke fails — record as a separate change `hw-encoder-mft-multi-phase-test-split` if needed.

---

### DD7 — TDD commit sequence (3 commits, matches Slice 2 precedent)

**Decision**: Three commits on `feat/hw-encoder-mft-single-frame-flush`:

#### C1 — RED: `test(infra): assert single-frame intel-qsv tests flush before recv`
- **Production change**: Add no-op stub `pub fn flush(&self) {}` (3 LOC) inside `impl WindowsMftH264Encoder { … }` block at `windows_mft.rs:1565`. NO `drain_pending` field, NO `pump_loop` change.
- **Test change**: Apply DD6 placement to all 8 tests (5 single-line insertions for T1–T5 + 3 restructured bodies for T6/T7/T8). Total ~37 test LOC.
- **CI**: `cargo build --workspace --locked` clean. `cargo nextest run --workspace` = 611 passed, 19 skipped (unchanged baseline). The 8 ignored HW tests don't run on CI.
- **RED preservation on Host A**: stub does nothing → 8 tests still TIMEOUT at `recv_timeout`. RED at runtime level. User confirms manually.

#### C2 — GREEN: `feat(infra): flush() drains MFT pipeline via COMMAND_DRAIN flag`
- **Production change**: Replace stub body. Add `drain_pending: AtomicBool` field on `MftEncoderShared` (1 LOC + 1 LOC default-init). Real `flush()` body: `self.state.drain_pending.store(true, Ordering::Release);` (1 LOC). pump_loop drain-check insertion at the post-NeedInput site (~6 LOC including `tracing::info!`). Doc comment per DD5 (~14 LOC). Total ~25 prod LOC.
- **CI**: `cargo build --features hw-encoder` clean. `cargo nextest run --workspace` = 611 still passes (HW tests skipped). `cargo clippy --features hw-encoder --tests -- -D warnings` clean.
- **GREEN on Host A**: 8 tests now PASS. Confirmed manually after BLOCKED_ON_SMOKE handoff.

#### C3 — POLISH (optional): `style(infra): cargo fmt for flush handler`
- Only if `cargo fmt --check --all` reports diff after C2. Mirrors Slice 2 commit `0110da6`.
- Optional one-liner cleanup: fix the channel-disconnect DRAIN spam (DD8) — see DD8 for decision.

**Rationale**: 3-commit pattern matches Slice 2 (#634 Section 5, #636 apply-progress). Strict TDD per init #186 v11 mandates RED before GREEN. Stub-based RED preserves runtime failure (timeouts) without compile breakage — same pattern as Slice 1 and Slice 2.

**Alternative considered**: 2-commit (RED+GREEN merged into one). **Rejected**: violates strict TDD audit trail; Slice 2 explicitly used 3-commit chain (#636) and that pattern is now project precedent.

---

### DD8 — Channel-disconnect DRAIN spam (Phase 0 discovery #710)

**Decision**: **DEFER** the channel-disconnect DRAIN spam fix to a separate cleanup change. Document the discovery in Slice 3's commit message body (`feat(infra): flush() drains MFT pipeline via COMMAND_DRAIN flag`) but do NOT modify the existing arm at `windows_mft.rs:1285-1294`.

**Background (from Phase 0 #710)**: The trace shows that when the channel is dropped, `pump_loop` fires `ProcessMessage(COMMAND_DRAIN, 0)` ~12× over ~7 ms before any HaveOutput event arrives. Root cause inspection: the `Disconnected` arm at line 1285 fires DRAIN then `break`s out of the inner `while ni_count > 0` loop. But the OUTER pump_loop iteration continues, polls `GetEvent(NO_WAIT)`, and may receive ANOTHER `METransformNeedInput` event (vendor was already in flight). That NeedInput increments `ni_count` again; the inner loop re-enters; `recv_timeout` again returns `Disconnected` (channel still closed); fires DRAIN again; break; and so on until the vendor stops emitting NeedInput. Vendor ignores duplicate DRAINs (the trace confirms behaviour is correct), so this is BENIGN noise — just trace spam.

**Why defer**:
1. Slice 3's stated scope (proposal #707 §3 OUT-of-scope) explicitly excludes "Refactoring channel-disconnect DRAIN site to share code with new flag-driven DRAIN site." Modifying it now expands the diff and risks regressing the post-disconnect drain path used by `mft_drain_after_channel_close_does_not_panic` and the 30-frame smoke test.
2. The flag-driven DRAIN (DD3/DD4) uses `swap(false)` which is structurally guarded against this spam — only ONE DRAIN per `flush()` call. The flag mechanism does NOT inherit the channel-disconnect bug.
3. The spam is observed only in shutdown sequences where verbosity is acceptable; no production user-facing impact.

**Future cleanup**: A separate, ~3-LOC change can add `let mut disconnect_drain_sent = false;` as a local in `pump_loop` and gate the `Disconnected` arm's `ProcessMessage` on `if !disconnect_drain_sent`. Tracked as v2 candidate `hw-encoder-mft-disconnect-drain-once` (XS, cleanup-only).

**Risk**: NONE for Slice 3. The pre-existing benign spam stays exactly as it is.

---

### DD9 — `flush()` visibility: always-`pub` (no `#[cfg(test)]` gate)

**Decision**: `pub fn flush(&self)` is unconditionally public. NOT gated behind `#[cfg(any(test, debug_assertions))]`.

**Rationale**:
- Spec R1 says "MUST expose `pub fn flush(&self)` as an inherent method" — the wording is unconditional.
- The `crates/sm-infra/tests/windows_mft_encode.rs` file is an **integration test** (`tests/` directory), which is compiled as a SEPARATE crate. `#[cfg(test)]` items inside `windows_mft.rs` are NOT visible to integration tests — they're only visible to unit tests inside `mod tests {}`. So gating with `#[cfg(test)]` would BREAK the 8 integration tests at compile time.
- `#[cfg(any(test, debug_assertions))]` would expose the method in debug builds only; release builds would lose the API. This is surprising and creates a debug-vs-release skew that violates project convention (no behavioural diff between profiles).
- The doc comment (DD5) explicitly warns production callers to NOT call this method. Documentation, not visibility, is the correct enforcement layer here. This matches the `request_keyframe(&self)` precedent (line 248): a public test-friendly method that production code uses sparingly under documented constraints.

**Alternative considered**: `#[doc(hidden)]` + always-public. **Rejected**: doc-hidden hides from rustdoc but the symbol is still part of the API surface; the warning is more useful when visible.

**Risk**: LOW — production code does not currently call `flush()`, and the doc comment establishes the contract.

---

### DD10 — Phase 0 probe test retention

**Decision**: KEEP both `mft_one_frame_drain_probe_phase_0` (line 898) and `mft_two_frame_drain_probe_phase_0` (line 944) in the test file as `#[ignore]`-gated regression tests. They become permanent in-code documentation of the empirical contract: "Intel QSV honors 1-frame and 2-frame DRAIN with ~250 ms latency."

**Rationale**:
- Phase 0 trace #710 describes them as "observation-only" probes added during the design phase — but they double as future regression guards. If a Windows update or vendor driver change ever breaks the 1-frame DRAIN behaviour, these probes will fail audibly.
- CI cost = 0 (both `#[ignore]`d, only run manually with `--run-ignored=all`).
- File-size cost = negligible (~70 LOC of test code already in place).
- Removing them would erase the empirical evidence trail that justified Approach C — bad SDD hygiene.

**Alternative considered**: Remove probes; rely on engram #710 for evidence. **Rejected**: code-level evidence is more robust against engram backend changes.

**Risk**: NONE.

---

## 4. Pseudocode

### 4.1 — `MftEncoderShared` extension (`windows_mft.rs:83`)

```rust
struct MftEncoderShared {
    keyframe_pending: AtomicBool,
    pending_bitrate: AtomicU32,
    dropped: AtomicU64,
    stop: AtomicBool,
    /// Set by `flush()`; consumed once per pump_loop iteration after NeedInput
    /// servicing. When `swap(false)` returns true, pump_loop fires
    /// MFT_MESSAGE_COMMAND_DRAIN exactly once. (DD2, DD4)
    drain_pending: AtomicBool,
}

impl Default for MftEncoderShared {
    fn default() -> Self {
        Self {
            keyframe_pending: AtomicBool::new(false),
            pending_bitrate: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            drain_pending: AtomicBool::new(false), // NEW
        }
    }
}
```

### 4.2 — Inherent `flush()` method (`windows_mft.rs:1565+`)

```rust
impl WindowsMftH264Encoder {
    // ... existing new_for_validation_test() ...

    /// Signal end-of-burst to the encoder pipeline. (full doc comment per DD5)
    pub fn flush(&self) {
        self.state.drain_pending.store(true, Ordering::Release);
    }
}
```

### 4.3 — Pump-loop drain-flag check site (`windows_mft.rs:~1297`, between NeedInput while-loop and idle sleep)

```rust
// ── Service NeedInput (submit frames) ─────────────────────────────────────
while ni_count > 0 {
    // ... existing body unchanged ...
}

// ── Caller-driven DRAIN (new — DD3/DD4) ──────────────────────────────────
// Consume drain_pending once per iteration. Multiple flush() calls between
// pump iterations collapse to one DRAIN. Falls into existing post-DRAIN flow:
// STREAM_CHANGE → renegotiate (Slice 2) → packet → DrainComplete → counters reset.
if state.drain_pending.swap(false, Ordering::AcqRel) {
    tracing::info!("pump_loop: explicit flush() — sending COMMAND_DRAIN");
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    }
}

// ── Idle sleep — avoid busy-wait when nothing happened ──
if !event_opt && ni_count == 0 && ho_count == 0 {
    std::thread::sleep(POLLING_SLEEP);
}
```

### 4.4 — Test stub (C1 RED commit)

```rust
impl WindowsMftH264Encoder {
    // ... existing ...

    /// Stub. C2 replaces body with real implementation.
    pub fn flush(&self) {
        // intentionally empty — RED commit; tests time out at recv_timeout.
    }
}
```

---

## 5. Test Modifications

### 5.1 — Single-frame tests (T1–T5): one-line insertion

For EACH of T1–T5, insert `enc.flush();` after the last `frame_tx.send(...).expect(...)` and before the `pkt_rx.recv_timeout(...)`. Pattern:

```rust
frame_tx
    .send(make_synthetic_frame(640, 480, 0))
    .expect("frame_tx should be open");
enc.flush(); // NEW (Slice 3)
let pkt = pkt_rx
    .recv_timeout(Duration::from_secs(5))
    .expect("encoded packet should arrive within 5 s");
```

| Test | File line ranges (master `daa9522`) |
|------|-------------------------------------|
| T1 `mft_encoded_packet_starts_with_annex_b_start_code` | insert at ~line 160 |
| T2 `mft_first_real_packet_is_annex_b` | insert at ~line 528 |
| T3 `mft_encoded_packet_timestamp_matches_capture_frame` | insert at ~line 286 |
| T4 `mft_setup_uses_config_dimensions_when_nonzero` | insert at ~line 603 |
| T5 `mft_setup_falls_back_when_config_dimensions_zero` | insert at ~line 633 |

### 5.2 — Multi-phase tests (T6/T7/T8): restructure to single-shot

#### T6 — `mft_request_keyframe_marks_next_packet_as_keyframe`

```rust
// AFTER restructure:
let send_frame = |i: u64| { frame_tx.send(...).expect(...); };

// Submit ALL frames in submission order. request_keyframe() before send(4)
// arms the IDR for that specific frame (state.keyframe_pending consumed by
// submit_frame in pump_loop).
send_frame(0);
send_frame(1);
send_frame(2);
send_frame(3);
enc.request_keyframe();
send_frame(4);

enc.flush(); // single-shot DRAIN

// Drain all 5 packets in order; apply assertion to packet #5 (forced IDR).
let recv_pkt = || pkt_rx.recv_timeout(Duration::from_secs(3))
    .expect("packet should arrive within 3 s");

let _initial_idr = recv_pkt();           // packet #1: initial IDR
let _ = recv_pkt();                       // packet #2
let _ = recv_pkt();                       // packet #3
let _ = recv_pkt();                       // packet #4
let forced_idr = recv_pkt();              // packet #5: forced IDR

assert!(forced_idr.is_keyframe, "...");
assert_eq!(&forced_idr.data[..4], &[0,0,0,1], "...");
assert_eq!(forced_idr.data[4] & 0x1F, 0x07, "...");

drop(frame_tx);
enc.stop().expect("stop should succeed");
```

#### T7 — `mft_keyframe_flag_cleared_after_idr_emitted`

```rust
send_frame(0); send_frame(1); send_frame(2);
enc.request_keyframe();
send_frame(3);
send_frame(4);

enc.flush();

let _ = recv_pkt();             // #1: initial IDR
let _ = recv_pkt();             // #2
let _ = recv_pkt();             // #3
let forced = recv_pkt();        // #4: forced IDR
let after_idr = recv_pkt();     // #5: post-IDR P-frame

assert!(forced.is_keyframe, "forced IDR must be a keyframe");
assert!(!after_idr.is_keyframe, "packet after forced IDR must have is_keyframe == false");
```

#### T8 — `mft_set_bitrate_updates_encoder_without_restart`

```rust
// Submit 3 frames at 4 Mbps (no flush yet — encoder is mid-stream).
for i in 0..3u64 {
    frame_tx.send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
        .expect("frame_tx open");
}

// Mid-stream bitrate change. Critical: pump_loop has NOT been DRAINed yet,
// so this exercises the production-relevant invariant (live bitrate update).
let result = enc.set_bitrate(8_000_000);
assert!(result.is_ok(), "set_bitrate(8_000_000) should return Ok(())");

// Submit 3 more frames at 8 Mbps (channel must still be open — encoder live).
for i in 3..6u64 {
    frame_tx.send(make_synthetic_frame(WIDTH, HEIGHT, i * 33))
        .expect("frame_tx must still be open after set_bitrate");
}

enc.flush(); // single-shot DRAIN at end

// Drain all 6 packets.
let mut received = 0usize;
for _ in 0..6 {
    let pkt = pkt_rx.recv_timeout(Duration::from_secs(3))
        .expect("packet should arrive after bitrate update");
    received += 1;
    println!("[T8.2] pkt {}: is_keyframe={} len={}", received, pkt.is_keyframe, pkt.data.len());
}
assert_eq!(received, 6, "expected all 6 packets to arrive");

drop(frame_tx);
enc.stop().expect("stop should succeed");
```

---

## 6. Forecast LOC

| File | Insertions | Deletions | Subject |
|------|-----------|-----------|---------|
| `crates/sm-infra/src/encode/windows_mft.rs` | ~26 | ~1 | `drain_pending` field (1) + Default init (1) + `flush()` method with doc (16) + pump_loop check (8) |
| `crates/sm-infra/tests/windows_mft_encode.rs` | ~37 | ~25 | 5× single-line `enc.flush()` (T1–T5) + 3 restructured test bodies (T6/T7/T8 net diff) |
| **Total** | **~63** | **~26** | **Net ~37 LOC delta — well under 400-line review budget; under 50-LOC AC-7/AC-8 ceiling** |

Phase 0 probes already in place (~70 LOC, retained per DD10) are NOT counted — they pre-exist the design phase.

---

## 7. Risk Re-assessment (vs spec #708 §7)

| Risk | Original sev/lik | Post-Phase 0 | New sev/lik | Notes |
|------|------------------|--------------|-------------|-------|
| OQ-1: Intel QSV honors 1F DRAIN? | HIGH/MED | LOCKED YES (#710) | RESOLVED | 258 ms drop→packet, 4425 B IDR |
| OQ-2: DRAIN terminal? | MED/LOW | LIKELY-TERMINAL | MED/LOW | Empirically inconclusive beyond "single-shot is enough"; DD6 designs around it via end-of-test placement |
| OQ-3: T6/T7/T8 placement | MED/LOW | DESIGNED | LOW/LOW | DD6 restructure preserves assertion contract; **SCOPE WARNING** if Host A reveals out-of-order packet emission |
| NVENC regression | LOW/MED | unchanged | LOW/LOW | DD3 site only fires DRAIN when `flush()` is called; existing 10 NVENC-passing tests + smoke don't call `flush()` → zero exposure |
| recv_timeout deadlines | LOW/LOW | OK | LOW/LOW | Phase 0 measured 258 ms; tests use 3–5 s; massive headroom |
| flush+disconnect race (S8) | LOW/LOW | unchanged | LOW/LOW | Both paths fire identical `ProcessMessage(COMMAND_DRAIN, 0)` — vendor ignores duplicates per #710 |
| Channel-disconnect spam | unknown | DISCOVERED (#710), benign | LOW/LOW | DD8 defers; benign per Phase 0 — vendor ignores duplicates |
| Multi-phase semantic drift | n/a | NEW | LOW/MED | DD6 restructure changes submission cadence; assertion contract preserved BUT relies on post-DRAIN packet ordering being FIFO |

**Top residual risk**: T6/T7/T8 post-DRAIN packet ordering. If Intel QSV emits the forced-IDR packet OUT of FIFO order (e.g. as packet #3 instead of packet #5), DD6 assertions fail. Mitigation: BLOCKED_ON_SMOKE handoff per init #186; smoke transcript reveals ordering; fallback documented in DD6 SCOPE WARNING.

---

## 8. Phase 0 Probe Test Retention (DD10 confirmation)

| Probe | Line | Status | Purpose |
|-------|------|--------|---------|
| `mft_one_frame_drain_probe_phase_0` | 898 | KEEP `#[ignore]`-gated | Regression guard for OQ-1 LOCKED YES |
| `mft_two_frame_drain_probe_phase_0` | 944 | KEEP `#[ignore]`-gated | Regression guard for 2-frame DRAIN parity |

Both stay in `crates/sm-infra/tests/windows_mft_encode.rs`. CI cost = 0. Run manually via `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode mft_one_frame_drain_probe_phase_0 --run-ignored=all`.

---

## 9. Out of Scope (carried from #707 §3)

- NVENC keyframe-flag detection (Slice 4 candidate `hw-encoder-mft-nvenc-keyframe-flag`)
- `default = ["hw-encoder"]` flip (separate slice `hw-encoder-default-on-flip`, gates on this AND Slice 4)
- `sm-domain` / `VideoEncoder` trait changes (R7/R14 FROZEN)
- Production callers of `flush()` (test affordance only)
- Channel-disconnect DRAIN spam fix (DD8 defer to v2 candidate)
- `mft_flush_emits_pending_output` new test (existing 8 fails ARE the RED signal per #707 §3)
- AMD AMF empirical verification

---

## 10. SDD Chain Anchors

- **Predecessor**: PR #18 (`hw-encoder-mft-vendor-compat-rework`, archive #699, master `daa9522`).
- **This slice**: `hw-encoder-mft-single-frame-flush` (Slice 3). Branch `feat/hw-encoder-mft-single-frame-flush`.
- **Successor (independent, both block default-on flip)**: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 4, M).
- **Engram chain**: explore #701 → proposal #707 → spec #708 → phase-0-trace #710 → **design (this) → tasks → apply-progress → verify-report → archive-report**.
