# SC-CONV-2-12 — Hardware Gate: Scenario B
## Cross-Generation Stale-Bye Flapping Fix (transport-convergence-half2)

**Gate type**: Two-PC hardware gate (DEFINITIVE merge gate)
**Blocks**: merge of `feat/transport-convergence-half2` → `master`
**Issue**: GitHub #66

---

## Purpose

This gate verifies that after a bilateral network outage (router off → on):
- The transport converges to exactly ONE live stream.
- The FIRST re-established stream HOLDS with ZERO spontaneous drops post-establishment.
- No false-"connected" flapping (stream-2s → overlay → repeat) is observed.
- The user does NOT need to manually stop and restart.
- Manual stop remains a valid escape if the user chooses it.

This is the acceptance criterion that automated unit tests (SC-CONV-2-1 through SC-CONV-2-11)
cannot substitute for, because real MSE/WebRTC ICE timing governs the convergence window.

---

## Prerequisites

- Two PCs running the **same release build** from `feat/transport-convergence-half2`.
- Both PCs on the same LAN (Wi-Fi or Ethernet through the same router).
- Router/switch accessible for a deliberate power-off or port-block.
- Log capture ready (see Logging section below).

---

## Build Instructions

Run the release build on both PCs before testing:

```powershell
# On Windows (from repo root):
cargo tauri build
```

The installer / `.msi` or the raw `.exe` bundle is in:
```
target\release\bundle\msi\
target\release\bundle\nsis\
```

Alternatively, build the Tauri app in release mode using the existing `tauri build` script.

> If the release build is impractical in this batch (long build), copy the debug binary
> `target\debug\screen-mirror.exe` for functional testing. For a definitive gate, use release.

---

## Scenario B Test Procedure

### 1. Setup

1. Start `screen-mirror` on **PC-1 (sender)** and **PC-2 (receiver)**.
2. Establish a streaming session: PC-2 should show the stream from PC-1.
3. Confirm the stream is stable for at least 30 seconds with no drops.
4. Open log capture on both PCs (see Logging section).

### 2. Induce Bilateral Outage

Choose one of the following methods:

**Option A: Router power-off**
- Power off the router/switch.
- Wait 10–15 seconds (enough for ICE to fail on both sides).
- Power the router back on.

**Option B: Wi-Fi disable on both PCs simultaneously**
- Disable Wi-Fi on PC-1 and PC-2 at approximately the same time.
- Wait 10–15 seconds.
- Re-enable Wi-Fi on both PCs.

**Option C: Router port-block (managed switch)**
- Block the ports connected to PC-1 and PC-2 on the managed switch.
- Wait 10–15 seconds.
- Unblock the ports.

### 3. Observe Convergence

After restoring the network:
- Watch the receiver screen (PC-2).
- The overlay may show "reconnecting" or "connecting" briefly.
- **The FIRST re-established stream MUST hold without dropping again.**

### 4. Pass / Fail Criteria

| Criterion | Pass | Fail |
|-----------|------|------|
| Converges to ONE live stream | Yes | No stream established |
| First re-established stream holds | Zero spontaneous drops post-establishment | Any drop within 60s of re-establishment |
| No false-"connected" flapping | No | Flapping: stream→2s→overlay→stream→... |
| Manual restart NOT required | User does NOT need to stop+start | User MUST stop+start to recover |
| Manual stop works | User can stop cleanly if desired | Stop fails or hangs |

Record the verdict: **PASS** or **FAIL**.

### 5. On FAIL

- Capture the full logs (see Logging section).
- Note the exact failure mode:
  - Flapping pattern (how many oscillations? interval?)
  - Whether the stream eventually stabilized or required manual intervention
  - Any crash or hang
- Open a blocking defect referencing this gate before merge.

---

## Logging

### Receiver (PC-2 / stream.rs path)

Filter for the stale-Bye and drain lines:

```powershell
# Capture all stderr output to a log file while running:
.\screen-mirror.exe 2>&1 | Tee-Object -FilePath receiver.log

# After the test, filter for relevant lines:
Select-String -Path receiver.log -Pattern "\[sm-signaling-drain\]"
Select-String -Path receiver.log -Pattern "stale Bye|Closed|LocalFailure|PeerBye"
```

**Key log lines to look for:**

- `[sm-signaling-drain] stale Bye attempt=N floor=M; dropping, drain stays alive (REQ-BYE-4)`
  - This means a stale Bye was correctly dropped. EXPECTED during Scenario B.
- `[sm-signaling-drain] Closed → forwarding LocalFailure{PeerBye} to supervisor`
  - This means a genuine Bye was honored. EXPECTED when the stream re-closes.
- `[sm-signaling-drain] OfferReceived ignored (signaling-only, D-RDF-2)`
  - Normal during rebuild phase.

### Sender (PC-1 / sender.rs path)

Filter for suppress-Bye lines:

```powershell
.\screen-mirror.exe 2>&1 | Tee-Object -FilePath sender.log

# After the test:
Select-String -Path sender.log -Pattern "\[sm-signaling-frame-loop\] Bye SUPPRESSED"
Select-String -Path sender.log -Pattern "\[sm-sender-coord\]"
```

**Key log lines to look for:**

- `[sm-signaling-frame-loop] Bye SUPPRESSED (D3)`
  - The OLD generation's Bye was correctly suppressed at source (D-6). EXPECTED during rebuild.
- `[sm-sender-coord] InitiateMdnsReset` / `InitiateRebuild`
  - Normal reconnection flow.

---

## Launchers

### PC-1 (Sender) — `launch-sender.ps1`

```powershell
# SC-CONV-2-12 gate — sender launcher
# Run this script on PC-1.
# Logs are written to sender-gate.log in the current directory.

$exe = ".\screen-mirror.exe"
if (-not (Test-Path $exe)) {
    $exe = "..\..\target\release\screen-mirror.exe"
}
if (-not (Test-Path $exe)) {
    Write-Error "Cannot find screen-mirror.exe. Build first: cargo tauri build"
    exit 1
}

Write-Host "Starting screen-mirror (sender). Logs -> sender-gate.log"
& $exe 2>&1 | Tee-Object -FilePath "sender-gate.log"
```

### PC-2 (Receiver) — `launch-receiver.ps1`

```powershell
# SC-CONV-2-12 gate — receiver launcher
# Run this script on PC-2.
# Logs are written to receiver-gate.log in the current directory.

$exe = ".\screen-mirror.exe"
if (-not (Test-Path $exe)) {
    $exe = "..\..\target\release\screen-mirror.exe"
}
if (-not (Test-Path $exe)) {
    Write-Error "Cannot find screen-mirror.exe. Build first: cargo tauri build"
    exit 1
}

Write-Host "Starting screen-mirror (receiver). Logs -> receiver-gate.log"
& $exe 2>&1 | Tee-Object -FilePath "receiver-gate.log"
```

---

## Distribution

To share this gate kit with the second PC:

1. Copy `target\release\screen-mirror.exe` (or the installer bundle).
2. Copy this `docs\hw-gate\sc-conv-2-12\` directory.
3. Run `launch-sender.ps1` on PC-1 and `launch-receiver.ps1` on PC-2.

---

## Gate Verdict Recording

After running the gate, record the verdict here:

```
Date:        ____________________
Build SHA:   feat/transport-convergence-half2 @ ________________
Verdict:     PASS / FAIL
Failure mode (if FAIL): ________________________________
Logs:        sender-gate.log, receiver-gate.log (attach to the PR)
Tester:      ____________________
```

---

## References

- Spec: `sdd/transport-convergence-half2/spec` (engram #1058)
- Design: `sdd/transport-convergence-half2/design` (engram #1059)
- Tasks T-15, T-16: `sdd/transport-convergence-half2/tasks` (engram #1060)
- Related unit tests: SC-CONV-2-1 through SC-CONV-2-11 (automated, all in `cargo nextest run --workspace`)
- Issue: GitHub #66
