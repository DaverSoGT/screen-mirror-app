# Apply Progress — hw-encoder-mft-nvenc-mid-stream-idr-mechanism

> **Phase**: Batch 2 (Phase 0 — ForceKeyFrame mechanism + P2 cross-vendor probes)
> **Artifact store**: hybrid (engram + openspec)
> **Branch**: `feat/hw-encoder-mft-nvenc-mid-stream-idr-mechanism`
> **Branch tip after Batch 2**: `3bd5b95`
> **Date**: 2026-05-10

---

## Batch 1 Status (completed)

| Task | Description | Status |
|------|-------------|--------|
| P0-1 | `EncoderVendor` enum + GUID detection in `probe_and_select_mft` | **DONE** (commit `6efd4c6`) |
| P0-2 | `cleanpoint_pending: AtomicBool` on `MftEncoderShared` | **DONE** (commit `6efd4c6`) |
| P0-3 | `request_keyframe_via_cleanpoint()` public method | **DONE** (commit `6efd4c6`) |
| P0-4 | `submit_frame()` updated with `force_cleanpoint: bool` + CleanPoint write | **DONE** (commit `6efd4c6`) |
| P0-5 | pump_loop: consume `cleanpoint_pending` before `submit_frame` | **DONE** (commit `6efd4c6`) |
| P0-6 | `request_keyframe()` vendor dispatch (NVENC → CleanPoint; else → G) | **DONE** (commit `6efd4c6`) |
| P0-7 | Phase 0 probe P1 `phase0_nvenc_cleanpoint_idr_via_input_sample_attribute` | **SCAFFOLDED** (commit `aee5750`) |
| P0-8 | Compile + clippy clean | **DONE** |
| P0-9 | Full workspace test suite (non-hw-encoder) | **DONE** — 611/611 passed, 19 skipped |

### P1 Run Result

