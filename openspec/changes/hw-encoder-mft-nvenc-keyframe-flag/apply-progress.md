# Apply Progress: hw-encoder-mft-nvenc-keyframe-flag (Slice 6)

> Mode: Strict TDD (ACTIVE)
> Artifact store: hybrid (engram + openspec)
> Branch: `feat/hw-encoder-mft-nvenc-keyframe-flag`
> Branch tip: `ae36499`
> Batch: 2 (Phase B.b — post-recreate probe)
> Status: needs-input — awaiting Host B run of new probe

---

## Phase A — Branch + Scaffolding

- [x] **TA.1** DONE — Branch `feat/hw-encoder-mft-nvenc-keyframe-flag` cut from master `c48ae46`.
  - Verified: `git status` clean (untracked `openspec/changes/` is pre-existing SDD dir).
  - Branch tip at start of batch: `c48ae46`.

- [x] **TA.2** DONE — Working tree confirmed clean; branch is at correct baseline.
  - Note: `cargo nextest run --workspace` was NOT run (requires hardware + OS constraint);
    clean branch + clippy pass on `hw-encoder` feature confirmed instead.
  - `cargo clippy --tests --features hw-encoder -p sm-infra -- -D warnings` → exit 0.

---

## Phase B — C0 Micro-Probe (original — COMPLETE; result FALSIFIED hypothesis)

- [x] **TB.1** DONE — Probe `phase0_nvenc_idr_packet_format_dump` added to
  `crates/sm-infra/tests/windows_mft_encode.rs`.
  - Commit: `b048b36` (`test(infra): C0 — phase0_nvenc_idr_packet_format_dump (Host B trace, #[ignore]-gated)`)
  - `cargo check --tests --features hw-encoder -p sm-infra` → OK
  - `cargo clippy --tests --features hw-encoder -p sm-infra -- -D warnings` → exit 0
  - Probe logs: `raw_prefix[0..min(8,len)]` hex, `len`, `is_keyframe`, `has_3byte_annex_b`, `has_4byte_annex_b`

