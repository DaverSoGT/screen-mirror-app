# Design: hw-encoder-mft-vendor-compat-slice1-nvenc

> Hybrid mode: openspec mirror; engram canonical at topic_key
> `sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/design`.
> Production-fix branch is forked from master `a35e1ae`, NOT from
> `phase0/slice1-nvenc-trace`. Phase 0 instrumentation (commit `bc6387f` on the
> trace branch) is throw-away and MUST NOT ship in production.

---

## 1. Approach summary

Phase 0 closed the central design question empirically. On Host B (NVIDIA NVENC,
HEAD `bc6387f`), an attribute-walk binary search ran 7 rounds — each building a
fresh `IMFMediaType` via `MFCreateMediaType()` and progressively layering
attributes. All 7 rounds were rejected with HRESULT `0xC00D6D76`
(`MF_E_INVALIDMEDIATYPE`), including Round 1 with only `MF_MT_MAJOR_TYPE` +
`MF_MT_SUBTYPE`. Crucially, `GetOutputAvailableType(0, 0)` SUCCEEDED on the same
MFT and returned a base type already carrying `MFMediaType_Video` /
`MFVideoFormat_H264` / `1920×1080` and (we infer) NVENC vendor-private
attributes that the from-scratch path cannot reproduce.

This refutes H-B1 (drop `MPEG2_PROFILE`), H-B2 (drop `AVG_BITRATE`), and H-B4
(drop `INTERLACE_MODE`) — none of those attributes were even present in Round 1
yet rejection was identical. **H-B3 is now the only viable path: clone the
type returned by `GetOutputAvailableType` and mutate caller-controlled
attributes on top of it.** Slice 1's design ratifies that path.

The change has a tightly bounded blast radius:

- The output-type construction inside `setup_mft` (lines 531–576 of
  `crates/sm-infra/src/encode/windows_mft.rs`) is replaced with a clone-and-
  overlay sequence (~30 LOC net production diff).
- A small enumeration loop wraps `GetOutputAvailableType` for graceful
  fallback (~10 LOC) per spec R1.4 / S1.3.
- The Phase 0 trace block (lines 578–845, ~190 LOC) is NOT carried into
  production: the production-fix branch starts from master `a35e1ae` where the
  trace block does not exist.
- An optional enumeration fallback in `init_mft_sync` for dual-GPU machines
  (~20 LOC, PQ-4) is included because the budget allows it.
- All predecessor invariants (NO_WAIT polling, dual-arm counters,
  `METransformDrainComplete` reset, HaveOutput-first ordering, stop-flag sole
  exit, `apply_pending_codec_settings` pattern) are PRESERVED unchanged.

---

## 2. Architecture decisions

