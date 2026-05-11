# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **API and behavior are subject to change without notice until v1.0.0.**
> Releases in the `0.x` series are pre-release / beta. Minor bumps (`0.1.0` →
> `0.2.0`) MAY contain breaking changes. Patch bumps (`0.1.0` → `0.1.1`) are
> bug-fix only.

## [Unreleased]

## [0.2.0] - 2026-05-10

### Changed

- **`WindowsMftH264Encoder` is now the default encoder on Windows.**
  The `hw-encoder` Cargo feature is enabled by default in `sm-infra`
  (`crates/sm-infra/Cargo.toml`). Bucket A bugs (async event priming,
  `GetEvent` stop-signal starvation, `pump_loop` deadlocks) were resolved
  in Slice 6 R2 (PR #22, archive engram #816). ForceKeyFrame via ICodecAPI
  `VT_UI4` BEFORE `ProcessInput` is vendor-uniform on Intel Quick Sync and
  NVIDIA NVENC.
- **Version bump `0.1.0` → `0.2.0`** (`src-tauri/Cargo.toml`). Adding a
  default feature qualifies as a minor bump per the project's changelog
  policy (patch = bug-fix only).

### Documentation

- `crates/sm-infra/Cargo.toml`: stale Bucket A comment replaced with
  current-state comment citing Slice 6 R2 closure and the env-var kill-switch.
- `crates/sm-infra/README.md`: hardware encoder section updated to reflect
  default-on status; `--features hw-encoder` removed from normal build and
  test commands; kill-switch documented as the Tier 1 rollback path.

### Compatibility

- **Windows hosts with a compatible GPU** (Intel Quick Sync, NVIDIA NVENC,
  AMD AMF): `WindowsMftH264Encoder` is selected automatically. No user action
  required.
- **Windows hosts without a compatible GPU**: `build_video_encoder` detects
  `InitFailed` and promotes `WindowsOpenH264Encoder` automatically. No user
  action required.
- **macOS and Linux**: `hw-encoder` is gated by
  `cfg(all(target_os = "windows", feature = "hw-encoder"))` — zero impact on
  non-Windows platforms.
- **Runtime rollback**: set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` before
  launching to force the software path without rebuilding (Tier 1 rollback,
  DD7).

## [0.1.0] - 2026-05-02

First public beta release. Windows-to-Windows screen-mirroring demo end-to-end:
capture → H.264 encode → WebRTC transport → fMP4 mux → MSE playback in a
Tauri shell.

### Added
- **Screen capture** (`sm-infra::capture`): `WindowsCaptureSource` adapter on
  top of `windows-capture 2.x`. Event-driven Windows Graphics Capture; emits
  `CaptureFrame` BGRA8 buffers via a `SyncSender` channel with drop-newest
  backpressure.
- **H.264 encoder** (`sm-infra::encode`): `WindowsOpenH264Encoder` adapter
  (OpenH264 0.9, software-only). Annex-B NAL output with in-band SPS/PPS,
  caller-forced IDR via `request_keyframe()`, runtime `set_bitrate()`,
  `dropped_frames()` counter. BGRA→I420 stride-aware conversion private to the
  adapter.
- **WebRTC transport** (`sm-infra::transport`): `str0m` 0.18 sender + receiver
  with rust-crypto backend. ICE-resilient to ICMP transient errors.
- **mDNS signaling** (`sm-infra::signaling`): auto-discovery on single-NIC LAN
  via `mdns-sd`; TCP control channel + ICE candidate trickle.
- **fMP4 muxer** (`sm-infra::render`): `Mp4Muxer` produces fragmented MP4 from
  Annex-B NAL units. RTP-timestamp-derived per-sample `trun` durations
  (`FpsTracker`, 8-delta median + IQR variance guard); init segment with
  parsed `SpsInfo` flowing in from a single SPS parse.
- **Domain ports** (`sm-domain`): `CaptureSource`, `VideoEncoder`,
  `VideoSender`, `VideoReceiver`, plus `session`/`signaling`/`supervisor` /
  `transport` modules. Hexagonal invariant enforced by
  `tests/no_platform_deps.rs`: `windows`, `tokio`, `openh264`, `str0m` all
  banned from the domain.
- **Tauri 2 shell** (`src-tauri`): `start_sender` / `stop_sender` /
  `sender_diagnostics` commands with `SenderBridge` state container,
  `SenderBuilderFn` test injection seam, and reconnect supervisor with
  configurable `ReconnectPolicy` (auto-reconnect on `IceFailed`).
- **Multi-page frontend** (`dist/`): `index.html` (host),
  `sender.html` + `sender.js` (sender UI), `viewer.html` + `mse-client.js`
  (MSE playback). Channel-based binary transport for fMP4 segments.
- **Quality gates**: `cargo check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, `cargo fmt --check --all`, `cargo nextest run --workspace`,
  `cargo deny check`.
- **JS test harness**: `vitest` + `happy-dom` covering `dist/sender.js` and
  `dist/mse-client.js` regression class (B11-S7..S12).

### Known Limitations

These are accepted constraints in v0.1.0. They are documented here so users can
work around them; addressing each is on the v2 roadmap.

1. **Single-NIC LAN only**: `enumerate_local_ipv4()` returns the first
   non-loopback IPv4. Hosts with multiple active NICs (e.g., VPN attached) may
   advertise the wrong one.
2. **Capture-rate variability**: Windows Graphics Capture is event-driven —
   it emits frames only when on-screen content changes. Static desktops
   capture at 1–5 fps; dynamic content at up to 30 fps. Visible as "stutter"
   on stationary screens. By design, not a defect.
3. **Single host candidate per peer**: `candidate_addr()` returns
   `Option<SocketAddr>`. Multi-NIC trickle is deferred.
4. **TCP signaling collision on same host**: `MdnsSignaling` hardcodes TCP
   control port `7889`. Running sender and receiver on the same Windows host
   collides on this port. Cross-host setups work because UDP is ephemeral.

### Installer

The Windows installer (`Screen Mirror_0.1.0_x64_en-US.msi` and the matching
NSIS `.exe`) for this release is **unsigned**. Windows SmartScreen will display
a "Windows protected your PC" warning on first launch — click **More info →
Run anyway** to proceed. Code signing is planned for a future release.

### Released artifacts

Built locally via `cargo tauri build` on Windows. SHA-256:

```
3C44FFD017CFA38683122EB00AF0E927F13C6C7F3A1E026E8FF59CB1C0853798  Screen Mirror_0.1.0_x64_en-US.msi
AD98689078B263FA181859014D7CD1BFE622896C920295EEECF6E06476E95B2B  Screen Mirror_0.1.0_x64-setup.exe
```

Both files are attached to the [v0.1.0 GitHub release](https://github.com/DaverSoGT/screen-mirror-app/releases/tag/v0.1.0).

[Unreleased]: https://github.com/DaverSoGT/screen-mirror-app/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/DaverSoGT/screen-mirror-app/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/DaverSoGT/screen-mirror-app/releases/tag/v0.1.0
