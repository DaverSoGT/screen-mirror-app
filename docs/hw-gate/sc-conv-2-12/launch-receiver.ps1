# SC-CONV-2-12 hardware gate — receiver launcher (PC-2)
# Run this on the RECEIVER PC.
# Logs are written to receiver-gate.log in the current directory.

$exe = ".\screen-mirror.exe"
if (-not (Test-Path $exe)) {
    $exe = "..\..\..\target\release\screen-mirror.exe"
}
if (-not (Test-Path $exe)) {
    Write-Error "Cannot find screen-mirror.exe. Build first with: cargo tauri build"
    exit 1
}

Write-Host "SC-CONV-2-12 gate: starting screen-mirror (receiver). Logs -> receiver-gate.log"
Write-Host "Key log lines to watch:"
Write-Host "  [sm-signaling-drain] stale Bye attempt=N floor=M; dropping  (D-4: stale Bye filtered)"
Write-Host "  [sm-signaling-drain] Closed -> forwarding LocalFailure{PeerBye}  (genuine Bye honored)"
Write-Host ""
Write-Host "Press Ctrl+C to stop."

& $exe 2>&1 | Tee-Object -FilePath "receiver-gate.log"
