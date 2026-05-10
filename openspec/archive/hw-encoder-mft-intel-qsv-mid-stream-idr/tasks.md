# Tasks: hw-encoder-mft-intel-qsv-mid-stream-idr (Slice 5 — Mechanism G)

> Strict TDD ACTIVE. Test runner: `cargo nextest run --workspace`.
> Artifact store: hybrid (engram + openspec).
> Branch: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` @ `918447a` (off master `5130e87`).
> Delivery strategy: single-PR (session-cached) + `size:exception` pre-approved.

---

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Branch baseline delta (already at `918447a`) | ~+200 LOC net vs master `5130e87` (probe + structural refactor) |
| Phase 1 DD10 deletions | −80 to −100 LOC (production) |
| Phase 2 DD4 bitrate re-apply | +10 to +15 LOC (production) |
| Phase 3 DD9 trait + DD12 docstring | +15 to +25 LOC (production) |
| Phase 4 T7.1 + T7.2 GREEN bodies + helper | +180 to +280 LOC (tests) |
| Phase 5 S-scenario coverage notes | 0 LOC (documentation in PR body) |
| Phase 6 verify gates | 0 LOC new |
| **Estimated total delta vs master `5130e87`** | **~+325 to +420 LOC net** |
| Files touched | 2 (production: `windows_mft.rs`; tests: `windows_mft_encode.rs`) |
| 400-line budget risk | **Medium** (upper bound ~420 LOC; net LOC likely 350–400 after deletions offset additions) |
| Chained PRs recommended | **No** |
| Suggested split | Single PR with `size:exception` |
| Delivery strategy | single-pr |
| Chain strategy | size-exception |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: size-exception
400-line budget risk: Medium

### Chained Delivery Option (pre-locked, activate only if net LOC exceeds 450 after apply batch 1)

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| PR-A | Phase 1 DD10 deletions + Phase 3 DD9/DD12 (refactor-only; no new mechanism) | PR-A → master | Base: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr`; confirms no NVENC regression |
| PR-B | Phase 2 DD4 + Phase 4 T7.1/T7.2 GREEN (mechanism + tests) | PR-B → PR-A branch | Depends on PR-A |

---

## Phase 0 — Already Done at `918447a` (RETROSPECTIVE / DONE)

- [x] T0.1 Phase 0 round 3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` added `#[ignore]`-gated in `windows_mft_encode.rs` — PASS on Host A (#783).
- [x] T0.2 `pump_loop` ownership refactor: `mft: IMFTransform` owned, `initial_codec_api: ICodecAPI` owned, `initial_event_gen: IMFMediaEventGenerator` owned; `pump_loop` returns `IMFTransform` (DD1).
- [x] T0.3 `MftEncoderShared.keyframe_recreate_pending: AtomicBool` field added in `windows_mft.rs:102` (DD6).
- [x] T0.4 `WindowsMftH264Encoder::request_keyframe_via_recreate()` public inherent method added at `windows_mft.rs:1954` (DD6 / DD11).
- [x] T0.5 Mechanism G handler in `pump_loop` at `windows_mft.rs:1454–1645` (DD3): END_OF_STREAM → COMMAND_DRAIN → poll DrainComplete → END_STREAMING → drop → ActivateObject → async_unlock → re-cast → setup_mft → reset counters → resume.
- [x] T0.6 Round 2 speculative C-prime + A production code reverted; round 1+2 probes retained `#[ignore]`-gated.

**Note**: DD4 (bitrate re-apply post-setup_mft) is NOT yet in the G handler at `918447a`. DD9 trait routing is NOT yet updated (trait `request_keyframe()` still arms `keyframe_pending` at line 261). DD10 deletions are NOT yet done. DD12 docstring is NOT yet updated.

---

## Phase 1 — DD10 Deletions: CleanPoint + ICodecAPI ForceKeyFrame (C1 Prerequisite)