| ID  | Decision | Status | Rationale | Source |
|-----|----------|--------|-----------|--------|
| DD1 | Replace from-scratch `MFCreateMediaType` + 8 setters with `GetOutputAvailableType(0, n)` clone + 4 caller-controlled mutators on top of the cloned base type. | **RATIFIED** | Phase 0 transcript #152: all 7 from-scratch rounds REJECTED with `0xC00D6D76`; `GetOutputAvailableType(0, 0)` SUCCEEDED. NVENC requires types carrying its vendor-private attributes — only types it ITSELF returned satisfy this. | Phase 0 evidence #152 |
| DD2 | Wrap `GetOutputAvailableType(0, n)` in a small enumeration loop bounded at `n < 16`, breaking on first OK or on `MF_E_NO_MORE_TYPES`. If no type is available, fail with `EncoderError::InitFailed("no output media type available from MFT")`. Do NOT fall back to from-scratch construction. | **RATIFIED** | Spec R1.4 / S1.3 mandates graceful fallback; Phase 0 proved from-scratch is dead on NVENC, so the fallback exit is `Err`, not `MFCreateMediaType`. The 16 cap prevents infinite loops on misbehaving MFTs. | Spec R1.4, PQ envelope |
| DD3 | On the cloned type, overlay exactly 4 attributes (caller-controlled): `MF_MT_FRAME_SIZE`, `MF_MT_FRAME_RATE`, `MF_MT_AVG_BITRATE`, `MF_MT_MPEG2_PROFILE`. Also overlay `MF_MT_INTERLACE_MODE = Progressive`. Do NOT touch `MF_MT_PIXEL_ASPECT_RATIO` (NVENC infers 1:1 by default and the absent state is observed in Phase 0). | **RATIFIED** | Phase 0 transcript shows NVENC's base type carries `MAJOR_TYPE`, `SUBTYPE`, and `FRAME_SIZE = 1920×1080` (advertised default) but NOT `FRAME_RATE`, `INTERLACE_MODE`, `MPEG2_PROFILE`, or `AVG_BITRATE`. We MUST set the 4 caller-controlled values; we MUST resolve `INTERLACE_MODE` consistently with the SW path. PAR is left absent. | Phase 0 transcript, predecessor consistency |
| DD4 | Setting `MF_MT_MPEG2_PROFILE = Main(77)` on the cloned type is attempted on the first try. If `SetOutputType` then returns `MF_E_INVALIDMEDIATYPE`, retry exactly ONCE without the profile attribute. If still rejected, return Err — do not fall back further. | **RATIFIED** | Phase 0 round 7 set MPEG2_PROFILE on a from-scratch type and was rejected, but that does not isolate the profile vs. the from-scratch source. Cloned-base behavior is unknown until apply runs on Host B. Single-retry keeps the round-trip count bounded; if NVENC actually rejects MPEG2_PROFILE on cloned base, the retry path produces a valid stream and a follow-up change introduces `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` post-stream-start. | Spec R1.4, predecessor warn-and-continue policy |
| DD5 | Include the `init_mft_sync` enumeration fallback (PQ-4): if `setup_mft(pactivates[i])` fails for `i < count`, attempt `pactivates[i+1]`; emit a `warn!` per skip; return the LAST attempt's error if all fail. | **RATIFIED, conditional resolved** | Estimated ~20 LOC; total Slice 1 diff stays under 80 production LOC, well within the 400-line budget. Self-heals dual-GPU machines. Single-GPU hosts (Host B) exercise only `pactivates[0]`, making smoke regression coverage automatic. The multi-MFT branch is documented as not unit-tested. | PQ-4 = include, line-budget headroom |
| DD6 | Phase 0 instrumentation is throw-away. Production-fix branch forks from master `a35e1ae`, NOT from `phase0/slice1-nvenc-trace`. The trace block at lines 578–845 of windows_mft.rs on the trace branch is NOT carried over. | **RATIFIED** | The trace branch has served its purpose (#152). Carrying `+190 LOC` of `eprintln!`/`tracing::trace!` instrumentation into production violates spec R4.3 (production-fix PR diff MUST NOT contain Phase 0 trace code) and pollutes release builds via `cfg(debug_assertions)` (debug builds still log). Branching from master keeps the production diff minimal. | Spec R4.3, decision D-1 |
| DD7 | Strict TDD discipline — RED before GREEN per capability: <ul><li>**C1** (`setup_mft` uses cloned base): Host B smoke regression — 10 currently-failing tests are RED, become GREEN after apply.</li><li>**C2** (graceful enumeration loop on Err): not unit-testable on a real `IMFTransform`; documented as smoke-only.</li><li>**C3** (`MPEG2_PROFILE` retry-without-profile fallback): apply-time evidence on Host B; if MPEG2_PROFILE accepted on cloned base the retry path is dead code, exercised only by code review.</li><li>**C4** (`init_mft_sync` enumeration fallback): single-GPU smoke covers `i = 0`; the `i = 1+` path is not unit-tested and not smoke-tested.</li></ul> | **RATIFIED** | The 10 failing Host B tests ARE the RED state. No new test file is needed. C2/C3/C4 unit tests would require mocking COM `IMFTransform`, which is not feasible in this codebase. Smoke regression on Host B is the primary GREEN evidence. | Project standard "Strict TDD ENABLED", apply-time pragmatism |
| DD8 | Single PR for Slice 1 (no chaining). Estimated production diff: ~50–80 LOC (replacement) – ~50 LOC (deleted setters) + ~10 LOC (enumeration loop wrapper) + ~20 LOC (init_mft_sync fallback) = ~30–60 NET production lines. Tests unchanged. | **RATIFIED** | Auto-chain delivery strategy splits only when 400-line cognitive budget is at risk. Slice 1's net diff is ~10× under budget. Chaining would add review overhead with zero benefit. | `auto-chain`, 400-line budget |
| DD9 | Public API of `VideoEncoder`, `EncoderConfig`, `EncodedPacket`, `EncoderError` is FROZEN. No internal flag additions required for this change (Slice 1 does NOT need the H-B1 `session_init_pending` flag because H-B1 is REFUTED — `MPEG2_PROFILE` is set on the type, not via `ICodecAPI`). | **RATIFIED** | Phase 0 refuted H-B1; spec R3 (apply_pending_codec_settings extension) becomes a no-op for this slice. R3.4 calling-contract invariant remains. R8 frozen-API invariant is automatically satisfied. | Phase 0 transcript #152, spec R8 |

---

## 3. Code-shape sketch

### 3.1 New `setup_mft` output-type block (replaces lines 531–576 + deletes Phase 0 block 578–845)

```rust
// Step 7b: SetOutputType FIRST (MFT requirement: output before input).
//
// NVENC rejects types built from scratch via MFCreateMediaType() with
// MF_E_INVALIDMEDIATYPE — confirmed by Phase 0 attribute walk (transcript
// #152). NVENC only accepts types it returned itself via
// GetOutputAvailableType(...), which carry vendor-private attributes that
// from-scratch construction cannot reproduce.
//
// Strategy: clone the type from GetOutputAvailableType(0, n), mutate
// caller-controlled attributes on top of it, then SetOutputType.
let out_type: IMFMediaType = get_output_type_from_mft(mft)?;

unsafe {
    // Caller-controlled overlays (4 + 1).
    out_type
        .SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | (h as u64))
        .map_err(|e| EncoderError::InitFailed(format!(
            "SetUINT64 FrameSize(out cloned): 0x{:08X}", e.code().0
        )))?;
    out_type
        .SetUINT64(&MF_MT_FRAME_RATE, ((config.framerate as u64) << 32) | 1)
        .map_err(|e| EncoderError::InitFailed(format!(
            "SetUINT64 FrameRate(out cloned): 0x{:08X}", e.code().0
        )))?;
    out_type
        .SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)
        .map_err(|e| EncoderError::InitFailed(format!(
            "SetUINT32 Bitrate(out cloned): 0x{:08X}", e.code().0
        )))?;
    out_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .map_err(|e| EncoderError::InitFailed(format!(
            "SetUINT32 Interlace(out cloned): 0x{:08X}", e.code().0
        )))?;

    // Profile: try with MPEG2_PROFILE first; on rejection retry once without.
    out_type
        .SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_MAIN)
        .map_err(|e| EncoderError::InitFailed(format!(
            "SetUINT32 Profile(out cloned): 0x{:08X}", e.code().0
        )))?;

    match mft.SetOutputType(0, &out_type, 0) {
        Ok(()) => {}
        Err(e) if e.code().0 as u32 == 0xC00D_6D76 => {
            // Profile rejected on cloned base — retry without it (DD4).
            tracing::warn!(
                "SetOutputType rejected MPEG2_PROFILE on cloned base; \
                 retrying without profile (HRESULT=0x{:08X})",
                e.code().0
            );
            // Remove and retry. IMFMediaType has DeleteItem(REFGUID).
            out_type.DeleteItem(&MF_MT_MPEG2_PROFILE).map_err(|e2| {
                EncoderError::InitFailed(format!(
                    "DeleteItem MPEG2_PROFILE: 0x{:08X}", e2.code().0
                ))
            })?;
            mft.SetOutputType(0, &out_type, 0).map_err(|e2| {
                EncoderError::InitFailed(format!(
                    "SetOutputType(retry-no-profile): 0x{:08X}", e2.code().0
                ))
            })?;
        }
        Err(e) => {
            return Err(EncoderError::InitFailed(format!(
                "SetOutputType(cloned): 0x{:08X}", e.code().0
            )));
        }
    }
}
```

### 3.2 New helper `get_output_type_from_mft` (DD2 enumeration loop)

```rust
/// Return the first available output media type from the MFT, enumerating
/// `GetOutputAvailableType(0, n)` for `n = 0..MAX_TYPES`. NVENC rejects
/// types built via `MFCreateMediaType()` (Phase 0 #152), so this is the
/// only viable source for the output type.
///
/// Errors:
/// - `EncoderError::InitFailed` if no output type is available within
///   `MAX_TYPES` indices, or if all returned errors are non-terminal.
fn get_output_type_from_mft(mft: &IMFTransform)
    -> Result<IMFMediaType, EncoderError>
{
    const MAX_TYPES: u32 = 16;
    let mut last_hr: u32 = 0;
    for n in 0..MAX_TYPES {
        match unsafe { mft.GetOutputAvailableType(0, n) } {
            Ok(t) => return Ok(t),
            Err(e) => {
                let code = e.code().0 as u32;
                last_hr = code;
                // MF_E_NO_MORE_TYPES = 0xC00D36B9: terminate loop.
                if code == 0xC00D_36B9 {
                    break;
                }
                // Other errors: try next index.
                tracing::trace!(
                    "GetOutputAvailableType(0, {}) -> HRESULT=0x{:08X}, trying next",
                    n, code
                );
            }
        }
    }
    Err(EncoderError::InitFailed(format!(
        "no output media type available from MFT (last HRESULT=0x{:08X})",
        last_hr
    )))
}
```

### 3.3 New `init_mft_sync` enumeration fallback (DD5)

The current `enumerate_and_activate` (lines 347–395) takes `pactivates[0]`
and returns one `IMFTransform`. To support fallback, the call site in
`init_mft_sync` (lines 318–343) must iterate, attempting `setup_mft` per
candidate. The minimal refactor:

```rust
// New helper — same shape as enumerate_and_activate but yields ALL activates.
fn enumerate_activates() -> Result<Vec<IMFActivate>, EncoderError> {
    let input_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut pactivates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            Some(&input_info),
            Some(&output_info),
            &mut pactivates as *mut _ as _,
            &mut count,
        )
        .map_err(|e| EncoderError::InitFailed(format!(
            "MFTEnumEx: 0x{:08X}", e.code().0
        )))?;
    }

    if count == 0 || pactivates.is_null() {
        return Err(EncoderError::InitFailed(
            "no hardware MFT H264 encoder registered".into(),
        ));
    }

    // Copy into Vec so caller can iterate without juggling the raw pointer.
    let mut out = Vec::with_capacity(count as usize);
    unsafe {
        let slice = std::slice::from_raw_parts(pactivates, count as usize);
        for slot in slice {
            if let Some(a) = slot {
                out.push(a.clone());
            }
        }
        CoTaskMemFree(Some(pactivates as *const _));
    }
    Ok(out)
}
```

In `init_mft_sync`, replace the single `enumerate_and_activate()` call with:

```rust
let activates = enumerate_activates().map_err(|e| { /* ...unchanged shutdown... */ e })?;

for (idx, activate) in activates.iter().enumerate() {
    let mft: IMFTransform = match unsafe { activate.ActivateObject() } {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "ActivateObject pactivates[{}] failed: 0x{:08X}; trying next",
                idx, e.code().0
            );
            continue;
        }
    };
    // Probe via setup attempt — if setup_mft would fail here we skip.
    // NOTE: setup_mft is called in run_encoder_thread, NOT in init_mft_sync,
    // so the fallback at init time can only catch ActivateObject and ICodecAPI
    // cast failures. The full setup_mft fallback is addressed in DD5 limitation
    // documented below.
    match mft.cast::<ICodecAPI>() {
        Ok(codec_api) => {
            // Cleanup remaining activates / shutdown unchanged.
            return Ok((mft, codec_api));
        }
        Err(e) => {
            tracing::warn!(
                "ICodecAPI cast pactivates[{}] failed: 0x{:08X}; trying next",
                idx, e.code().0
            );
            continue;
        }
    }
}
// All candidates failed.
unsafe {
    let _ = MFShutdown();
    CoUninitialize();
}
Err(EncoderError::InitFailed(
    "all hardware MFT candidates failed activation/cast".into(),
))
```

**DD5 LIMITATION**: `setup_mft` runs on the encoder thread (`run_encoder_thread`,
line 444) AFTER `init_mft_sync` returns. The enumeration fallback as designed
covers `ActivateObject` and `ICodecAPI` cast failures only — it does NOT
re-enumerate when `setup_mft` itself rejects an MFT. Extending fallback to
post-`setup_mft` would require either (a) moving `setup_mft` into
`init_mft_sync` (architectural shift, not Slice 1 scope) or (b) propagating an
"activates list" through the encoder thread spawn. Neither fits the 400-line
budget. Spec R5.2 ("if `setup_mft(pactivates[i])` fails, attempt
`pactivates[i+1]`") is partially satisfied: the change covers the activation/
cast failure path; deferred extension lives in
`hw-encoder-mft-enumeration-fallback-deep` if Slice 2 reveals demand.

### 3.4 What is NOT changed

Lines that MUST remain byte-identical (R2 invariants):

- `pump_loop` (lines ~1020–1140 in the post-Phase-0 file, equivalent block on
  master): NO_WAIT polling, dual-arm `ni_count`/`ho_count`, HaveOutput-first
  drain, `METransformDrainComplete` reset, stop-flag sole exit.
- `apply_pending_codec_settings` (lines 996–1024): the existing keyframe +
  bitrate ICodecAPI path is preserved unchanged.
- `setup_mft` Steps 7a (ASYNC_UNLOCK), 7c (input type NV12), 7f–7h (FLUSH +
  BEGIN_STREAMING + START_OF_STREAM): unchanged.
- `MftEncoderShared`: no new fields. (H-B1's `session_init_pending` flag is
  REFUTED and not added.)
- Public API in `crates/sm-domain/src/encode.rs`: byte-identical (R8 frozen).

---

## 4. Data flow

```
                       ┌───────────────────────┐
                       │  init_mft_sync (caller │
                       │   thread, MTA COM)    │
                       └──────────┬────────────┘
                                  │
                                  ▼
                  enumerate_activates() → Vec<IMFActivate>
                                  │
                                  ▼  (DD5: try each candidate)
                  ActivateObject + cast<ICodecAPI>
                                  │
                                  ▼
                       (mft, codec_api) returned
                                  │
                                  ▼
                       ┌───────────────────────┐
                       │  run_encoder_thread   │
                       │  (encoder thread, MTA)│
                       └──────────┬────────────┘
                                  │
                                  ▼
                            setup_mft(&mft)
                                  │
                  ┌───────────────┴────────────────┐
                  │                                │
                  ▼                                ▼
       Step 7a ASYNC_UNLOCK              Step 7b OUTPUT TYPE
       (unchanged)                       ── DD1 ───────────────
                                           get_output_type_from_mft(mft)
                                              ─ enumerate (0, n) loop ─ DD2 ─
                                              ─ first OK or NO_MORE_TYPES ─
                                           overlay 5 attrs (DD3)
                                           SetOutputType
                                              ↳ on REJECT(0xC00D6D76):
                                                DeleteItem(MPEG2_PROFILE)
                                                retry once (DD4)
                                  │
                                  ▼
       Step 7c INPUT TYPE NV12 (unchanged)
       Step 7f–7h FLUSH + BEGIN_STREAMING + START_OF_STREAM (unchanged)
                                  │
                                  ▼
                            pump_loop (R2 invariants UNCHANGED)
                                  │
                                  ├── HaveOutput → ProcessOutput → tx
                                  ├── NeedInput  → apply_pending_codec_settings
                                  │                + ProcessInput
                                  ├── DrainComplete → reset counters (no exit)
                                  └── stop flag → exit
```

**Only attribute-negotiation changes.** Pump loop, dual-arm counters, NO_WAIT
polling, `METransformDrainComplete` handling, ICodecAPI keyframe / bitrate
path: all untouched.

---

## 5. API surface

### 5.1 Public API — FROZEN

`crates/sm-domain/src/encode.rs`:

- `VideoEncoder` trait — byte-identical
- `EncoderConfig` struct — byte-identical
- `EncodedPacket` struct — byte-identical
- `EncoderError` enum — byte-identical

`no_platform_deps.rs` invariant: PRESERVED.

### 5.2 Internal helpers — additions

| Symbol | Visibility | Notes |
|--------|------------|-------|
| `get_output_type_from_mft(&IMFTransform) -> Result<IMFMediaType, EncoderError>` | module-private | New. DD2. |
| `enumerate_activates() -> Result<Vec<IMFActivate>, EncoderError>` | module-private | New. DD5. Replaces `enumerate_and_activate` body; the old function may be removed entirely or shrunk to wrap `enumerate_activates().map(|v| v[0].ActivateObject())` for a one-call path that is no longer used. |

### 5.3 Internal helpers — unchanged

`apply_pending_codec_settings`, `build_imfsample`, `extract_bytes`,
`effective_dimensions`, `pump_loop`, `setup_mft` (signature), `run_encoder_thread`,
`MftEncoderShared` (fields).

---

## 6. Failure modes

| ID | Trigger | Detection | Behavior |
|----|---------|-----------|----------|
| F1 | `GetOutputAvailableType(0, n)` returns Err for all `n < MAX_TYPES`. | `last_hr` accumulated; loop exits on `MF_E_NO_MORE_TYPES` or `MAX_TYPES` cap. | Return `EncoderError::InitFailed` with last HRESULT. Encoder thread exits cleanly via `run_encoder_thread` early-return path. |
| F2 | `SetOutputType(cloned base + 5 overlays)` returns `0xC00D6D76` despite cloned base. | HRESULT match in `setup_mft`. | DD4 retry: `DeleteItem(MPEG2_PROFILE)` + one `SetOutputType` retry. If still rejected, return `Err`. |
| F3 | `SetOutputType(cloned base, no profile)` STILL rejects. | HRESULT non-zero on retry. | Return `EncoderError::InitFailed("SetOutputType(retry-no-profile)")`. Documented as out-of-Slice-1 scope; would require `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` follow-up change. |
| F4 | Enumeration loop max-iter exhausted (16 indices, none OK). | `MAX_TYPES` reached, no break on `MF_E_NO_MORE_TYPES`. | Return `EncoderError::InitFailed`. 16 is well above any plausible NVENC type-count (typically 1–3). |
| F5 | All `IMFActivate` candidates fail `ActivateObject` or `ICodecAPI` cast (DD5 init-time fallback). | `init_mft_sync` exhausts the `Vec<IMFActivate>`. | Return `EncoderError::InitFailed("all hardware MFT candidates failed activation/cast")`. Smoke-untestable on single-GPU hosts. |
| F6 | `DeleteItem(MPEG2_PROFILE)` itself fails (highly unlikely — IMFMediaType always supports DeleteItem). | HRESULT non-zero. | Return `EncoderError::InitFailed("DeleteItem MPEG2_PROFILE")`. |

All failure modes preserve predecessor behaviour: encoder thread early-return,
`stop()` joins cleanly within deadline (R2 / S2.1 / S2.2 unaffected).

---

## 7. Testing strategy

### 7.1 Capability ↔ requirement ↔ test mapping

| Capability | Spec rows | RED state | GREEN evidence | Method |
|------------|-----------|-----------|----------------|--------|
| C1 — `setup_mft` uses cloned base | R1.1, R1.2, R1.3, R6.1, R8.1 | 10 currently-failing NVENC smoke tests on Host B (#152) | Same 10 tests PASS post-apply | Smoke regression on Host B; required transcript per R6.3 |
| C2 — Graceful enumeration loop on Err | R1.4, S1.3 | Cannot smoke (NVENC always returns OK at n=0); not unit-testable on real `IMFTransform` | Code review only; runtime behaviour exercised opportunistically when other vendors are first encountered | Documented gap; SHALL be cited in apply-progress and verify-report |
| C3 — `MPEG2_PROFILE` retry-without-profile fallback | R1.4 (extension) | Path may be dead code on Host B if NVENC accepts MPEG2_PROFILE on cloned base | Code review confirms retry path compiles and matches DD4 contract; runtime activation deferred to follow-up if NVENC rejects | Documented gap; smoke ratifies whichever path executes |
| C4 — `init_mft_sync` activation fallback | R5.1, R5.2, R5.3, R5.4, S5.2 | Cannot smoke on single-GPU hosts | Single-GPU smoke (Host B) PASSES via index 0; multi-GPU path is code-review only | Documented gap; spec `A4` already permits this |
| Predecessor invariants | R2.1–R2.5, S2.1, S2.2 | 7 currently-passing tests + T-NEW-1 + T-NEW-2 | All still PASS post-apply | Smoke regression on Host B + workspace nextest |
| Public API frozen | R8.1, R8.2, R8.3, S8.1 | n/a (precondition) | `cargo check --workspace` and verify-phase API diff | Gate 1 + verify |
| 7 quality gates | R7.1, S7.1, S7.2 | n/a | All 7 GREEN | Local quality gates per project standards |

### 7.2 Test order (per Strict TDD)

1. **RED is already established**: 10 NVENC smoke tests fail on master and on
   the trace branch HEAD with HRESULT `0xC00D6D76`. The Phase 0 transcript
   (#152) IS the RED evidence.
2. **GREEN apply**: implement DD1–DD5 on the production-fix branch (forked from
   master `a35e1ae`). No new test files. No test edits.
3. **GREEN verify**: re-run `cargo nextest run -p sm-infra --test
   windows_mft_encode --features hw-encoder --run-ignored=ignored-only` on Host
   B; expect 18/18 PASS. Capture transcript per R6.3.

### 7.3 Out-of-test-coverage paths (acknowledged)

| Path | Why uncovered | Risk mitigation |
|------|---------------|-----------------|
| `get_output_type_from_mft` enumeration loop continuation | NVENC always returns OK at `n=0`; loop body's `continue` never executes on Host B | Code review; if Slice 2 (Intel QSV) exposes Err at n=0, that path is exercised then |
| `MPEG2_PROFILE` retry-without-profile (DD4) | Apply-time apply may reveal NVENC accepts profile; if so, retry is dead | Code review; verify-time transcript MUST log either "OK first try" or "OK after profile delete" so the path actually exercised is auditable |
| `init_mft_sync` activation fallback `i = 1+` | Single-GPU on Host B and Host A | Architectural review; spec A4 acknowledges |
| All `_E_NOT_FOUND` / `MF_E_NO_MORE_TYPES` discriminator branches in DD2 | Won't fire on NVENC | Code review |

---

## 8. Risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| R-1 | NVENC rejects `MF_MT_MPEG2_PROFILE` on cloned base type. DD4 retry produces a profile-less stream; H.264 decoders may not interoperate. | Medium | Medium | DD4 retry produces a valid stream (Main is the default profile when absent); follow-up change adds `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` if interoperability gap is observed. |
| R-2 | `MF_MT_INTERLACE_MODE` overlay also rejected by NVENC on cloned base (Phase 0 did not test interlaced overlay on cloned). | Low | Low | If apply reveals this, mirror DD4 pattern: `DeleteItem(MF_MT_INTERLACE_MODE)` + retry. NVENC infers Progressive when absent. Slice 1 may need a 2nd retry round. |
| R-3 | `enumerate_activates` allocation (Vec + clone of `IMFActivate`) introduces a COM refcount leak if `MFTEnumEx`'s ownership semantics differ from current single-take pattern. | Low | High (memory leak in encoder init path) | `IMFActivate::clone()` is COM AddRef; `CoTaskMemFree` releases the array memory (not the activates). Vec drop releases each clone. Predecessor's single-take pattern is equivalent: both leak nothing. Apply MUST add a `cargo clippy` pass to confirm no `forget()` patterns. |
| R-4 | `setup_mft` enumeration loop interacts adversely with Intel QSV in Slice 2 (different vendor returns different `n=0` shape). | Low (Slice 2 problem) | Medium for Slice 2 | Slice 2 starts from Slice 1's GREEN state; Phase 0 on Host A re-runs the same probe. If Intel QSV's base type lacks attributes Slice 1 NVENC overlay assumes are accepted (e.g., `INTERLACE_MODE`), that surfaces in Slice 2 design. |
| R-5 | `DeleteItem` on `IMFMediaType` may not be available in `windows = "0.62.2"` bindings under that name. | Low | Low | If absent, alternative is `out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, 0)` or rebuild-via-clone. Apply-time verification, mechanical fix. |
| R-6 | Slice 1 fix accidentally breaks Host A (Intel QSV) baseline by adding the enumeration loop, even though Intel QSV's `setup_mft` already fails at runtime in `ProcessOutput` (Manifestation A). | Low | Medium (worse Slice 2 baseline) | Slice 1 only changes attribute negotiation in `setup_mft`; Manifestation A fires in `ProcessOutput`, downstream of `setup_mft` and unaffected by output-type construction (Intel accepts attribute-built types). Verify on Host A confirms baseline unchanged. |
| R-7 | DD5 enumeration fallback as scoped (init-time only, NOT post-`setup_mft`) misleads operators into expecting deeper resilience. | Low | Low | Apply-progress and archive-report MUST document the limitation explicitly (per §3.3 LIMITATION note). Spec R5.2's "if `setup_mft(pactivates[i])` fails" wording is partially fulfilled — verify-phase notes the gap. |
| R-8 | Production branch fork point divergence: if Slice 1 lands AFTER another change merges to master, the production-fix branch base (`a35e1ae`) becomes stale and rebase is required. | Medium | Low | Fork point is recorded in apply-progress; rebase is a mechanical operation; the trace branch (`bc6387f`) remains untouched and discardable. |

---

## 9. Open questions resolved

| Question | Source | Resolution |
|----------|--------|------------|
| H-B1 — `MPEG2_PROFILE = Main(77)` rejected by NVENC? | Explore #147 | **REFUTED**. Round 1 (only MAJOR_TYPE+SUBTYPE, no profile) was rejected. The from-scratch source itself is the rejection cause. |
| H-B2 — `AVG_BITRATE` rejected by NVENC? | Explore #147 | **REFUTED**. Round 1 had no bitrate either. |
| H-B3 — `GetOutputAvailableType`-guided clone-and-overlay? | Explore #147 | **CONFIRMED**. `GetOutputAvailableType(0, 0)` returns OK on NVENC; clone-and-overlay is the load-bearing fix. |
| H-B4 — `INTERLACE_MODE` rejected? | Explore #147 | **REFUTED**. Round 1 had no INTERLACE_MODE. |
| H-B5 — Resolution-specific rejection (640×480 vs 1920×1080)? | Explore #147 | **REFUTED**. Both 640×480 and 1920×1080 fail identically (Phase 0 #152 §4). |
| PQ-1 — Phase 0 fresh runs on Host B? | Explore #147 | **DONE** (PQ-1=C; transcript #152). |
| PQ-2 — One change or two slices? | Explore #147 | **TWO SLICES** (PQ-2=B). Slice 1 = NVENC; Slice 2 deferred. |
| PQ-3 — Add T-NEW-3? | Explore #147 | **DEFERRED**. |
| PQ-4 — Enumeration fallback in `init_mft_sync`? | Explore #147 | **INCLUDED** (DD5). Init-time scope only (LIMITATION documented). |
| PQ-5 — `MF_MT_DEFAULT_STRIDE` on input type? | Explore #147 | **SLICE 2** (out of Slice 1 scope). |
| OQ — Does NVENC accept `MPEG2_PROFILE` on cloned base? | Phase 0 transcript #152 §3 implication | **DEFERRED to apply-time**. DD4 specifies single-retry-without-profile fallback to handle either branch. |
| OQ — Does NVENC accept `INTERLACE_MODE` on cloned base? | Phase 0 transcript #152 §1 (absent in advertised type) | **DEFERRED to apply-time**. Recommendation: overlay; if rejected, mirror DD4 pattern. |
| OQ — Should PAR be overlaid? | Phase 0 transcript #152 §1 | **NO** (DD3). NVENC's advertised type omits PAR; it infers 1:1 by default. Predecessor `setup_mft` overlaid PAR but Phase 0 round 4 (with PAR) still rejected — adding PAR to cloned base is unjustified. |

---

## 10. Slice 1 line-budget estimate

### 10.1 Production diff (excluding Phase 0 trace branch — that branch is throw-away)

| Section | Action | Estimated LOC |
|---------|--------|---------------|
| `setup_mft` lines 531–576 (output-type construction) | DELETE 8 setters + 1 `MFCreateMediaType` | ~−45 |
| `setup_mft` new clone-and-overlay block | INSERT (DD1 + DD3 + DD4) | ~+55 |
| New `get_output_type_from_mft` helper (DD2) | INSERT | ~+25 |
| New `enumerate_activates` helper (DD5) | INSERT | ~+30 |
| Old `enumerate_and_activate` (DD5 supersedes) | DELETE or keep as 5-line wrapper | ~−40 (or ~−30) |
| `init_mft_sync` lines 318–343 | REWRITE (DD5) | net ~+15 |

**Net production diff**: ~+40 to ~+55 LOC. Comfortably under the 400-line
single-PR budget. **No need to chain.**

### 10.2 Test diff

Zero. The 10 RED tests on Host B move to GREEN without test-side changes
(R6.1).

### 10.3 Phase 0 instrumentation removal

Not part of the production-fix diff. The trace block (~190 LOC) lives
exclusively on `phase0/slice1-nvenc-trace` (commit `bc6387f`). The production
branch is forked from master `a35e1ae`, which never carried the trace block.
Per DD6, the trace branch is throw-away; the production-fix PR diff against
master will not show those lines.

---

## 11. Predecessor invariant mapping (preserved, not re-specified)

| Pred-Req | Description | Slice 1 obligation |
|----------|-------------|--------------------|
| Pred-R1 | NO_WAIT polling | `pump_loop` untouched |
| Pred-R2 | Dual-arm counters | `pump_loop` untouched |
| Pred-R3 | HaveOutput-first drain | `pump_loop` untouched |
| Pred-R4 | DrainComplete reset | `pump_loop` untouched |
| Pred-R5 | Stop flag sole exit | `pump_loop` untouched |
| Pred-R6 | apply_pending pattern | `apply_pending_codec_settings` untouched |
| Pred-T-NEW-1 | stop idle ≤ 2s | S2.1 — not regressed (PASS in Phase 0 #152) |
| Pred-T-NEW-2 | stop active ≤ 2s | S2.2 — verify on production branch (#152 ran on trace branch with secondary failure mode; production branch should restore PASS) |

---

## Result Contract

- **status**: done
- **executive_summary**: Phase 0 empirically proved that NVENC's H.264 MFT rejects every from-scratch `IMFMediaType` (10 of 18 Host B smokes fail with `MF_E_INVALIDMEDIATYPE`) and that `GetOutputAvailableType(0, 0)` succeeds; Slice 1 ratifies H-B3 — clone the MFT-returned base type and overlay 5 caller-controlled attributes (FRAME_SIZE, FRAME_RATE, AVG_BITRATE, INTERLACE_MODE, MPEG2_PROFILE) with single-retry-without-profile fallback (DD4) and a bounded enumeration loop (DD2); a small init-time activation fallback (DD5) covers dual-GPU machines partially. The production diff lands on a branch forked from master `a35e1ae` (NOT from the throw-away trace branch); estimated net production diff is ~40–55 LOC, single PR.
- **artifacts**: `sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/design` (engram) + `openspec/changes/hw-encoder-mft-vendor-compat-slice1-nvenc/design.md`
- **next_recommended**: `sdd-tasks` (spec + design now both done; tasks consume both)
- **risks**: see §8 (R-1 profile rejection on cloned base, R-2 INTERLACE_MODE rejection on cloned base, R-3 IMFActivate refcount on enumeration, R-7 DD5 init-only scope limitation)
- **skill_resolution**: injected
- **dd_count**: 9
- **production_diff_estimate_loc**: ~40–55 net (production only; tests unchanged; Phase 0 trace block lives exclusively on `phase0/slice1-nvenc-trace` and is not carried)
