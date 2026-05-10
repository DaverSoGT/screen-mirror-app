# Proposal: hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Round 2 — AGGRESSIVE replacement)

> Phase: SDD propose (Round 2 — post-Phase-0-P2 breakthrough).
> Change: `hw-encoder-mft-nvenc-mid-stream-idr-mechanism`.
> Branch: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` @ `efc0f36` (off master `c48ae46`).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/proposal` + this file).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Status: DRAFT.
> Date: 2026-05-10. Author: SDD `sdd-propose` executor.

**Inputs (empirical evidence base — load-bearing)**:
- Engram **#803** — exploration round 2 (six mechanism candidates analyzed)
- Engram **#800** — Round-1 hypothesis (3-byte Annex-B detection bug) FALSIFIED
- Engram **#801** — Mechanism G NOT vendor-uniform (29/29 P-frames on NVENC post-recreate); Slice 5 overclaim falsified
- Engram **#807** — P1 (CleanPoint INPUT write) FALSIFIED on NVENC (30/30 P-frames; DD10 inline comment "NVENC honored CleanPoint" was wrong)
- Engram **#808** — Research finding: Chromium + FFmpeg both use `CODECAPI_AVEncVideoForceKeyFrame` BEFORE ProcessInput with `VT_UI4=1`; HCK-required for Win8+ hardware MFTs
- Engram **#809** — P2 BREAKTHROUGH: ForceKeyFrame BEFORE+VT_UI4 is **VENDOR-UNIFORM** (NVENC IDR at idx 0 len=49998; Intel QSV IDR at idx 1 len=8356, 1-frame in-flight latency well within tolerance)
- Engram **#186 v14** — sdd-init project context (note: contains the now-known-wrong "Mechanism G vendor-uniform" overclaim; corrigendum scheduled for v15)

---

## 1. Why

NVENC mid-stream IDR is the last gate before flipping the HW encoder feature flag default-on. Slice 5 closed Intel QSV mid-stream IDR via Mechanism G (drop+recreate the IMFTransform), believing the mechanism was vendor-uniform. Cross-vendor smoke and the C0.b probe falsified that claim on NVENC: Mechanism G's recreate sequence executes cleanly but NVENC emits 29/29 P-frames post-recreate. CleanPoint INPUT-write (the previously documented NVENC mechanism per the DD10 inline comment) was also falsified by Phase 0 P1 — 30/30 P-frames, no IDR.

Phase 0 P2 then validated `CODECAPI_AVEncVideoForceKeyFrame` via `ICodecAPI::SetValue` called BEFORE `ProcessInput` with `VT_UI4=1` as **vendor-uniform** (NVENC: IDR at idx 0; Intel QSV: IDR at idx 1, both within the existing 30-frame test tolerance). This is the canonical sequence used by Chromium and FFmpeg, and is HCK-mandated for Win8+ hardware encoder MFTs. It is faster than Mechanism G on Intel QSV, simpler than Mechanism G's 10-step recreate handler, and works on NVENC where Mechanism G fails.

This proposal LOCKS the AGGRESSIVE replacement option chosen by the user: replace Mechanism G + CleanPoint scaffolding + `EncoderVendor` IDR dispatch with a single vendor-uniform mechanism, and retroactively correct three cumulative Slice 4/5 architectural overclaims.

Success criteria:
- T7.1, T7.2 PASS on Host A (Intel QSV) AND Host B (NVENC) without `#[ignore]`.
- T8.2 (bitrate update without restart) remains PASS cross-vendor.
- Net LOC delta is negative (~−250 lines) — simplification, not addition.
- Mid-stream IDR latency: ~0ms on NVENC (idx 0), ~33ms on Intel QSV (idx 1 at 30fps).
- Slice 5 archive corrigendum captured in Slice 6 R2 archive (Slice 5 archive itself remains immutable historical record).
- Default-on flip becomes unblocked (out-of-scope to perform here; tracked separately).

---

## 2. What changes (production)

### ADD (some already on branch from Phase 0 Batch 2 at `beda9ed`)