> Spec: R10, S13. Design: DD10. All sites in `crates/sm-infra/src/encode/windows_mft.rs`.
> Prerequisite for C2 GREEN. Run after T4.1 (C1 RED commit) so deletions land in C2 GREEN commit.

- [x] T1.1 In `CodecApiSwap` struct at `windows_mft.rs:1036`: DELETE `force_keyframe: bool` field (DD10). Update struct doc comment to reflect bitrate-only purpose.
- [x] T1.2 In `swap_pending_codec_settings()` at `windows_mft.rs:1046–1061`: DELETE the `force_keyframe` swap line (`state.keyframe_pending.swap(false, AcqRel)`) and the corresponding `force_keyframe` field in the returned `CodecApiSwap`. Update trace log if needed (DD10).
- [x] T1.3 In `fire_pending_codec_settings()` at `windows_mft.rs:1086–1092`: DELETE the `if swap.force_keyframe { … SetValue(CODECAPI_AVEncVideoForceKeyFrame) … }` block (DD10). Remove `CODECAPI_AVEncVideoForceKeyFrame` usage here.
- [x] T1.4 In `restore_pending_codec()` at `windows_mft.rs:1106–1108`: DELETE the `if swap.force_keyframe { state.keyframe_pending.store(true, Release); }` branch (DD10).
- [x] T1.5 In `submit_frame()` at `windows_mft.rs:1682–1703`: DELETE `force_keyframe: bool` parameter, DELETE the `if force_keyframe { sample.SetUINT32(CleanPoint, 1) … }` block (DD10). Update call sites inside pump_loop at `windows_mft.rs:~1255` to remove the `force_keyframe` argument.
- [x] T1.6 In `submit_frame()` docstring: remove references to `force_keyframe` and `MFSampleExtension_CleanPoint` write path; clarify that CleanPoint is read-only in production (only `collect_output` reads it for IDR detection).
- [x] T1.7 DELETE `MftEncoderShared.keyframe_pending: AtomicBool` field at `windows_mft.rs:85,108` (DD10). Remove `Default::default()` initialization entry.
- [x] T1.8 DELETE import `CODECAPI_AVEncVideoForceKeyFrame` from the `windows::Win32::Media::MediaFoundation` use block at `windows_mft.rs:33` (DD10). Verify `MFSampleExtension_CleanPoint` import at `windows_mft.rs:40` is KEPT (read path in `collect_output` still uses it). Also deleted `make_variant_bool`, `VARIANT_TRUE`, `VARIANT_FALSE`, `VT_BOOL` (dead code after ForceKeyFrame path removal).
- [x] T1.9 After all deletions: run `cargo build --features sm-infra/hw-encoder` → PASS. Run `cargo clippy --all-targets --all-features --locked -- -D warnings` → exit 0. Run `cargo fmt --check --all` → exit 0. No dead-code warnings introduced.

---

## Phase 2 — DD4: Bitrate Re-Apply Post-Recreate (C2 GREEN Production)

> Spec: R15 (set_bitrate contract persistence). Design: DD4. File: `windows_mft.rs`.
> Placement: inside Mechanism G handler, AFTER `setup_mft()` call (line ~1635), BEFORE `ni_count = 0` reset.

- [x] T2.1 Inside the Mechanism G handler in `pump_loop` at `windows_mft.rs:1635–1638`: add DD4 SWAP-FIRE block immediately after the `setup_mft(&mft, config)?` success path — call `swap_pending_codec_settings(state)` on the fresh `ICodecAPI` (now re-cast as `codec_api`) then `fire_pending_codec_settings(&codec_api, &bitrate_swap)`. Add inline comment referencing DD4 and explaining `force_keyframe` is absent (deleted by DD10; the recreate IS the keyframe).
- [x] T2.2 Add a brief doc comment annotation to the G handler block (or the DD4 insertion site) explaining the race semantics: if a newer `set_bitrate(N)` arrived between the G SWAP and FIRE, it sits in `pending_bitrate` and the NEXT pump_loop iteration's normal SWAP picks it up — no loss (Slice 4 DD3 pattern).
- [x] T2.3 Confirm `mft_set_bitrate_updates_encoder_without_restart` (T8.2) still compiles and logically passes with DD4 + DD10 changes (structural review — no code change to test body; verify by inspection). CONFIRMED: T8.2 only exercises pending_bitrate path; no change to test body needed.

