# Design: hw-encoder-mft-intel-qsv-mid-stream-idr (Slice 5 — Mechanism G architecture)

> Phase: SDD design. Inputs: proposal v2 #776, spec #777 (must be rewritten for G — not blocking design),
> Phase 0 round 1 #779, round 2 #780, **round 3 #783 (G PASS)**, Slice 4 design v2 #749, sdd-init #186 v13.
> Artifact store: hybrid (this file + engram topic_key `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/design`).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-09. Branch: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` @ `918447a` (off master `5130e87`).

---

## Executive summary

Mechanism G — drop + re-`ActivateObject` the `IMFTransform` from inside `pump_loop` — is the only empirically validated mid-stream IDR path on Intel QSV (rounds 1+2 invalidated C/C-prime/A; round 3 #783 validated G with ~9 ms tear-down + IDR at post-recreate index 0, encoder alive). At branch tip `918447a` the structural refactor (owned `IMFTransform` + `keyframe_recreate_pending` + handler skeleton + public `request_keyframe_via_recreate()`) already lands; this design captures the architecture decisions (DD1–DD12) and resolves the proposal's deferred decision **D-CODECAPI-POST-RECREATE (DD4)** with rationale + production sketch. CleanPoint and `CODECAPI_AVEncVideoForceKeyFrame` paths are eliminated (DD10). Slice 4 SWAP-FIRE is preserved for `set_bitrate` and re-applied post-recreate to satisfy T8.2 across recreate boundaries.

---

## Decisions

### DD1 — `pump_loop` ownership refactor (proposal D-OWNERSHIP-REFACTOR)

| Aspect | Decision |
|--------|---------|
| Choice | `pump_loop(mft: IMFTransform, activate_factory: &IMFActivate, codec_api: ICodecAPI, event_gen: IMFMediaEventGenerator, …) -> IMFTransform`. All three handles owned (was: `&IMFTransform`). |
| Rationale | G drops + replaces the handle mid-loop; a `&mut IMFTransform` cannot be re-bound across `drop(old) → ActivateObject(new)`. |
| Invariant | every exit path returns the (possibly recreated) `IMFTransform` so `run_encoder_thread` can issue end-of-stream messages on the final handle (`windows_mft.rs:817–827`). |
| Risk | Borrow-lifetime mistakes during refactor (round 3 already executed it; verify no behavioral drift on non-G paths via S6/S7/S8). |

### DD2 — `mft_activate_factory: IMFActivate` field (proposal D-IMFACTIVATE-CLONE)

| Aspect | Decision |
|--------|---------|
| Choice | Replace `winning_activate: Option<IMFActivate>` (consumed by `.take()` in `start()`, `windows_mft.rs:228`) with `mft_activate_factory: IMFActivate` (clone, not take). `IMFActivate` clone in windows-rs = AddRef on the underlying COM ptr — MTA-safe. |
| Rationale | pump_loop needs the factory across the encoder thread's lifetime to call `ActivateObject` again on G. `.take()` is incompatible with multi-recreate sessions. |
| Empirical | round 3 #783 L524-L531 — 2nd `ActivateObject` succeeds without `E_UNEXPECTED`. |
| Implementation sketch | `start()` clones the factory into the `ComSend` wrapper for the thread; `Drop` releases the local clone after `stop()`. |
| Residual risk | 3rd or Nth `ActivateObject` not stress-tested. Mitigation: stress probe (`request_keyframe_via_recreate × 5–10` cycles) at verify phase as candidate or carry-forward. |

### DD3 — Mechanism G recreate sequence (proposal D-RECREATE-SEQUENCE)

Locked handler order (matches `windows_mft.rs:1474–1645`):

```
swap(keyframe_recreate_pending, false, AcqRel) && !draining
  → END_OF_STREAM
  → COMMAND_DRAIN
  → poll METransformDrainComplete (bounded 5 s; discard HaveOutput during window)
  → END_STREAMING
  → drop(old_mft)               // forces COM Release
  → activate_factory.ActivateObject() → new mft
  → MF_TRANSFORM_ASYNC_UNLOCK on new mft
  → re-cast ICodecAPI + IMFMediaEventGenerator from new mft
  → setup_mft(new mft)           // SetInputType + SetOutputType (re-derived from EncoderConfig) +
                                 // FLUSH + BEGIN_STREAMING + START_OF_STREAM
  → reset ni_count = ho_count = 0; draining = false; output_format_known = None
  → resume pump_loop