**FALSIFIED** (engram #807, trace `nvenc-p1-trace.log`):
- All 30 post-request packets `is_keyframe=false` — NVENC ignores `MFSampleExtension_CleanPoint` on input (current driver).
- Vendor dispatch stays on CleanPoint for NVENC in production code (unchanged); mechanism is wrong but infrastructure is reusable.

---

## Batch 2 Status (just completed)

| Task | Description | Status |
|------|-------------|--------|
| B2-1 | `CODECAPI_AVEncVideoForceKeyFrame` re-imported (was DD10-deleted) | **DONE** (commit `beda9ed`) |
| B2-2 | `force_keyframe_icodecapi_pending: AtomicBool` on `MftEncoderShared` | **DONE** (commit `beda9ed`) |
| B2-3 | pump_loop NeedInput path: consume `force_keyframe_icodecapi_pending` BEFORE `submit_frame` | **DONE** (commit `beda9ed`) |
| B2-4 | `request_keyframe_via_force_keyframe_icodecapi()` public method | **DONE** (commit `beda9ed`) |
| B2-5 | Compile + clippy clean | **DONE** — both green |
| B2-6 | Phase 0 probe P2-NVENC `phase0_nvenc_force_keyframe_via_codecapi_before_processinput` | **SCAFFOLDED** (commit `3bd5b95`) |
| B2-7 | Phase 0 probe P2-Intel `phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput` | **SCAFFOLDED** (commit `3bd5b95`) |
| B2-8 | Full workspace test suite (non-hw-encoder) | **DONE** — 611/611 passed, 19 skipped |

---

## TDD Cycle Evidence

Strict TDD mode: ACTIVE. Phase 0 probes are the valid TDD pattern:
each `#[ignore]`-gated probe drives the mechanism implementation.

| Task | Test | Layer | Safety Net | RED | GREEN | TRIANGULATE | REFACTOR |
|------|------|-------|------------|-----|-------|-------------|----------|
| B2-1..B2-4 (Candidate B mechanism) | `phase0_*_force_keyframe_via_codecapi_before_processinput` | Integration (HW, both hosts) | 611/611 pre-existing | Probe written (#[ignore]-gated) | PENDING (Host A + Host B runs) | N/A (probe-only) | Clippy clean |
| B2-6 (P2-NVENC probe) | Same | Integration (HW, Host B) | N/A (new) | Written | PENDING | N/A | Clean |
| B2-7 (P2-Intel probe) | Same | Integration (HW, Host A) | N/A (new) | Written | PENDING | N/A | Clean |

---

## Vendor Dispatch Table (current — unchanged from Batch 1)

| CLSID prefix | Vendor | Mid-stream IDR mechanism |
|---|---|---|
| `{60F44560-` | `NvidiaNvenc` | CleanPoint write on input IMFSample (Candidate A — FALSIFIED on P1, infrastructure retained) |
| `{4BE8D3C0-` | `IntelQsv` | Mechanism G (IMFTransform drop+recreate, Slice 5) |
| (other) | `Unknown` | Mechanism G fallback + WARN log |

**Note**: Dispatch will be updated once P2 results are known. Candidate B probes call
`request_keyframe_via_force_keyframe_icodecapi()` directly, bypassing dispatch.

---

## Files Changed

| File | Action | Description |
|------|--------|-------------|
| `crates/sm-infra/src/encode/windows_mft.rs` | Modified | Batch 1: `EncoderVendor`, GUID detection, `cleanpoint_pending`, `request_keyframe_via_cleanpoint()`, `submit_frame(force_cleanpoint)`, vendor dispatch. Batch 2: `CODECAPI_AVEncVideoForceKeyFrame` import, `force_keyframe_icodecapi_pending`, pump_loop ForceKeyFrame path (BEFORE ProcessInput), `request_keyframe_via_force_keyframe_icodecapi()` |
| `crates/sm-infra/tests/windows_mft_encode.rs` | Modified | Batch 1: P1 probe. Batch 2: P2-NVENC probe + P2-Intel QSV probe (both #[ignore]-gated) |
| `openspec/changes/hw-encoder-mft-nvenc-mid-stream-idr-mechanism/apply-progress.md` | Updated | This file |

---

## Commits

| SHA | Message |
|-----|---------|
| `6efd4c6` | `feat(infra): add EncoderVendor dispatch + cleanpoint_pending mechanism for NVENC mid-stream IDR (Slice 6 R2 candidate A)` |
| `aee5750` | `test(infra): C0 P1 — phase0_nvenc_cleanpoint_idr_via_input_sample_attribute (#[ignore]-gated)` |
| `beda9ed` | `feat(infra): add CODECAPI_AVEncVideoForceKeyFrame mechanism (Candidate B, before ProcessInput, VT_UI4)` |
| `3bd5b95` | `test(infra): C0 P2 — cross-vendor force_keyframe_via_codecapi_before_processinput probes (NVENC + Intel QSV)` |

---

## Phase 0 P2 Run Status

**PENDING — awaiting Host A (Intel QSV) and Host B (NVENC) traces.**

### Host B command (NVENC — P2-NVENC probe)

```powershell
git fetch origin
git pull
$env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
cargo nextest run --release --features hw-encoder -p sm-infra `
  --test windows_mft_encode phase0_nvenc_force_keyframe_via_codecapi_before_processinput `
  --run-ignored only --no-capture
```

### Host A command (Intel QSV — P2-Intel probe)

```powershell
git fetch origin
git pull
$env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
cargo nextest run --release --features hw-encoder -p sm-infra `
  --test windows_mft_encode phase0_intel_qsv_force_keyframe_via_codecapi_before_processinput `
  --run-ignored only --no-capture
```

---

## Decision Tree (P2 outcomes)

| P2-NVENC | P2-Intel QSV | Outcome |
|----------|--------------|---------|
| PASS | PASS | Candidate B is **vendor-uniform** — can replace Mechanism G and CleanPoint dispatch entirely. Move to sdd-propose with unified Candidate B architecture. |
| PASS | FAIL | Candidate B is **NVENC-only** — dispatch NVENC→B, Intel→G. Move to sdd-propose with vendor dispatch architecture (B for NVENC, G for Intel). |
| FAIL | PASS | Candidate B is **Intel-only** (unlikely). Dispatch Intel→B (replaces G?), NVENC→escalate. |
| FAIL | FAIL | Both Candidate A and Candidate B fail on NVENC. Escalate to Candidate C (GOP size toggle) or P3 (Hybrid G + Candidate A on post-recreate frame). |
