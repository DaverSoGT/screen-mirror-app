# sm-infra

Platform adapters for screen-mirror. Implements the domain ports defined in
`sm-domain` for each supported operating system.

## What this crate does

- `capture` — Windows Graphics Capture adapter (`WindowsCaptureSource`) backed by
  `windows-capture` v2. Delivers BGRA8 frames over a bounded `std::sync::mpsc`
  channel with drop-newest backpressure.

All platform-specific code is gated by `cfg(target_os = "...")` so that only the
relevant adapter compiles per target. Non-Windows targets see an empty `capture`
module and the crate compiles cleanly on all three CI platforms.

## Running unit tests

Non-ignored tests (pure logic, no live WGC session required):

```sh
cargo nextest run -p sm-infra
```

Expected output on Windows: 4+ tests pass, 6 skipped (the `#[ignore]` integration
tests). On non-Windows: 0 tests collected (the Windows-gated code is excluded by
`cfg`).

## Running Windows integration tests

Integration tests require an interactive desktop session with Windows Graphics
Capture support (Windows 10 1903+ or Windows 11). They are annotated `#[ignore]`
and guarded by a runtime `GraphicsCaptureApi::is_supported()` check so they exit
cleanly on headless hosts without failing.

Run them on a Windows machine with an active display:

```sh
cargo nextest run -p sm-infra --run-ignored only
```

### Which tests are `#[ignore]`

All tests in `crates/sm-infra/tests/windows_capture.rs` are ignored. Current list:

| Test name | What it verifies |
|-----------|------------------|
| `windows_capture_enumerate_monitors_returns_at_least_one` | Monitor enumeration returns >= 1 entry; exactly one primary. |
| `windows_capture_new_bad_index_returns_monitor_not_found` | Out-of-range index yields `CaptureError::MonitorNotFound`. |
| `windows_capture_new_primary_returns_ok` | `new()` with default config succeeds. |
| `windows_capture_delivers_at_least_3_frames` | `start` + recv >= 3 frames + `stop` end-to-end. |
| `windows_capture_stop_is_idempotent` | Calling `stop()` twice returns `Ok(())` both times. |
| `windows_capture_drops_frames_when_consumer_slow` | Slow consumer triggers `dropped_frames() > 0`. |

### Runtime guard pattern

Each ignored test begins with:

```rust
if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
    eprintln!("SKIP: Windows Graphics Capture not supported on this host");
    return;
}
```

This ensures the test exits cleanly on Windows Server Core, headless CI, or
Windows 10 < 1903 without a panic.

## Local Windows clippy

CI runs clippy on Ubuntu only (tracked as follow-up change `ci-windows-clippy`).
Before merging any PR that touches Windows-gated code, run manually:

```sh
cargo clippy -p sm-infra --target x86_64-pc-windows-msvc --all-targets -- -D warnings
```

## COM / WinRT note

`WindowsCaptureSource` does NOT call `RoInitialize`, `CoInitializeEx`, or any
variant. COM apartment initialization is fully delegated to `windows-capture`'s
`start_free_threaded()` path, which runs WGC on a dedicated OS thread with its own
apartment. This is safe to call from a Tauri 2 application without apartment
conflicts (R10 of the spec).