```

Why each step: round-1+2 evidence (no MFT-API mid-stream signal triggers IDR on the same handle); G's setup-sequence on a fresh COM object is the only vendor-uniform path to first-frame IDR. Skipping any step risks driver edge cases (round 2 C-prime ENCODER_DIED is the cautionary tale).

### DD4 — D-CODECAPI-POST-RECREATE — RESOLVED: option (a) re-apply pending bitrate (DEFERRED FROM PROPOSAL)

| Option | Tradeoff | Decision |
|--------|----------|---------|
| (a) Re-apply `pending_bitrate` post-`setup_mft` via SWAP-FIRE | Preserves caller's `set_bitrate(N)` across recreate. Adds ~10 LOC + small race surface. | **CHOSEN.** |
| (b) Accept reset to `EncoderConfig.bitrate_bps` defaults | Simpler. Breaks T8.2 invariant under `set_bitrate(x); request_keyframe_via_recreate();`. | Rejected. |

**Rationale.** The new `IMFTransform`'s `ICodecAPI` starts at `EncoderConfig` defaults (fresh COM object — no carry-over). The caller contract for `set_bitrate(N)` is "value persists until next set_bitrate or stop". Slice 5 must not silently regress this on a recreate cycle; doing so would surface as a T8.2 false-negative if a future test interleaves `set_bitrate(x)` with `request_keyframe_via_recreate()`. Option (a) preserves the contract with negligible cost.

**Production sketch (within the G handler, after Step G6 `setup_mft` and before resuming the loop):**

```rust
// DD4 — re-apply pending bitrate on the fresh ICodecAPI.
//
// Snapshot current pending atomics, fire on the new codec_api. This treats
// the recreate boundary like a SWAP-FIRE iteration: any in-flight set_bitrate
// must take effect on the NEW handle since the old handle was dropped.
//
// Concurrency: caller may race set_bitrate() with request_keyframe_via_recreate().
// pending_bitrate is already AcqRel, so swap_pending_codec_settings sees the
// most recent caller write. compare_exchange in restore_pending_codec preserves
// last-write-wins if a NEWER set_bitrate slipped in between SWAP and FIRE.
let bitrate_swap = swap_pending_codec_settings(state);
fire_pending_codec_settings(&codec_api, &bitrate_swap);
// keyframe_pending field still consumed inside swap_pending_codec_settings,
// but the FIRE branch for force_keyframe is DELETED (DD10) — the recreate
// itself IS the keyframe, so swap.force_keyframe is intentionally ignored.
```

**Ordering.** Re-application must happen AFTER `setup_mft` (so the new ICodecAPI is fully initialized) and BEFORE pump_loop resumes consuming frames (so the next `ProcessInput` sees the correct bitrate).

**Race with concurrent `set_bitrate`.** Slice 4 DD3 already handles this: if a newer `set_bitrate(N)` lands between SWAP and FIRE, the new value is in `pending_bitrate` and will be picked up by the NEXT pump_loop iteration's normal SWAP — no loss, no double-apply. Same pattern applies here.

**Alternative not chosen.** Stash the bitrate in a separate `restore_bitrate: AtomicU32` before tear-down and FIRE it post-setup. Rejected — duplicates pending_bitrate's function and creates two sources of truth. SWAP-FIRE on the existing atomics is sufficient.

### DD5 — Drain race / arbitration with Slice 4 DD14 stack-local `draining: bool`

| Aspect | Decision |
|--------|---------|
| Choice | G handler is GATED on `&& !draining` (already implemented `windows_mft.rs:1477`). |
| Composition | Slice 4 DD14 owns the `let mut draining = false;` stack-local and protects ProcessInput-during-drain. G's recreate ALSO drains (END_OF_STREAM + COMMAND_DRAIN, bounded poll for DrainComplete). The two states compose by mutual exclusion: G is skipped when an explicit-flush drain is in-flight; the next pump_loop iteration picks up the recreate after DrainComplete clears `draining`. |
| Inside the handler | The G block does NOT set `draining = true` (it blocks synchronously on its own DrainComplete poll). It DOES reset `draining = false` at the end (`windows_mft.rs:1640`), which is idempotent if no flush was in flight. |
| Invariant | One drain window at a time. If both signals arrive (`drain_pending == true` AND `keyframe_recreate_pending == true`), the explicit flush proceeds first; the keyframe recreate fires next iteration. Behavioral contract: each request consumed exactly once. |
| Risk | Latency stacks (flush + recreate ≈ 250 ms + ~50–400 ms first-encode). Acceptable per D-SCOPE-LATENCY (DD11). |

### DD6 — Atomic semantics for `keyframe_recreate_pending`

| Aspect | Decision |
|--------|---------|
| Choice | `AtomicBool` with `swap(false, Ordering::AcqRel)` for read-and-clear (`windows_mft.rs:1474–1476`). Caller arms with `store(true, Ordering::Release)` (`windows_mft.rs:1956`). |
| AcqRel rationale | swap acquires any prior `set_bitrate` / state writes (visible to subsequent G handler steps) and releases the cleared state to the next iteration's load. |
| Idempotency | Multiple `request_keyframe_via_recreate()` calls during in-flight drain collapse to a single recreate — store is idempotent, swap consumes once (matches Slice 4 DD3 R5 exactly-once semantics for `keyframe_pending`). |
| Naming | Field is `keyframe_recreate_pending` (NOT `keyframe_pending`) — distinct from Slice 4's CleanPoint/ForceKeyFrame channel that is being deleted (DD10). |

### DD7 — Phase 0 probe retention (Slice 4 DD7 convention)

| Round | Probe | Status post-fix |
|-------|-------|----------------|
| 1 (#779) | `phase0_intel_qsv_idr_via_drain_resume_*` | Retain `#[ignore]`-gated. Empirical record that drain+resume does NOT produce IDR. |
| 2 (#780) | `phase0_intel_qsv_idr_via_flush_resume_*`, `phase0_intel_qsv_idr_via_gop_size_toggle` | Round 2 probes were REMOVED in `918447a` (correct — speculative production reverted). Round 1 evidence remains via round 1 probe bodies. |
| 3 (#783) | `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` | Retain `#[ignore]`-gated. PRIMARY G regression gate. |

Why retain round 1: future Intel QSV driver changes could resurface drain+resume IDR (defensive evidence; cheap to keep).

### DD8 — T7.1/T7.2 GREEN body cadence (proposal D-SLICE-4-CARRY-FORWARD)

| Aspect | Decision |
|--------|---------|
| Cadence | `priming batch (e.g. 12 frames) → flush → drain → request_keyframe_via_recreate() → push 1 IDR-target frame → flush → drain → recv with eventually-style assertion within batch`. |
| Assertion | "next packet IS keyframe within next N frames" (helper `assert_keyframe_within_next_n_frames(packets, n)`), NOT strictly N=next-immediate. |
| Rationale | G has measurable drain latency (in-flight batch must complete before recreate's END_OF_STREAM+DRAIN cycle finishes). Strict next-frame-IDR fails intermittently; eventually-style is correct AND realistic. Spec S4/S5 already documents this contract. |
| N | Tasks-phase decides exact N (suggest 5–10 depending on batch size + drain latency observed in #783 trace). |
| Helper location | Test-only helper at top of `crates/sm-infra/tests/windows_mft_encode.rs` shared between T7.1 and T7.2. |

### DD9 — `VideoEncoder::request_keyframe` trait routing (proposal D-TRAIT-IMPL)

| Aspect | Decision |
|--------|---------|
| Choice | `impl VideoEncoder for WindowsMftH264Encoder { fn request_keyframe(&self) { self.request_keyframe_via_recreate() } }`. Public inherent method `request_keyframe_via_recreate()` (explicit name, signals cost). |
| Rationale | Trait callers get correct G semantics uniformly; advanced callers can call the inherent method directly to make the cost explicit at call sites. Resolves Slice 4 carry-forward divergence (trait impl was setting `keyframe_pending` while only ICodecAPI ForceKeyFrame consumed it on Intel QSV — the path was a no-op). |
| Compatibility | Test code that imports `WindowsMftH264Encoder` directly + calls `.request_keyframe()` continues to work (auto-deref to trait method on concrete type). |
| Existing `request_keyframe()` body at `:261–263` | DELETED — sole writer of `keyframe_pending` was this method; with that field repurposed for SWAP only as `swap.force_keyframe` which is now ignored (DD10), the AtomicBool is effectively dead. See DD10 for full deletion list. |

### DD10 — CleanPoint + ICodecAPI ForceKeyFrame deletion scope (proposal D-CLEANPOINT-DEPRECATION)

DELETE all production references to mid-stream IDR via CleanPoint or ICodecAPI ForceKeyFrame. G is the only path.

| Site | File:line | Action |
|------|-----------|--------|
| `MFSampleExtension_CleanPoint=1` setter in `submit_frame` | `windows_mft.rs:1690–1702` | DELETE the `if force_keyframe { … }` block. |
| `submit_frame` `force_keyframe: bool` parameter | `windows_mft.rs:1682,1687` | DELETE the parameter; update call site at `windows_mft.rs:1255` (Slice 4 DD1 SWAP path). |
| `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` in `fire_pending_codec_settings` | `windows_mft.rs:1086–1092` | DELETE the `if swap.force_keyframe { … }` block. |
| `CodecApiSwap.force_keyframe: bool` field | `windows_mft.rs:1036` | DELETE the field. |
| `swap_pending_codec_settings` `force_keyframe` swap | `windows_mft.rs:1047` | DELETE the line; struct literal at `:1058–1061` drops the field. |
| `restore_pending_codec` `force_keyframe` branch | `windows_mft.rs:1106–1108` | DELETE the `if swap.force_keyframe { … }` block. |
| `keyframe_pending: AtomicBool` field on `MftEncoderShared` | `windows_mft.rs:85` | DELETE the field. |
| `MftEncoderShared::default()` initializer | `windows_mft.rs:108` | DELETE the line. |
| Old `request_keyframe()` setter on `WindowsMftH264Encoder` | `windows_mft.rs:261–263` | DELETE; trait method now routes to `request_keyframe_via_recreate()` (DD9). |
| `CODECAPI_AVEncVideoForceKeyFrame` import | `windows_mft.rs:33` | DELETE. |
| `MFSampleExtension_CleanPoint` import | `windows_mft.rs:40` | KEEP — `collect_output` reads it for IDR detection (`windows_mft.rs:1809`). The READ path stays; only the WRITE paths are deleted. |
| `is_keyframe` detection (read path) at `windows_mft.rs:1809` | `windows_mft.rs:1809` | KEEP UNCHANGED. G produces IDR; `collect_output` correctly identifies it via either CleanPoint=1 set by the encoder OR `annex_b_contains_idr()` fallback. |

**T8.2 verification.** `mft_set_bitrate_updates_encoder_without_restart` only exercises the bitrate path (not ForceKeyFrame). After deletion, T8.2's SWAP-FIRE bitrate branch in `swap_pending_codec_settings`/`fire_pending_codec_settings` remains intact and continues to PASS. Verified by inspection of test body (Slice 4 archive #773 baseline).

### DD11 — Latency profile documentation (proposal D-SCOPE-LATENCY)

Doc-comment on `request_keyframe_via_recreate()` (`windows_mft.rs:1934–1958`) MUST EXPLICITLY document:

- **Tear-down + recreate cost**: ~9 ms (round 3 #783 L524→L531).
- **In-flight batch drain time**: variable, depends on caller batch size. Observed ~250 ms in #783 with 30-frame priming.
- **First post-recreate frame is IDR**: setup-sequence guarantee (vendor-uniform).
- **Concurrency**: backed by `AtomicBool::swap(AcqRel)`; multiple concurrent calls collapse to a single recreate.
- **Production callers tuning live-stream latency** must factor in the drain cost; "next frame is keyframe" is misleading without context.

The doc comment at branch tip `918447a` covers most of this; design locks the wording is canonical and must NOT be removed by future refactors.

### DD12 — `flush()` docstring fix (proposal D-DOCSTRING-FIX)

`flush()` docstring at `windows_mft.rs:1912–1929` currently says (line 1915–1916): *"The encoder is effectively terminal per session on Intel QSV — do not call `flush()` mid-stream."* This is STALE.

**New text (locked):**

> `flush()` triggers `MFT_MESSAGE_COMMAND_DRAIN`. After `METransformDrainComplete`, the pump_loop resumes the stream by sending `BEGIN_STREAMING + START_OF_STREAM` (Slice 4 DD17/F2) — `flush()` is SAFE to call mid-stream and is NOT terminal. Latency: ~250 ms drain roundtrip (Phase 0 trace #710). For forced mid-stream IDR, use `request_keyframe_via_recreate()` (Slice 5 — Mechanism G). Production callers should use `request_keyframe_via_recreate()` for keyframe forcing; `flush()` is a cadence affordance for tests.

---

## Component map (verified at branch tip `918447a`)

```
crates/sm-infra/src/encode/windows_mft.rs
├── MftEncoderShared             (DD10: keyframe_pending DELETE; keyframe_recreate_pending KEEP)
├── WindowsMftH264Encoder
│   ├── winning_activate         (DD2: rename → mft_activate_factory; clone not take)
│   ├── start()                  (DD2: clone factory into ComSend; do NOT consume)
│   ├── request_keyframe()       (DD9/DD10: trait impl re-routes to request_keyframe_via_recreate; old body DELETE)
│   └── request_keyframe_via_recreate()  (DD11: doc comment locked)
├── flush()                       (DD12: docstring fix)
├── CodecApiSwap                  (DD10: force_keyframe field DELETE)
├── swap_pending_codec_settings   (DD10: force_keyframe swap DELETE)
├── fire_pending_codec_settings   (DD10: force_keyframe FIRE branch DELETE)
├── restore_pending_codec         (DD10: force_keyframe restore branch DELETE)
├── submit_frame                  (DD10: force_keyframe param + CleanPoint write DELETE)
├── pump_loop                     (DD1, DD3, DD4, DD5, DD6 — handler at :1454–1645 mostly correct; DD4 add bitrate re-apply post-setup_mft)
├── collect_output                (UNCHANGED — read path keeps CleanPoint detection)
└── run_encoder_thread (caller)   (DD2: clone factory before spawn; pump_loop call site at :817–827 unchanged structurally)

crates/sm-infra/tests/windows_mft_encode.rs
├── T7.1 mft_request_keyframe_marks_next_packet_as_keyframe       (DD8: GREEN body, eventually-style)
├── T7.2 mft_keyframe_flag_cleared_after_idr_emitted              (DD8: GREEN body, eventually-style)
├── T8.2 mft_set_bitrate_updates_encoder_without_restart          (UNCHANGED — DD10 deletion does not touch bitrate path)
├── phase0_intel_qsv_idr_via_drain_resume_*                       (DD7: KEEP #[ignore]-gated, regression evidence)
├── phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr  (DD7: KEEP #[ignore]-gated, PRIMARY G gate)
└── (helper) assert_keyframe_within_next_n_frames                 (DD8: NEW shared helper)
```

---

## Data flow per `request_keyframe_via_recreate()` call

```
caller (WebRTC loop, app, test):
  WindowsMftH264Encoder::request_keyframe_via_recreate()
     → state.keyframe_recreate_pending.store(true, Release)            [DD6]

pump_loop iteration N:
  GetEvent → DrainComplete? → ni_count = ho_count = 0; draining = false
  while ho_count > 0: collect_output → tx.send(packet)
  while ni_count > 0:
     [DD14 GUARD] if draining { break }
     [DD1 SWAP]   bitrate_swap = swap_pending_codec_settings(state)     [DD10: no force_keyframe]
     submit_frame(mft, frame)                                            [DD10: no CleanPoint write]
     ProcessInput Ok → ni_count -= 1
     [DD1 FIRE]   fire_pending_codec_settings(&codec_api, &bitrate_swap) [DD10: only bitrate]

  if drain_pending.swap(false, AcqRel): COMMAND_DRAIN; draining = true   [Slice 4 DD14]

  if keyframe_recreate_pending.swap(false, AcqRel) && !draining:         [DD6, DD5]
     // ── Mechanism G handler ──                                        [DD3]
     END_OF_STREAM + COMMAND_DRAIN
     poll METransformDrainComplete (bounded 5 s; discard HaveOutput)
     END_STREAMING
     drop(old_mft)
     mft = activate_factory.ActivateObject()                              [DD2]
     MF_TRANSFORM_ASYNC_UNLOCK
     codec_api = mft.cast::<ICodecAPI>()
     event_gen = mft.cast::<IMFMediaEventGenerator>()
     setup_mft(&mft, config)   // SetInputType + SetOutputType + FLUSH + BEGIN + START
     // ── DD4: re-apply pending bitrate post-recreate ──
     bitrate_swap = swap_pending_codec_settings(state)
     fire_pending_codec_settings(&codec_api, &bitrate_swap)
     ni_count = ho_count = 0; draining = false; output_format_known = None
     resume

(next iteration)
  next ProcessOutput from new mft → packet 0 has is_keyframe = true       [setup-sequence]
```

---

## Testing strategy

| Layer | What to test | Approach |
|-------|-------------|----------|
| Unit (`#[cfg(test)] mod tests`) | None new — DD10 deletions verified by compile + existing unit tests passing. | `cargo test -p sm-infra --lib`. |
| Integration GREEN gate (Host A Intel QSV) | T7.1 + T7.2 PASS post-fix; T8.2 PASS unchanged. | `cargo nextest run -p sm-infra --features hw-encoder`. |
| Integration regression (Host A) | Slice 3 T1–T5 + 30-frame smoke + Slice 4 DD17/F2 flush PASS. | Full suite. |
| Integration informational (Host B NVENC) | T7.1/T7.2 informational (G is vendor-uniform but NVENC unverified empirically — AC-3 / OQ-10). | Manual smoke at verify phase. |
| Phase 0 regression (Host A) | Round 1 + Round 3 probes runnable `#[ignore]`-gated. | `cargo nextest run … --run-ignored=ignored-only`. |
| Stress (residual risk DD2) | Optional: 5–10× recreate cycles in a single session. | Verify-phase candidate; carry-forward if not implemented. |

---

## Migration / rollout

No data migration. Single-PR delivery (proposal D-DELIVERY) on `feat/hw-encoder-mft-intel-qsv-mid-stream-idr`. Flag `default = []` unchanged. `hw-encoder-default-on-flip` is gated on this slice + Slice 6 (NVENC keyframe-flag).

---

## Review Workload Forecast

| Metric | Value |
|--------|-------|
| Production LOC delta vs `918447a` | +50 to +120 (DD10 deletions = -X; DD4 bitrate re-apply = +Y; DD12 docstring = +5; DD9 trait routing = +5; DD2 factory clone = +10) |
| Test LOC delta vs `918447a` | +150 to +250 (T7.1 + T7.2 GREEN bodies + helper) |
| Total branch delta vs master `5130e87` | ~+400 to +650 (branch already +40 at `918447a`; total realistic ~+440 to +690 vs master) |
| **Chained PRs recommended** | **No** (within 800-LOC cap; 400-LOC budget exceeded → `size:exception` per proposal D-DELIVERY override, comparable to Slice 4 archive #773 outcome) |
| **400-line budget risk** | **Medium** (T-helper growth could push it). |
| **Decision needed before apply** | **No** (delivery_strategy resolved at proposal D-DELIVERY: single PR, override accepted) |

---

## Open questions

- [ ] DD2 stress test (5–10× recreate cycles) — implement in verify, carry-forward, or defer to Slice 7? Tasks-phase decides.
- [ ] DD8 helper N parameter (5? 10? batch-size-aware?) — tasks-phase locks once T7.1 GREEN body is drafted.
- [ ] AC-3 NVENC informational smoke — verify-phase Host B run is RECOMMENDED but not BLOCKING per proposal.

---

## SDD chain anchors

- Predecessor: PR #20 / `8fa1a61` / Slice 4 archive #773. Master baseline: `5130e87`.
- Branch baseline: `918447a` (round 3 probe + structural ownership refactor + G handler skeleton).
- Engram chain: explore #775 → proposal v2 #776 → spec v1 #777 (must be rewritten for G — orchestrator gate) → phase-0 round 1 #779 + round 2 #780 + round 3 #783 → **design (this)** → tasks → apply → verify → archive.
- Successors: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6); both gate `hw-encoder-default-on-flip`.