---

## Phase 3 — DD9 Trait Routing + DD12 Docstring Fix (C2 GREEN)

> Spec: R2, R14. Design: DD9, DD12. File: `windows_mft.rs`.

- [x] T3.1 In `impl VideoEncoder for WindowsMftH264Encoder` at `windows_mft.rs:261–263`: replace the body of `request_keyframe(&self)` with `self.request_keyframe_via_recreate()` (DD9). Remove the now-dead `self.state.keyframe_pending.store(true, Release)` line. The `keyframe_pending` field deleted in Phase 1 T1.7; compiler confirmed no remaining uses.
- [x] T3.2 Update the `flush()` docstring at `windows_mft.rs:1912–1929` to DD12 locked wording: `flush()` triggers `MFT_MESSAGE_COMMAND_DRAIN`; after `METransformDrainComplete`, pump_loop resumes via `BEGIN_STREAMING + START_OF_STREAM` (Slice 4 DD17/F2); `flush()` is SAFE mid-stream and NOT terminal. Latency ~250 ms (Phase 0 trace #710). For forced mid-stream IDR use `request_keyframe_via_recreate()` (Slice 5 — Mechanism G). "effectively terminal per session" language removed.
- [x] T3.3 Update `request_keyframe_via_recreate()` docstring at `windows_mft.rs:1934–1958`: removed "PHASE 0 EMPIRICAL" + "Risk: Intel QSV driver may reject the 2nd ActivateObject call" hedge (round 3 #783 disproved). Updated to DD11 locked wording: (a) drain latency ~50–300 ms, (b) ~9 ms tear-down + recreate, (c) IDR guarantee, (d) AcqRel atomic semantics, (e) concurrent calls collapse to one recreate.
- [x] T3.4 Run `cargo build --features sm-infra/hw-encoder` → PASS. Run `cargo fmt --check --all` → exit 0 (after `cargo fmt` applied). All formatting fixes confirmed.

---

## Phase 4 — T7.1 + T7.2 GREEN Bodies (Strict TDD C1 RED → C2 GREEN)

> Spec: R11, R12, S4, S5. Design: DD8. File: `crates/sm-infra/tests/windows_mft_encode.rs`.
> TDD CADENCE: T4.1 is the C1 RED commit (install test bodies BEFORE Phase 1+2+3 production changes).
> T4.5 is the C2 GREEN commit (Phase 1+2+3 changes make T7.1+T7.2 pass).

### C1 RED Commit (install test structure first)

- [x] T4.1 Add `assert_keyframe_within_next_n_frames(packets: &[EncodedPacket], n: usize)` helper near the top of `windows_mft_encode.rs` (before test functions, after existing helpers). It MUST assert that at least one packet in the slice has `is_keyframe=true`, panicking with a clear message if not. `n` is passed from the caller for intent documentation only; the assertion is across the full slice.
- [x] T4.2 Replace T7.1 `mft_request_keyframe_marks_next_packet_as_keyframe` body (currently master carry-forward) with G-semantics eventually-style body (DD8, spec S4): push priming batch (12 frames) → `encoder.flush()` → drain via `recv_timeout` loop until `RecvTimeoutError::Disconnected` or at least 1 keyframe received → `encoder.request_keyframe()` → push 1 IDR-target frame → `encoder.flush()` → collect post-recreate packets via `recv_timeout` loop (deadline ≥ 5 s to absorb G latency) → `assert_keyframe_within_next_n_frames(&post_recreate_packets, 5)`. `#[ignore]` updated to Slice 5 reason (stays gated for CI).
- [x] T4.3 Replace T7.2 `mft_keyframe_flag_cleared_after_idr_emitted` body (currently master carry-forward) with G-semantics eventually-style body (DD8, spec S5): similar priming cadence → `request_keyframe()` → push 1 IDR-target frame + 1 P-target → `encoder.flush()` → collect post-recreate packets → assert keyframe in batch AND assert second packet (if present) `is_keyframe=false` (exactly-once IDR). `#[ignore]` updated to Slice 5 reason.
- [x] T4.4 Confirm NVENC variants of T7.1 and T7.2 (if they exist as separate functions) retain `#[ignore]` with updated carry-forward note pointing to `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6). No body change for NVENC. CONFIRMED: no separate NVENC variants exist — single test function per spec covers both paths; NVENC path implicitly `#[ignore]`-gated through same attribute.
- [x] T4.5 Commit C1 RED `75f438a`: `test(infra): C1 RED — install T7.1/T7.2 GREEN bodies for Mechanism G (Slice 5)`. Build gates PASS: cargo build + nextest 611/611 + clippy -D warnings + fmt --check. Host A --run-ignored RED confirmation PENDING (user).

### C2 GREEN Commit

- [x] T4.6 Commit C2 GREEN (Phases 1+2+3 + T4.6): `feat(infra): C2 GREEN — wire Mechanism G + delete CleanPoint/ForceKeyFrame paths (Slice 5)`. nextest 611/611 PASS on Host-dev (non-HW). T7.1+T7.2 expected GREEN on Host A (--run-ignored). Host A + Host B verification PENDING (user).

---

## Phase 5 — Spec Scenario Coverage Map

> No LOC changes. Documents mapping of S1–S16 to tests or structural checks. Reference in PR description.

- [ ] T5.1 Verify S1 (API exists + arms atomic): confirmed by compile + `new_for_validation_test()` unit test inspection. No new test needed; structural.
- [ ] T5.2 Verify S2 (idempotency: 3x arm → 1x recreate): covered implicitly by T7.1 cadence (only one IDR visible at post-recreate index 0; `keyframe_indices=[0]` from round 3 probe is baseline). Document in PR body. No additional integration test needed at this time (optional stress probe deferred to verify phase).
- [ ] T5.3 Verify S3 (pre-encode graceful call): structural guarantee from `AtomicBool` arm; no new test required. Document in PR body.
- [ ] T5.4 S4 → T7.1 GREEN (T4.2). S5 → T7.2 GREEN (T4.3). S6 → T8.2 (unchanged, confirmed passing). S7 → T1–T5 Slice 3 suite (regression gate, Phase 6). S8 → 30-frame smoke (Phase 6). S9 → Phase 0 round 3 probe retained `#[ignore]` (T0.1, already on branch). S10/S11 → observable via S9 probe. S12 → NVENC T7.1/T7.2 `#[ignore]` retained (T4.4). S13 → verified by Phase 1 grep gate (T6.4). S14 → `git diff 5130e87 -- crates/sm-domain/` = 0 lines (T6.5). S15 → docstring + `git log` (T3.2). S16 → C1 RED / C2 GREEN cadence (T4.5/T4.6).

---

## Phase 6 — Verify Gates

> Run after C2 GREEN commit. All gates must PASS before PR open.

- [x] T6.1 `cargo build --features sm-infra/hw-encoder` → exit 0. PASS.
- [x] T6.2 `cargo clippy --all-targets --all-features --locked -- -D warnings` → exit 0. PASS (clean after fmt).
- [x] T6.3 `cargo fmt --check --all` → exit 0. PASS.
- [x] T6.4 `grep -n "CleanPoint.*SetUINT32\|force_keyframe\|CODECAPI_AVEncVideoForceKeyFrame" crates/sm-infra/src/encode/windows_mft.rs` → zero WRITE-path matches in production functions. PASS. (`MFSampleExtension_CleanPoint` in `collect_output` READ path confirmed still present.)
- [x] T6.5 `git diff 5130e87 -- crates/sm-domain/` → 0 lines diff. PASS (sm-domain FROZEN).
- [ ] T6.6 Host A: `cargo nextest run --workspace --features sm-infra/hw-encoder` → ≥ 658/664 tests passing (AC-2). T7.1 + T7.2 PASS. T8.2 PASS. T1–T5 PASS. 30-frame smoke PASS.
- [ ] T6.7 Host A: `cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode -E 'test(/^phase0_intel_qsv_idr_via_imftransform_recreate/)' --run-ignored=ignored-only --test-threads=1 --no-fail-fast --no-capture` → PASSES (regression gate for round 3 probe — AC-6, S9).
- [ ] T6.8 Host A: confirm Phase 0 round 1 probes (`phase0_intel_qsv_idr_via_drain_resume_*`) still present `#[ignore]`-gated and compile (regression evidence retention — AC-6, DD7).
- [ ] T6.9 Host B (NVENC informational): run Host B full suite → ≥ 660/664 (AC-3). T8.2 PASS. T7.1/T7.2 NVENC variants remain `#[ignore]`. 30-frame smoke PASS. No new FAIL vs master baseline.
- [ ] T6.10 Verify `mft_activate_factory: IMFActivate` (or equivalent rename from `winning_activate`) is present in `WindowsMftH264Encoder` struct — AddRef-clone path confirmed (DD2, AC-16). Confirm `start()` does NOT consume the factory via `.take()` but clones it for the thread.

---

## Spec → Task Cross-Reference

| Spec R# | Design DD | Tasks |
|---------|-----------|-------|
| R1 (API exists) | DD6 | T0.4 ✅ |
| R2 (trait routing) | DD9 | T3.1 |
| R3 (idempotent atomic) | DD6 | T0.3 ✅, T5.2 |
| R4 (G sequence ordered) | DD3 | T0.5 ✅ |
| R5 (owned handles) | DD1 | T0.2 ✅ |
| R6 (IMFActivate clone) | DD2 | T6.10 |
| R7 (first-frame IDR) | DD3 | T0.5 ✅, T4.2, T4.3 |
| R8 (encoder survives) | DD3 | T6.6, T6.7 |
| R9 (latency doc + eventually-style) | DD11 | T3.3, T4.2, T4.3 |
| R10 (CleanPoint + ForceKeyFrame deleted) | DD10 | T1.1–T1.8 |
| R11 (T7.1 GREEN Intel QSV) | DD8 | T4.2, T4.5, T4.6 |
| R12 (T7.2 GREEN Intel QSV) | DD8 | T4.3, T4.5, T4.6 |
| R13 (Phase 0 probes retained) | DD7 | T0.1 ✅, T6.7, T6.8 |
| R14 (flush() docstring) | DD12 | T3.2 |
| R15 (zero regression) | — | T6.6, T6.9 |
| R16 (sm-domain frozen) | DD9 | T6.5 |
| R17 (TDD cadence C0/C1/C2) | — | T4.5, T4.6 |
| R18 (single-PR / LOC) | D-DELIVERY | Review Workload Forecast above |

---

## Commit Sequence Summary

| Commit | Label | Phases | Compile | T7.1/T7.2 (Host A) |
|--------|-------|--------|---------|---------------------|
| `918447a` | C0-R3 (existing) | Phase 0 (DONE) | PASS | FAIL (master bodies) |
| C1 | RED | Phase 4 T4.1–T4.5 | PASS | FAIL (G bodies, no handler yet) |
| C2 | GREEN | Phase 1 + 2 + 3 + T4.6 | PASS | PASS |
| C3 (opt) | POLISH | Phase 6 fmt/clippy only | PASS | PASS |