- `force_keyframe_icodecapi_pending: AtomicBool` on `MftEncoderShared` (already present).
- `request_keyframe_via_force_keyframe_icodecapi()` method on the encoder (already present).
- ForceKeyFrame write path in `pump_loop` NeedInput service path BEFORE `submit_frame()`: when the flag is swapped from `true` to `false` via `swap(false, AcqRel)`, call `codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT { vt: VT_UI4, ulVal: 1 })` (already present).
- Simplify `request_keyframe()` trait impl to invoke `request_keyframe_via_force_keyframe_icodecapi()` directly (no vendor dispatch).
- Doc-comment block on `request_keyframe()` and on the pump_loop write site citing P2 evidence (#809), the research finding (#808), and the Microsoft HCK property table.
- `Slice 6 R2 corrigendum` block in the Slice 6 archive (apply phase) referencing the Slice 5 archive's "Mechanism G vendor-uniform" overclaim and pointing readers to #801, #807, #809.

### DELETE (the cleanup pass — see D5/D6/D7 for budget)

- `keyframe_recreate_pending: AtomicBool` on `MftEncoderShared`.
- `request_keyframe_via_recreate()` method.
- Mechanism G `pump_loop` handler block (the 10-step `END_OF_STREAM → COMMAND_DRAIN → NOTIFY_END_STREAMING → drop → ActivateObject → setup_mft → COMMAND_FLUSH → NOTIFY_BEGIN_STREAMING → NOTIFY_START_OF_STREAM → resume` sequence).
- `cleanpoint_pending: AtomicBool` on `MftEncoderShared`.
- `request_keyframe_via_cleanpoint()` method.
- CleanPoint INPUT-write block in `submit_frame()` (the `sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)` write, gated on `cleanpoint_pending`).
- DD10 inline comment block at `windows_mft.rs:1108-1110`. Replace with a comment block citing P2 (#809), HCK property table (#808), and the architectural lesson "vendor-specific claims require trace-level empirical validation".
- `EncoderVendor` enum **dispatch logic** in `request_keyframe()` (the `match self.vendor { … }` arm). The enum itself follows D-ENUM (retained for INFO logging only).
- The `mft_activate_factory: Option<IMFActivate>` field — only used by Mechanism G's recreate path. With G gone, the activate factory is not needed at runtime (the encoder is constructed once).
- Slice 5 DD4 — bitrate re-apply post-`setup_mft`. With recreate gone, there is no post-recreate moment requiring bitrate restoration; the SWAP-FIRE bitrate write path remains intact for normal mid-stream bitrate updates (T8.2 coverage).
- Slice 5 DD9 — trait routing comment block (replaced by a single-mechanism comment).
- Slice 5 round-3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` — see D9 (deletion alongside Mechanism G code).

### RETAIN (defense-in-depth and empirical history)

- `MFSampleExtension_CleanPoint` **READ** path in `collect_output` (decode-time NAL detection of clean points; the READ path was always correct — only the WRITE path was wrong). See D8.
- Phase 0 NVENC probes added during Slice 6 R2: P0 priming dump, P0.b post-recreate dump, P1 CleanPoint, P2-NVENC, P2-Intel — `#[ignore]`-gated regression evidence per DD7. See D13.
- `EncoderVendor` enum + GUID detection in `probe_and_select_mft` for **INFO logging only** (no behavioral dispatch). See D-ENUM option (b).

---

## 3. Decisions (D-list)

Each decision: locked option, alternatives, rationale citing the empirical evidence (engram observation IDs), risk/mitigation.

### D1 — Mechanism: ForceKeyFrame via ICodecAPI BEFORE ProcessInput (LOCKED)

- **Locked**: `ICodecAPI::SetValue(&CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` invoked BEFORE the corresponding `ProcessInput` call on the target frame. The `force_keyframe_icodecapi_pending` flag is consumed via `swap(false, AcqRel)` immediately preceding `submit_frame()` in the pump_loop NeedInput service path.
- **Alternatives considered**:
  - Mechanism G (drop + ActivateObject + setup_mft) — REJECTED. Falsified on NVENC (#801: 29/29 P-frames post-recreate). Slower on Intel QSV (~60-310ms recreate cost vs ~33ms 1-frame latency for ForceKeyFrame).
  - CleanPoint INPUT write — REJECTED. Falsified on NVENC (#807: 30/30 P-frames after CleanPoint=1 set on input sample). Per #808, CleanPoint is specified as an **output-side** attribute (encoder signals clean point downstream); using it on INPUT was non-standard and not honored by either vendor in production.
  - GOP-size toggle (`CODECAPI_AVEncMPVGOPSize` to 1 → frame → restore) — REJECTED. Untested in the project. Three ICodecAPI calls per IDR vs one. Race condition: GOP size change must be consumed BEFORE the IDR-target frame; timing with pump_loop is non-trivial.
  - Full re-init (DRAIN + new `IMFMediaType` + setup, no `ActivateObject`) — REJECTED. Speculative; #803 Candidate F. Mechanism G already performs a deeper reset (fresh COM object); a softer renegotiation is unlikely to succeed where G failed.
- **Rationale**: P2 (#809) directly validates this mechanism cross-vendor. Chromium (`media_foundation_video_encode_accelerator_win.cc:~2299`) and FFmpeg (`libavcodec/mfenc.c::mf_send_frame()`) both use this exact sequence as their production keyframe path on Windows MFTs. The HCK Win8+ certification (#808) requires hardware encoder MFTs to honor `CODECAPI_AVEncVideoForceKeyFrame`.
- **Risk/mitigation**: Driver variance could in principle affect behavior. Mitigation: HCK property is mandatory for certified hardware MFTs; Phase 0 probes (D13) retained as `#[ignore]`-gated regression evidence so any future regression surfaces immediately on Host A or Host B.

### D2 — Variant type: VT_UI4 with value 1 (LOCKED)

- **Locked**: `VARIANT { vt: VT_UI4, Anonymous: VARIANT_0 { Anonymous: VARIANT_0_0 { ulVal: 1, … } } }`.
- **Alternatives considered**:
  - `VT_BOOL=TRUE` — REJECTED. Slice 4 used this (Mechanism A SWAP-FIRE) and concluded "Intel QSV does not honor ForceKeyFrame mid-stream". Per #809 and #808, that conclusion was wrong: the variant type was incorrect.
- **Rationale**: Microsoft HCK property table (#808) specifies `VT_UI4` for `CODECAPI_AVEncVideoForceKeyFrame`. Chromium and FFmpeg both use `VT_UI4`. P2 (#809) validates `VT_UI4=1` as cross-vendor working.
- **Risk/mitigation**: Trivial — wrong variant fails fast (`E_INVALIDARG` on SetValue). Already confirmed working in Phase 0 P2 implementation at `beda9ed`.

### D3 — Timing: BEFORE ProcessInput on the target frame (LOCKED)

- **Locked**: Consume `force_keyframe_icodecapi_pending` via `swap(false, AcqRel)` and call `SetValue` immediately BEFORE the `submit_frame()` that wraps the next `ProcessInput`. This is the same loop iteration; no race window.
- **Alternatives considered**:
  - AFTER ProcessInput (Slice 4 SWAP-FIRE position) — REJECTED. Per Microsoft docs and #808: the property "applies to the next frame received as input." Calling AFTER `ProcessInput` targets the FOLLOWING frame, which arrives 1-frame later and may also race with the bitrate SWAP-FIRE write path. Empirically inferior in #809 evidence (Slice 4's negative verdict was based on this position).
- **Rationale**: Chromium calls `SetValue` BEFORE `ProcessInput` on the same frame. P2 (#809) validates this position cross-vendor.
- **Risk/mitigation**: None — this is the canonical position. The pump_loop NeedInput service path is single-threaded with respect to its own ProcessInput call; ordering is deterministic.

### D4 — Vendor dispatch ELIMINATED for IDR mechanism (LOCKED)

- **Locked**: `request_keyframe()` invokes `request_keyframe_via_force_keyframe_icodecapi()` directly with no `match self.vendor` arm. Single code path.
- **Alternatives considered**:
  - Per-vendor mechanism dispatch (NVENC → mechanism X, Intel QSV → mechanism Y) — REJECTED. P2 (#809) shows ForceKeyFrame works on both; vendor dispatch is unnecessary complexity.
- **Rationale**: Vendor-uniform empirical evidence (#809) eliminates the need for dispatch. Project history shows three architectural overclaims (Slice 4 Intel QSV, Slice 5 Mechanism G, Slice 5 DD10 NVENC CleanPoint) — every vendor-specific assumption has been falsified. The simplest correct primitive is the right answer.
- **Risk/mitigation**: If a future driver update breaks ForceKeyFrame on one vendor, vendor dispatch can be reintroduced surgically. The retained `EncoderVendor` enum (D-ENUM option (b)) makes that easy.

### D-ENUM — `EncoderVendor` enum disposition (LOCKED to option (b))

- **Locked**: Retain `EncoderVendor { IntelQsv, NvidiaNvenc, Unknown }` enum + GUID detection in `probe_and_select_mft`, but use it for INFO logging ONLY. Add a `WARN` log for `Unknown` to flag unrecognized hardware. No behavioral dispatch.
- **Alternatives considered**:
  - (a) Delete the enum entirely — REJECTED. Loses diagnostic value for future cross-vendor debugging (the project has spent two slices uncovering vendor-specific issues; the cost of retaining ~30 LOC of enum + detection is well below the cost of re-adding it later when a new vendor ships a different driver).
- **Rationale**: Diagnostic value is high; cost is low; retains a vendor-aware structural hook in case a future regression requires per-vendor dispatch again.
- **Risk/mitigation**: Logged vendor name is informational only. `Unknown` WARN draws attention to unrecognized hardware without affecting behavior.

### D5 — Mechanism G deletion (LOCKED)

- **Locked**: Delete `request_keyframe_via_recreate()`, the Mechanism G `pump_loop` handler block, and `keyframe_recreate_pending: AtomicBool`. Approximately 200 LOC deletion.
- **Alternatives considered**:
  - Retain Mechanism G as fallback for `Unknown` vendor — REJECTED. With ForceKeyFrame as vendor-uniform mechanism, fallback is dead code. `Unknown` vendors get the same ForceKeyFrame call as recognized ones; if it fails, the test failure surfaces in cross-vendor smoke before any release.
- **Rationale**: Mechanism G fails on NVENC (#801) and is slower than ForceKeyFrame on Intel QSV (~60-310ms recreate cost vs ~33ms 1-frame latency, #809). Deleting it removes ~200 LOC of brittle drop+recreate orchestration including Slice 5's race-fix (#787) and bitrate re-apply DD4.
- **Risk/mitigation**: Regression on Intel QSV — mitigated because P2-Intel (#809) proves ForceKeyFrame works on Intel QSV (idx 1 well within the 30-frame `assert_keyframe_within_next_n_frames` tolerance). Cross-vendor smoke gate before merge (D14) catches any regression.

### D6 — CleanPoint scaffolding deletion (LOCKED)

- **Locked**: Delete `request_keyframe_via_cleanpoint()`, `cleanpoint_pending: AtomicBool`, and the CleanPoint INPUT-write block in `submit_frame()`. Approximately 50 LOC deletion.
- **Alternatives considered**:
  - Retain as a future fallback — REJECTED. P1 (#807) falsified the mechanism on NVENC; project history says it never validated on Intel QSV either. Dead code; no value retaining.
- **Rationale**: Mechanism never validated working on either vendor. Inline comment "NVENC honored CleanPoint" was the only basis and is empirically wrong (#807).
- **Risk/mitigation**: None — deleted code was never on the production IDR path.

### D7 — DD10 inline comment correction (LOCKED)

- **Locked**: Delete the now-invalid comment block at `windows_mft.rs:1108-1110`. Replace with a comment citing:
  - P2 evidence (#809) that ForceKeyFrame BEFORE+VT_UI4 is vendor-uniform.
  - Research source (#808): Chromium + FFmpeg both use this exact sequence; HCK Win8+ certification mandates it.
  - Architectural lesson: "vendor-specific claims about MFT behavior require trace-level empirical validation, not inferred-from-logs reasoning."
- **Alternatives considered**:
  - Leave the old comment in place with a TODO — REJECTED. The comment is load-bearing wrong; future readers will defer to it. Direct correction is mandatory.
- **Rationale**: The DD10 comment caused two slices' worth of misdirection (Slice 5 deleted CleanPoint based on the wrong vendor assumption; Slice 6 R1 explored CleanPoint re-introduction based on the same comment). Correcting it definitively prevents repeat.
- **Risk/mitigation**: None.

### D8 — `MFSampleExtension_CleanPoint` READ path retained (LOCKED)

- **Locked**: Keep the existing `collect_output` read path that inspects `MFSampleExtension_CleanPoint` on output samples for IDR detection. Defense-in-depth alongside Annex-B AUD `primary_pic_type` parsing.
- **Alternatives considered**:
  - Delete the READ path alongside the WRITE path — REJECTED. Conflates two independent mechanisms. The READ path is correct (decode-time NAL detection of clean points signaled by the encoder); only the WRITE path was wrong.
- **Rationale**: The MS attribute is specified as output-side. Reading it is canonical; writing it on input was the bug.
- **Risk/mitigation**: None — proven correct in production.

### D9 — Slice 5 round-3 probe disposition (LOCKED to deletion)

- **Locked**: Delete `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` alongside Mechanism G code deletion. The probe directly invokes `request_keyframe_via_recreate()`; once that method is gone, the probe will not compile.
- **Alternatives considered**:
  - Retain as `#[ignore]`-gated archived probe with `#[allow(dead_code)]` and copies of the Mechanism G handler stitched in — REJECTED. Code that doesn't reflect production is a maintenance liability worse than gone code.
  - Migrate to a new probe testing the equivalent ForceKeyFrame mechanism — NOT NEEDED. P2-Intel (#809) already provides this evidence in `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput`.
- **Rationale**: The Slice 5 round-3 probe was empirical evidence that Mechanism G worked on Intel QSV. With Mechanism G deprecated and deleted, that evidence is historical (already captured in Slice 5 archive #791) and no longer relevant to current code. The new P2-Intel probe is the regression-evidence successor.
- **Risk/mitigation**: Slice 5 archive #791 retains the original empirical record. The Slice 6 archive will capture the deprecation context.

### D10 — T7.1 + T7.2 update (LOCKED)

- **Locked**: T7.1 and T7.2 already call `request_keyframe()` (the trait method); the routing change to ForceKeyFrame is internal. No test signature changes.
- **Verification rule**: Read T7.1 and T7.2 at apply time. If they assert `idx 0` strictness, relax to `assert_keyframe_within_next_n_frames(30)` (the existing helper) to accommodate Intel QSV's idx-1 in-flight latency. P2-Intel (#809) shows IDR at idx 1; this is well within tolerance and reflects real-world pipeline behavior.
- **Alternatives considered**:
  - Force idx-0 strictness everywhere — REJECTED. Intel QSV's pipeline carries 1 in-flight frame; idx 0 is impossible without flushing first (which would break test realism).
- **Rationale**: Existing 30-frame tolerance was set in Slice 5 with this exact pipeline-latency consideration in mind.
- **Risk/mitigation**: Cross-vendor smoke (D14) confirms both pass.

### D11 — Latency contract (LOCKED)

- **Locked**: Document the empirical latency contract on `request_keyframe()` doc-comment:
  - NVENC: ~0ms (IDR at post-request idx 0; len ~50KB SPS+PPS+IDR).
  - Intel QSV: ~33ms at 30fps (IDR at post-request idx 1; len ~8KB).
  - Both within the existing 30-frame test tolerance.
- **Alternatives considered**:
  - Leave latency undocumented — REJECTED. Slice 5 documented Mechanism G's ~60-310ms recreate cost; that figure is now obsolete and would mislead future readers. Updating the contract is mandatory.
- **Rationale**: Empirical evidence (#809). Sets accurate expectations for downstream callers (e.g., disconnect/reconnect IDR requests).
- **Risk/mitigation**: None.

### D12 — Slice 5 archive corrigendum (LOCKED)

- **Locked**: At Slice 6 R2 archive time, add a corrigendum section to the **Slice 6 R2 archive** (NOT to the Slice 5 archive itself; Slice 5 archive remains immutable as historical record). The corrigendum:
  - Names the three Slice 4/5 architectural overclaims:
    1. Slice 4: "Intel QSV does not honor mid-stream `CODECAPI_AVEncVideoForceKeyFrame`" — wrong. The variant was `VT_BOOL` instead of `VT_UI4` and the call was AFTER `ProcessInput` instead of BEFORE.
    2. Slice 5: "Mechanism G is vendor-uniform" — wrong on NVENC.
    3. Slice 5 DD10: "NVENC honored CleanPoint instead [of ForceKeyFrame]" — wrong on current NVENC driver.
  - Cites the falsification evidence: #800, #801, #807, #809.
  - Provides the canonical mechanism going forward (ForceKeyFrame BEFORE+VT_UI4) per Microsoft HCK + Chromium + FFmpeg precedent.
- **Locked, additional**: sdd-init v15 to incorporate the corrigendum into project context so future explores start with corrected architectural assumptions.
- **Alternatives considered**:
  - Modify the Slice 5 archive in place — REJECTED. Archives are immutable historical records by convention. Corrigenda go in the next slice's archive with explicit cross-references.
- **Rationale**: Project convention (#186) requires immutable archives and forward-pointing corrigenda.
- **Risk/mitigation**: None.

### D13 — Phase 0 probe retention policy (LOCKED)

- **Locked**: All Phase 0 probes added during Slice 6 R2 retained as `#[ignore]`-gated regression evidence per DD7:
  - `phase0_nvenc_idr_packet_format_dump` (priming format diagnostic).
  - `phase0_nvenc_post_recreate_idr_format_dump` (Mechanism G falsification, #801).
  - `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute` (CleanPoint falsification, #807).
  - `phase0_nvenc_force_keyframe_via_codecapi_before_processinput` (P2-NVENC success, #809).
  - `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput` (P2-Intel success, #809).
- **Probes deleted alongside Mechanism G code** (D9):
  - `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` (Slice 5 round 3; tests deleted code).
- **Alternatives considered**:
  - Delete all Phase 0 probes once mechanism locks in — REJECTED. They form the empirical history that documents WHY ForceKeyFrame is the right answer. They are the regression net for future driver/SDK changes.
  - Promote Phase 0 probes to non-ignored (run in CI) — REJECTED. They require hardware and produce ~40MB trace logs; CI runs Host A only. `#[ignore]`-gated runs on demand on Host A and Host B.
- **Rationale**: Project convention DD7 (Slice 5) established Phase 0 probes as `#[ignore]`-gated regression evidence. This slice extends that pattern.
- **Risk/mitigation**: None.

### D14 — Cross-vendor smoke required pre-merge (LOCKED)

- **Locked**: Both Host A (Intel QSV) AND Host B (NVENC) full smoke MUST be GREEN before PR opens. Specifically:
  - T7.1, T7.2, T8.2 PASS on both hosts.
  - All retained Phase 0 probes (D13) PASS on the host they target (NVENC probes on Host B; Intel QSV probes on Host A).
  - `cargo nextest run --workspace` PASS on both hosts.
  - `cargo clippy --all-targets --all-features --locked -- -D warnings` PASS on both hosts.
- **Alternatives considered**:
  - Host A only smoke before PR — REJECTED. The entire premise of Slice 6 R2 is that Host A-only validation is insufficient. Both hosts must gate.
- **Rationale**: Three architectural overclaims trace to insufficient cross-vendor validation pre-merge. This gate prevents repeat.
- **Risk/mitigation**: Slows merge cadence by ~1 day per slice. Acceptable cost given the alternative (ship a no-op fix again).

---

## 4. Test inventory delta

### Existing tests transitioning to PASS on both vendors

| Test | Host A (Intel QSV) | Host B (NVENC) | Notes |
|------|--------------------|----------------|-------|
| T7.1 `mft_request_keyframe_marks_next_packet_as_keyframe` | PASS (was passing via Mechanism G; passes via ForceKeyFrame) | PASS (was failing; passes with new mechanism) | Internal routing change; signature unchanged |
| T7.2 `mft_keyframe_flag_cleared_after_idr_emitted` | PASS (same as T7.1) | PASS | Internal routing change |
| T8.2 `mft_set_bitrate_updates_encoder_without_restart` | PASS (no change; SWAP-FIRE bitrate path intact) | PASS | Bitrate path is independent; recreate gone but bitrate write path stays |

### NEW unit tests (CI-runnable, no hardware)

- `force_keyframe_icodecapi_pending_flag_default_false` — verify default state of `AtomicBool`.
- `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true` — verify the trait method arms the flag.
- `force_keyframe_icodecapi_pending_swap_clears_to_false_after_consume` — verify `swap(false, AcqRel)` semantics.

### NEW integration tests (Host A + Host B)

- (Optional, may extend T7.1 instead) `mft_force_keyframe_via_codecapi_emits_idr_within_30_frames` — cross-vendor naming reflects the explicit mechanism.

### Phase 0 probes retained (D13)

- `phase0_nvenc_idr_packet_format_dump`
- `phase0_nvenc_post_recreate_idr_format_dump`
- `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute`
- `phase0_nvenc_force_keyframe_via_codecapi_before_processinput`
- `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput`

### Phase 0 probes deleted alongside Mechanism G (D9)

- `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` (Slice 5 round 3; tests deleted code).

---

## 5. LOC budget / PR strategy

| Bucket | Lines |
|--------|-------|
| ADD (production code, simplified dispatch + improved doc comments) | ~+50 |
| DELETE (Mechanism G handler + CleanPoint scaffolding + EncoderVendor IDR dispatch + DD10 comment + Slice 5 round-3 probe + DD4 bitrate-reapply + DD9 trait routing comment + `mft_activate_factory` field if confirmed Mechanism-G-only) | ~−300 |
| **Net** | **~−250 LOC** |

- Single PR. Under the 400-line single-PR budget after delta (deletions count differently in size budgets, but even without that the net is negative).
- `size:exception` NOT needed.
- Work-unit commit cadence: (1) delete CleanPoint scaffolding; (2) delete Mechanism G code path; (3) delete `EncoderVendor` IDR dispatch (retain enum for INFO); (4) replace DD10 comment + add ForceKeyFrame doc comment; (5) update T7.1/T7.2 if needed; (6) update Slice 6 R2 archive corrigendum.

---

## 6. Out of scope (explicit)

- **Default-on flip** — Slice 6 R2 closure unblocks it, but the flip itself is a separate slice.
- **AMD vendor support** — no Host C; not in scope for this slice.
- **Disconnect drain-once cosmetic** — deferred (project backlog).
- **Encoder-API changes beyond IDR mechanism** — bitrate, rate-control, GOP size, B-frames all out of scope. T8.2 bitrate path stays intact via SWAP-FIRE.
- **Per-vendor dispatch reintroduction** — D-ENUM retains the enum for diagnostic logging; no dispatch added.
- **NVENC SDK-specific properties** — out of scope. The HCK-mandated `CODECAPI_AVEncVideoForceKeyFrame` is sufficient and authoritative.

---

## 7. Risks (top 3)

### Risk 1 — Driver variance on `CODECAPI_AVEncVideoForceKeyFrame` (MEDIUM)

NVIDIA or Intel could ship a future driver update that changes ForceKeyFrame behavior. The HCK property is mandatory for Win8+ certification, but driver bugs do occur.

**Mitigation**: Phase 0 probes (D13) retained as `#[ignore]`-gated regression net. Cross-vendor smoke gate (D14) before any release. If a regression surfaces, the retained `EncoderVendor` enum (D-ENUM option (b)) makes it trivial to add per-vendor dispatch surgically without re-architecting.

### Risk 2 — Mechanism G deletion blast radius (MEDIUM)

~200 LOC deletion + DD4 bitrate-reapply removal + counters reset removal in pump_loop. Risk of collateral breakage in Intel QSV path (e.g., bitrate updates, drain-on-stop) if Mechanism G's incidental scaffolding shared state with other paths.

**Mitigation**: T8.2 (bitrate update without restart) PASS gate on both hosts catches bitrate regressions. Strict TDD nextest gate catches functional breakage. Clippy `-D warnings` catches dead-code reachability issues. Cross-vendor smoke (D14) is the final gate before merge.

### Risk 3 — Intel QSV idx-1 latency surprise to downstream callers (LOW)

If any production caller depends on Mechanism G's idx-0 latency on Intel QSV (e.g., an upstream test or RTC layer that expects the next packet to be IDR), ForceKeyFrame's idx-1 in-flight latency could be a regression.

**Mitigation**: Production callers consume packets in arrival order via `next_packet()`; idx 0 vs idx 1 is invisible to them. Test tolerance is 30 frames (`assert_keyframe_within_next_n_frames`). Latency contract documented on `request_keyframe()` (D11). Cross-vendor smoke (D14) confirms.

---

## 8. Phase 0 evidence summary (DONE)

Phase 0 of Slice 6 R2 is **COMPLETE**. Probes P0 (priming), P0.b (post-recreate), P1 (CleanPoint), P2-NVENC, and P2-Intel have produced the empirical evidence base for D1–D14. All implementation infrastructure for the locked mechanism (`force_keyframe_icodecapi_pending` flag, `request_keyframe_via_force_keyframe_icodecapi()` method, pump_loop write site) is already on the branch at `efc0f36` (Phase 0 Batch 2 commit `beda9ed`).

The apply phase needs **no additional Phase 0 work**. It can proceed directly to spec/design/tasks/apply with the empirical evidence locked.

---

## 9. References

- **Engram observations** (load-bearing empirical record): #800, #801, #803, #807, #808, #809, #186 v14.
- **Trace logs** (on disk): `nvenc-c0-trace.log`, `test.txt` (C0.b NVENC), `nvenc-p1-trace.log`, `nvenc-p2-trace.log.txt`, `intel-p2-trace.log`.
- **Production code reference**: Chromium `media_foundation_video_encode_accelerator_win.cc:~2299-2307`; FFmpeg `libavcodec/mfenc.c::mf_send_frame()`.
- **Microsoft documentation**: `https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avencvideoforcekeyframe` (HCK Win8+ certification property).
- **Project history**: Slice 4 archive (DD1 SWAP-FIRE Mechanism A — wrong timing, wrong variant); Slice 5 archive #791 (Mechanism G, "vendor-uniform" overclaim falsified by #801).
- **Branch state**: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` @ `efc0f36`. Already contains: P0/P0.b/P1/P2 probes, CleanPoint scaffolding (Batch 1, to be deleted), ForceKeyFrame implementation (Batch 2, retained).

---

## 10. Next phases

`sdd-spec` and `sdd-design` can run in parallel. Both depend only on this proposal.

- **`sdd-spec`** — formalize the requirement contract: `request_keyframe()` MUST cause an IDR within N frames cross-vendor; specific test scenarios; latency expectations; failure modes.
- **`sdd-design`** — formalize the implementation contract: where the flag lives, how it's consumed, exact `VARIANT` construction, interaction with `submit_frame()` and bitrate SWAP-FIRE, error handling on `SetValue` failure.

After spec + design land, `sdd-tasks` produces the work-unit task list, then `sdd-apply` executes the cleanup pass.
