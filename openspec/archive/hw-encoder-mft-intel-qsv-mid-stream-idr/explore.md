## Exploration: hw-encoder-mft-intel-qsv-mid-stream-idr

### Bug Summary

Intel QSV does NOT honor `MFSampleExtension_CleanPoint=1` (set on IMFSample before ProcessInput) nor `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, true)` (fired post-ProcessInput per Slice 4 SWAP-FIRE DD1) for mid-stream forced IDR. Evidence: P0-B probe at branch tip `b4b3238` returned `keyframe_indices=[0]` — only the priming initial IDR is a keyframe; frames submitted after `request_keyframe()` emit as P-frames.

Failing tests: T7.1 (`mft_request_keyframe_marks_next_packet_as_keyframe`, line 319) and T7.2 (`mft_keyframe_flag_cleared_after_idr_emitted`, line 413) — both carry-forwarded with `#[ignore]`.

### Current State

**Files affected**:
- `crates/sm-infra/src/encode/windows_mft.rs`: pump_loop (~1108), swap_pending_codec_settings (~1028), fire_pending_codec_settings (~1052), restore_pending_codec (~1088), draining guard (~1309), submit_frame (~1462), DrainComplete/F2 handler (~1181-1213), flush() inherent method (~1710)
- `crates/sm-infra/tests/windows_mft_encode.rs`: T7.1 (~319), T7.2 (~413), P0-A (~1139), P0-B (~1275)

**Key structural facts from code reading**:

1. `submit_frame()` sets `MFSampleExtension_CleanPoint=1` when `force_keyframe=true` (line 1475). This is the ONLY CleanPoint write path.
2. `fire_pending_codec_settings()` calls `ICodecAPI::SetValue(CODECAPI_AVEncVideoForceKeyFrame, VT_BOOL=true)` AFTER ProcessInput returns Ok(()) (line 1072). This is the post-ProcessInput FIRE from SWAP-FIRE DD1.
3. `draining: bool` guard (F1/DD14) sits at top of `while ni_count > 0` loop, BEFORE SWAP. This is correct and must NOT be disturbed.
4. DrainComplete handler (F2/DD17) sends BEGIN_STREAMING + START_OF_STREAM after resetting ni_count/ho_count=0 and draining=false (lines 1201-1203). T8.2 proves this allows post-drain encode continuation.
5. `flush()` docstring says "effectively terminal per session on Intel QSV — do not call flush() mid-stream." BUT F2 was specifically added to make flush+continue work. The docstring is stale relative to Slice 4 DD17 fix. T8.2 is the authoritative evidence: drain → resume cycle works.
6. `CODECAPI_AVEncMPVGOPSize` is NOT currently imported. It is available in `windows::Win32::Media::MediaFoundation` (confirmed via docs-rs). It requires `Win32_Media_MediaFoundation` feature (already enabled).
7. `MFSampleExtension_Discontinuity` availability in the `windows` crate needs verification — not currently imported.
8. Two current CODECAPI symbols: `CODECAPI_AVEncCommonMeanBitRate` (bitrate) and `CODECAPI_AVEncVideoForceKeyFrame` (force IDR). Adding `CODECAPI_AVEncMPVGOPSize` requires one import line change only.
9. `collect_output` detects `is_keyframe` via BOTH `MFSampleExtension_CleanPoint` on the output sample AND `annex_b_contains_idr()` bitstream scan (line 1590). So even if Intel QSV does not set CleanPoint on output, the NAL-type-5 scan will detect it. The T7.1/T7.2 FAILURE is that NO IDR NAL is emitted at all — not a detection problem.
10. Microsoft docs for `CODECAPI_AVEncMPVGOPSize` say "Set this property before starting a recording." This is a RED FLAG for Mechanism A viability mid-stream.

### Mechanisms Evaluated

#### Mechanism A — GOP-size toggle via ICodecAPI (CODECAPI_AVEncMPVGOPSize)

**Concept**: Before submitting the IDR-target frame, call `ICodecAPI::SetValue(CODECAPI_AVEncMPVGOPSize, 1)` (every frame = GOP head = IDR). Submit frame. Restore GOP size to original (e.g. 30). Subsequent frames become P-frames.

**Pros**:
- Clean conceptual signal: GOP size 1 means "every frame is a new GOP" = IDR by definition
- No stream interruption if it works
- ~20 LOC production change
- No `sm-domain` trait changes