- [x] **TB.2** COMPLETE — Host B ran `phase0_nvenc_idr_packet_format_dump`.
  - Result: **FALSIFIED original hypothesis** (engram #800).
  - pkt 0: `len=4337 is_keyframe=TRUE raw_prefix=[00, 00, 00, 01, 09, 10, 00, 00] has_4byte_annex_b=true`
  - NVENC priming IDR emits 4-byte Annex-B (identical to Intel QSV). Priming detection works.
  - Bug must be in the POST-RECREATE path (Mechanism G), not the priming path.
  - Phases C–H (based on falsified 3-byte hypothesis) are SUSPENDED pending new hypothesis.

---

## Phase B.b — C0.b Post-Recreate Probe (Batch 2 — new investigation)

- [x] **TB.b.1** DONE — Probe `phase0_nvenc_post_recreate_idr_format_dump` added to
  `crates/sm-infra/tests/windows_mft_encode.rs`.
  - Commit: `ae36499` (`test(infra): C0.b — phase0_nvenc_post_recreate_idr_format_dump (re-investigation post-falsification)`)
  - `cargo check --tests --features hw-encoder -p sm-infra` → OK
  - `cargo clippy --tests --features hw-encoder -p sm-infra -- -D warnings` → exit 0
  - Probe exercises: 5 priming frames → flush+drain (logged) → `request_keyframe_via_recreate()` → 30 post-recreate frames → flush+drain (logged) → SUMMARY block
  - Logs every packet with: `raw_prefix`, `is_keyframe`, `len`, `has_3byte_annex_b`, `has_4byte_annex_b`
  - SUMMARY: `total_priming`, `total_post_recreate`, `first_post_recreate_idx`, `first_post_recreate_is_keyframe`, `first_post_recreate_raw_prefix`, `first_post_recreate_len`
  - No assertions — observation-only.

- [ ] **TB.b.2** BLOCKED — awaiting Host B run of new probe.
  - Status: needs-input

---

## TDD Cycle Evidence

| Task | RED | GREEN | REFACTOR | Status |
|------|-----|-------|----------|--------|
| TB.1 (C0 probe scaffold) | N/A — probe only | N/A | N/A | DONE |
| TB.2 (Host B run — original probe) | — | — | — | COMPLETE (falsified hypothesis) |
| TB.b.1 (C0.b probe scaffold) | N/A — probe only | N/A | N/A | DONE |
| TB.b.2 (Host B run — post-recreate probe) | — | — | — | BLOCKED awaiting user |
| TC.1 (C1 RED helper) | not started | — | — | SUSPENDED (pending new hypothesis from TB.b.2) |
| TC.2 (C1 RED tests) | not started | — | — | SUSPENDED |
| TD.1 (C2 GREEN fix) | — | not started | — | SUSPENDED |

---

## Host B Command (Batch 2 — new probe)

Run on Host B (NVIDIA NVENC GPU required):

```powershell
$env:RUST_LOG="sm_infra::encode=trace,windows_mft_encode=trace"
cargo nextest run --release --features hw-encoder -p sm-infra `
  --test windows_mft_encode phase0_nvenc_post_recreate_idr_format_dump `
  --run-ignored only --no-capture
```

### What to look for in the output

Look at the SUMMARY line near the end of the output:

```
[NVENC-P0b] SUMMARY total_priming=N total_post_recreate=M
  first_post_recreate_idx=Some(0)
  first_post_recreate_is_keyframe=Some(...)
  first_post_recreate_raw_prefix=Some([...])
  first_post_recreate_len=Some(...)
```

Decision gate:
- `first_post_recreate_is_keyframe=Some(true)` → post-recreate detection works; bug is
  elsewhere (timing? assertion logic in T7.1?). Report full SUMMARY to orchestrator.
- `first_post_recreate_is_keyframe=Some(false)` → bug confirmed in post-recreate path.
  Compare `first_post_recreate_raw_prefix` vs priming pkt 0 prefix to form new hypothesis.
- `OUTCOME=ENCODER_DIED` or `OUTCOME=EMPTY_DRAIN` → Mechanism G fails on NVENC
  (ActivateObject rejected? second activation not supported). Report to orchestrator.

---

## Commits on Branch

| SHA | Label | Message |
|-----|-------|---------|
| `b048b36` | C0 PROBE | `test(infra): C0 — phase0_nvenc_idr_packet_format_dump (Host B trace, #[ignore]-gated)` |
| `ae36499` | C0.b PROBE | `test(infra): C0.b — phase0_nvenc_post_recreate_idr_format_dump (re-investigation post-falsification)` |

## Files Changed

| File | Action |
|------|--------|
| `crates/sm-infra/tests/windows_mft_encode.rs` | Modified — added C0 probe (batch 1) + C0.b probe (batch 2; 303 lines inserted) |
| `openspec/changes/hw-encoder-mft-nvenc-keyframe-flag/apply-progress.md` | Updated — this file |
| `openspec/changes/hw-encoder-mft-nvenc-keyframe-flag/tasks.md` | Updated (batch 1) |

## Remaining Tasks (SUSPENDED — pending new hypothesis from TB.b.2 trace)

- [ ] TC.1 — C1 RED: extract start-code detection helper (form after TB.b.2 evidence)
- [ ] TC.2 — C1 RED: unit tests against new hypothesis
- [ ] TD.1 — C2 GREEN: implement fix per new hypothesis
- [ ] TD.2 — Comments per design doc
- [ ] TD.3 — Host A full suite + round-3 probe
- [ ] TE.1 — clippy --all-targets --all-features --locked -D warnings
- [ ] TE.2 — final nextest + sm-domain diff
- [ ] TF.1 — Host B full suite (T7.1 + T7.2 + probes)
- [ ] TG.1–3 — PR open, CI, merge
- [ ] TH.1–2 — archive + sdd-init v15
