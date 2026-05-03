# sm-infra

Platform adapters for screen-mirror. Implements the domain ports defined in
`sm-domain` for each supported operating system.

## What this crate does

- `capture` — Windows Graphics Capture adapter (`WindowsCaptureSource`) backed by
  `windows-capture` v2. Delivers BGRA8 frames over a bounded `std::sync::mpsc`
  channel with drop-newest backpressure.

- `encode` — Windows OpenH264 software encoder (`WindowsOpenH264Encoder`) and the
  bounded packet channel constant (`ENCODE_CHANNEL_CAPACITY`). Accepts
  `CaptureFrame`s on its input channel, performs stride-aware BGRA→I420 conversion
  internally using BT.601 limited-range coefficients, and emits Annex-B H.264
  packets via OpenH264 (Cisco, BSD-2). On non-Windows targets this module compiles
  to an empty stub.

All platform-specific code is gated by `cfg(target_os = "...")` so that only the
relevant adapter compiles per target. Non-Windows targets see empty `capture` and
`encode` modules and the crate compiles cleanly on all three CI platforms.

## Running unit tests

Non-ignored tests (pure logic, no live capture or encoder session required):

```sh
cargo nextest run -p sm-infra
```

Expected output on Windows: 20+ tests pass, 9 skipped (the `#[ignore]` integration
tests). On non-Windows: only the platform-agnostic `bgra_to_i420` tests run (the
Windows-gated code is excluded by `cfg`).

## Running Windows integration tests

Integration tests require an interactive desktop session and are annotated
`#[ignore]`. Run all ignored tests on a Windows machine:

```sh
cargo nextest run -p sm-infra --run-ignored only
```

To run only the encoder integration tests:

```sh
cargo nextest run -p sm-infra --run-ignored only --tests windows_encode
```

To run only the capture integration tests:

```sh
cargo nextest run -p sm-infra --run-ignored only --tests windows_capture
```

### Encoder integration tests (`crates/sm-infra/tests/windows_encode.rs`)

These tests run the full encoder stack (BGRA→I420 + OpenH264 SW encode). They
require a Windows host but no live capture session — all frames are synthetic.

NASM in PATH gives OpenH264 a 2–3× SIMD speedup on the encode hot loop. NASM is
OPTIONAL — without it, OpenH264 falls back to portable C.

| Test name | What it verifies |
|-----------|------------------|
| `synthetic_bgra_30_frames_yields_idr_and_p_frames` | 30 synthetic 1920×1080 frames → ≥1 IDR + ≥10 P-frames within 3 s; Annex-B start code at offset 0. |
| `request_keyframe_midstream_produces_idr_on_next_packet` | `request_keyframe()` forces IDR on the next encoded packet. |
| `slow_consumer_increments_dropped_frames` | Deliberate slow consumer with capacity-2 channel → `dropped_frames() > 0`. |

### Composability smoke example (`crates/sm-infra/examples/encode_smoke.rs`)

End-to-end smoke that wires `WindowsCaptureSource` → `WindowsOpenH264Encoder` and
writes an Annex-B `.h264` file. Captures the primary monitor for ~10 s, calls
`request_keyframe()` at +5 s, and prints a summary on exit (frame counts,
dropped-frame counters on both stages, output file size).

```sh
cargo run -p sm-infra --example encode_smoke
cargo run -p sm-infra --example encode_smoke -- my_capture.h264
```

Verify the output with `ffplay -i encode_smoke.h264` or `ffprobe -i encode_smoke.h264`.

### Capture integration tests (`crates/sm-infra/tests/windows_capture.rs`)

These tests require an interactive desktop session with Windows Graphics Capture
support (Windows 10 1903+ or Windows 11). They are guarded by a runtime
`GraphicsCaptureApi::is_supported()` check so they exit cleanly on headless hosts.

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

### Hardware encoder smoke tests (`crates/sm-infra/src/encode/windows_mft.rs`)

The `WindowsMftH264Encoder` uses Windows Media Foundation Transform (MFT) with
`MFTEnumEx(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)`. Hardware
tests are annotated `#[ignore]` and run on a Windows host with a dedicated GPU.

