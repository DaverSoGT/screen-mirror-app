# Proposal v2: hw-encoder-mft-intel-qsv-mid-stream-idr (Slice 5 — Intel QSV mid-stream forced IDR)

> Phase: SDD propose (REVISION v2 — supersedes v1 #776).
> Inputs: original explore #775, re-explore round 2 #781, Phase 0 round 1 trace #779, Phase 0 round 2 trace #780, **Phase 0 round 3 trace #783 (Mechanism G PASS)**, predecessor archive #773 (Slice 4), predecessor design #749 v2 (DD1/DD3/DD14/DD17), sdd-init #186 v13.
> Artifact store: hybrid (engram topic_key `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/proposal` UPSERT + `openspec/changes/hw-encoder-mft-intel-qsv-mid-stream-idr/proposal.md` overwrite).
> Strict TDD: ACTIVE (`cargo nextest run --workspace`).
> Date: 2026-05-09.
> Branch: `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` @ `918447a` (off master `5130e87`).
> v1 → v2 delta: D-MECHANISM revised C → **G**; 7 new D-* added; D-CLEANPOINT-DEPRECATION strengthened (G is the ONLY mid-stream IDR path); 1 D-* deferred to design (D-CODECAPI-POST-RECREATE).

---

## 0. Why v2

Proposal v1 (#776) locked **Mechanism C (drain + resume cycle, reusing Slice 4 DD17/F2 BEGIN_STREAMING + START_OF_STREAM)** as the Intel QSV mid-stream forced-IDR path, gated on Phase 0 probe C1. Three rounds of empirical Phase 0 evidence on Host A (Intel QSV `{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}`) have invalidated every MFT-API-level mechanism we tested and validated **Mechanism G (drop + recreate `IMFTransform` within `pump_loop`)** as the only path that produces an IDR mid-stream. v2 locks G as the load-bearing mechanism, encodes the new ownership/recreate semantics, and supersedes v1 D-MECHANISM accordingly.

This is NOT a re-explore — Round 2 explore #781 already enumerated alternatives (G, H, C-double-prime, F, I, J, K) and recommended G as the architecturally grounded primary. Round 3 #783 empirically confirmed PASS. v2 records the locked decision chain.

---

## 1. Inputs

| Source | Topic key / Path | Observation ID |
|--------|------------------|----------------|
| Original exploration (Slice 5) | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/explore` | #775 |
| Re-explore round 2 (Mechanism G recommended) | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/explore-round-2` | #781 |
| Phase 0 round 1 trace (C invalidated) | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/phase-0-trace` | #779 |
| Phase 0 round 2 trace (C-prime + A invalidated) | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/phase-0-trace-round-2` | #780 |
| **Phase 0 round 3 trace (G PASS — load-bearing evidence)** | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/phase-0-trace-round-3` | **#783** |
| sdd-init project context (v13) | `sdd-init/screen-mirror-app` | #186 |
| Predecessor archive (Slice 4) | `sdd/hw-encoder-mft-codec-api-counter-desync/archive-report` | #773 |
| Predecessor design v2 (DD14/F1, DD17/F2) | `sdd/hw-encoder-mft-codec-api-counter-desync/design` | #749 |
| Proposal v1 (this slice — superseded) | `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/proposal` (rev 1) | #776 |

---

## 2. Intent — Bug restatement

Intel QSV (`{4BE8D3C0-0515-4A37-AD55-E4BAE19AF471}`) does NOT honor any documented MFT-level mid-stream IDR signal:

- `MFSampleExtension_CleanPoint=1` on the input `IMFSample` — **ignored** (Slice 4 P0-B at `b4b3238`: `keyframe_indices=[0]`).
- `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, true)` post-`ProcessInput` (Slice 4 SWAP-FIRE DD1) — **ignored** (Slice 4 P0-B same evidence).
- `MFT_MESSAGE_COMMAND_DRAIN` + `BEGIN_STREAMING` + `START_OF_STREAM` resume (Mechanism C, Slice 4 DD17/F2 reuse) — **ignored** mid-stream (round 1 #779: encoder alive, post-resume frame `is_keyframe=false`; ~37ms latency).
- `MFT_MESSAGE_COMMAND_FLUSH` + `BEGIN_STREAMING` + `START_OF_STREAM` resume (Mechanism C-prime) — **kills encoder** (round 2 #780: pump thread `Disconnected` after second FLUSH; root cause: FLUSH discards types per MSDN, second FLUSH on the same handle in a streaming session is fatal on Intel QSV).
- `CODECAPI_AVEncMPVGOPSize=1` mid-stream toggle (Mechanism A) — **ignored** (round 2 #780: encoder alive, P-frame emitted; matches alax.info/blog/1823 / Intel SDK GitHub #2776 evidence that QSV ignores mid-stream codec API changes).

**Mechanism G (drop + recreate `IMFTransform` within `pump_loop`)** is the only path empirically validated. It produces an IDR by leveraging the **setup-sequence pattern** — every fresh `IMFActivate::ActivateObject` + `setup_mft` (FLUSH + types + BEGIN_STREAMING + START_OF_STREAM) emits its first encoded packet as IDR by H.264 spec. Round 3 #783 PASS confirms Intel QSV permits 2nd `ActivateObject` on the same `IMFActivate` factory within the same process/COM apartment (no `E_UNEXPECTED`), and that the 9ms tear-down + recreate plus subsequent first-encode latency is acceptable for the WebRTC PLI / new-viewer use case.

**Failing tests carry-forward from Slice 4 archive #773**: T7.1 (`mft_request_keyframe_marks_next_packet_as_keyframe`, line 319) and T7.2 (`mft_keyframe_flag_cleared_after_idr_emitted`, line 413) sit `#[ignore]` with master-body cadence at branch tip `5130e87`. v2 restores GREEN intent with Mechanism G semantics.

### Success criteria
- AC-1: Host A T7.1 + T7.2 PASS with new GREEN bodies (Mechanism G semantics; eventually-style assertions tolerant of drain + recreate latency within the test batch boundary).
- AC-2: Host A ≥ 658/664 maintained (Slice 4 baseline).
- AC-3: Host B (NVENC) T7.1/T7.2 status unchanged (NVENC keyframe-flag detection bug stays in separate slice candidate `hw-encoder-mft-nvenc-keyframe-flag`).
- AC-4: T8.2 (set_bitrate) MUST continue PASS on both vendors (Mechanism G does NOT intercept the bitrate path).
- AC-7: ≤ 800 LOC realistic against master `5130e87` (revised from v1 350 cap — see D-LOC-FORECAST below; structural ownership refactor + recreate path is unavoidably larger than v1 Mechanism C).
- AC-8: sm-domain UNCHANGED (Slice 4 DD9 inherited).

---

## 3. Scope

### IN scope
- Production refactor of `pump_loop`: own `IMFTransform`, `ICodecAPI`, `IMFMediaEventGenerator` by value (was: borrowed `&IMFTransform` + sibling refs). Required for Mechanism G drop + replace.
- Production refactor of `MftEncoderShared`: store `mft_activate_factory: IMFActivate` (clone via AddRef before transfer to encoder thread instead of `.take()`-consuming the original). Empirically validated round 3.
- New public method `WindowsMftH264Encoder::request_keyframe_via_recreate()` arming `keyframe_recreate_pending: AtomicBool`.
- `VideoEncoder::request_keyframe()` trait impl on `WindowsMftH264Encoder` calls `self.request_keyframe_via_recreate()` (replaces the prior CleanPoint + ForceKeyFrame path; eliminates trait→production divergence flagged in Slice 4 carry-forward).
- Mechanism G handler inside `pump_loop` with explicit ordered sequence: END_OF_STREAM → DRAIN (wait for `METransformDrainComplete`) → END_STREAMING → drop `IMFTransform` → 2nd `IMFActivate::ActivateObject` → `setup_mft` → FLUSH → BEGIN_STREAMING → START_OF_STREAM → re-cast `IMFMediaEventGenerator` + `ICodecAPI` from the new `IMFTransform` → continue `pump_loop`. ~9ms tear-down + recreate empirically (round 3 trace #783 L524–L531).
- **ELIMINATE `MFSampleExtension_CleanPoint` from production** (`windows_mft.rs:~1475`, `submit_frame()`). G is the ONLY mid-stream IDR path; CleanPoint was empirically no-op on Intel QSV (Slice 4 P0-B) and no longer needed defensively because the trait `request_keyframe()` impl now routes through G uniformly.
- **ELIMINATE `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` from production** (`windows_mft.rs:~1072`, `fire_pending_codec_settings()`). Same rationale.
- T7.1 + T7.2 GREEN bodies enabled with Mechanism G semantics (eventually-style assertion within batch boundary, NOT next-packet-immediate).
- Fix stale `flush()` inherent docstring at `windows_mft.rs:~1696` ("terminal per session" → STALE since Slice 4 DD17/F2; further STALE under G recreate flow).
- Phase 0 round 3 probe `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr` retained `#[ignore]`-gated post-fix per DD7 / Slice 3+4 convention.
- Revert Phase 0 round 1 + round 2 speculative production additions (`flush_state_pending`, `force_idr_gop_pending`, their methods + pump_loop hooks) where they are NOT load-bearing under G — preserve the structural ownership refactor and Mechanism G handler at branch tip `918447a`.
- TDD: C0 (round 3 probe already at tip `918447a`) → C1 (RED — T7.1/T7.2 GREEN bodies failing on master cadence) → C2 (GREEN — production refactor at `918447a` already structurally complete; cleanup + trait routing + CleanPoint/ForceKeyFrame deletion + docstring) → C3 (fmt + clippy polish).

### OUT of scope (explicit)
- NVENC keyframe-flag detection bug (separate slice `hw-encoder-mft-nvenc-keyframe-flag`). G should also work on NVENC (vendor-uniform setup-sequence pattern), but smoke validation is an AC-3 informational result, not a gate for this slice.
- DRAIN spam cleanup (separate slice `hw-encoder-mft-disconnect-drain-once`).
- `default = ["hw-encoder"]` flip (gated on this + NVENC keyframe-flag).
- sm-domain trait changes (FROZEN — Slice 4 DD9). `request_keyframe()` trait method signature is preserved; only the `WindowsMftH264Encoder` impl body changes.
- Sub-50ms IDR latency requirement. Mechanism G has measurable drain + recreate latency cost (drain time of in-flight batch + ~9ms tear-down + recreate + first-encode ~50–300ms). This is fundamentally different from NVENC's instant CleanPoint-honoring IDR. Documented as expected behavior.
- MediaSDK / VPL integration (Mechanism F).
- Vendor-conditional `is_intel_qsv` (Slice 4 DD-VENDOR / DD5 inherited — G is vendor-uniform).
- Mechanisms A / B / C / C-prime / C-double-prime / D / E / F / H / I / J / K (rejected per round 1 + 2 + 3 empirical chain and explore #781 tradeoff table).
- **Re-applying `pending_codec_*` (bitrate / profile) after recreate**: deferred to design (D-CODECAPI-POST-RECREATE). NOT locked here.

### Anti-scope
- This is NOT a new explore. Round 1 + 2 + re-explore #781 + round 3 already covered the design space.
- Do NOT add NVENC-specific Mechanism G fast-path complexity — out of scope.
- Do NOT touch Slice 4 production code paths except as required to delete the now-unused CleanPoint / ForceKeyFrame call sites and update the stale `flush()` docstring.
- Do NOT propose Mechanism H (external `restart()`) or F (MediaSDK / VPL) as alternates — G validated, no need.

---

## 4. Locked Decisions (v2)

### D-MECHANISM (REVISED v2) — Mechanism G locked, supersedes v1 Mechanism C

**Decision**: When `keyframe_recreate_pending` is observed in `pump_loop`, drop the current `IMFTransform` and recreate a fresh one via 2nd `IMFActivate::ActivateObject` on the cached `mft_activate_factory`. The first packet emitted after recreate is IDR by H.264 setup-sequence semantics — vendor-uniform, NOT dependent on any QSV mid-stream codec_api compliance.

**Empirical chain** (load-bearing evidence; design CANNOT lock without it):

| Round | Mechanism | Outcome | Evidence |
|-------|-----------|---------|----------|
| 1 (#779, tip `18cb09e`) | C — DRAIN + BEGIN_STREAMING + START_OF_STREAM (reuse Slice 4 DD17/F2) | **FAIL** — encoder alive, `is_keyframe=false`, latency ~37ms. Root cause: DRAIN preserves H.264 codec state per MSDN; BEGIN_STREAMING + START_OF_STREAM after DrainComplete restarts only the MFT-API stream session, NOT Intel QSV's internal H.264 bitstream state. |
| 2 (#780, tip `3168ef8`) | C-prime — FLUSH + BEGIN_STREAMING + START_OF_STREAM (same handle, no type re-apply) | **FAIL** — encoder DIED (channel `Disconnected`). Root cause: 2nd FLUSH mid-stream is fatal on Intel QSV. (MSDN clarification: FLUSH does NOT invalidate types, but Intel QSV driver crashes anyway on second FLUSH after a streaming session.) |
| 2 (#780) | A — `CODECAPI_AVEncMPVGOPSize=1` mid-stream toggle | **FAIL** — encoder alive, `is_keyframe=false`. Confirms QSV ignores mid-stream codec_api changes. |
| **3 (#783, tip `918447a`)** | **G — drop + recreate `IMFTransform` within `pump_loop`** | **PASS** — `is_keyframe=true len=8356` at post-recreate pkt 0; `keyframe_indices=[0]`; `encoder_died=false`; tear-down + recreate ~9ms (L524 → L531). |

**Why G works**: every fresh `ActivateObject` produces a brand-new `IMFTransform` instance. `setup_mft` on the new instance executes the canonical setup sequence (FLUSH + SetInputType + SetOutputType + BEGIN_STREAMING + START_OF_STREAM) on a fresh handle — no carried-over H.264 bitstream state. By H.264 spec, the first encoded sample of any newly initialized encoder MUST be IDR (parameter sets must be present). Vendor-uniform.

**Why round 1 + 2 mechanisms failed**: every same-handle mechanism (C, C-prime, C-double-prime, A) attempts to convince Intel QSV's internal H.264 codec state to "restart its GOP" via MFT-API signals that are documented but not honored mid-stream by this specific driver. Only re-instantiating the underlying codec via fresh `ActivateObject` forces a clean GOP boundary.

**Rejection of Mechanism H (external `restart()`)**: same setup-sequence grounding as G but requires struct-level channel reconstruction (rx/tx are consumed by `start()`); ~80 LOC + breaks the current API contract for callers. G is internal to `pump_loop` and transparent to callers.

**Rejection of Mechanism F (MediaSDK / VPL)**: out-of-MFT-scope; significant new infrastructure; build system changes; reserved as last resort if G ever regresses.

**Vendor scope**: G works on Intel QSV (round 3 PASS). Should also work on NVENC (vendor-uniform setup-sequence). Host B smoke run during verify is informational (AC-3); NVENC's separate keyframe-flag detection bug (`hw-encoder-mft-nvenc-keyframe-flag`) is unrelated and stays out of scope.

### D-CLEANPOINT-DEPRECATION (PRESERVED FROM v1 + STRENGTHENED) — Eliminate `MFSampleExtension_CleanPoint` and `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` from production

**Decision**: REMOVE both call sites unconditionally. Mechanism G is the ONLY mid-stream IDR path.

- `submit_frame()` `MFSampleExtension_CleanPoint=1` (`windows_mft.rs:~1475`) — DELETED. Empirically no-op on Intel QSV (Slice 4 P0-B). Was retained "defensively for NVENC" in v1; v2 removes it because the trait `request_keyframe()` impl now routes uniformly through G, so NVENC also goes through G.
- `fire_pending_codec_settings()` `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_BOOL=true)` (`windows_mft.rs:~1072`) — DELETED. Empirically no-op on Intel QSV.
- `CodecApiSwap.force_keyframe: bool` field — DELETED (no consumers post-deletion).

REVERSES Slice 4 DD2 ("conservative bet on CleanPoint") FULLY; CLOSES carry-forward note R3 from Slice 4 archive #773.

NVENC has its own separate keyframe-flag detection issue tracked under candidate slice `hw-encoder-mft-nvenc-keyframe-flag`. v2 does NOT add NVENC fast-path complexity to keep G uniform; if NVENC ever needs CleanPoint as a faster path, it is a separate slice.

### D-CODECAPI-POST-RECREATE (DEFERRED to design) — Strategy for `pending_codec_*` after recreate

**Status**: NOT locked in proposal. Design phase will resolve via DD entry.

**Question**: After Mechanism G recreates the `IMFTransform`, the new `ICodecAPI` handle starts with `EncoderConfig` defaults applied at `setup_mft` time. Any pending mid-stream `pending_bitrate` / `pending_profile` (set by callers between the recreate trigger and the actual recreate event) — should they be:

- (a) **Re-applied** post-recreate by snapshotting `pending_codec_*` atomics into stack-locals at the recreate trigger, then re-firing them via the SWAP-FIRE pattern after the fresh `setup_mft` + first-frame submission?
- (b) **Accepted as reset** to `EncoderConfig` defaults (caller's responsibility to re-issue `set_bitrate()` / `set_profile()` after observing IDR)?

**Risk if deferred wrong**: option (b) regresses T8.2 (set_bitrate) if a caller invokes `request_keyframe()` immediately after `set_bitrate()` and the recreate happens before fire fires. Option (a) is safer but adds ~20–30 LOC and a new SWAP-FIRE site.

**Resolution path**: design phase will inspect `set_bitrate`/`request_keyframe` interaction patterns in tests (T7, T8) and lock either (a) or (b) with explicit DD reasoning. Spec will emit a Requirement that codifies the chosen behavior.

### D-OWNERSHIP-REFACTOR (NEW v2) — `pump_loop` owns `IMFTransform` + `ICodecAPI` + `IMFMediaEventGenerator`

**Decision**: `pump_loop` signature changes from borrowing `&IMFTransform` (and sibling refs) to owning the three handles by value (or wrapped in `Option` for the recreate slot). Required because Mechanism G needs to `drop()` and replace the `IMFTransform`; you cannot replace what you only borrow.

**Production refactor at branch tip `918447a` is structurally complete** for round 3 probe support. Behavior on the non-G path (no `keyframe_recreate_pending`) is unchanged: the owned handles are used identically to how the borrowed refs were used.

**Reason for proposal-level lock** (not just a DD): this is a load-bearing structural change with cross-cutting implications for sub-functions previously taking `&IMFTransform` (e.g., `setup_mft`, `submit_frame`, `service_needs_input`). All such callees must accept `&IMFTransform` borrowed FROM the owned slot for the duration of the call, then release the borrow before any potential recreate can happen. Locking at proposal level ensures spec + design treat ownership as an invariant, not a detail.

### D-IMFACTIVATE-CLONE (NEW v2) — `MftEncoderShared` stores `mft_activate_factory: IMFActivate` (AddRef-cloned, not `.take()`-consumed)

**Decision**: `MftEncoderShared` retains a clone of the `IMFActivate` factory used by `start()`'s probe-and-select sequence. The clone is created via the windows-rs COM AddRef pattern (idiomatic `.clone()` on the COM interface) before transfer to the encoder thread. The original `IMFActivate` is no longer `.take()`-consumed.

**Reason**: Mechanism G's drop + recreate cycle requires a 2nd `ActivateObject` call. That call needs the same `IMFActivate` factory that produced the original `IMFTransform`. Empirically validated round 3 #783: Intel QSV permits 2nd `ActivateObject` on the same factory within the same process / COM apartment without `E_UNEXPECTED`. (This was the primary risk flagged in re-explore #781; round 3 disproved it.)

**LOC impact**: ~5–10 LOC at the `start()` site to swap `.take()` for `.clone()`-then-store, plus the `MftEncoderShared` field declaration.

### D-RECREATE-SEQUENCE (NEW v2) — Mechanism G handler exact ordering

**Decision**: When `pump_loop` observes `keyframe_recreate_pending = true` (and is not already in a recreate cycle), execute the following ordered sequence:

1. `MFT_MESSAGE_NOTIFY_END_OF_STREAM` — signals last input received.
2. `MFT_MESSAGE_COMMAND_DRAIN` — request flush of in-flight samples.
3. Drain loop: `ProcessOutput` until `METransformDrainComplete` event observed (or timeout — see D-RECREATE-TIMEOUT below at design phase).
4. `MFT_MESSAGE_NOTIFY_END_STREAMING` — exits the streaming session.
5. `drop(imf_transform)` — release the old `IMFTransform` COM handle.
6. `mft_activate_factory.ActivateObject(&IMFTransform::IID)` — fresh handle (2nd call on the same factory).
7. `setup_mft(new_handle, &encoder_config)` — re-derives input + output types from cached `EncoderConfig` (DOES NOT require explicit `MFT_MEDIA_TYPE` snapshot, per round 3 evidence — `MFT_E_TRANSFORM_TYPE_NOT_SET` not observed).
8. (Inside `setup_mft`): `MFT_MESSAGE_COMMAND_FLUSH` + types + `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` + `MFT_MESSAGE_NOTIFY_START_OF_STREAM`.
9. Re-cast `IMFMediaEventGenerator` and `ICodecAPI` from the new `IMFTransform` (COM `cast` calls).
10. Reset `keyframe_recreate_pending = false`, reset `ni_count = 0`, `ho_count = 0`, `draining = false`.
11. Continue `pump_loop` — the next frame consumed from `rx` becomes the first input to the fresh handle, encoded as IDR.

**Empirical timing** (round 3 #783):
- Tear-down + recreate: ~9ms (L524 22:10:46.703 → L531 22:10:46.712).
- First-encode latency post-recreate: ~390ms total from arm to first packet (includes ActivateObject + setup_mft + FLUSH + BEGIN + START + 30 frames pushed by test cadence + first encode). Intrinsic recreate cost is ~9ms; the rest is test-cadence dependent.
- `STREAM_CHANGE` event NOT observed post-recreate (Intel QSV does not negotiate STREAM_CHANGE when types are identical).

**First post-recreate frame IS IDR** by setup-sequence semantics — empirical: round 3 #783 L642 `[G] post-recreate pkt 0 — is_keyframe=true len=8356`.

### D-API-SURFACE (NEW v2) — Public method `request_keyframe_via_recreate()`

**Decision**: Add `pub fn request_keyframe_via_recreate(&self)` to `WindowsMftH264Encoder`. Implementation: `self.shared.keyframe_recreate_pending.store(true, Ordering::Release)`.

**Naming rationale**: explicit `_via_recreate` suffix (vs. generic `request_keyframe`) signals to direct callers of `WindowsMftH264Encoder` that this path has measurable cost (~9ms tear-down + drain latency on the in-flight batch + first-encode latency). The name is intentionally NOT a generic verb because the cost profile is fundamentally different from NVENC's instant-IDR-via-CleanPoint behavior. Callers using the trait surface go through `VideoEncoder::request_keyframe()` (D-TRAIT-IMPL) and don't see the cost suffix.

### D-TRAIT-IMPL (NEW v2) — `VideoEncoder::request_keyframe()` routes to `request_keyframe_via_recreate()`

**Decision**: The `VideoEncoder` trait impl on `WindowsMftH264Encoder` for `request_keyframe()` calls `self.request_keyframe_via_recreate()`. This replaces the prior CleanPoint + ForceKeyFrame path.

**Reason**: eliminates the trait→production divergence flagged in Slice 4 carry-forward (the trait method existed but its production effect on Intel QSV was empirically a no-op, making the trait API misleading). Post-v2, the trait method has guaranteed semantics on both vendors via Mechanism G's vendor-uniform setup-sequence.

### D-SLICE-4-CARRY-FORWARD (NEW v2) — Restore T7.1 + T7.2 GREEN with Mechanism G semantics

**Decision**: T7.1 (`mft_keyframe_flag_cleared_after_idr_emitted`, `:319`) and T7.2 (`mft_request_keyframe_marks_next_packet_as_keyframe`, `:413`) — currently `#[ignore]` with master-body cadence and CARRY-FORWARD comments at branch tip `5130e87` (Slice 4 archive #773 R3 carry-forward) — are ENABLED with Mechanism G GREEN bodies.

**Cadence pattern**: batch-push N priming → flush → drain priming → `request_keyframe()` → push 1 IDR-target → flush → recv with eventually-style assertion (within batch boundary, NOT next-packet-immediate — see D-SCOPE-LATENCY).

**NVENC carry-forward**: NVENC versions of T7.1/T7.2 remain `#[ignore]` per separate `hw-encoder-mft-nvenc-keyframe-flag` slice. Their unblocking is NOT a v2 AC.

CARRY-FORWARD comments and `#[ignore]` REMOVED on the Intel QSV side.

### D-SCOPE-LATENCY (NEW v2) — Mid-stream IDR via G has measurable latency cost

**Decision**: Document Mechanism G's latency profile explicitly so test assertions and downstream documentation reflect the cost difference vs. NVENC's instant-IDR pattern.

**Cost components**:
- Drain time of in-flight batch (variable; depends on how full the MFT pipeline is when `request_keyframe` arms — typically 1–3 frames of pipeline depth on QSV).
- Tear-down + recreate: ~9ms intrinsic (round 3 evidence).
- First-encode latency post-recreate: ~50–300ms (depends on submission cadence; round 3 saw ~390ms total but most was test-cadence overhead).

**Test assertion implication**: T7.1/T7.2 must use **eventually-style assertion within the test batch boundary** (e.g., "within the batch of K packets recv'd post-`request_keyframe()`, at least one is IDR") rather than "next packet immediately is IDR". The v1 cadence (which assumed next-packet-immediate) is incompatible with G semantics.

**Production implication**: callers requesting an IDR for WebRTC PLI should expect ~50–300ms response time, comparable to network RTT. Documented in `request_keyframe_via_recreate()` doc comment.

### D-DOCSTRING-FIX (CARRY-FORWARD FROM v1, STRENGTHENED) — Update stale `flush()` docstring

**Decision**: Update the inherent `flush()` method docstring at `windows_mft.rs:~1696`. Current text claims "terminal per session on Intel QSV — do not call flush() mid-stream" / "Production callers MUST NOT call this method".

Both claims are STALE since Slice 4 DD17/F2 (post-DrainComplete BEGIN_STREAMING + START_OF_STREAM resume makes flush()→continue valid) AND further STALE under Mechanism G (the recreate path makes flush() conceptually irrelevant — IDR comes from a fresh handle, not from a flush of the old one).

New docstring states: "flush() drains in-flight samples and resumes the stream via Slice 4 DD17/F2; for forced mid-stream IDR, use `request_keyframe_via_recreate()` (Slice 5 D-MECHANISM Mechanism G)."

Trivial; locked here to avoid doc-rot and to give a clear mental model to future readers.

### D-PHASE0 (PRESERVED FROM v1, EXTENDED) — Phase 0 round 1 + 2 + 3 probes are empirical record

**Decision**: All three rounds of Phase 0 probes are PRESERVED at branch `918447a` as the empirical record of why Mechanism G was chosen. Per Slice 3 + 4 DD7 / Slice 5 D-PROBES-RETENTION below, they remain `#[ignore]`-gated post-fix as regression evidence.

The ONLY probe that is the load-bearing PASS gate going forward is round 3's `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`. Round 1 + 2 probes serve as historical regression guards (proving C, C-prime, A invalid).

### D-DELIVERY (PRESERVED FROM v1) — Single PR (override session `auto-chain`)

**Decision**: Branch `feat/hw-encoder-mft-intel-qsv-mid-stream-idr` off master `5130e87`. Single PR. Despite the LOC bump from v1 ~250–320 to v2 ~600–800 (D-LOC-FORECAST below), the change is structurally cohesive (ownership refactor + recreate path + trait routing + deletion of two no-op call sites are tightly coupled). NO `size:exception` requested at proposal level — design + tasks phase will assess against `delivery_strategy=single-pr` cached at session start.

If review feedback in tasks phase indicates the budget exceeds reviewer tolerance, the slice may be SPLIT into:
- PR-A: structural ownership refactor (D-OWNERSHIP-REFACTOR + D-IMFACTIVATE-CLONE) — pure refactor, behavior identical, ~300 LOC.
- PR-B: Mechanism G handler + trait routing + CleanPoint/ForceKeyFrame deletion + T7.1/T7.2 GREEN + docstring fix — ~400–500 LOC.

Locked here as a known split path; tasks-phase Review Workload Guard will resolve.

### D-SCOPE (PRESERVED FROM v1) — Intel QSV is the locked target; NVENC informational

**Decision**: This slice's AC-1 is Intel QSV T7.1/T7.2 PASS on Host A. NVENC is informational (AC-3) — Mechanism G is vendor-uniform per setup-sequence semantics, but NVENC's separate keyframe-flag detection bug (NAL-type-5 detection on the output sample) is tracked under candidate slice `hw-encoder-mft-nvenc-keyframe-flag` and is NOT this slice's responsibility to resolve.

### D-PRESERVE-SLICE-4-DD (REVISED FROM v1) — Slice 4 design decisions PRESERVED where applicable; some now N/A

| Slice 4 DD | v1 status | v2 status |
|------------|-----------|-----------|
| DD1 SWAP-FIRE | Preserved for `set_bitrate`; ForceKeyFrame FIRE dropped | **PRESERVED for `set_bitrate` only**. ForceKeyFrame FIRE FULLY DELETED per D-CLEANPOINT-DEPRECATION. |
| DD3 restoration semantics | Preserved | Preserved (still applies to `set_bitrate` SWAP-FIRE). |
| DD14/F1 drain-state guard | Preserved + EXTENDED with new SET site #3 (keyframe-driven drain) | **PRESERVED unchanged**. Mechanism G uses a separate `keyframe_recreate_pending: AtomicBool`, NOT the existing `drain_pending` atomic. No new SET site on `drain_pending`. The existing `draining: bool` stack-local guard inside `pump_loop` does NOT need to interact with G (G's drain is local to the recreate handler, not signaled via the atomic). |
| DD17/F2 BEGIN_STREAMING + START_OF_STREAM post-DrainComplete | Preserved as load-bearing for Mechanism C | **PRESERVED but no longer load-bearing for IDR**. Still load-bearing for `flush()` user-facing semantics + T8.2 (set_bitrate path). G's setup-sequence (inside `setup_mft` post-recreate) emits the equivalent BEGIN + START messages on the NEW handle, not the old one. |
| DD7 probe retention | Preserved for C1+C2 | PRESERVED for round 1 + 2 + 3 probes. |
| DD6 RED smoke gate | Preserved | Preserved. |
| DD9 sm-domain FROZEN | Preserved | Preserved. |
| DD16 batch+flush+drain test cadence | Preserved | **REVISED**: T7.1/T7.2 still use batch+flush+drain cadence, BUT assertion changes to eventually-style (D-SCOPE-LATENCY). |
| DD-VENDOR / DD5 vendor-uniform path | Preserved | Preserved (G is vendor-uniform). |

### D-LOC-FORECAST (REVISED FROM v1) — Realistic ~600–800 (cap 800; hard cap 1000 with split path option)

| Component | Naive | Realistic (×1.5 on production refactor) |
|-----------|-------|------------------------------------------|
| Production: ownership refactor (`pump_loop` signature + sub-function signatures) | 200–250 | 250–350 |
| Production: `mft_activate_factory: IMFActivate` clone field + `start()` site update | 10–20 | 10–20 |
| Production: `keyframe_recreate_pending: AtomicBool` + `request_keyframe_via_recreate()` | 15–25 | 15–25 |
| Production: Mechanism G handler in `pump_loop` (D-RECREATE-SEQUENCE) | 80–120 | 100–150 |
| Production: `VideoEncoder::request_keyframe()` trait routing | 5–10 | 5–10 |
| Production: DELETE CleanPoint + ForceKeyFrame call sites + `CodecApiSwap.force_keyframe` field | -20 to -30 (negative) | -20 to -30 |
| Production: docstring fix on `flush()` | 5–10 | 5–10 |
| Tests: T7.1/T7.2 GREEN bodies (eventually-style cadence) | 60–100 | 80–120 |
| Tests: Phase 0 round 3 probe (already at `918447a`) | ~150 | ~150 (already counted in branch delta) |
| Tests: Phase 0 round 1 + 2 probes (preserved for regression) | ~280 + ~220 = ~500 | ~500 (already counted) |
| **Net new vs master `5130e87`** | **~580–745** | **~700–850** |

Cap: 800 LOC realistic against master. Hard cap: 1000. If exceeded, invoke D-DELIVERY split path (PR-A pure refactor + PR-B mechanism + tests).

This is a significant bump from v1's 350 cap. Justified by:
1. Mechanism G is structurally heavier than v1 Mechanism C (which reused existing DD17/F2 infrastructure with no new ownership semantics).
2. Three rounds of Phase 0 probes (already on branch) account for ~500 LOC of test code that is RETAINED as regression guards per DD7.
3. Net new production LOC for Mechanism G (~370–565) is comparable in scale to Slice 4 DD14/F1 + DD17/F2 combined.

### D-PROBES-RETENTION (PRESERVED FROM v1, EXTENDED) — `#[ignore]`-gated regression tests

**Decision**: All three rounds of Phase 0 probes retained `#[ignore]`-gated post-fix per Slice 3 DD10 / Slice 4 DD7. Round 3 probe is the load-bearing PASS gate; rounds 1 + 2 are regression evidence proving C / C-prime / A invalid.

### D-DRAIN-RACE (REVISED FROM v1) — Concurrent `request_keyframe_via_recreate()` + `flush()` priority

**Decision**: Both `keyframe_recreate_pending` and `drain_pending` may be observed by `pump_loop` in the same iteration. Lock priority: process `keyframe_recreate_pending` FIRST (G's recreate cycle internally drains the in-flight batch via END_OF_STREAM + DRAIN before tear-down, which subsumes any pending user flush). After recreate completes, the stale `drain_pending=true` is OBSERVED but the stream is already in fresh state — DD-level decision deferred to design: either (a) clear `drain_pending` atomically inside the recreate handler to skip a redundant no-op DRAIN on the new handle, or (b) let the no-op DRAIN proceed (idempotent, harmless).

Lower priority than v1's Mechanism C race because G's recreate path is self-contained (no shared state with `drain_pending` other than the priority arbitration above).

---

## 5. Open Questions Resolution (v2)

| OQ (v2) | Description | Status |
|---------|-------------|--------|
| OQ-1 (was v1 OQ-1) | First frame after DRAIN+resume on Intel QSV is IDR? | **RESOLVED — NO** (round 1 #779 invalidates Mechanism C). |
| OQ-2 (NEW v2) | First frame after FLUSH-based reset (C-prime) on Intel QSV is IDR? | **RESOLVED — NO + ENCODER_DIED** (round 2 #780 invalidates Mechanism C-prime). |
| OQ-3 (was v1 OQ-3) | `CODECAPI_AVEncMPVGOPSize=1` produces IDR mid-stream on Intel QSV? | **RESOLVED — NO** (round 2 #780 invalidates Mechanism A). |
| OQ-4 (NEW v2) | First frame after fresh `IMFTransform` recreate (Mechanism G) on Intel QSV is IDR? | **RESOLVED — YES** (round 3 #783 PASS). |
| OQ-5 (NEW v2) | 2nd `ActivateObject` on the same `IMFActivate` factory throws `E_UNEXPECTED` on Intel QSV? | **RESOLVED — NO** (round 3 #783 evidence; primary risk from #781 disproved). |
| OQ-6 (NEW v2) | `MFT_E_TRANSFORM_TYPE_NOT_SET` (0xC00D6D60) after recreate on Intel QSV? | **RESOLVED — NO** (round 3 #783; `setup_mft` post-recreate re-derives types from cached `EncoderConfig` without explicit `MFT_MEDIA_TYPE` snapshot). |
| OQ-7 (NEW v2) | `STREAM_CHANGE` event after recreate? | **RESOLVED — NO** (round 3 #783; types identical, no negotiation needed). |
| OQ-8 (NEW v2 — DEFERRED) | Should `pending_codec_*` (bitrate/profile) be re-applied after recreate? (D-CODECAPI-POST-RECREATE) | **DEFERRED to design**. |
| OQ-9 (NEW v2 — DEFERRED) | Drain-race arbitration detail (clear `drain_pending` inside G handler vs. let no-op DRAIN proceed)? (D-DRAIN-RACE) | **DEFERRED to design**. |
| OQ-10 (was v1 OQ-6) | G works on NVENC too? | **DEFERRED to verify-phase** (Host B smoke; AC-3 informational; G expected to work per setup-sequence semantics but not gated). |

---

## 6. Predecessor Patterns Reused

- Slice 4 DD1 SWAP-FIRE — preserved for `set_bitrate` only; ForceKeyFrame FIRE FULLY DELETED.
- Slice 4 DD3 restoration semantics — preserved (still applies to bitrate path).
- Slice 4 DD14/F1 drain-state guard — preserved unchanged (G uses separate atomic).
- Slice 4 DD17/F2 BEGIN_STREAMING + START_OF_STREAM — preserved for `flush()` user semantics; G's setup-sequence emits equivalent on the NEW handle.
- Slice 4 DD7 probe retention — preserved (rounds 1 + 2 + 3).
- Slice 4 DD6 RED smoke gate.
- Slice 3 + 4 DD16 batch+flush+drain test cadence — preserved with eventually-style assertion update (D-SCOPE-LATENCY).
- Slice 4 D-DELIVERY single-PR override (with split path option per LOC bump).
- Project conventions: tracing-before-explore #592, BLOCKED_ON_SMOKE #582, `.engram/` tracked #698, cargo fmt direct invocation #581, `#[allow]` over `#[expect]` #580, post-archive housekeeping #732.

---

## 7. Risks (top entries)

| Sev | Lik | Risk | Mitigation |
|-----|-----|------|------------|
| HIGH | LOW | T7.1/T7.2 GREEN bodies fail on Host A despite round 3 PASS (test cadence mismatch with G latency profile) | D-SCOPE-LATENCY mandates eventually-style assertion. Apply phase RED commit will surface this immediately; design phase locks the cadence pattern. |
| HIGH | LOW | NVENC regression: G works on Intel but breaks NVENC's existing CleanPoint-honoring path because we DELETED CleanPoint | Host B smoke during verify (AC-3 informational). G is vendor-uniform per setup-sequence; if regression observed, re-introduce CleanPoint as belt-and-suspenders for NVENC behind `is_nvenc` flag — but this is OUT-OF-SCOPE per current D-SCOPE; would require slice extension. |
| MED | MED | LOC budget blow-up triggers PR review backpressure (~700–850 vs v1 350) | D-DELIVERY split path option pre-locked: PR-A pure refactor + PR-B mechanism. Tasks-phase Review Workload Guard arbitrates. |
| MED | MED | D-CODECAPI-POST-RECREATE chosen wrong at design phase (e.g., option (b) regresses T8.2 set_bitrate when interleaved with request_keyframe) | Design DD must inspect set_bitrate ↔ request_keyframe interaction tests; if option (b), explicit doc + test guard for the interleave case. |
| MED | LOW | 2nd `ActivateObject` on the same `IMFActivate` factory leaks COM resources (slow leak across many recreate cycles) | Round 3 single-recreate PASS; multi-recreate stress not covered. Design phase: document recreate-cycle COM lifetime; verify phase: add stress probe (10+ recreate cycles in one test) if budget allows. |
| MED | LOW | Concurrent `set_bitrate()` during recreate handler (between END_OF_STREAM and post-recreate `setup_mft`) — bitrate SWAP-FIRE on a torn-down handle | Existing `draining: bool` guard inside `pump_loop` already prevents pump_loop SWAP-FIRE during drain; recreate cycle should set `draining=true` for the duration (DD-level lock at design). |
| MED | LOW | `STREAM_CHANGE` event NOT observed in round 3 but appears in production under different cadence | OQ-7 RESOLVED on probe cadence; production cadence may differ. Design phase: ensure recreate handler is robust to STREAM_CHANGE (Slice 2 STREAM_CHANGE handler should absorb on the new handle). |
| MED | MED | ~9ms tear-down + ~50–300ms first-encode latency breaks WebRTC QoE for fast-PLI scenarios | NOT a current product req. Documented in D-SCOPE-LATENCY. Comparable to network RTT on which IDR is requested. |
| LOW | LOW | docstring fix slips to follow-up PR | D-DOCSTRING-FIX locks change in same PR. |
| LOW | LOW | sm-domain trait freeze pressure | G is internal; trait method signature unchanged (D-TRAIT-IMPL keeps the trait API stable). |

**No proposal-level BLOCKS** — round 3 PASS already cleared the Phase 0 gate. Spec + design can proceed in parallel.

---

## 8. Acceptance Criteria

1. AC-1: Host A T7.1 + T7.2 PASS with new GREEN bodies (Mechanism G semantics; eventually-style assertion within batch boundary).
2. AC-2: Host A ≥ 658/664 maintained.
3. AC-3: Host B (NVENC) T7.1/T7.2 status unchanged — INFORMATIONAL; NVENC NAL-type-5 detection bug stays separate slice.
4. AC-4: T8.2 set_bitrate PASS on both vendors. Implies D-CODECAPI-POST-RECREATE design decision must not regress this.
5. AC-5: nextest GREEN; clippy `--all-targets --all-features --locked -- -D warnings` clean; fmt clean.
6. AC-6: All three rounds of Phase 0 probes preserved at branch tip post-fix as `#[ignore]`-gated regression guards.
7. AC-7: ≤ 800 LOC realistic against master `5130e87`; hard cap 1000 with split path.
8. AC-8: sm-domain UNCHANGED; trait method signature `request_keyframe()` unchanged.
9. AC-9: `default = []` unchanged.
10. AC-10: `flush()` inherent docstring updated to reflect Slice 4 DD17/F2 + Slice 5 G semantics.
11. AC-11: `MFSampleExtension_CleanPoint=1` call site DELETED from `submit_frame()`.
12. AC-12: `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame)` call site DELETED from `fire_pending_codec_settings()`.
13. AC-13: `CodecApiSwap.force_keyframe: bool` field DELETED.
14. AC-14: Public `WindowsMftH264Encoder::request_keyframe_via_recreate()` method exists and is the only path arming `keyframe_recreate_pending`.
15. AC-15: `VideoEncoder::request_keyframe()` trait impl on `WindowsMftH264Encoder` calls `request_keyframe_via_recreate()`.
16. AC-16: `MftEncoderShared` stores `mft_activate_factory: IMFActivate` (cloned, not `.take()`-consumed).

---

## 9. SDD Chain Anchors

- Predecessor: PR #20 / merge `8fa1a61` / Slice 4 archive #773. Master baseline: `5130e87` (post-Slice-4 housekeeping).
- Branch baseline: `918447a` (Phase 0 round 3 probe + structural ownership refactor for Mechanism G).
- Successors / parallel: `hw-encoder-mft-nvenc-keyframe-flag` (Slice 6 candidate, separate). Optional: `hw-encoder-mft-disconnect-drain-once` (XS).
- Both this slice + NVENC keyframe-flag gate `hw-encoder-default-on-flip`.
- Supersedes: proposal v1 #776 (D-MECHANISM C); spec #777 v1 must be rewritten for G in the spec phase.

---

## Result Contract

- **status**: complete
- **executive_summary**: v2 supersedes v1 — Mechanism G (drop + recreate `IMFTransform` within `pump_loop`) locked as the load-bearing mid-stream forced-IDR path on Intel QSV. Empirical chain: round 1 (#779) invalidates Mechanism C (drain+resume); round 2 (#780) invalidates C-prime (FLUSH-based reset → ENCODER_DIED) and A (CODECAPI_AVEncMPVGOPSize toggle); round 3 (#783) validates G with PASS at branch `918447a` (~9ms tear-down + recreate, first post-recreate frame is IDR by setup-sequence semantics, vendor-uniform). 12 locked decisions: D-MECHANISM (revised, G); D-CLEANPOINT-DEPRECATION (strengthened — DELETE CleanPoint AND ForceKeyFrame fully); D-OWNERSHIP-REFACTOR (`pump_loop` owns IMFTransform + ICodecAPI + IMFMediaEventGenerator); D-IMFACTIVATE-CLONE (`MftEncoderShared` stores AddRef-cloned IMFActivate factory); D-RECREATE-SEQUENCE (END_OF_STREAM → DRAIN → END_STREAMING → drop → ActivateObject 2nd → setup_mft → re-cast → continue); D-API-SURFACE (new `request_keyframe_via_recreate()`); D-TRAIT-IMPL (trait method routes through G); D-SLICE-4-CARRY-FORWARD (T7.1+T7.2 GREEN with eventually-style assertion); D-SCOPE-LATENCY (~50–300ms; documented); D-DOCSTRING-FIX (carry-forward, strengthened); D-DELIVERY (single-PR with split path option); D-LOC-FORECAST (~700–850 net new vs master). 1 deferred to design: D-CODECAPI-POST-RECREATE (re-apply pending_codec_* after recreate or accept reset). 1 deferred to design: D-DRAIN-RACE arbitration detail. 8 OQs RESOLVED, 2 DEFERRED. NVENC keyframe-flag detection stays separate slice. No proposal-level BLOCKS — round 3 PASS cleared the Phase 0 gate.
- **artifacts**:
  - engram `sdd/hw-encoder-mft-intel-qsv-mid-stream-idr/proposal` (UPSERT — v2 supersedes v1 #776 at same topic_key)
  - `openspec/changes/hw-encoder-mft-intel-qsv-mid-stream-idr/proposal.md` (overwritten v2)
- **next_recommended**: `sdd-spec` (v2 — rewrite #777 for Mechanism G semantics; Requirements R-set must reflect G + ownership refactor + AC-set updates) and `sdd-design` (v2 — DD entries for D-CODECAPI-POST-RECREATE, D-DRAIN-RACE detail, D-RECREATE-SEQUENCE invariants, D-OWNERSHIP-REFACTOR cross-cutting borrow lifetimes); both can run in parallel.
- **decisions_locked** (v2): 12 (D-MECHANISM revised, D-CLEANPOINT-DEPRECATION strengthened, D-OWNERSHIP-REFACTOR, D-IMFACTIVATE-CLONE, D-RECREATE-SEQUENCE, D-API-SURFACE, D-TRAIT-IMPL, D-SLICE-4-CARRY-FORWARD, D-SCOPE-LATENCY, D-DOCSTRING-FIX, D-DELIVERY, D-LOC-FORECAST). Plus preserved: D-PHASE0, D-SCOPE, D-PRESERVE-SLICE-4-DD, D-PROBES-RETENTION, D-DRAIN-RACE.
- **decisions_deferred** (v2): 2 (D-CODECAPI-POST-RECREATE; D-DRAIN-RACE arbitration detail).
- **risks** (top 5): HIGH/LOW T7.1/T7.2 cadence mismatch with G latency → eventually-style mandated; HIGH/LOW NVENC regression from CleanPoint deletion → Host B smoke informational; MED/MED LOC blow-up → split path pre-locked; MED/MED D-CODECAPI-POST-RECREATE wrong choice → design DD must inspect interleave; MED/LOW COM lifetime on multi-recreate → stress probe candidate at verify.
- **skill_resolution**: injected
- **delivery_recommendation**: single-pr (D-DELIVERY) with pre-locked split path (PR-A refactor + PR-B mechanism) if tasks-phase Review Workload Guard arbitrates against single PR.
- **mechanism_locked**: G (drop + recreate IMFTransform within pump_loop) — supersedes v1 Mechanism C.
- **phase_0_probes_planned**: 1 retained as load-bearing PASS gate (round 3 `phase0_intel_qsv_idr_via_imftransform_recreate_first_frame_is_idr`); 4 retained as regression evidence (round 1 C1 + C2; round 2 C-prime + A).
- **loc_forecast_realistic** (net new vs master): 700–850; cap 800; hard cap 1000.
- **supersedes**: proposal v1 #776 D-MECHANISM (C).
- **empirical_basis**: round 1 #779 (C invalid) + round 2 #780 (C-prime + A invalid) + round 3 #783 (G PASS).