**Cons**:
- Microsoft docs explicitly say "Set this property BEFORE starting a recording" — suggests NOT intended for mid-stream use
- Intel QSV has a well-documented pattern of ignoring mid-stream codec parameter changes (alax.info/blog/1823 shows SetValue for quality is ignored mid-stream)
- GOP size change latency: if the new GOP size takes 1 frame of pipeline latency to take effect, there is a 1-frame offset to the IDR
- Must be combined with the current CleanPoint=1 setting (belt-and-suspenders) since we can't know which mechanism Intel QSV actually honors
- Restore timing: if GOP-size=1 fires post-ProcessInput (FIRE step), when does the RESTORE of GOP-size=30 fire? The current CodecApiSwap struct carries only `force_keyframe: bool` — needs extension or a separate mechanism
- High risk: the same "Intel ignores mid-stream codec_api changes" behavior documented elsewhere likely applies here

**LOC estimate**: ~25 production LOC (new CODECAPI import, GOP-size set+restore in fire_pending_codec_settings, CodecApiSwap extension or separate atomic for GOP restore)
**Phase 0 probe**: Send priming frames → request_keyframe() (which adds CODECAPI_AVEncMPVGOPSize=1 to fire queue alongside CleanPoint=1) → send IDR-target frame → flush() → drain. Observe keyframe_indices.
**Vendor uniformity**: Unknown for NVENC. Likely harmless (NVENC tolerates CleanPoint which already works for set_bitrate path). Mark "verify in Phase 0".
**sm-domain impact**: NONE (no trait changes needed)
**Latency cost**: 0 additional ms if effective immediately; 1-frame delay (~33ms at 30fps) if takes effect next GOP boundary
**Risk rating**: HIGH (strong empirical precedent that Intel QSV ignores mid-stream codec API changes)

#### Mechanism B — MFSampleExtension_Discontinuity

**Concept**: Set `MFSampleExtension_Discontinuity=1` on the IDR-target IMFSample (in addition to CleanPoint=1) before ProcessInput. Some MFTs treat Discontinuity as "stream has been interrupted, force IDR on this sample."

**Pros**:
- Per-sample attribute, same pattern as CleanPoint
- Near-zero LOC change (~5 lines in submit_frame)
- Non-breaking addition — NVENC will silently ignore it same as CleanPoint
- No CodecApiSwap changes needed

**Cons**:
- Microsoft documentation says Discontinuity is for DECODER use, not encoder use: "Indicates there was a discontinuity in the stream and this sample is the first after the gap." Decoders use it for error concealment, NOT encoders for IDR triggering.
- There is NO Microsoft documentation supporting Discontinuity as an encoder IDR-trigger mechanism
- Intel QSV behavior with Discontinuity on encoder input is completely undocumented and empirically unknown
- If Intel QSV interprets Discontinuity as "full stream reset" rather than "IDR this frame", it could cause output disruption or dropped frames
- Risk: this may be the same as CleanPoint (already tried and fails on Intel QSV), just with a different attribute name
- `MFSampleExtension_Discontinuity` availability in the `windows` crate needs verification (not found in codebase scan)

**LOC estimate**: ~8 production LOC
**Phase 0 probe**: Same cadence as P0-B but with Discontinuity=1 added to the sample. Observe whether keyframe_indices changes from [0] to include the IDR-target frame index.
**Vendor uniformity**: NVENC will likely ignore (safe). Intel QSV: empirically unknown.
**sm-domain impact**: NONE
**Latency cost**: 0 additional ms
**Risk rating**: HIGH (no documentation supports this as encoder IDR mechanism; likely equivalent to CleanPoint which already fails)

#### Mechanism C — Drain+resume cycle (COMMAND_DRAIN → DrainComplete → F2 resume → submit IDR-target frame as first frame of new stream)

**Concept**: When `request_keyframe()` is called, use the drain+resume cycle that ALREADY EXISTS (F2/DD17) as the IDR trigger. The first frame after DrainComplete+BEGIN_STREAMING+START_OF_STREAM is ALWAYS an IDR (stream restart). Pump loop integration: when `keyframe_pending=true` AND we're NOT currently draining AND a new frame is about to be submitted, trigger COMMAND_DRAIN first, wait for DrainComplete (F2 handles resume), then submit the IDR-target frame.

