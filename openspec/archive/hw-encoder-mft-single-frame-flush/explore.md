# Exploration: hw-encoder-mft-single-frame-flush (Slice 3 — Intel QSV single-frame drain)

> Artifact store: hybrid — engram observation #701 (`sdd/hw-encoder-mft-single-frame-flush/explore`) + this file
> Date: 2026-05-09
> Master tip: daa9522 (post PR #18)

---

## Executive Summary

The 8 single-frame timeouts on Intel QSV Host A are caused by the vendor requiring ≥3 frames in its pipeline before emitting ANY output, a pre-streaming initialization behavior that `MF_E_TRANSFORM_STREAM_CHANGE` renegotiation (Slice 2) cannot address. The recommended fix is **Approach C**: add an explicit `flush()` inherent method to `WindowsMftH264Encoder` that fires `MFT_MESSAGE_COMMAND_DRAIN` via an atomic flag — leveraging the already-validated DRAIN path used at channel-disconnect. This keeps `sm-domain` FROZEN (method is inherent, not on the `VideoEncoder` trait), requires no bitstream-corrupting frame padding, and requires only ~35 LOC across production + 8 test call sites.

**CRITICAL GATE**: a Phase 0 empirical trace on Host A must confirm Intel QSV honors DRAIN for 1-frame submissions before the design is locked, per the `tracing-before-explore` convention (#592).

---

## The 8 Failing Tests

1. `mft_encoded_packet_starts_with_annex_b_start_code` — 1 frame → `recv_timeout(5s)`
2. `mft_first_real_packet_is_annex_b` — 1 frame → `recv_timeout(5s)`
3. `mft_encoded_packet_timestamp_matches_capture_frame` — 1 frame → `recv_timeout(5s)`
4. `mft_setup_uses_config_dimensions_when_nonzero` — 1 frame → `recv_timeout(5s)`
5. `mft_setup_falls_back_when_config_dimensions_zero` — 1 frame → `recv_timeout(5s)`
6. `mft_request_keyframe_marks_next_packet_as_keyframe` — frame 0 + `recv_timeout(3s)` for IDR
7. `mft_keyframe_flag_cleared_after_idr_emitted` — frame 0 + `recv_timeout(3s)` for IDR
8. `mft_set_bitrate_updates_encoder_without_restart` — 3 frames + `recv_timeout(3s)` per frame

All fail at the FIRST `recv_timeout`. Confirmed by `smoke-trace.log` (pre-Slice 2): only 2× `METransformNeedInput` events fired, then vendor went silent.

---

## Recommended Approach: C — explicit `flush()` inherent method

```rust
impl WindowsMftH264Encoder {
    pub fn flush(&self) {
        self.shared.drain_pending.store(true, Ordering::Release);
        // pump_loop sees flag, fires MFT_MESSAGE_COMMAND_DRAIN
    }
}
```

In each failing test:
```rust
frame_tx.send(make_synthetic_frame(...)).expect("send");
enc.flush();  // ← NEW: signal drain
let pkt = pkt_rx.recv_timeout(Duration::from_secs(5)).expect("packet");
```

**Pros**: semantically correct, sm-domain stays FROZEN, reuses validated DRAIN path (already proven by `mft_drain_after_channel_close_does_not_panic`), ~35 LOC total, cross-vendor safe (DRAIN is async-MFT spec).

**Cons**: 8 test call sites + new public inherent method. Phase 0 must confirm Intel QSV honors 1-frame DRAIN.

---

## Approach Comparison

| Approach | Mechanism | LOC | Verdict |
|----------|-----------|-----|---------|
| **A: Pump-loop auto-drain on no-input timeout** | Detect no input → drain | medium | NO — too aggressive, may drain mid-stream |
| **B: Frame padding (N-1 dupes)** | Submit duplicate frames to push past ≥3 threshold | ~20 | REJECTED — corrupts bitstream, breaks output-content tests, brittle threshold |
| **C: Explicit `flush()` inherent method** | Caller signals end-of-burst via atomic flag | ~35 | **RECOMMENDED** |
| **D: Vendor-conditional (Intel QSV branch)** | Detect vendor + special path | high | REJECTED — anti-pattern, fragile to drivers |
| **E: Test-side `drop(frame_tx)`** | Tests close channel before recv | ~8 | FALLBACK — only if C blocked by Phase 0 |

---

## Frozen Invariants

- `sm-domain` API FROZEN per R14/R15 from Slice 2. `flush()` is inherent on `WindowsMftH264Encoder`, NOT on `VideoEncoder` trait.
- `default = []` unchanged. HW path stays opt-in. Default-on flip is `hw-encoder-default-on-flip` slice (still gated).
- Conventional commits, no AI footers, no `Co-Authored-By`.
- Strict TDD: RED commit before GREEN.
- `.engram/` IS tracked (convention #698). Don't touch.

---

## Open Questions for Propose Phase

1. **OQ-1 (CRITICAL — Phase 0 gate)**: Does Intel QSV honor `MFT_MESSAGE_COMMAND_DRAIN` with only 1 frame submitted? Or does it emit empty `METransformDrainComplete`?
2. **OQ-2**: Is DRAIN terminal? Can encoder accept new `ProcessInput` after DRAIN, or does it require teardown?
3. **OQ-3**: For multi-frame tests (T6/T7/T8): WHERE to call `flush()` — after each frame or only at end-of-burst?
4. **OQ-4**: Should `flush()` be callable while `frame_tx` is alive, or document "must drop channel first"?
5. **OQ-5**: Confirm `mft_drain_after_channel_close_does_not_panic` PASSES on master daa9522 (likely yes per archive 10/18 PASS).

---

## Phase 0 Gate (BEFORE design lock)

Per `tracing-before-explore` convention #592:

**Required experiment on Host A** (Intel QSV, master daa9522):
1. Add a 1-frame trace test that: submits 1 frame → drops `frame_tx` → `recv_timeout(10s)` with `RUST_LOG=sm_infra::encode=trace`
2. Add a 2-frame variant for comparison
3. Capture full event sequence

**Decision tree**:
- 1-frame DRAIN → packet output: Approach C works for all 8 tests
- 1-frame DRAIN → empty: Approach C still works but tests 1–5 must submit ≥2 frames
- Both 1F and 2F empty: Escalate, alternative mechanism needed

**BLOCKED_ON_SMOKE**: Design CANNOT be locked until Phase 0 trace evidence is in engram.

---

## Affected Files (forecast)

- `crates/sm-infra/src/encode/windows_mft.rs` — add `drain_pending: AtomicBool` to shared state, add `pub fn flush()`, pump_loop drain-flag check after NeedInput servicing
- `crates/sm-infra/tests/windows_mft_encode.rs` — add `enc.flush()` calls in 8 failing tests before first `recv_timeout`
- NO changes to `crates/sm-domain/` (FROZEN)
- NO changes to `crates/sm-infra/Cargo.toml` (`default = []` unchanged)

---

## Risks

| Sev | Likelihood | Risk | Mitigation |
|-----|------------|------|------------|
| HIGH | MED | Intel QSV doesn't honor 1-frame DRAIN | Phase 0 gate; fall back to E (≥2 frames) |
| MED | LOW | DRAIN is terminal — breaks multi-frame tests T6/T7/T8 | Design clarifies semantics; flush only at end-of-burst |
| MED | LOW | sm-domain freeze violated if propose decides trait-level drain needed | Lock inherent-only in propose |
| LOW | MED | NVENC regression from explicit DRAIN while channel open (untested) | Phase 0 trace also covers NVENC if Host B available |
| LOW | LOW | smoke-output.log shows latent "encoder thread dying on start" — check baseline on master tip before Phase 0 | Confirm 10/18 PASS on master daa9522 first |

Full content: engram observation #701.