**Preconditions:**
- Windows 10 1709 (Fall Creators Update) or Windows 11
- A GPU with a hardware H.264 encoder (Intel Quick Sync, NVIDIA NVENC, AMD AMF,
  or compatible)
- Up-to-date GPU driver

**Run hardware encoder tests manually:**

```sh
cargo nextest run -p sm-infra --features hw-encoder --run-ignored only
```

**Force software encoder (bypass HW enumeration):**

Set the environment variable `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to skip
`MFTEnumEx` and always use the OpenH264 software encoder. Useful for debugging,
CPU benchmarks, or machines without a compatible GPU:

```sh
$env:SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER = "1"
cargo nextest run -p sm-infra
```

**Factory fallback behaviour:** if no hardware MFT encoder is found (`InitFailed`
returned by `WindowsMftH264Encoder::new`), `build_video_encoder` automatically
falls back to `WindowsOpenH264Encoder`. A one-time `tracing::info!` log records
the reason.

## Transport & Signaling

The `transport` module (`Str0mVideoSender`, `Str0mVideoReceiver`) and the
`signaling` module (`MdnsSignaling`, `LoopbackSignaling`) deliver the WebRTC
media link landed by the `transport-webrtc-str0m` change. The transport stack
is cross-platform (str0m + RustCrypto + mdns-sd, no OS-specific gates).

### Running transport tests

Non-ignored unit + integration tests run as part of the standard workspace
suite:

```sh
cargo nextest run -p sm-infra
```

This covers the loopback signaling fixture, Annex-B helpers, str0m sender /
receiver lifecycle, and the `transport_loopback` integration tests that do NOT
require live DTLS (e.g. ICE connectivity, lifecycle, dropped-frame observability).

To run only transport-related integration tests:

```sh
cargo nextest run -p sm-infra --tests transport_loopback
```

### Running ignored transport / signaling tests

Three transport / signaling tests are `#[ignore]` because they need either mDNS
multicast or a complete DTLS handshake to pass deterministically. Run them all
on a Windows or Linux host with multicast support:

```sh
cargo nextest run -p sm-infra --run-ignored only
```

| Test | File | Why `#[ignore]` |
|------|------|----------------|
| `mdns_signaling_pair_round_trip` | `src/signaling/mdns.rs` | Requires mDNS multicast on the loopback / LAN interface; CI runners frequently disable multicast. |
| `transport_loopback_media_flow_end_to_end` | `tests/transport_loopback.rs` | Requires str0m's DTLS handshake to complete; verifies `Str0mVideoSender` → `Str0mVideoReceiver` actually delivers `EncodedPacket`s over loopback UDP. |
| `transport_loopback_rtcp_pli_reaches_encoder` | `tests/transport_loopback.rs` | Same DTLS prerequisite + receiver-side `request_keyframe()` must reach the sender's encoder. |

### Composability smoke example (`crates/sm-infra/examples/transport_smoke.rs`)

End-to-end smoke that wires `Str0mVideoSender` ↔ loopback UDP ↔
`Str0mVideoReceiver` with `LoopbackSignaling` for SDP / ICE exchange. Pumps
synthetic Annex-B IDR frames at ~30 fps for 5 s and asserts that at least one
keyframe is received on the far side.

```sh
cargo run -p sm-infra --example transport_smoke
```

Expected output: ICE connects within ~1 s, ~150 keyframes received over
loopback UDP, no panics on shutdown.

### mDNS LAN smoke (deferred — requires hardware)

Task 7.6 from the original change calls for a LAN smoke between two physical
machines (one publisher, one subscriber) over real mDNS. This is NOT
automatable in CI and is documented here for manual verification:

1. Build on machine A: `cargo build -p sm-infra --example transport_smoke`.
2. Build on machine B: same command.
3. Adapt `examples/transport_smoke.rs` to use `MdnsSignaling` instead of
   `LoopbackSignaling` and split the example into "publisher" and "subscriber"
   roles.
4. Run the publisher on machine A and the subscriber on machine B (same VLAN,
   multicast permitted).
5. Verify the subscriber sees `TransportEvent::Connected` and at least one
   `EncodedPacket { is_keyframe: true, .. }` within 10 s.

This validates the real-world mDNS discovery path that the unit / integration
tests stub out for determinism.

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