**Pros**:
- PROVEN to work: T8.2 confirms drain → BEGIN_STREAMING+START_OF_STREAM → new frames produces output
- The first frame after stream restart is IDR by H.264 spec — NOT vendor-dependent
- Addresses the root cause directly: Intel QSV does NOT honor mid-stream ForceKeyFrame, but DOES restart the GOP at stream start
- Reuses existing DrainComplete/F2 infrastructure exactly as designed
- No new CODECAPI calls needed
- NVENC-safe: NVENC also does stream restarts correctly (confirmed by T8.2 PASS on both hosts)
- sm-domain trait changes: NONE

**Cons**:
- LATENCY: ~250ms drain roundtrip latency on Intel QSV (Phase 0 trace #710). This is the biggest risk.
- Integration complexity: the pump_loop must know to trigger DRAIN when keyframe_pending=true, BEFORE consuming the IDR-target frame from the channel. Currently drain is only triggered by: (a) explicit flush() call (drain_pending atomic), (b) channel disconnect. Need a new path: "drain triggered by keyframe request."
- Frame ordering: frames already queued BEFORE the keyframe request must be drained first. This is actually CORRECT behavior (drain pending frames, then IDR-target frame is first of new stream).
- Two-flush scenario: if flush() is called while keyframe_pending=true, we must not double-drain. The existing `draining: bool` guard handles this at the ProcessInput level, but the drain-trigger logic needs care.
- T7.1/T7.2 cadence: the tests use recv_timeout(3s). With ~250ms drain latency + N frames before the IDR, the 3s timeout should be sufficient. However, the cadence must account for the drain — the IDR will arrive AFTER a drain+resume cycle, not immediately after sending the frame.
- The `flush()` docstring says "production callers MUST NOT call this method" — but this slice would essentially REUSE the drain mechanism internally for `request_keyframe()`. That's different from exposing flush() to production callers; it's internal pump_loop behavior.
- Possible STREAM_CHANGE event on stream restart: may trigger renegotiation. Need to verify if BEGIN_STREAMING+START_OF_STREAM after COMMAND_DRAIN causes STREAM_CHANGE on Intel QSV.

**LOC estimate**: ~50-70 production LOC (new `keyframe_drain_pending: AtomicBool` or reuse keyframe_pending to trigger drain, pump_loop drain-trigger integration, T7.1/T7.2 cadence updates to use flush() before recv)
**Phase 0 probe**: Batch-push N priming frames → flush() to drain them → recv priming output → request_keyframe() → send IDR-target frame → flush() again → recv IDR-target output. Observe: (a) does a packet arrive? (b) is it is_keyframe=true? (c) does annex_b_contains_idr() return true?
**Vendor uniformity**: YES, proven for both Intel QSV and NVENC via T8.2
**sm-domain impact**: NONE
**Latency cost**: ~250ms on Intel QSV (Phase 0 trace #710). This is the drain roundtrip. In production WebRTC use (IDR requested on network loss / new viewer), 250ms is tolerable.
**Risk rating**: LOW-MEDIUM. The main risk is the 250ms latency and the pump_loop integration complexity. The mechanism itself is proven.

#### Mechanism D — ICodecAPI BEFORE ProcessInput + NOTACCEPTING retry (reversal of DD1)

**Concept**: Reverse the Slice 4 SWAP-FIRE order for ForceKeyFrame specifically — call `ICodecAPI::SetValue(ForceKeyFrame, true)` BEFORE ProcessInput (original pre-Slice-4 ordering). Add NOTACCEPTING retry: on `MF_E_NOTACCEPTING`, sleep 5ms, retry ProcessInput up to 3 times.

**Pros**:
- If the "BEFORE ProcessInput" order actually works for Intel QSV ForceKeyFrame but was triggering NOTACCEPTING for a different reason (counter desync rather than ordering), retry might help

**Cons**:
- Partially undoes Slice 4 DD1 SWAP-FIRE, which was empirically proven to fix the counter desync panic
- P0-A empirically confirmed: BEFORE-ProcessInput ordering causes MF_E_NOTACCEPTING on Intel QSV
- Retry adds sleep/polling to the pump_loop hot path — violates the POLLING_SLEEP ≤50ms design constraint
- Even if NOTACCEPTING is resolved by retry, P0-B already showed that the ICodecAPI ForceKeyFrame mechanism ITSELF doesn't produce an IDR on Intel QSV (keyframe_indices=[0])
- This addresses the wrong root cause: the problem is NOT ordering but that Intel QSV ignores ForceKeyFrame entirely
- Risk of regressing Mode 1 (T8.2) if SWAP-FIRE is reversed

**LOC estimate**: ~20 production LOC (separate firing order, retry loop)
**sm-domain impact**: NONE
**Risk rating**: VERY HIGH (known to cause NOTACCEPTING; P0-B proved ForceKeyFrame itself fails on Intel QSV regardless of order)
**RECOMMENDATION**: DISCARD. The root cause is that Intel QSV ignores ICodecAPI ForceKeyFrame entirely, not an ordering issue.

#### Mechanism E — FLUSH (not DRAIN) before ICodecAPI

**Concept**: `MFT_MESSAGE_COMMAND_FLUSH` (discards all queued data, resets MFT state, but does NOT produce output) before ICodecAPI SetValue.

**Pros**: None meaningful — flush discards all queued frames

**Cons**:
- FLUSH discards in-flight frames silently — every frame queued BEFORE the keyframe request is LOST
- No documented behavior that FLUSH causes subsequent frames to be IDR on Intel QSV
- `MFT_MESSAGE_COMMAND_FLUSH` is already called in `setup_mft()` at startup — Intel QSV does NOT produce an IDR after startup FLUSH + BEGIN_STREAMING unless it's the first frame (which IS an IDR)
- This is WORSE than DRAIN because DRAIN at least emits the queued frames as output before the reset

**Risk rating**: VERY HIGH, frames lost
**RECOMMENDATION**: DISCARD.

#### Mechanism F — Vendor-specific CODECAPI queries (CODECAPI_AVEncVideoIntraRefreshType, CODECAPI_AVLowLatencyMode, vendor GUIDs)

**Concept**: Query Intel QSV for additional codec properties via `ICodecAPI::IsModifiable` / `IsSupported` to find properties that trigger IDR.

**Candidates to probe**:
- `CODECAPI_AVEncVideoIntraRefreshType`: controls intra-refresh pattern, not point IDR
- `CODECAPI_AVLowLatencyMode` (or `CODECAPI_AVEncCommonRealTime`): low-latency mode may force smaller GOPs but not a specific IDR
- `CODECAPI_AVEncCommonRealTime`: sets real-time encoding mode — may affect IDR cadence
- Vendor-specific Intel QSV GUIDs: Intel SDK historically exposed `{4BE5A994-...}` type GUIDs for extended QSV control. These are NOT in the windows crate and would require raw GUID construction.

**Assessment**: There is NO documented Intel QSV MFT-level API for "force an IDR on the next frame" other than the mechanisms already tried. Intel's own recommendation (per Intel SDK docs) is to use `MFXVideoENCODE_Reset` with `mfxExtEncoderResetOption`, which is a MediaSDK/VPL API, NOT an MFT API.

**Risk rating**: HIGH (speculative, requires empirical Phase 0 to determine what properties Intel QSV MFT exposes via ICodecAPI)
**RECOMMENDATION**: Low-priority Phase 0 option if C fails.

### Approaches Comparison Table

| Mechanism | Concept | Risk | LOC | Phase 0 Probe Needed? | Latency Cost | Vendor Uniform? |
|-----------|---------|------|-----|----------------------|--------------|-----------------|
| A — GOP-size toggle | CODECAPI_AVEncMPVGOPSize=1 before IDR frame | HIGH | ~25 | YES | 0-33ms | Unknown |
| B — Discontinuity attr | MFSampleExtension_Discontinuity=1 on sample | HIGH | ~8 | YES | 0ms | Unknown |
| C — Drain+resume | Trigger COMMAND_DRAIN on keyframe_pending, IDR=first frame of new stream | LOW-MED | ~60 | YES (latency measure) | ~250ms | YES (proven T8.2) |
| D — BEFORE retry | Reverse DD1 + NOTACCEPTING retry | VERY HIGH | ~20 | NO | minor | NO (regresses Mode 1) |
| E — FLUSH | COMMAND_FLUSH + ICodecAPI | VERY HIGH | ~15 | NO | 0ms | NO (loses frames) |
| F — Vendor GUIDs | Intel-specific CODECAPI queries | HIGH | ~40+ | YES | unknown | NO |

### Recommendation

**MECHANISM C (drain+resume) is the recommended primary mechanism.** It is the only approach proven to work on Intel QSV (via T8.2 evidence from Slice 4), addresses the root cause (Intel QSV ALWAYS emits IDR at stream start), and avoids all dependency on Intel QSV honoring any mid-stream codec API signals.

**However, Phase 0 must validate TWO things before locking the design**:

1. **Phase 0 Probe C1** (`phase0_intel_qsv_idr_via_drain_resume_first_frame_after_drain_is_idr`):
   - Confirm that the first frame submitted AFTER DrainComplete+BEGIN_STREAMING+START_OF_STREAM carries is_keyframe=true (i.e., NAL type 5 in bitstream)
   - Cadence: push 3 priming frames → flush() → drain output (confirm priming packets received) → push 1 IDR-target frame → flush() again → recv IDR-target packet → assert is_keyframe=true
   - Expected: is_keyframe=true; annex_b_contains_idr()=true; keyframe_indices=[0, 3] (or similar — IDR at both stream start and post-drain first frame)

2. **Phase 0 Probe C2** (`phase0_intel_qsv_idr_via_drain_resume_latency_measure`):
   - Measure actual round-trip latency for drain → DrainComplete → resume → IDR output
   - Same cadence as C1 but with Instant::now() timing around the flush→recv sequence
   - Expected: ≤500ms total (within T7.1/T7.2 recv_timeout(3s) margin)

3. **Phase 0 Probe A1** (`phase0_intel_qsv_idr_via_gop_size_toggle_pre_submit`):
   - As a low-effort complement to C, test CODECAPI_AVEncMPVGOPSize=1 set BEFORE ProcessInput (in the SWAP step, not FIRE step)
   - This deviates from the current "FIRE after ProcessInput" pattern but is worth testing as a complementary signal
   - If A works AND C works, use C as primary but document A as fallback data point

**Decision tree after Phase 0**:
- C1 probe shows is_keyframe=true on post-drain first frame → **proceed with Mechanism C** (implement pump_loop keyframe-drain path)
- C1 probe shows is_keyframe=false → drain+resume DOES NOT guarantee IDR on Intel QSV → escalate to re-explore with Mechanism F (vendor GUID query)
- A1 probe shows is_keyframe=true via GOP-size toggle → document as alternative, but C is still preferred (less latency risk reasoning)
- Both C1 and A1 fail → mechanism unknown, may require Intel QSV-specific SDK (not MFT-level) — out of scope for this slice

### Phase 0 Probe Plan

All probes use:
- `#[cfg(feature = "hw-encoder")]`
- `#[ignore = "Phase 0 trace probe — manual run on Host A (Intel QSV)"]`
- Retained as permanent regression guards post-fix (DD7/Slice 3+4 convention)

**Probe C1**: `phase0_intel_qsv_idr_via_drain_resume_first_frame_is_idr`
- Cadence: push 3 frames (no recv) → flush() → collect_loop (recv up to 5s for priming output) → request_keyframe() → push 1 IDR-target frame → flush() → recv_timeout(5s) for IDR-target output
- Assertions: at least one priming packet received; IDR-target packet is_keyframe=true; annex_b_contains_idr() on IDR-target bytes = true
- Expected: keyframe_indices = [0, N] where N = first post-drain packet index
- This is the PRIMARY GATE for Mechanism C

**Probe C2**: `phase0_intel_qsv_idr_via_drain_resume_latency_measure`
- Same as C1 but with timing instrumentation
- Records: time from flush() call to DrainComplete event to IDR packet arrival
- Expected: ≤500ms per drain cycle; total T7.1 test time under 3s

**Probe A1**: `phase0_intel_qsv_idr_via_gop_size_toggle`
- Cadence: push 3 frames (no recv) → request_keyframe() → push 1 IDR-target frame → flush() → drain
- Note: Mechanism A requires CODECAPI_AVEncMPVGOPSize import
- Expected: if keyframe_indices contains 3 (IDR-target frame), Mechanism A works (BONUS evidence)
- This probe is LOW PRIORITY; C1 is the primary gate

### Affected Areas

- `crates/sm-infra/src/encode/windows_mft.rs`:
  - `MftEncoderShared` struct: may need `keyframe_drain_pending: AtomicBool` or reuse `keyframe_pending` for drain trigger
  - `swap_pending_codec_settings()`: reads keyframe_pending; if keyframe pending, pump must drain first
  - `pump_loop()`: new drain-trigger path when keyframe_pending=true before NeedInput servicing
  - `flush()` docstring: update "terminal per session" note (stale since Slice 4 DD17)
  - Imports: add `CODECAPI_AVEncMPVGOPSize` if Mechanism A is implemented as complementary fallback
- `crates/sm-infra/tests/windows_mft_encode.rs`:
  - T7.1 and T7.2: update test cadence to use flush() before recv (batch-push + flush pattern, matching T8.2)
  - Add Phase 0 probes C1, C2, (optionally A1)

### Open Questions (OQ List)

- OQ-1: Does the first frame after DrainComplete+BEGIN_STREAMING+START_OF_STREAM on Intel QSV ALWAYS emit as an IDR NAL (type 5)? → Phase 0 Probe C1
- OQ-2: What is the actual drain+resume latency for a keyframe request? → Phase 0 Probe C2
- OQ-3: Does CODECAPI_AVEncMPVGOPSize=1 (GOP toggle) produce an IDR on Intel QSV when set mid-stream? → Phase 0 Probe A1 (low-priority)
- OQ-4: Does `MFSampleExtension_Discontinuity` exist as a symbol in `windows = 0.62.2` with `Win32_Media_MediaFoundation`? → Code compile check (not a Phase 0 hardware probe)
- OQ-5: Does the Mechanism C pump_loop integration cause MF_E_TRANSFORM_STREAM_CHANGE on Intel QSV after BEGIN_STREAMING+START_OF_STREAM? (Slice 2 introduced STREAM_CHANGE renegotiation exactly for this reason — verify it handles the drain+resume case) → Phase 0 Probe C1 (catch STREAM_CHANGE events in drain recv loop)
- OQ-6: Does the drain+resume IDR cycle work on NVENC too (T7.1/T7.2 should pass on both hosts)? → Phase 0 Probe C1 run on Host B as well

### Predecessor Patterns to Reuse

- **SWAP-FIRE pattern (DD1)**: Keep unchanged for `set_bitrate`. For `force_keyframe`, Mechanism C replaces the ICodecAPI call with a drain-trigger; CleanPoint=1 may still be set on the first post-drain frame as belt-and-suspenders.
- **F1 drain-state guard (DD14)**: MUST be preserved unchanged. The new keyframe-drain path must interact correctly with the draining flag.
- **F2 post-drain BEGIN_STREAMING+START_OF_STREAM (DD17)**: Reuse exactly as-is. The DrainComplete handler already does the right thing.
- **Phase 0 batch+flush+drain probe cadence**: All new probes follow the T8.2 and P0-A/P0-B patterns.
- **DD6 STOP rule**: RED smoke gate before C2 remains mandatory.
- **Single-PR override**: per #764 forecast ~230-340 LOC well under 400; no chained PR needed.

### Risk Register

| Mechanism | Risk | Anti-risk Fallback |
|-----------|------|-------------------|
| C (drain+resume) — RECOMMENDED | OQ-1: post-drain first frame may NOT be IDR if Intel QSV treats BEGIN_STREAMING differently mid-session | Phase 0 Probe C1 disproves → explore Mechanism F (vendor GUID query via ICodecAPI::IsSupported scan) |
| C | OQ-2: drain latency may exceed test timeouts | Increase T7.1/T7.2 recv_timeout from 3s to 5s (already used in P0-B); not a production problem for WebRTC use case |
| C | pump_loop integration: keyframe_drain path may double-drain if flush() also pending | Use `draining` flag and single atomic drain-trigger; GUARD-BEFORE-SWAP prevents double ProcessInput |
| A (GOP toggle) | Intel QSV ignores mid-stream CODECAPI as documented by alax.info/blog/1823 | Phase 0 Probe A1 disproves → skip A entirely |
| B (Discontinuity) | No documented encoder IDR semantic; windows crate symbol availability unknown | Only add if C fails and OQ-4 confirms symbol available |

### LOC Forecast

- Production changes (Mechanism C): ~50-70 LOC (pump_loop drain-trigger path, CodecApiSwap/shared state update, docstring fix)
- Test changes (T7.1/T7.2 cadence updates): ~40-60 LOC
- Phase 0 probes (C1 + C2 + A1): ~120-150 LOC
- Total estimated: ~210-280 LOC
- 400-line budget risk: LOW (well under budget even with probes)
- Decision needed before apply: NO
- Chained PRs recommended: NO

### Ready for Proposal

YES — Mechanism C (drain+resume) is well-evidenced from prior Slice 4 work. Phase 0 probes C1/C2 must gate the final design. The proposal phase should define the Phase 0 gate criteria and the pump_loop design for keyframe-driven drain.
