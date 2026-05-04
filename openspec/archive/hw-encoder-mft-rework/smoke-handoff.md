# Smoke Handoff — hw-encoder-mft-rework

Per `BLOCKED_ON_SMOKE` rule (#186, #586): verify CANNOT issue `APPROVED_FOR_ARCHIVE` until the user supplies an HW smoke transcript on a real GPU host.

## Invocation

```powershell
# Kill any zombie test processes from prior runs
Get-Process | Where-Object { $_.Name -like "windows_mft_encode-*" } | Stop-Process -Force -ErrorAction SilentlyContinue

# Run the 18 HW smoke tests (16 existing + T-NEW-1 + T-NEW-2)
cargo nextest run -p sm-infra `
  --features hw-encoder `
  --test windows_mft_encode `
  --run-ignored=all `
  --test-threads=1
```

`--test-threads=1` avoids GPU contention. The `.config/nextest.toml` (committed in C1, slow-timeout removed in C2) governs per-test deadlines.

## Expected outcome on the typical Windows GPU host

- 9/18 PASS (T-NEW-1, T-NEW-2, and 7 existing tests that exercise paths not affected by Bug 1 Layer B)
- 5 ABORT with exit code `0xC0000005` (Bug 1 Layer B — vendor `ProcessOutput` driver crash; OUT-OF-SCOPE for this PR; see engram `sdd/hw-encoder-mft-rework/bug-1-deeper`)
- 3 timeout + 1 slow-fail (residual Bug 1 Layer B manifestations)

If your transcript matches this 9/18 split, the PR is HONEST about its residual. If it shows worse than 9/18 PASS or different failure modes, capture stdout + stderr and surface — the regression must be investigated before merge.

## Where to save the transcript

Save the full `cargo nextest` output (stdout + stderr) to engram with topic_key `sdd/hw-encoder-mft-rework/smoke-transcript`, type `discovery`, scope `project`, capture_prompt: false. The verify phase reads this to decide APPROVED vs BLOCKED_ON_SMOKE.

## What you must accept before merge

This PR DOES NOT fix Bug 1 Layer B (driver-level access violation in vendor `ProcessOutput`). Layer B is tracked as future change `hw-encoder-mft-vendor-priming-crash`. By merging this PR you accept that:

1. The HW path remains `default = []` (opt-in only)
2. 9/18 HW smoke tests will continue to fail until Layer B is addressed in a future change
3. Production behavior is unaffected — OpenH264 SW continues to be the production encoder

## Cross-references

- PR: https://github.com/DaverSoGT/screen-mirror-app/pull/16
- Spec requirement coverage: see `openspec/changes/hw-encoder-mft-rework/spec.md` (R1–R13, smoke-required flags per req)
- Design rationale: see `openspec/changes/hw-encoder-mft-rework/design.md` (DD1–DD8)
