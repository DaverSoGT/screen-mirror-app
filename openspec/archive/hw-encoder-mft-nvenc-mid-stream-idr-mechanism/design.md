# Design: hw-encoder-mft-nvenc-mid-stream-idr-mechanism (Slice 6 R2)

> Phase: SDD design.
> Branch: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism` @ `efc0f36` (off master `c48ae46`).
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design`
> + `openspec/changes/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design.md`).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-10.
> Inputs: proposal #810 (D1–D14 + D-ENUM), spec #811 (R1–R18 + S1–S21),
>         P2 breakthrough #809, research #808, explore #803, falsifications #800/#801/#807,
>         Slice 5 design #784 (DD format template + Mechanism G architecture being deleted).

---

## Executive summary

Slice 6 R2 collapses three competing mid-stream IDR mechanisms (Mechanism G recreate, CleanPoint INPUT write, ICodecAPI ForceKeyFrame) into ONE vendor-uniform mechanism: `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)` invoked **BEFORE** `submit_frame()`/`ProcessInput`. The implementation already exists on branch (Phase 0 Batch 2, `beda9ed`); this slice is a **cleanup pass** that deletes the dead Mechanism G + CleanPoint + EncoderVendor IDR-dispatch scaffolding while retaining the vendor enum for diagnostic logging and the CleanPoint READ path for output-side IDR detection. Net delta ~−250 LOC. Single PR. The Slice 5 archive remains immutable; the Slice 6 R2 archive carries a corrigendum naming the three Slice 4/5 architectural overclaims this slice retracts.

---

## 1. Architecture chosen

**Pattern**: One-shot atomic flag (`AtomicBool`) consumed by the encoder pump-loop NeedInput service path, gating an `ICodecAPI::SetValue` call BEFORE `IMFTransform::ProcessInput`. The flag is set from the public surface (`request_keyframe()` trait method); pump_loop is the sole reader.

**Layering**:

```
sm_domain::VideoEncoder        (FROZEN — trait surface unchanged)
        │
        ▼
WindowsMftH264Encoder::request_keyframe()        — single AtomicBool::store(true, Release)
        │ Arc<MftEncoderShared>
        ▼
MftEncoderShared.force_keyframe_icodecapi_pending: AtomicBool   — sole IDR signal
        │
        ▼
pump_loop NeedInput service path:
    swap(false, AcqRel)   — one-shot consume
        │ if true
        ▼
    codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &make_variant_u32(1))
        │
        ▼
    submit_frame() → IMFTransform::ProcessInput        (BEFORE: timing is load-bearing)
```

**Boundaries**:

- IDR signaling is **internal to `windows_mft.rs`**. No new public API; `request_keyframe()` trait method is the only entry point.
- The three currently-existing "named" methods (`request_keyframe_via_recreate`, `request_keyframe_via_cleanpoint`, `request_keyframe_via_force_keyframe_icodecapi`) collapse to one (DD4); the trait body inlines its single line of work (DD1).
- `EncoderVendor` enum stays in scope for INFO/WARN logging (DD5), behavioral dispatch deleted.
- `MFSampleExtension_CleanPoint` import is retained — used by `collect_output` for output-side IDR detection (DD7); WRITE-side import (`MFSampleExtension_CleanPoint` set on input samples) is removed alongside the CleanPoint scaffolding (DD3).

**Why this shape**:

1. **Empirical**: P2 traces (#809) — NVENC IDR at idx 0, Intel QSV IDR at idx 1, both within 30-frame test tolerance.
2. **Canonical**: Chromium `media_foundation_video_encode_accelerator_win.cc:2299-2307` and FFmpeg `libavcodec/mfenc.c::mf_send_frame()` both use this exact sequence (#808).
3. **HCK-mandated**: `CODECAPI_AVEncVideoForceKeyFrame` is REQUIRED Win8+ hardware encoder MFT certification property (#808).
4. **Simplicity**: Vendor dispatch elimination collapses three code paths into one; net delta is ~−250 LOC.

---

## 2. Components / data flow

### Component map (verified at branch tip `efc0f36`)

```
crates/sm-infra/src/encode/windows_mft.rs
├── MftEncoderShared                                              (DD2)
│   ├── pending_bitrate           : AtomicU32                     (RETAIN — Slice 4 SWAP-FIRE)
│   ├── dropped                   : AtomicU64                     (RETAIN)
│   ├── stop                      : AtomicBool                    (RETAIN)
│   ├── drain_pending             : AtomicBool                    (RETAIN — Slice 4 DD17/F2)
│   ├── keyframe_recreate_pending : AtomicBool                    (DELETE — Mechanism G)
│   ├── cleanpoint_pending        : AtomicBool                    (DELETE — Slice 6 R2 Batch 1)
│   └── force_keyframe_icodecapi_pending : AtomicBool             (RETAIN — sole IDR flag)
│
├── EncoderVendor enum + from_clsid_str()                         (RETAIN — diagnostic only, DD5)
│
├── WindowsMftH264Encoder
│   ├── vendor: EncoderVendor                                     (RETAIN — INFO logging only, DD5)
│   ├── mft_activate_factory: Option<IMFActivate>                 (DELETE — only Mechanism G reads it, DD2)
│   ├── ...
│   ├── new()                                                     (RETAIN — vendor still set for logging)
│   ├── start()                                                   (DD2 — drop the .take() of activate_factory; reroute via the start path used today; see implementation note below)
│   ├── request_keyframe() trait impl                             (DD1 — single-line, no dispatch)
│   ├── request_keyframe_via_recreate()                           (DELETE — Mechanism G, DD4)
│   ├── request_keyframe_via_cleanpoint()                         (DELETE — Batch 1, DD4)
│   └── request_keyframe_via_force_keyframe_icodecapi()           (KEEP — pub for test access, DD4 + DD13)
│
├── pump_loop                                                     (DD3)
│   ├── NeedInput service path (line ~1486-1652)
│   │   ├── swap_pending_codec_settings (bitrate)                (RETAIN — Slice 4 DD1)
│   │   ├── force_cleanpoint = cleanpoint_pending.swap(...)      (DELETE)
│   │   ├── if force_keyframe_icodecapi_pending.swap(...) {       (RETAIN — sole IDR write)
│   │   │     codec_api.SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_UI4=1)
│   │   │   }
│   │   ├── submit_frame(force_cleanpoint = ...)                  (DD3 — drop force_cleanpoint param)
│   │   └── fire_pending_codec_settings (bitrate)                (RETAIN — Slice 4 DD1)
│   │
│   ├── drain_pending handler                                     (RETAIN — Slice 4 DD17/F2)
│   └── keyframe_recreate_pending handler (Mechanism G ~150 LOC)  (DELETE — DD3)
│
├── submit_frame                                                  (DD3 — drop force_cleanpoint param + body)
├── collect_output                                                (RETAIN — DD7, READ MFSampleExtension_CleanPoint)
├── make_variant_u32                                              (RETAIN — used by ForceKeyFrame and bitrate)
└── CodecApiSwap / swap_/fire_/restore_pending_codec              (RETAIN — bitrate channel only)

crates/sm-infra/tests/windows_mft_encode.rs
├── Top-of-file Phase 0 inventory comment                         (DD10 — NEW, lists 5 retained probes)
├── T7.1 mft_request_keyframe_marks_next_packet_as_keyframe       (DD9 — body unchanged; uses helper N=30)
├── T7.2 mft_keyframe_flag_cleared_after_idr_emitted              (DD9 — body unchanged; uses helper N=30)
├── T8.2 mft_set_bitrate_updates_encoder_without_restart          (RETAIN — bitrate path independent)
├── assert_keyframe_within_next_n_frames helper                    (RETAIN — Slice 5 helper)
├── phase0_intel_qsv_idr_via_drain_resume_*  (round-1, ×2)        (DELETE — DD8)
├── phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr (DELETE — DD8, Slice 5 round-3)
├── phase0_nvenc_idr_packet_format_dump                           (RETAIN — DD10)
├── phase0_nvenc_post_recreate_idr_format_dump                    (RETAIN — DD10)
├── phase0_nvenc_cleanpoint_idr_via_input_sample_attribute        (RETAIN — DD10, P1 falsification)
├── phase0_nvenc_force_keyframe_via_codecapi_before_processinput  (RETAIN — DD10, P2-NVENC success)
└── phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput (RETAIN — DD10, P2-Intel success)
```

### Data flow per `request_keyframe()` call

```
caller → request_keyframe() trait impl
       │
       ▼
state.force_keyframe_icodecapi_pending.store(true, Release)        [DD1]
       │
pump_loop iteration N (NeedInput service path):
       │
       ├─ recv_timeout(FRAME_RECV_TIMEOUT) → frame
       ├─ nv12_convert(...)
       │
       ├─ IF force_keyframe_icodecapi_pending.swap(false, AcqRel):  [DD3]
       │     codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame,
       │                        &make_variant_u32(1))
       │     // SetValue rejection: WARN + continue (R4, DD13 carry-forward)
       │
       ├─ submit_frame(&mft, &nv12, ts, dur)                         [DD3 — no force_cleanpoint]
       │     // ProcessInput receives the frame; per MS spec the property is
       │     // applied to THIS frame because we set it BEFORE ProcessInput.
       │
       └─ fire_pending_codec_settings (bitrate FIRE)                  [Slice 4 DD1, RETAIN]

(next iteration)
       └─ ProcessOutput → packet idx 0 (NVENC) or idx 1 (Intel QSV) is_keyframe=true
```

### Integration points

- **Trait surface**: `VideoEncoder::request_keyframe(&self)` — body is one line; signature unchanged (FROZEN per spec §6).
- **Test surface**: `WindowsMftH264Encoder::request_keyframe_via_force_keyframe_icodecapi()` — `pub` inherent method retained for direct probe access (DD13).
- **Public-API contract**: SWAP-FIRE bitrate channel (Slice 4 DD1) is unchanged; T8.2 stays GREEN.

---

## 3. Decisions (DD-*)

> Format follows Slice 5 design #784 — each DD references the proposal D-* it implements
> and the spec R-* it satisfies. Rejected alternatives are recorded inline.

### DD1 — `request_keyframe()` trait body collapses to one atomic store

**Choice**: Replace the current `match self.vendor { ... }` body (lines 396–405) with:

```rust
fn request_keyframe(&self) {
    self.state
        .force_keyframe_icodecapi_pending
        .store(true, Ordering::Release);
}
```

The doc-comment cites D11 latency contract (NVENC idx 0 ~0ms; Intel QSV idx 1 ~33ms; both within 30-frame tolerance), research #808 (Chromium/FFmpeg canonical sequence + HCK requirement), and P2 evidence #809.

**Implements**: D1 (mechanism), D4 (vendor dispatch eliminated), D11 (latency contract).
**Satisfies**: R1, R9, R10. Scenarios S1, S2, S11.

**Rejected alternatives**:
- (a) Keep `match self.vendor` arm but route both vendors to the same body — pure dead code, fails R9 ("MUST NOT drive IDR mechanism dispatch").
- (b) Keep `request_keyframe_via_force_keyframe_icodecapi()` as the trait body — WORKS but DD4 reasoning (YAGNI) prefers the inline.

---

### DD2 — `MftEncoderShared` field cleanup + factory deletion

**Choice**:

- DELETE field `keyframe_recreate_pending: AtomicBool` (line 186 + Default::new line 236).
- DELETE field `cleanpoint_pending: AtomicBool` (line 200 + Default::new line 237).
- RETAIN field `force_keyframe_icodecapi_pending: AtomicBool` (line 226 + Default::new line 238) — now the sole IDR flag.
- DELETE field `mft_activate_factory: Option<IMFActivate>` from `WindowsMftH264Encoder` (line 281).

**Audit (grep-confirmed at `efc0f36`)**:

| Symbol | Reads outside Mechanism G? | Decision |
|--------|----------------------------|----------|
| `keyframe_recreate_pending` | None — only read at line 1689–1692 (G handler) and unit test 2201 | DELETE field + unit test |
| `cleanpoint_pending` | None — only read at line 1537 (CleanPoint write branch) and unit test 2230 | DELETE field + unit test |
| `mft_activate_factory` | (a) `start()` line 363 `.take()` — must be re-routed to a single-shot owned `IMFActivate` field; (b) Drop body line 430 `take()` cleanup; (c) test ctor 2292 | DELETE the multi-call factory pattern; the encoder still owns ONE `IMFActivate` consumed by `start()` because the destructive-probe pattern is unchanged — see implementation note below |

**Implementation note for `mft_activate_factory` deletion**: The factory pattern (`Option<IMFActivate>` retained across recreate cycles) was introduced in Slice 5 to support Mechanism G's repeat `ActivateObject` calls. With G deleted, the encoder consumes `IMFActivate` exactly once during `start()` — which is the original Slice 3/4 ownership model. Two safe shapes apply:

- **(a) Keep the field name**, keep `Option<IMFActivate>` for the take-on-start pattern, but DELETE all G-handler references. Net delete = G handler block + flag field + factory references in pump_loop. Field rename to `winning_activate` (Slice 4 name) is cosmetic — not in scope.
- **(b) Rename + reshape**: delete factory and reuse `winning_activate` name. Apply phase will choose; design recommends **(a) keep the field, rename optional**, because rename is mechanically clean but adds noise to the diff.

**Implements**: D5 (Mechanism G deletion), D6 (CleanPoint deletion).
**Satisfies**: R5 (Mechanism G symbol absence), R6 (CleanPoint symbol absence). Scenarios S6, S7.

---

### DD3 — pump_loop NeedInput service path simplification

**Choice**: pump_loop NeedInput service path keeps the existing structure but with three precise edits inside the `Ok(frame) => { ... }` arm (lines 1512–1625):

1. **DELETE** the CleanPoint swap line (lines 1531–1537) — the `let force_cleanpoint = state.cleanpoint_pending.swap(...)` block including the GUARD-BEFORE-SWAP doc-comment.
2. **RETAIN** the ForceKeyFrame swap+SetValue block (lines 1539–1586) verbatim — this is the load-bearing IDR write.
3. **DELETE** the `force_cleanpoint` argument from the `submit_frame(...)` call (line 1593) and from the `submit_frame` function signature (line 1931). Also delete the `force_cleanpoint` body block in `submit_frame` (lines 1934–1950).

**OUTSIDE the Ok-arm**: DELETE the entire Mechanism G handler block (lines 1670–1877) — from the `// Mechanism G: consume...` comment through the `tracing::info!("Mechanism G ... resume")` line. This is ~205 LOC of pump_loop body.

**Ordering invariant** (RETAINED): `swap` BEFORE `submit_frame` → ProcessInput. This is the canonical Chromium/FFmpeg sequence. The existing comment block at lines 1539–1558 already documents this — it stays.

**GUARD-BEFORE-SWAP lesson preserved**: The CleanPoint block (deleted) used the simple swap form because there is no drain window on the INPUT path. The same reasoning applies to the retained ForceKeyFrame block: NO `!draining` guard is needed — the swap is consumed only when a frame is being submitted. The Slice 5 race-fix lesson (#787, GUARD-BEFORE-SWAP for the recreate gate) is referenced ONLY to justify why the simple swap form is correct on the input path. The new DD6 inline comment (below) cites this lineage.

**Implements**: D1 (mechanism), D3 (BEFORE timing), D5 (Mechanism G handler block delete).
**Satisfies**: R2 (pump_loop swap+SetValue BEFORE submit_frame), R5 (Mechanism G handler delete), R6 (CleanPoint write delete). Scenarios S3, S4, S6, S7.

**Rejected alternatives**:
- Keep `force_cleanpoint` arg as `bool = false` for forward-compatibility — adds dead-code warnings; rejected.
- Convert ForceKeyFrame block to a helper function — premature; one-call site, prefer inline.

---

### DD4 — Inherent `request_keyframe_via_*` method cleanup

**Choice**:

- DELETE `request_keyframe_via_recreate()` (lines 2199–2203).
- DELETE `request_keyframe_via_cleanpoint()` (lines 2228–2232).
- **KEEP** `request_keyframe_via_force_keyframe_icodecapi()` (lines 2261–2265) **as `pub`** for direct test/probe access.

Doc-comment on the kept method is updated to remove the "Phase 0 probe escape hatch" framing and instead document it as the production mechanism, with a NOTE that the `VideoEncoder::request_keyframe()` trait body is functionally identical (calls the same atomic store) and is the preferred entry point for non-test callers.

**Why KEEP not inline-and-delete**:
- The two retained P2 probes (NVENC + Intel QSV) and the falsification probes (P0.b, P1) call this method DIRECTLY. Removing it forces probes to construct shared state via a non-public path or duplicate the atomic store call.
- One-line bodies are cheap; the named method is a readable test affordance and a documented mechanism citation point.
- YAGNI applies to `_via_recreate` and `_via_cleanpoint` (their mechanisms are deleted); it does NOT apply to the one mechanism we keep.

**Implements**: D5, D6, D-ENUM.
**Satisfies**: R5, R6, R14 (probes still compile against the kept method).

**Rejected alternative**: Inline the body into the trait method and delete the inherent method. The two `_via_recreate` and `_via_cleanpoint` deletions are clear; this third one trades 5 LOC for losing the named test affordance — not worth it.

---

### DD5 — `EncoderVendor` enum: retain for diagnostic logging only

**Choice**: RETAIN the `EncoderVendor` enum (lines 134–160) and the `vendor: EncoderVendor` field (line 272). RETAIN GUID-based detection in `probe_and_select_mft` (lines 721–745). RETAIN INFO/WARN logging at probe time. DELETE the behavioral consumer at `request_keyframe()` (lines 401–404) — that becomes DD1.

**Audit**: `Grep self.vendor|state.vendor|.vendor` returns exactly two matches at `efc0f36`:
- Line 401 (the dispatch branch — DELETED by DD1).
- Line 2215 (the doc-comment for `request_keyframe_via_cleanpoint` — DELETED with the method by DD4).

Zero behavioral consumers remain after DD1+DD4. The field is read ONLY via the logging closure inside `probe_and_select_mft` BEFORE `WindowsMftH264Encoder` is constructed (the value flows from `init_mft_sync` → `Self { vendor, ... }`).

**Implements**: D-ENUM (option b).
**Satisfies**: R9. Scenarios S10.

**Rejected alternatives**:
- (a) Delete the enum entirely — saves ~30 LOC but loses the cross-vendor log signal that has been load-bearing for triage in Phases 0 + falsifications. The `Unknown` WARN is a future-proofing hook for new GPU vendors.

---

### DD6 — DD10 inline-comment block replacement

**Audit** (corrects a proposal/spec address): the proposal and spec both cite `windows_mft.rs:1108-1110` for the DD10 comment block; the actual current location at `efc0f36` is **lines 1236–1238**, inside the `fire_pending_codec_settings` doc-comment:

```rust
/// DD10: `CODECAPI_AVEncVideoForceKeyFrame` SetValue branch removed — Intel QSV does
/// not honor mid-stream ICodecAPI ForceKeyFrame; NVENC honored CleanPoint instead.
/// Both are deleted. Mid-stream IDR is produced exclusively by Mechanism G.
```

The comment shifted because pre-existing edits at `1108-1110` (setup_mft signaling block) are unrelated. The design treats this as a precise grep target: the string `Intel QSV does\n/// not honor` and `NVENC honored CleanPoint instead` is unique in the file (grep returns one match each at lines 1236–1237).

**Choice**: DELETE the 3-line DD10 comment block at lines 1236–1238. REPLACE with a 10-line block that captures the Slice 6 R2 finding:

```rust
/// Slice 6 R2: `CODECAPI_AVEncVideoForceKeyFrame` is the vendor-uniform mid-stream IDR
/// mechanism. Called BEFORE `IMFTransform::ProcessInput` with `VARIANT { vt: VT_UI4, ulVal: 1 }`.
/// Empirical evidence: Phase 0 P2 (engram #809) — NVENC IDR at idx 0 (immediate) and
/// Intel QSV IDR at idx 1 (1-frame in-flight latency, within `assert_keyframe_within_next_n_frames(30)`).
/// HCK Win8+ certification mandates this property for hardware encoder MFTs (research #808).
/// Production references: Chromium `media_foundation_video_encode_accelerator_win.cc` +
/// FFmpeg `libavcodec/mfenc.c::mf_send_frame()` use the identical sequence.
/// Slice 5 archive corrigendum (Slice 6 R2 archive): three architectural overclaims
/// retracted — Slice 4 "Intel QSV does not honor ForceKeyFrame" (wrong timing AFTER +
/// wrong VARIANT VT_BOOL); Slice 5 "Mechanism G is vendor-uniform" (#801 falsified on NVENC);
/// Slice 5 DD10 "NVENC honored CleanPoint" (#807 falsified, 30/30 P-frames).
```

The replacement comment goes ABOVE the `fn fire_pending_codec_settings(...)` signature (line 1239), preserving the "DD10 lineage" trail for `git blame`. The two earlier DD10 comments at lines 1202 and 1217 (in `CodecApiSwap` and `swap_pending_codec_settings`) lose their relevance once the deletions land — mark them as `// (Slice 6 R2: DD10 history retained — IDR is now via force_keyframe_icodecapi_pending; bitrate-only SWAP-FIRE here.)`.

The comment at line 2052 inside `collect_output` (`// DD10: read is_keyframe from MFSampleExtension_CleanPoint attribute.`) is a READ-path comment — DD7 retains it unchanged.

The comment at line 2139 (`// DD10 note: make_variant_bool() remains deleted ...`) becomes obsolete because its current text references a "still unneeded" claim that is wrong now (we DO use `make_variant_u32(1)` for ForceKeyFrame). Update to: `// Slice 6 R2: make_variant_u32(1) is used for both bitrate and ForceKeyFrame; VT_BOOL is unused (Slice 4 falsification, research #808).`

**Implements**: D7.
**Satisfies**: R8. Scenarios S9.

**Rejected alternatives**:
- Single-line replacement — too terse; the corrigendum is load-bearing context for future maintainers.
- Move the new block to a module-level rustdoc — pollutes the unrelated `// ── Pump-loop timing constants ──` section; the function-level location at line 1239 is the natural read target during `fire_pending_codec_settings` review.

---

### DD7 — `MFSampleExtension_CleanPoint` READ path retention

**Choice**: RETAIN unchanged the READ at line 2056 of `collect_output`:

```rust
let clean_point = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) } != 0;
let is_keyframe = clean_point || annex_b_contains_idr(&annex_b);
```

RETAIN the import `MFSampleExtension_CleanPoint` at line 67. The use-comment at line 64 (`// MFSampleExtension_CleanPoint: used for BOTH the READ path in collect_output (IDR detection) ... [WRITE part]`) loses its WRITE arm — update to: `// MFSampleExtension_CleanPoint: used by collect_output() for output-side IDR detection. WRITE path was deleted in Slice 6 R2 (#809 falsified the WRITE direction on both vendors); READ path is defense-in-depth alongside annex_b_contains_idr scanning.`

**Implements**: D8.
**Satisfies**: R7. Scenarios S8.

**Rejected alternatives**:
- DELETE the READ — would regress IDR detection on output samples that DO carry CleanPoint=1 (the encoder's natural clean-point signaling). The READ side was never falsified.
- Replace with a `debug_assert!` that bytes contain IDR NAL — adds runtime overhead in test builds, no production benefit.

---

### DD8 — Phase 0 probe deletions

**Choice**: DELETE the Slice 5 round-3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` (line 2013, ~280 LOC including doc + body). It calls `enc.request_keyframe_via_recreate()` directly (line 2117 grep hit) — the method is deleted by DD4, so the probe will not compile.

**Audit of round-1 probes** (`phase0_intel_qsv_idr_via_drain_resume_*` at lines 1653 and 1819):
- They call `enc.flush()` only (no `request_keyframe_via_*`).
- They compile cleanly even after Mechanism G + CleanPoint deletions.
- They are NOT in spec §R14 retention list.

**Decision on round-1 probes**: DELETE. Reasoning:
1. Round-1 probes empirically validated `flush()`-as-IDR-trigger on Intel QSV in Slice 5 R0/R1 evidence (#779). With Slice 6 R2 establishing `force_keyframe_icodecapi_pending` as the sole IDR mechanism, those probes test a code path (`flush()` → COMMAND_DRAIN → resume) that is no longer the IDR trigger — `flush()` is now strictly a drain affordance (Slice 4 DD17/F2).
2. They are NOT mentioned in the proposal RETAIN list (proposal §2 RETAIN: only the 5 "Phase 0 R2 probes"). Spec §R14 lists the 5 specific names.
3. Their `#[ignore]` text references "Mechanism C drain+resume" — a name that no longer maps to current architecture, so retention is misleading.
4. Their empirical record is preserved in the Slice 5 archive (#791) and engram observation #779.

**Implements**: D9 (Slice 5 round-3 probe deletion); proposal §RETAIN limit (round-1 probes are NOT retained).
**Satisfies**: R15. Scenarios S17.

**Rejected alternative**:
- Keep round-1 probes `#[ignore]`-gated as historical evidence. Rejected because (1) the spec retention list is explicit and excludes them; (2) misleading test names violate Slice 5 lesson #787 ("inline comments are not empirical evidence"); (3) the historical record is the engram observation, not the test code.

---

### DD9 — T7.1 + T7.2 inspection

**Audit at `efc0f36`** (read lines 366–498 for T7.1 and 521–684 for T7.2 — only the relevant constants + helper invocations are quoted below):

```rust
// T7.1, lines 374–378:
const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const IDR_TOLERANCE: usize = 30;
// ...
// T7.1, line 493:
let idr_idx = assert_keyframe_within_next_n_frames(&post_pkts, IDR_TOLERANCE);
```

T7.1 already uses `IDR_TOLERANCE = 30` with the eventually-style helper (Slice 5 design DD8 carry-forward). T7.2 mirrors this pattern (constants at lines 528–532, helper at line ~660). No body changes are required for the new mechanism — the routing change is internal (DD1 collapses the trait body).

**Choice**:
- Apply phase MUST remove `#[ignore]` on T7.1 (line 365) AND T7.2 (line 520) on **both Host A and Host B** runs. Both tests already use the eventually-style assertion that accommodates NVENC idx 0 and Intel QSV idx 1.
- Apply phase MUST update the doc-comments at lines 357–363 (T7.1 "WHY this test is EXPECTED to FAIL on Host A at C1") and lines 513–518 (T7.2 mirror) — both reference "Slice 5 Mechanism G" and "C1 RED state". Replace with Slice 6 R2 framing: routing is `request_keyframe()` → `force_keyframe_icodecapi_pending` → ForceKeyFrame BEFORE ProcessInput; expected PASS on both vendors per #809.
- The `#[ignore]` annotation message ("Slice 5 Mechanism G — runs on Host A only with --run-ignored") becomes wrong; either delete the annotation entirely (if the test is CI-runnable on hardware) or update the message. Deletion is preferred per spec R11/R12.

**Implements**: D10 (no signature changes), D14 (cross-vendor gate).
**Satisfies**: R11, R12. Scenarios S12, S13, S14, S15.

**Note**: This is a READ-only inspection at design phase. Apply phase performs the edits; tasks phase enumerates the edit work units.

---

### DD10 — Phase 0 probe retention + top-of-file inventory comment

**Choice — retained probes (DELIBERATE)**:

| # | Probe | Origin | Purpose post-Slice-6-R2 |
|---|-------|--------|------------------------|
| 1 | `phase0_nvenc_idr_packet_format_dump` | Slice 6 P0 (#800) | NVENC priming-IDR format diagnostic (`raw_prefix=[00,00,00,01,09,10]`, AUD primary_pic_type=0x10). Defense-in-depth against future NAL-detection regressions. |
| 2 | `phase0_nvenc_post_recreate_idr_format_dump` | Slice 6 P0.b (#801) | Mechanism G falsification on NVENC (29/29 P-frames post-recreate). Empirical anchor for D5 deletion. |
| 3 | `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute` | Slice 6 R2 P1 (#807) | CleanPoint INPUT falsification on NVENC (30/30 P-frames). Empirical anchor for D6 deletion. |
| 4 | `phase0_nvenc_force_keyframe_via_codecapi_before_processinput` | Slice 6 R2 P2-NVENC (#809) | P2 success evidence on NVENC (IDR idx 0). Empirical anchor for D1 mechanism. |
| 5 | `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput` | Slice 6 R2 P2-Intel (#809) | P2 success evidence on Intel QSV (IDR idx 1, retroactive correction of Slice 4 verdict). Empirical anchor for D1 + D12 corrigendum. |

All 5 stay `#[ignore]`-gated (`#[ignore = "Phase 0 trace probe — manual run on Host {A|B}"]`) — they are regression evidence, not CI-runnable assertions.

**NEW: Top-of-file inventory comment** in `crates/sm-infra/tests/windows_mft_encode.rs` (above the `use ...` block, ~line 1):

```rust
// ─── Phase 0 trace-probe inventory (Slice 6 R2 — informational) ──────────────────
//
// The five probes below are `#[ignore]`-gated regression evidence for the Slice 6 R2
// mid-stream IDR mechanism. They are NOT CI-runnable; they require Host A (Intel QSV)
// or Host B (NVENC) hardware. Each probe maps to an engram observation:
//
//   1. phase0_nvenc_idr_packet_format_dump                              → engram #800 (P0)
//   2. phase0_nvenc_post_recreate_idr_format_dump                       → engram #801 (P0.b)
//   3. phase0_nvenc_cleanpoint_idr_via_input_sample_attribute           → engram #807 (P1)
//   4. phase0_nvenc_force_keyframe_via_codecapi_before_processinput     → engram #809 (P2-NVENC)
//   5. phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput → engram #809 (P2-Intel)
//
// Probes 1–3 are FALSIFICATION evidence (Mechanism G + CleanPoint on NVENC); probes 4–5
// are SUCCESS evidence (vendor-uniform ForceKeyFrame BEFORE ProcessInput, VT_UI4=1).
// Reference: Slice 6 R2 archive corrigendum + research #808 (Chromium/FFmpeg/HCK).
//
// Slice 5 round-1 (`phase0_intel_qsv_idr_via_drain_resume_*`) and round-3
// (`phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`) probes were
// DELETED in Slice 6 R2 — they tested mechanisms that are no longer the IDR trigger
// (Mechanism G removed) or symbols that no longer exist. Historical record:
// Slice 5 archive (#791) + engrams #779, #780, #783.
// ──────────────────────────────────────────────────────────────────────────────────
```

**Implements**: D13 (probe retention).
**Satisfies**: R14. Scenarios S17.

**Rejected alternatives**:
- Delete the falsification probes (P0.b, P1) — loses the empirical falsifications that retroactively justify the corrigendum. Cheap to keep; high diagnostic value if a future driver regresses.
- Move the inventory comment into a separate `phase0_inventory.md` doc — fragments context; better next to the code where reviewers look.

---

### DD11 — Slice 5 archive corrigendum mechanics

**Choice**: The corrigendum is authored as part of the **Slice 6 R2 archive-report**, NOT as a modification to Slice 5 archive. Three operational rules:

1. **Slice 5 archive immutability**: `openspec/archive/hw-encoder-mft-intel-qsv-mid-stream-idr/archive-report.md` and engram observation #791 (Slice 5 archive-report topic) MUST NOT be edited. They are the authoritative historical record of what Slice 5 believed at merge time.
2. **Corrigendum location**: A new section `## Retroactive Corrections to Slice 5` lives inside the Slice 6 R2 archive-report (created by the future archive phase). Engram topic_key for that report: `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/archive-report`.
3. **Three overclaims to retract** (locked from spec §9):

| Overclaim | Slice / Source | Reality (Slice 6 R2) |
|-----------|----------------|----------------------|
| "Intel QSV does not honor ForceKeyFrame mid-stream" | Slice 4 DD5 + DD10 inline comment | False. Wrong timing (AFTER ProcessInput) + wrong variant (VT_BOOL). BEFORE+VT_UI4 works (#809). |
| "Mechanism G is vendor-uniform" | Slice 5 DD11 + #784 design | False. G yields 0 IDR on NVENC (29/29 P-frames, #801). Validated only on Intel QSV. |
| "NVENC honored CleanPoint instead" | Slice 5 DD10 inline comment | False. CleanPoint=1 on input yields 30/30 P-frames on NVENC (#807). |

4. **sdd-init v15 update** (per D12): the cross-cutting `sdd-init/{project}` engram observation MUST be updated post-archive to reflect the corrigendum in its "Discoveries" section. This is the responsibility of the archive phase, not this design phase.

**Implements**: D12.
**Satisfies**: spec §9 (Slice 5 Archive Corrigendum). Scenarios — none directly (archive-phase concern).

**Rejected alternatives**:
- Edit Slice 5 archive in place — violates archive immutability convention; breaks `git blame` provenance for the original verdicts.
- Put the corrigendum in `windows_mft.rs` as a top-of-file comment — pollutes production source with archival cross-references; the archive-report is the right home.

---

### DD12 — Implementation order (apply-phase guidance)

**Choice — strict TDD commit cadence (matches spec §10, refined)**:

**C0 — PROBES baseline** (already on branch at `efc0f36` from Phase 0 Batch 2 `beda9ed`):
- ForceKeyFrame infrastructure present.
- 5 retained Phase 0 probes compile + run `--run-ignored=ignored-only`.
- T7.1/T7.2 `#[ignore]`-gated; expected FAIL when un-`#[ignore]`-d (current `request_keyframe()` routes to deleted-soon Mechanism G via `vendor=Unknown` or to deleted-soon CleanPoint via `vendor=NvidiaNvenc`).

**C1 — RED commit** (new):
- Remove `#[ignore]` from T7.1 + T7.2 (both Host A and Host B). Update the `WHY EXPECTED TO FAIL` doc-comments to Slice 6 R2 framing.
- Body of `request_keyframe()` still has the `match self.vendor` dispatch (UNCHANGED at C1). Tests fail because (a) Host A routes to `request_keyframe_via_recreate` and the recreate handler will be deleted in C2 → tests fail; (b) Host B routes to `request_keyframe_via_cleanpoint` and the CleanPoint write will be deleted in C2 → tests fail.
- **Alternative (preferred)**: ALSO at C1, change `request_keyframe()` to `self.state.force_keyframe_icodecapi_pending.store(true, Release)` (DD1). This makes C1 RED only because ForceKeyFrame mechanism is at least nominally wired but the fields/handlers being deleted are still present (compile fine, tests pass on hardware) — actually that's GREEN, so the "RED" label is wrong.

**Concrete C1/C2 split chosen**: In strict-TDD spirit, the RED is "tests un-`#[ignore]`-d". With current branch state, T7.1/T7.2 PASS on Host A (Mechanism G works there) and FAIL on Host B (NVENC routes to CleanPoint which is empirically falsified, P1 #807). So C1 = un-`#[ignore]` only. C2 = the cleanup pass (DD1 trait collapse + DD2 field deletes + DD3 pump_loop deletes + DD4 method deletes + DD5 dispatch delete + DD6 comment replace + DD8 probe delete + DD10 inventory comment add). With C2 applied, T7.1/T7.2 PASS on BOTH hosts (DD1 trait routes to ForceKeyFrame, P2 #809 confirms idx-0 NVENC + idx-1 Intel QSV).

**C2 — GREEN commit** (the cleanup pass — single atomic commit, ~−250 LOC net):
1. DD1: `request_keyframe()` trait body → one-line atomic store.
2. DD2: delete `keyframe_recreate_pending`, `cleanpoint_pending` fields. Decide on `mft_activate_factory` reshape (recommend keep field, just delete G handler references).
3. DD3: pump_loop NeedInput delete CleanPoint swap; pump_loop delete Mechanism G handler block; submit_frame delete `force_cleanpoint` param + body.
4. DD4: delete `request_keyframe_via_recreate()`, `request_keyframe_via_cleanpoint()`. Keep `request_keyframe_via_force_keyframe_icodecapi()`.
5. DD5: confirm no other `self.vendor` consumers (already audited in DD5).
6. DD6: replace DD10 inline comment.
7. DD7: confirm READ path unchanged.
8. DD8: delete Slice 5 round-3 probe + round-1 probes.
9. DD10: add top-of-file inventory comment.

**C3 — POLISH** (separate commit):
- New CI-runnable unit tests (R18 → S19/S20/S21):
  - `force_keyframe_icodecapi_pending_flag_default_false`
  - `request_keyframe_sets_force_keyframe_icodecapi_pending_to_true`
  - `force_keyframe_icodecapi_pending_swap_clears_to_false_after_consume`
- `cargo fmt`.
- `cargo clippy --all-targets --all-features --locked -- -D warnings` clean.

**Splitting C2**: Net delta of ~−250 LOC fits well within the 400-LOC budget. SINGLE atomic commit C2 is preferred for `git bisect` clarity (cleanup is cohesive — splitting it into "deletions" + "simplifications" creates intermediate states where tests are partially correct). If the diff balloons during apply (e.g. unanticipated cascading lints), tasks-phase may split C2 into:
- C2a: code deletions (Mechanism G handler + CleanPoint write + named methods + fields).
- C2b: comment replacement + dispatch trait collapse + probe inventory.

But the design recommends starting with single-commit C2 and splitting only on demand.

**Implements**: D5/D6/D7/D8/D9/D11.
**Satisfies**: R5–R10, R14, R15, R16. Scenarios S6, S7, S9, S10, S11, S17.

---

### DD13 — Test-only API exposure for retained probes

**Choice**: `request_keyframe_via_force_keyframe_icodecapi()` stays `pub` (not `pub(crate)`, not `#[cfg(test)]`-gated) — same visibility as `flush()` (line 2169). Reasoning:

- The retained Phase 0 probes (4 + 5 in DD10) live in the `tests/` crate-level test directory and call this method directly.
- Marking it `pub(crate)` would break the integration tests because `tests/windows_mft_encode.rs` is a separate crate from `sm_infra` (integration-test isolation).
- Marking it `#[cfg(test)]` would NOT make it visible to integration tests (only unit tests inside the same crate see `#[cfg(test)]` items).
- The two deleted methods (`_via_recreate`, `_via_cleanpoint`) had identical visibility for the same reason. Their deletion does not affect this rationale.

The doc-comment on the retained method (lines 2234–2260 currently) MUST be rewritten:
- Drop the "Phase 0 probe escape hatch" framing.
- Document it as the "named production mechanism, identical to `request_keyframe()` (trait) but explicit about the underlying mechanism for callers/probes that need to make the choice visible".
- Add NOTE: "Production callers SHOULD prefer the trait `request_keyframe()` for symmetry across encoder backends."

**Implements**: D-ENUM rationale (test affordance retained), D13.
**Satisfies**: R14. Scenarios S17, S20.

---

### DD14 — Cross-vendor smoke pre-merge gate

**Choice**: The apply phase MUST NOT open the PR until BOTH hosts have run:

```powershell
# Host A (Intel QSV) and Host B (NVENC), each:
cargo nextest run --workspace --features sm-infra/hw-encoder
cargo clippy --all-targets --all-features --locked -- -D warnings
# Plus retained Phase 0 probes (sanity, ignored by default):
cargo nextest run -p sm-infra --features hw-encoder \
    -E 'test(phase0_) and not test(_drain_resume_)' \
    --run-ignored=ignored-only
```

PASS criteria:
1. T7.1 + T7.2 PASS on both hosts (un-`#[ignore]`-d at C1, GREEN at C2).
2. T8.2 PASS on both hosts (bitrate path independent — Slice 4 carry-forward).
3. `cargo nextest run --workspace` GREEN — no NEW regressions vs Slice 5 archive baseline. Pre-existing flakes from Slice 5 are documented carry-forward, not regressions.
4. `cargo clippy ... -D warnings` zero warnings.
5. The 5 retained Phase 0 probes run cleanly (`--run-ignored=ignored-only`); P2 traces match #809 expectations (NVENC idx 0, Intel QSV idx 1).

**Apply-agent handoff**: when apply phase completes the C2 commit, it SHOULD produce a handoff message to the user with both Host A and Host B command lines (PowerShell on Windows), instructing the user to run them and report results before PR opens.

**Implements**: D14.
**Satisfies**: R11, R12, R13, R16, R17. Scenarios S12–S18.

**Rejected alternative**:
- CI cross-vendor enforcement — out of scope; project does NOT have CI Hosts A and B (manual smoke per Slice 4/5 convention).

---

## 4. Cross-references

| DD | Implements (proposal) | Satisfies (spec) | Scenarios |
|----|----------------------|------------------|-----------|
| DD1 | D1, D4, D11 | R1, R9, R10 | S1, S2, S11 |
| DD2 | D5, D6 | R5, R6 | S6, S7 |
| DD3 | D1, D3, D5, D6 | R2, R5, R6 | S3, S4, S6, S7 |
| DD4 | D5, D6, D-ENUM | R5, R6, R14 | S6, S7, S17 |
| DD5 | D-ENUM | R9 | S10 |
| DD6 | D7 | R8 | S9 |
| DD7 | D8 | R7 | S8 |
| DD8 | D9, §RETAIN | R15 | S17 |
| DD9 | D10, D14 | R11, R12 | S12–S15 |
| DD10 | D13 | R14 | S17 |
| DD11 | D12 | spec §9 | (archive-phase) |
| DD12 | D5, D6, D7, D8, D9, D11 | R5–R10, R14, R15, R16 | S6, S7, S9–S11, S17 |
| DD13 | D-ENUM, D13 | R14 | S17, S20 |
| DD14 | D14 | R11–R13, R16, R17 | S12–S18 |

---

## 5. Risks (top 3 — implementation-grounded mitigations)

### Risk 1 — Driver variance on `CODECAPI_AVEncVideoForceKeyFrame` (MEDIUM severity, LOW likelihood)

**Implementation-specific concern**: A future NVENC or Intel QSV driver update breaks the HCK mandate (Win8+ certification implies the property MUST be honored). This is the same shape as the Slice 4 falsification — empirical evidence is current-driver-bound.

**Mitigation**:
- The 5 retained Phase 0 probes (DD10) form a runnable regression suite. If T7.1/T7.2 fail on a future host, run probes 4+5 (`force_keyframe_via_codecapi_before_processinput`) to localize the fault to driver vs code.
- `EncoderVendor` enum + GUID detection (DD5) is preserved precisely so a future surgical reintroduction of vendor dispatch is a small diff (re-wire `request_keyframe()` body to `match self.vendor`). Cost-of-reintroduction is ~10 LOC.
- DD14 cross-vendor smoke gate prevents merging if either host regresses.

### Risk 2 — Mechanism G deletion blast radius (MEDIUM severity, LOW likelihood)

**Implementation-specific concern**: ~205 LOC of pump_loop body deletion + ~90 LOC of methods + 1 field could touch unintended invariants (e.g. the `draining: bool` stack-local guard from Slice 4 DD14).

**Mitigation**:
- The `draining` flag set sites (lines 1644 disconnect, 1663 explicit flush) are OUTSIDE the Mechanism G handler block (lines 1670–1877). Deletion does NOT remove the `draining` invariant.
- T8.2 PASS on both hosts (R13 + S16) is the load-bearing regression check — bitrate path is INDEPENDENT of the IDR mechanism, so any blast-radius regression surfaces there first.
- DD12 single-commit C2 keeps the deletion atomic — `git bisect` lands on one commit if a regression appears.
- spec §6 frozen-surface declarations include `collect_output` CleanPoint READ (R7) and Slice 3/4 Phase 0 probes; these are explicit "do not touch" guardrails.

### Risk 3 — Intel QSV idx-1 latency surprise in production (LOW severity, LOW likelihood)

**Implementation-specific concern**: Production callers that synchronize on `request_keyframe()` and immediately consume the next packet expecting `is_keyframe=true` will observe `is_keyframe=false` on Intel QSV (idx 1, not idx 0) — 1-frame latency.

**Mitigation**:
- DD1 doc-comment on `request_keyframe()` documents the latency contract (NVENC idx 0 ~0ms; Intel QSV idx 1 ~33ms) — the explicit MS spec language ("the property applies to the next frame received as input") is the architectural anchor.
- T7.1/T7.2 use `assert_keyframe_within_next_n_frames(30)` which trivially handles the 1-frame Intel QSV latency.
- Production callers in this codebase consume packets in arrival order (`pkt_rx.recv_timeout` loop) — the latency is invisible to them as long as they accept "IDR within 30 frames" semantics, which the WebRTC layer does.
- If a future caller needs strict idx-0 IDR on Intel QSV, the documented escape hatch is `request_keyframe_via_force_keyframe_icodecapi()` followed by a 1-frame discard — no API change.

---

## 6. Open questions / handoff to tasks

1. **DD2 mft_activate_factory reshape**: keep field with `Option<IMFActivate>` (mechanically minimal) vs rename to `winning_activate` (Slice 4 name) — tasks-phase decides.
2. **DD12 C2 split decision**: single atomic commit vs C2a (code deletions) + C2b (comment + inventory). Default = single. Tasks-phase reconsiders only if cascading lint cleanup ballons the diff beyond 400 LOC.
3. **DD13 doc-comment text**: exact wording of the rewritten `request_keyframe_via_force_keyframe_icodecapi()` doc — tasks-phase drafts; design locks the 4 invariants (drop "probe escape hatch" framing; document as named production mechanism; cross-reference trait method; cite #809 + #808).
4. **DD9 T7.1/T7.2 `#[ignore]` text**: delete annotation entirely (tests CI-runnable on hardware) or keep with updated text — recommend delete; tasks-phase confirms.

---

## 7. SDD chain anchors

- Predecessor: Slice 5 PR #21 / `c48ae46` / archive #791.
- Branch baseline: `efc0f36` (Phase 0 Batch 2 `beda9ed` — ForceKeyFrame infrastructure present).
- Successor: `hw-encoder-default-on-flip` (gated on Slice 5 + Slice 6 R2).
- Engram chain: explore #803 → falsifications #800 / #801 / #807 → research #808 → P2 #809 → proposal #810 → spec #811 → **design (this)** → tasks → apply → verify → archive.
- Artifact store: hybrid — engram `sdd/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design` + `openspec/changes/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/design.md`.
