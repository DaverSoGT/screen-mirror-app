# SC-CONV-2-12 hardware gate — sender launcher (PC-1)
# Run this on the SENDER PC.
# Logs are written to sender-gate.log in the current directory.

$exe = ".\screen-mirror.exe"
if (-not (Test-Path $exe)) {
    $exe = "..\..\..\target\release\screen-mirror.exe"
}
if (-not (Test-Path $exe)) {
    Write-Error "Cannot find screen-mirror.exe. Build first with: cargo tauri build"
    exit 1
}

Write-Host "SC-CONV-2-12 gate: starting screen-mirror (sender). Logs -> sender-gate.log"
Write-Host "Key log lines to watch:"
Write-Host "  [sm-signaling-frame-loop] Bye SUPPRESSED (D3)  (D-6: OLD-gen Bye suppressed at source)"
Write-Host "  [sm-sender-coord] InitiateMdnsReset / InitiateRebuild  (reconnection flow)"
Write-Host ""
Write-Host "Press Ctrl+C to stop."

& $exe 2>&1 | Tee-Object -FilePath "sender-gate.log"
