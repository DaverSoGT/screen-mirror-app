# Design: hw-encoder-mft-rework

> Phase: SDD design. Inputs: proposal #595 (9 LOCKED decisions, 5 OQs, R1–R5), explore #594 (Pattern A/B/C + Fix A/B), apply-progress #590 (current pump_loop shape after PR #15), prior precedent design #587 + design-tail #588 (DD format), project context #186 (Strict TDD, smoke conventions).
> Artifact store: hybrid (engram `sdd/hw-encoder-mft-rework/design` + this file). Strict TDD: ACTIVE. Mode: interactive. Delivery: single PR (Decision #3).
> This document captures **mechanics**. The 9 locked decisions in proposal #595 §6 are NOT re-debated.

---

## 1. Inputs Read

### Engram observations
- #595 (sdd/hw-encoder-mft-rework/proposal) — 9 LOCKED decisions, 5 OQs (OQ-1..OQ-5), R1–R5 risks, smoke plan.
- #594 (sdd/hw-encoder-mft-rework/explore) — Pattern B pseudocode at §2, line refs `windows_mft.rs:737-845` for current pump_loop, smoke-test forecast.
- #590 (sdd/hardware-accel-encoder-smoke-fixes/apply-progress) — confirms PR #15 ships effective_dimensions(), Drop fix, MF_TRANSFORM_ASYNC_UNLOCK ordering, per-packet Annex-B sniff, drain smoke test.
- #587 + #588 (sdd/hardware-accel-encoder-smoke-fixes/design + design-tail) — DD format precedent, 50KB split convention.
- #186 (sdd-init/screen-mirror-app) — Strict TDD runner `cargo nextest run --workspace`, BLOCKED_ON_SMOKE rule, `mft_<scenario>_<expectation>` naming.

### Files read end-to-end (current state on master `f01f27f`)
- `crates/sm-infra/src/encode/windows_mft.rs` (1141 lines) — pump_loop at lines 712–847, current event arms NeedInput/HaveOutput/MEEndOfStream + catch-all warn at line 840.
- `crates/sm-infra/tests/windows_mft_encode.rs` (745 lines) — 16 `#[ignore]` smoke tests, `init_tracing()` helper at line 36, `make_synthetic_frame` at line 51.
- `crates/sm-infra/src/encode/factory.rs` (200 lines) — HW-first/SW-fallback contract with `force_sw` env var.
- `crates/sm-domain/src/encode.rs` (lines 1–80) — `VideoEncoder` port frozen, `EncoderConfig` already has `width/height`.
- `crates/sm-infra/Cargo.toml` — `default = []`, `hw-encoder = []` opt-in.

---

## 2. Architecture Overview

### 2.1 Threading model (UNCHANGED)

```
┌─────────────────────┐                           ┌─────────────────────┐
│  Caller thread      │                           │  Encoder thread     │
│  (test / sender)    │                           │  (spawned in start) │
│                     │  Arc<MftEncoderShared>    │                     │
│  enc.start()────────┼──────atomics──────────────┼──── pump_loop()     │
│  enc.stop()         │   stop, keyframe_pending, │     ↑               │
│   └── join handle   │   pending_bitrate, dropped│     │ NO_WAIT poll  │
│  Drop {             │                           │     ↓               │
│   release COM,      │  mpsc::Receiver<Frame>    │   IMFTransform      │
│   MFShutdown,       │──────────────────────────►│   IMFEventGenerator │
│   CoUninit          │                           │                     │
│  }                  │  mpsc::SyncSender<Packet> │                     │
│                     │◄──────────────────────────│                     │
└─────────────────────┘                           └─────────────────────┘
```

NO new threads. NO new sync primitives. Single OS thread per encoder, owned by the spawned closure. (Pattern C explicitly out per explore §2.)

### 2.2 New pump_loop control flow (Pattern B + Fix B unified)

```
loop {
  ── stop check ─────────────────────────────────────────────
  if state.stop.load(Acquire) { break; }            // ≤ 1 ms latency vs sleep tick

  ── poll one event NO_WAIT ─────────────────────────────────
  match event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
    Ok(event) =>
      match event.GetType()? {
        METransformNeedInput     => ni_count  += 1,
        METransformHaveOutput    => ho_count  += 1,
        METransformDrainComplete => { reset_after_drain(&mut ni_count, &mut ho_count); /* signal EOS path */ }
        MEEndOfStream            => break,
        other                    => tracing::warn!("unhandled event_type=0x{:08X}", other),
      },
    Err(MF_E_NO_EVENTS_AVAILABLE) => { /* nothing this tick */ }
    Err(MF_E_SHUTDOWN | E_ABORT)  => break,
    Err(other)                    => { tracing::error!(...); break; }
  }

  ── service order: DRAIN OUTPUT FIRST, then INPUT ──────────
  while ho_count > 0 {
    collect_output(...)?;              // success → ho_count -= 1; failure (E_UNEXPECTED) → warn + ho_count -= 1
    ho_count -= 1;
  }
  while ni_count > 0 {
    match service_one_need_input(...)  // returns Continue, BreakLoop, or RetryLater
      Continue   => ni_count -= 1,
      BreakLoop  => break_outer,
      RetryLater => break_inner,       // re-poll events; do NOT consume the credit
  }

  ── idle sleep tick ────────────────────────────────────────
  if no_event_received && ho_count == 0 && ni_count == 0 {
    std::thread::sleep(POLLING_SLEEP);  // 1 ms — bounds stop-detection latency
  }
}
```

Key invariants:
1. **Drain-first ordering**: HaveOutput services BEFORE NeedInput on every iteration (Decision #1 + explore §2 Pattern B). This is what unblocks vendor priming (Bug 1).
2. **NO_WAIT polling**: GetEvent never blocks. Stop signal is honored within `POLLING_SLEEP` (1 ms).
3. **One event per tick**: The loop reads at most ONE event per iteration before servicing accumulated counters. This avoids starving counter-service while events stream in.
4. **Counters never underflow**: `ho_count -= 1` only after either successful `collect_output` OR explicit error consumption (vendor-priming `E_UNEXPECTED` consumes the spurious credit; see §6).

---

## 3. State Machine

### 3.1 Counter state (pump-loop local stack)

```rust
let mut ni_count: u32 = 0;  // count of unserviced METransformNeedInput events
let mut ho_count: u32 = 0;  // count of unserviced METransformHaveOutput events
```

**Lifetime**: declared inside `pump_loop`, lives on the pump thread's stack, dropped at function return. NOT shared between threads. NO atomics needed.

**Increment points**:
- `ni_count += 1`: ONLY on `Ok(event)` whose type is `METransformNeedInput.0`.
- `ho_count += 1`: ONLY on `Ok(event)` whose type is `METransformHaveOutput.0`.

**Decrement points** (per OQ-1 resolution — see DD2):
- `ni_count -= 1`: AFTER `service_one_need_input` returns `Continue`. On `RetryLater` (channel timeout), counter stays put — the next iteration retries.
- `ho_count -= 1`: AFTER `collect_output` returns OR after a logged `E_UNEXPECTED` (vendor priming consumed the credit).

**Invariants** (verified at debug-assert boundaries):
- `ni_count == 0` immediately after the `while ni_count > 0` loop body completes a full pass without `RetryLater`.
- `ho_count == 0` immediately after the `while ho_count > 0` loop body.
- No NEW credits are added while servicing (servicing reads no events — events arrive only at the top of the next outer iteration).

### 3.2 Stop-signal flow

- `state.stop: Arc<AtomicBool>` — already exists, set by `WindowsMftH264Encoder::stop()` (line 220) and from `Drop::stop()` chain (line 249).
- Check cadence: top of every `loop { ... }` iteration (BEFORE the GetEvent poll).
- Latency bound: `POLLING_SLEEP` (1 ms) + service time of one full counter pass. For idle case (no events, no work), latency ≈ 1 ms. For active case, latency ≈ 1 ms + worst-case per-event service (collect_output or submit_frame, both fast: <5 ms).
- `stop_check_interval` is implicit — no separate timer. Every iteration checks once.

### 3.3 DrainComplete handling (Decision #4 + OQ-4 resolution → DD3)

When the pump receives `METransformDrainComplete`:

```rust
METransformDrainComplete => {
    // After COMMAND_DRAIN, the MFT signals "no more output coming, no more input wanted".
    // Reset both counters: vendor MFTs MAY have emitted spurious NeedInput credits
    // pre-drain that we MUST NOT honor post-drain (would return MF_E_NOTACCEPTING).
    // Spurious HaveOutput credits would return E_UNEXPECTED (no input to produce output from).
    if ni_count > 0 || ho_count > 0 {
        tracing::debug!(
            "DrainComplete received with non-zero counters (ni={}, ho={}) — resetting",
            ni_count, ho_count
        );
    }
    ni_count = 0;
    ho_count = 0;
    // Continue the loop — let the top-of-iteration stop check be the single break point.
    // Do NOT break here: in normal stop, state.stop is already true; in disconnected-drain,
    // the next iteration sees stop=true (set by stop()) OR the channel drain logic completes
    // independently. DrainComplete is "MFT is quiesced", not "client wants exit".
}
```

This resolves **OQ-4 explicitly**: option (a) — reset counters and continue. Top-of-iteration stop check is the single break point.

### 3.4 Edge cases

| Event source | Edge case | Behavior |
|--------------|-----------|----------|
| `GetEvent` | `MF_E_NO_EVENTS_AVAILABLE` (HRESULT `0x80040204`) | Expected branch. No log. Sleep tick if also no counter work. |
| `GetEvent` | `MF_E_SHUTDOWN` (HRESULT `0xC00D3E85`) | Graceful exit. `tracing::info!("event generator shut down")` + break. |
| `GetEvent` | `E_ABORT` (HRESULT `0x80004004`) | Graceful exit (already handled at line 747 today). `tracing::info!("GetEvent E_ABORT")` + break. |
| `GetEvent` | other HRESULT | `tracing::error!` + break. |
| `ProcessOutput` | `E_UNEXPECTED` | WARN + consume the `ho_count` credit + continue (vendor-priming spurious HaveOutput; do NOT exit). |
| `ProcessOutput` | `MF_E_TRANSFORM_NEED_MORE_INPUT` | Already handled (line 881) — return `Ok(None)`; consume credit. |
| `ProcessOutput` | `MF_E_TRANSFORM_STREAM_CHANGE` | Already handled (line 882) — return `Ok(None)`; consume credit; continue. |
| `ProcessInput` | `MF_E_NOTACCEPTING` (HRESULT `0xC00D36B5`) | Counter logic is wrong if reached. `debug_assert!(false, ...)` + `tracing::error!` in release + break. |
| Channel `frame_rx` | `RecvTimeoutError::Timeout` | NeedInput credit NOT consumed; break inner `while`, re-poll events on next outer iteration. |
| Channel `frame_rx` | `RecvTimeoutError::Disconnected` | Send `COMMAND_DRAIN`. NeedInput credit NOT consumed. Subsequent iterations service drained HaveOutput, eventually receive `METransformDrainComplete` (or `MEEndOfStream` per existing path). |
| Top-of-loop | `state.stop == true` | Break immediately. |

---

## 4. Code-level Changes (file-by-file diff sketch)

### 4.1 `crates/sm-infra/src/encode/windows_mft.rs`

#### 4.1.1 New imports (top of file)

```rust
use windows::Win32::Media::MediaFoundation::{
    // ... existing imports ...
    MEEndOfStream, METransformDrainComplete /* NEW */, METransformHaveOutput, METransformNeedInput,
    MF_E_NOTACCEPTING /* NEW */, MF_E_NO_EVENTS_AVAILABLE /* NEW */, MF_E_SHUTDOWN /* NEW */,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_EVENT_FLAG_NO_WAIT /* NEW; remove MF_EVENT_FLAG_NONE if no other uses */,
    // ...
};
```

**Apply-time check**: verify each NEW import resolves under `windows = "0.62.2"` features already enabled in `Cargo.toml` (`Win32_Media_MediaFoundation`). All five are part of the MediaFoundation namespace; should be present. If any missing, that is a Cargo feature gap to fix in Phase 0 anchor — NOT a design redesign.

#### 4.1.2 New constant (module scope, near `H264_PROFILE_MAIN`)

```rust
/// Polling sleep duration when no event is available and no counter work is pending.
/// Bounds stop-signal detection latency to ≤ 1 ms (Decision #5 in proposal #595).
/// At 30 fps (33 ms frame budget) this adds ≤ 3% latency floor and < 0.1% CPU at idle.
const POLLING_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);
```

#### 4.1.3 `pump_loop` — full body redesign (lines 712–847)

Signature UNCHANGED:

```rust
fn pump_loop(
    mft: &IMFTransform,
    codec_api: &ICodecAPI,
    event_gen: &IMFMediaEventGenerator,
    state: &MftEncoderShared,
    rx: Receiver<sm_domain::CaptureFrame>,
    tx: SyncSender<EncodedPacket>,
    output_format_known: &mut Option<bool>,
    config: &EncoderConfig,
)
```

Body (pseudocode-level — apply phase writes the full code):

```rust
fn pump_loop(...) {
    use crate::encode::bgra_to_nv12::{Nv12, convert as nv12_convert};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let mut nv12_scratch = Nv12::new(1, 1);
    let mut seq: u64 = 0;
    let mut current_ts = Duration::ZERO;
    let frame_dur_100ns = if config.framerate > 0 {
        10_000_000i64 / config.framerate as i64
    } else { 333_333 };

    // Counter-based dual-arm state (Pattern B from explore #594 §2).
    let mut ni_count: u32 = 0;
    let mut ho_count: u32 = 0;
    // Heartbeat/log throttle state (resolves OQ-2 — see DD8).
    let mut last_logged_ni: u32 = u32::MAX;
    let mut last_logged_ho: u32 = u32::MAX;

    loop {
        // ── Stop check ──────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            tracing::debug!("pump_loop stop signal observed (ni={}, ho={})", ni_count, ho_count);
            break;
        }

        // ── Poll one event (non-blocking) ───────────────────────────
        let mut event_received = false;
        match unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => {
                event_received = true;
                let event_type = match unsafe { event.GetType() } {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::error!("GetType failed: 0x{:08X}", e.code().0);
                        break;
                    }
                };
                tracing::trace!("pump_loop event_type=0x{:08X}", event_type);

                if event_type == METransformNeedInput.0 as u32 {
                    ni_count += 1;
                } else if event_type == METransformHaveOutput.0 as u32 {
                    ho_count += 1;
                } else if event_type == METransformDrainComplete.0 as u32 {
                    // DD3: reset counters; do not break. Top-of-loop stop check is the single exit.
                    if ni_count > 0 || ho_count > 0 {
                        tracing::debug!(
                            "DrainComplete: resetting non-zero counters (ni={}, ho={})",
                            ni_count, ho_count
                        );
                    }
                    ni_count = 0;
                    ho_count = 0;
                    tracing::info!("pump_loop received METransformDrainComplete");
                } else if event_type == MEEndOfStream.0 as u32 {
                    tracing::info!("pump_loop received MEEndOfStream; exiting");
                    break;
                } else {
                    tracing::warn!(
                        "pump_loop received unhandled event_type=0x{:08X}; continuing loop",
                        event_type
                    );
                }
            }
            Err(e) => {
                let code = e.code();
                if code == MF_E_NO_EVENTS_AVAILABLE {
                    // Expected idle branch — no log.
                } else if code == MF_E_SHUTDOWN {
                    tracing::info!("pump_loop GetEvent MF_E_SHUTDOWN; exiting");
                    break;
                } else if code.0 as u32 == 0x8000_4004 {
                    // E_ABORT — graceful shutdown signal (preserves existing line 747 behavior).
                    tracing::info!("pump_loop GetEvent E_ABORT; exiting");
                    break;
                } else {
                    tracing::error!("GetEvent failed: 0x{:08X}", code.0);
                    break;
                }
            }
        }

        // ── DD8: counter snapshot only on change ────────────────────
        if (ni_count, ho_count) != (last_logged_ni, last_logged_ho) {
            tracing::debug!("pump_loop counters: ni={}, ho={}", ni_count, ho_count);
            last_logged_ni = ni_count;
            last_logged_ho = ho_count;
        }

        // ── Service HaveOutput FIRST (drain-before-input) ───────────
        let mut output_disconnected = false;
        while ho_count > 0 {
            match collect_output(mft, output_format_known, current_ts, &mut seq) {
                Ok(Some(pkt)) => {
                    match tx.try_send(pkt) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            state.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            output_disconnected = true;
                            ho_count -= 1; // consume credit before exit
                            break;
                        }
                    }
                    ho_count -= 1;
                }
                Ok(None) => {
                    // need-more-input or stream-change — credit consumed.
                    ho_count -= 1;
                }
                Err(EncoderError::EncodeFailed(reason))
                    if reason.starts_with("ProcessOutput: 0x80004005") /* E_UNEXPECTED */ =>
                {
                    // Vendor-priming spurious HaveOutput — log + consume credit + continue.
                    // See DD4 / explore §2 Bug 1 vendor priming notes.
                    tracing::warn!("ProcessOutput E_UNEXPECTED (vendor priming); consuming credit");
                    ho_count -= 1;
                }
                Err(e) => {
                    tracing::error!("collect_output: {e}");
                    break;
                }
            }
        }
        if output_disconnected {
            break;
        }

        // ── Service NeedInput SECOND ────────────────────────────────
        let mut needs_repoll = false;
        while ni_count > 0 {
            // Apply pending keyframe / bitrate BEFORE ProcessInput.
            apply_pending_codec_settings(codec_api, state);

            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(frame) => {
                    current_ts = frame.timestamp;
                    nv12_convert(&frame, &mut nv12_scratch);
                    match submit_frame(mft, &nv12_scratch, frame.timestamp, frame_dur_100ns) {
                        Ok(()) => { ni_count -= 1; }
                        Err(EncoderError::EncodeFailed(reason))
                            if reason.starts_with("ProcessInput: 0xC00D36B5") /* MF_E_NOTACCEPTING */ =>
                        {
                            // Counter desync — should be unreachable. Hard-stop in debug.
                            debug_assert!(false, "ProcessInput MF_E_NOTACCEPTING — counter logic wrong");
                            tracing::error!("ProcessInput MF_E_NOTACCEPTING (ni_count={}); exiting pump", ni_count);
                            return;
                        }
                        Err(e) => {
                            tracing::warn!("ProcessInput failed: {e}; consuming credit");
                            ni_count -= 1;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Don't burn the NeedInput credit; re-poll events next iteration.
                    needs_repoll = true;
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Upstream closed — drain. Don't burn the credit; let DrainComplete reset counters.
                    unsafe {
                        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
                    }
                    needs_repoll = true;
                    break;
                }
            }
        }

        // ── Idle sleep tick ─────────────────────────────────────────
        if !event_received && !needs_repoll && ni_count == 0 && ho_count == 0 {
            std::thread::sleep(POLLING_SLEEP);
        }
    }

    tracing::debug!("pump_loop exited cleanly (ni={}, ho={})", ni_count, ho_count);
}
```

#### 4.1.4 New helper: `apply_pending_codec_settings`

Extract the keyframe + bitrate apply block (currently inlined in NeedInput arm at lines 766–788) into a small helper to keep `pump_loop` readable. NOT a behavior change.

```rust
/// Apply pending request_keyframe / set_bitrate signals to the codec.
/// Called BEFORE ProcessInput per DD10/DD11 in design #573.
fn apply_pending_codec_settings(codec_api: &ICodecAPI, state: &MftEncoderShared) {
    if state.keyframe_pending.swap(false, Ordering::AcqRel) {
        let v = make_variant_bool(true);
        unsafe {
            let _ = codec_api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
        }
    }
    let new_bps = state.pending_bitrate.swap(0, Ordering::AcqRel);
    if new_bps != 0 {
        let v = make_variant_u32(new_bps);
        unsafe {
            if let Err(e) = codec_api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) {
                tracing::warn!("ICodecAPI::SetValue(bitrate) rejected: 0x{:08X}", e.code().0);
            }
        }
    }
}
```

#### 4.1.5 What is NOT modified

- `Drop` impl (lines 246–276) — UNCHANGED. PR #15 ordering is correct.
- `init_mft_sync`, `enumerate_and_activate`, `setup_mft`, `effective_dimensions` — UNCHANGED.
- `submit_frame`, `collect_output`, `extract_bytes`, `build_imfsample`, `avcc_to_annex_b` — UNCHANGED.
- `make_variant_u32`, `make_variant_bool`, `H264_PROFILE_MAIN`, `CoUninitGuard` — UNCHANGED.
- All existing `#[cfg(test)] mod tests` — UNCHANGED.
- `MFT_OUTPUT_DATA_BUFFER`, `ProcessMessage(NOTIFY_END_OF_STREAM/END_STREAMING)` post-loop block (lines 465–468) — UNCHANGED.

#### 4.1.6 Estimated diff

- Removed: ~110 lines (entire pump_loop body lines 721–846).
- Added: ~165 lines (new pump_loop body) + ~15 lines (helper) + ~5 lines (constant + imports).
- Net: ~75 lines added.
- Total file size: 1141 → ~1216 lines.

### 4.2 `crates/sm-infra/tests/windows_mft_encode.rs`

#### 4.2.1 New test T-NEW-1: `mft_stop_during_idle_returns_within_deadline`

```rust
/// T-NEW-1 (Bug 2 stop starvation, idle path) — `stop()` must return within 2 s
/// after `start()` with NO frames sent.
///
/// **Before fix**: `pump_loop` blocks indefinitely in `GetEvent(MF_EVENT_FLAG_NONE)`;
/// `stop()` sets `state.stop = true` but `join()` deadlocks because GetEvent is blocked.
/// **After fix**: `pump_loop` polls `MF_EVENT_FLAG_NO_WAIT` and checks `state.stop` at
/// every iteration, so `stop()` returns within `POLLING_SLEEP` (1 ms) + join overhead.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_stop_during_idle_returns_within_deadline() {
    init_tracing();
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (_frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Do NOT send any frames. Wait briefly to ensure the pump_loop is in its main loop.
    std::thread::sleep(Duration::from_millis(100));

    // Call stop() and assert it returns within 2 s.
    let t0 = Instant::now();
    enc.stop().expect("stop must return Ok(()) within deadline");
    let elapsed = t0.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "stop() during idle took {elapsed:?} — pump_loop is starving on GetEvent"
    );
}
```

#### 4.2.2 New test T-NEW-2: `mft_stop_during_active_encode_returns_within_deadline`

```rust
/// T-NEW-2 (Bug 2 stop starvation, active path) — `stop()` must return within 2 s
/// during active encoding WITHOUT closing `frame_tx` first.
///
/// **Before fix**: even with active encoding, calling `stop()` mid-stream without
/// dropping `frame_tx` deadlocks because GetEvent blocks between events and stop is
/// invisible until the next NeedInput/HaveOutput tick (which may be > 33 ms but
/// could also be unbounded if vendor MFT pauses).
/// **After fix**: `state.stop` check at top of loop honors the signal within 1 ms,
/// independent of MFT event cadence.
#[test]
#[ignore = "hardware H.264 MFT required — run manually on a GPU-capable host"]
fn mft_stop_during_active_encode_returns_within_deadline() {
    init_tracing();
    let mut enc = WindowsMftH264Encoder::new(EncoderConfig {
        width: 640,
        height: 480,
        ..EncoderConfig::default()
    })
    .expect("WindowsMftH264Encoder::new must succeed on a HW-capable machine");

    let (frame_tx, frame_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);
    let (pkt_tx, _pkt_rx) = mpsc::sync_channel(sm_infra::encode::ENCODE_CHANNEL_CAPACITY);

    enc.start(frame_rx, pkt_tx).expect("start should succeed");

    // Send 5 frames mid-stream WITHOUT closing frame_tx.
    for i in 0..5u64 {
        frame_tx
            .send(make_synthetic_frame(640, 480, i * 33))
            .expect("frame_tx should be open during active encode");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Call stop() while frame_tx is still alive — this is the real-world shutdown shape
    // (sender drops the encoder while still holding frame_tx; Drop chain MUST stop within bound).
    let t0 = Instant::now();
    enc.stop().expect("stop must return Ok(()) within deadline");
    let elapsed = t0.elapsed();

    // frame_tx still alive here — keep it for a moment to prove stop didn't depend on Disconnected.
    drop(frame_tx);

    assert!(
        elapsed < Duration::from_secs(2),
        "stop() during active encode took {elapsed:?} — pump_loop did not honor stop signal"
    );
}
```

#### 4.2.3 No modifications to existing 16 tests

Existing tests rely on the same `start()`/`stop()` API. They get faster/more reliable as a side effect of the pump_loop redesign, but the test SOURCE does not change.

#### 4.2.4 Estimated diff

- Added: ~75 lines (2 tests + comments).
- Removed: 0.
- File size: 745 → ~820 lines.

### 4.3 `crates/sm-infra/Cargo.toml`

**NO CHANGE.** Decision #8 in proposal #595: `default = []` stays. Flip to default-on is a separate follow-up change.

---

## 5. Threading & Safety

### 5.1 Pump thread = same thread as today

- One OS thread per encoder, spawned in `start()` (line 202).
- All MFT/COM calls (`GetEvent`, `ProcessInput`, `ProcessOutput`, `ProcessMessage`) happen on this thread.
- Counter state (`ni_count`, `ho_count`) lives on the pump thread's stack — NOT shared, NO atomics needed.
- Heartbeat/log throttle state (`last_logged_ni`, `last_logged_ho`) — same.

### 5.2 Stop signal: existing `Arc<AtomicBool>`

- `state: Arc<MftEncoderShared>` already holds `stop: AtomicBool`.
- Read with `Ordering::Acquire` (matches existing line 738).
- Written by `stop()` with `Ordering::Release` (matches existing line 220).
- NO new sync primitive added.

### 5.3 COM threading: unchanged

- `MTA_INIT` per existing setup in `init_mft_sync` (line 285) and `run_encoder_thread` (line 413).
- Encoder thread joins MTA via `CoInitializeEx(COINIT_MULTITHREADED)`.
- `CoUninitGuard` (lines 386–397) handles thread-exit cleanup unchanged.

### 5.4 Drop ordering: unchanged

- PR #15's fix at lines 246–276 stays intact.
- `stop()` → join thread → `drop(codec_api.take())` → `drop(mft.take())` → `MFShutdown` → `CoUninitialize`.
- The pump_loop redesign affects WHEN the join completes (much faster now), not the order of cleanup.

### 5.5 Send/Sync invariants: unchanged

- `WindowsMftH264Encoder` still `unsafe impl Send + Sync` (lines 140–141).
- `ComSend<T>` wrapper still used to transfer COM interfaces into the spawned thread closure (lines 199–200).
- The static assertion `adapter_is_send_sync` (line 1122) continues to gate this at compile time.

---

## 6. Error Handling

| Source | HRESULT / variant | Code | Action | Log level |
|--------|-------------------|------|--------|-----------|
| `GetEvent` | `MF_E_NO_EVENTS_AVAILABLE` | `0x80040204` | Continue (sleep tick if also no work) | none |
| `GetEvent` | `MF_E_SHUTDOWN` | `0xC00D3E85` | Break (graceful) | INFO |
| `GetEvent` | `E_ABORT` | `0x80004004` | Break (graceful) | INFO |
| `GetEvent` | other | various | Break | ERROR |
| `event.GetType` | any error | various | Break | ERROR |
| `ProcessOutput` | `MF_E_TRANSFORM_NEED_MORE_INPUT` | existing | `Ok(None)`; consume credit | none |
| `ProcessOutput` | `MF_E_TRANSFORM_STREAM_CHANGE` | existing | `Ok(None)`; consume credit | none |
| `ProcessOutput` | `E_UNEXPECTED` | `0x80004005` | Consume credit; continue (vendor-priming spurious credit) | WARN |
| `ProcessOutput` | other | various | Break | ERROR |
| `ProcessInput` | `MF_E_NOTACCEPTING` | `0xC00D36B5` | `debug_assert!(false)`; ERROR + return (counter desync, unreachable) | ERROR |
| `ProcessInput` | other | various | Consume credit; continue | WARN |
| `frame_rx` | `RecvTimeoutError::Timeout` | n/a | DON'T burn ni credit; break inner; re-poll events | none |
| `frame_rx` | `RecvTimeoutError::Disconnected` | n/a | Send `COMMAND_DRAIN`; DON'T burn ni credit; break inner | INFO |
| `tx.try_send` (output) | `Full` | n/a | `dropped.fetch_add(1)`; consume ho credit | none |
| `tx.try_send` (output) | `Disconnected` | n/a | Consume credit; break outer loop | INFO |

**HRESULT verification**: all five new HRESULT constants (`MF_E_NO_EVENTS_AVAILABLE`, `MF_E_SHUTDOWN`, `MF_E_NOTACCEPTING`, `METransformDrainComplete`, `MF_EVENT_FLAG_NO_WAIT`) MUST be present in `windows = "0.62.2"` MediaFoundation namespace. Apply Phase 0 verifies via `cargo check --features hw-encoder`. If absent, that is a Cargo `windows` features gap (currently `Win32_Media_MediaFoundation` is enabled which should cover all of these — see DR-NEW-1 in §11).

---

## 7. Logging Strategy (resolves OQ-2 → DD8)

| Level | Event | Rate-limit |
|-------|-------|------------|
| TRACE | Every event received with `event_type=0x{:08X}` | None — TRACE is opt-in via `RUST_LOG` |
| DEBUG | Counter snapshot `(ni, ho)` ON CHANGE only | Comparison against `last_logged_ni/ho` — emits only when tuple changes |
| DEBUG | `pump_loop stop signal observed (ni=N, ho=N)` (one-shot at exit) | Once per pump |
| DEBUG | `DrainComplete: resetting non-zero counters (ni=N, ho=N)` (rare) | Once per drain |
| DEBUG | `pump_loop exited cleanly (ni=N, ho=N)` (one-shot at exit) | Once per pump |
| INFO | `pump_loop received METransformDrainComplete` | Once per drain (rare) |
| INFO | `pump_loop received MEEndOfStream; exiting` | Once per pump |
| INFO | `pump_loop GetEvent MF_E_SHUTDOWN/E_ABORT; exiting` | Once per pump |
| WARN | `unhandled event_type=0x{:08X}; continuing loop` | Per-event; vendor MFTs RARELY emit unknown |
| WARN | `ProcessOutput E_UNEXPECTED (vendor priming); consuming credit` | Per-event; should fire ≤ a handful of times at startup |
| WARN | `ProcessInput failed: ...; consuming credit` | Per-event; should be rare |
| WARN | `ICodecAPI::SetValue(bitrate) rejected: 0x{:08X}` | Per-bitrate-change attempt |
| ERROR | `GetEvent failed: 0x{:08X}` (unexpected HRESULT) | Per-event; followed by break |
| ERROR | `GetType failed: 0x{:08X}` | Per-event; followed by break |
| ERROR | `collect_output: ...` (non-vendor errors) | Per-event; followed by break |
| ERROR | `ProcessInput MF_E_NOTACCEPTING (ni_count=N); exiting pump` | Per-event; followed by `return` (debug_assert!) |

**Rate-limit policy lock**: counter DEBUG snapshots use change-only emission (~tens per second worst case during transient state, far below TRACE flood). At 30 fps steady-state, `(ni, ho)` oscillates between `(0, 0)` and `(1, 0)` / `(0, 1)`, producing ~30 DEBUG lines/sec under `RUST_LOG=debug` — acceptable. Under `RUST_LOG=warn` (default), only WARN/ERROR/INFO fire — quiet steady state.

NO sampling, NO time-based throttling — change-only emission is sufficient.

---

## 8. Test Design (RED targets for apply)

### 8.1 T-NEW-1 `mft_stop_during_idle_returns_within_deadline`

| Aspect | Detail |
|--------|--------|
| Setup | `init_tracing()`, `WindowsMftH264Encoder::new(640×480)`, channels with `ENCODE_CHANNEL_CAPACITY`, `start()`, sleep 100 ms |
| Send | NONE — idle path |
| Action | `enc.stop()`, measure `Instant::now() - t0` |
| Assertion | `elapsed < Duration::from_secs(2)` AND `stop()` returned `Ok(())` |
| Cleanup | Encoder dropped at scope exit (Drop calls stop() again — idempotent per existing T13.1) |
| Why RED on current | Current `pump_loop` blocks on `GetEvent(MF_EVENT_FLAG_NONE)` waiting for events that never arrive (no frames sent → MFT never emits NeedInput/HaveOutput); `stop()` sets flag but `join()` waits forever. Test hangs and trips the smoke-test wall-clock. |
| Why GREEN after | New pump_loop checks `state.stop` every ≤ 1 ms; `stop()` returns in < 10 ms typical. |
| Smoke required | yes (HW MFT) |

### 8.2 T-NEW-2 `mft_stop_during_active_encode_returns_within_deadline`

| Aspect | Detail |
|--------|--------|
| Setup | Same as T-NEW-1 |
| Send | 5 frames at 640×480, 20 ms apart |
| Action | `enc.stop()` WITHOUT `drop(frame_tx)` first; measure elapsed; THEN `drop(frame_tx)` |
| Assertion | `elapsed < Duration::from_secs(2)` AND `stop()` returned `Ok(())` |
| Cleanup | `drop(frame_tx)` after measurement; encoder dropped at scope exit |
| Why RED on current | Current `pump_loop` blocks on `GetEvent` between events; even mid-stream, the MFT emits events with vendor-dependent cadence. If the vendor pauses (e.g. waiting for next frame), GetEvent is blocked indefinitely. `stop()` cannot interrupt. |
| Why GREEN after | NO_WAIT polling honors stop within 1 ms regardless of MFT event cadence. |
| Smoke required | yes (HW MFT) |

### 8.3 Existing 16 tests (no source changes; behavioral expectations recorded)

Per explore #594 §6 forecast table, the existing tests transition as follows:

| Test | Current (per #591) | After this change |
|------|---------------------|-------------------|
| `mft_new_then_drop_does_not_av` | PASS (no pump) | PASS |
| `mft_new_on_hw_capable_machine_returns_ok` | PASS (no pump) | PASS |
| `mft_new_returns_init_failed_when_no_hardware_mft` | PASS (no pump) | PASS |
| `mft_new_does_not_submit_frames_to_mft_during_init` | PASS | PASS |
| `mft_encoded_packet_starts_with_annex_b_start_code` | FAIL (stop hangs) | PASS |
| `mft_thirty_frame_smoke_emits_at_least_one_keyframe` | FAIL (stop hangs) | PASS |
| `mft_encoded_packet_timestamp_matches_capture_frame` | FAIL | PASS |
| `mft_request_keyframe_marks_next_packet_as_keyframe` | FAIL | PASS |
| `mft_keyframe_flag_cleared_after_idr_emitted` | FAIL | PASS |
| `mft_set_bitrate_updates_encoder_without_restart` | FAIL | PASS |
| `mft_first_real_packet_is_annex_b` | FAIL | PASS |
| `mft_setup_uses_config_dimensions_when_nonzero` | FAIL | PASS |
| `mft_setup_falls_back_when_config_dimensions_zero` | FAIL | PASS |
| `mft_drain_after_channel_close_does_not_panic` | FAIL (stop hangs) | PASS |
| `mft_stop_is_idempotent` | FAIL | PASS |
| `mft_drop_without_stop_does_not_leak_thread` | FAIL (leak) | PASS |

Total: 16/16 expected PASS post-change + 2 NEW PASS = 18/18 smoke target.

---

## 9. Decisions Table (DD1–DD10)

Mirror prior precedent (#587 §14 + #588). Each DD: Decision / Source OQ or Risk / Choice / Rationale / Apply phase impact.

| # | Decision | Source | Choice | Rationale | Apply phase impact |
|---|----------|--------|--------|-----------|---------------------|
| **DD1** | pump_loop architecture refinement: counter-based dual-arm with NO_WAIT polling. | Proposal Decision #1 (Pattern B), #2 (Fix B); explore §2/§3. | Single redesign of `pump_loop` body lines 712–847 with `ni_count`/`ho_count` u32 stack-locals, `MF_EVENT_FLAG_NO_WAIT`, drain-first ordering, top-of-loop stop check. | Pattern B + Fix B share the same loop body; combining them is the unique coherent fix. ~75 net LOC change to one function. | Apply Phase 2 commit. |
| **DD2** | Counter decrement timing: AFTER successful service. On error, two sub-policies. | OQ-1. | (a) `ho_count -= 1` after `Ok(_)`, OR after logged `E_UNEXPECTED` (consume vendor-priming spurious credit), OR after non-vendor `Err` (NOT consumed — break instead). (b) `ni_count -= 1` after `Ok(())` from `submit_frame`, OR after logged non-`MF_E_NOTACCEPTING` Err (skip-frame consumption). On `RecvTimeoutError::Timeout`/`Disconnected`, credit is NOT consumed — re-poll on next iteration. | Maintains the spec invariant "counter == pending unserviced events". Vendor-priming spurious `E_UNEXPECTED` on HaveOutput consumes the credit because the event itself was vendor-emitted (we received it; we just couldn't service it productively). On NeedInput, channel-timeout means "we're not ready to feed this credit yet" — keep the credit so the next iteration can retry. | Apply Phase 2 commit. |
| **DD3** | METransformDrainComplete arm: reset both counters; do NOT break. | OQ-4 (resolved here per task instruction). | Reset `ni_count = 0; ho_count = 0;` and continue loop. Top-of-loop stop check (set externally by stop() OR set by the orchestrator of the drain via Disconnected path) is the sole exit. INFO-log the event. | Per Microsoft async-MFT contract, DrainComplete means "MFT is quiesced — no more output coming, no more input wanted". Counter credits emitted before drain MUST NOT be honored after, because the MFT is in a state where ProcessInput would return MF_E_NOTACCEPTING and ProcessOutput would return E_UNEXPECTED. Continuing the loop (rather than breaking) lets the existing post-drain code (NOTIFY_END_OF_STREAM in run_encoder_thread:466) execute via the normal stop path. | Apply Phase 3 commit. Resolves OQ-4. |
| **DD4** | Vendor-priming `E_UNEXPECTED` from `ProcessOutput`: consume credit + WARN + continue. | Explore §2 Bug 1 vendor priming description; current code at line 884 maps any non-NeedMoreInput / non-StreamChange error to `EncoderError::EncodeFailed` which then breaks the pump. | Recognize the specific HRESULT `0x80004005` (E_UNEXPECTED) in the post-`collect_output` match arm via string-prefix match on the formatted error reason; consume the `ho_count` credit and continue. Do NOT treat as fatal. | Vendor MFTs (Intel QSV, NVENC, AMF) emit `HaveOutput` BEFORE first `NeedInput` as part of pipeline priming. `ProcessOutput` at that point fails with `E_UNEXPECTED` (no real output yet). The credit is real — vendor told us — we just couldn't extract a sample. Consume it and move on. After 1–3 such priming events, vendor settles into normal NeedInput-then-HaveOutput cadence. | Apply Phase 2 commit. String-prefix match on EncoderError::EncodeFailed reason is brittle — alternative is to refactor collect_output to return a typed error (HRESULT raw u32) — but that's a larger refactor. The string-prefix approach is pragmatic; DR-NEW-2 documents the brittleness risk. |
| **DD5** | `MF_E_NOTACCEPTING` from `ProcessInput`: `debug_assert!(false, ...)` + ERROR-log + `return` from pump_loop. | Bug 1 invariant; spec-mandated by Microsoft async-MFT contract (1:1 NeedInput → ProcessInput). | If this fires, our counter logic has a bug. Hard-stop in debug builds (panic via debug_assert), error-log + clean exit in release. | Surfaces counter desyncs immediately during smoke testing; doesn't crash production. | Apply Phase 2 commit. |
| **DD6** | POLLING_SLEEP constant: `Duration::from_millis(1)` inline at module scope. | Proposal Decision #5; OQ-5. | `const POLLING_SLEEP: Duration = Duration::from_millis(1);` near `H264_PROFILE_MAIN` declaration. NOT exposed via `EncoderConfig` — YAGNI. | Direct port of proposal Decision #5. Tunability is a follow-up if vendor-specific need emerges. | Apply Phase 2 commit. |
| **DD7** | Test deadline constant for T-NEW-1 / T-NEW-2: inline `Duration::from_secs(2)` at the assertion. | OQ-5 (test-deadline portion). | Inline constant at each test's assertion site. NO shared `tests/common/timeouts.rs` (would be one module for one constant — premature DRY). | 2-second deadline gives 1000× margin over the 1 ms POLLING_SLEEP target while leaving room for thread spawn/join overhead and CI host variance. Inline keeps each test self-documenting. | Apply Phase 1 commit (test-only). |
| **DD8** | Counter logging strategy: TRACE every event (existing line 763 pattern); DEBUG counter snapshot ON CHANGE only via `last_logged_ni/ho` u32 sentinels. | OQ-2. | Two stack-local u32 sentinels (init `u32::MAX`); compare `(ni_count, ho_count) != (last_logged_ni, last_logged_ho)` after every counter mutation block; emit DEBUG snapshot + update sentinels. NO time-based throttling, NO sampling. | Change-only emission gives heartbeat at every state transition without flood. At 30 fps steady state, ~30 DEBUG lines/sec — well below TRACE volume. Default `RUST_LOG=warn` mutes them entirely. | Apply Phase 2 commit. |
| **DD9** | Helper extraction: `apply_pending_codec_settings(codec_api, state)` for keyframe + bitrate apply. | Readability of new pump_loop body. | Extract lines 766–788 into module-private fn before `submit_frame`. NO behavior change — pure refactor included in same commit as pump_loop redesign. | Keeps pump_loop body under ~150 lines for code-review tractability. The extraction is mechanical — a single contiguous block is moved verbatim. | Apply Phase 2 commit (same commit as pump_loop). |
| **DD10** | Cargo.toml `default = []` stays unchanged. | Proposal Decision #8. | NO modification to `crates/sm-infra/Cargo.toml`. | Decoupling rewrite-merge from default-flip preserves blast-radius isolation. The flip is a separate change gated on smoke transcript + soak. | Apply Phase 0 verifies absence; no commit. |

---

## 10. Phase Sequencing for Apply

Single PR per Decision #3, but apply commits in logical work-units (Strict TDD: every (impl) commit preceded by RED (test) commit).

### Phase 0 — Anchor (no commit)
- Confirm baseline: `cargo nextest run --workspace` GREEN (618/618 PASS, 35 skipped, per #590 Phase 5 baseline).
- Confirm `cargo check --features hw-encoder` succeeds and the new imports resolve (`MF_E_NO_EVENTS_AVAILABLE`, `MF_E_SHUTDOWN`, `MF_E_NOTACCEPTING`, `METransformDrainComplete`, `MF_EVENT_FLAG_NO_WAIT`). If any missing, this is a Cargo `windows` features gap — STOP and escalate (DR-NEW-1).
- Re-run `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` — confirm clean.

### Phase 1 — Test scaffolding (RED commit)
- Add T-NEW-1 + T-NEW-2 to `crates/sm-infra/tests/windows_mft_encode.rs`.
- `cargo nextest run --workspace` still GREEN (new tests are `#[ignore]`-gated).
- HW smoke (manual; if user runs locally) → both new tests would HANG or fail with timeout on master HEAD. RED for the smoke domain only.
- Commit message: `test(infra): add MFT stop-deadline smoke tests for idle and active paths (RED)`

### Phase 2 — pump_loop redesign + helper extraction (GREEN commit, partial)
- Implement DD1–DD10 in `crates/sm-infra/src/encode/windows_mft.rs`:
  - New imports.
  - `POLLING_SLEEP` constant.
  - `apply_pending_codec_settings` helper extraction.
  - pump_loop body rewrite (counters, NO_WAIT poll, drain-first, sleep tick, error arms).
  - Vendor-priming `E_UNEXPECTED` consume-credit + WARN.
  - `MF_E_NOTACCEPTING` debug_assert.
  - DD8 logging.
- After this commit: T-NEW-1 + T-NEW-2 pass on smoke; existing 16 tests pass on smoke (Bucket A unblocked because counters drain output before submitting input).
- DrainComplete arm NOT yet added — receives "unhandled event_type" warn but does not crash. This is fine for Phase 2 because the existing drain test will pass via the existing MEEndOfStream path.
- Commit message: `feat(infra): rewrite pump_loop to dual-arm NO_WAIT polling for vendor priming + stop deadline (GREEN)`

### Phase 3 — DrainComplete arm + counter-reset semantics
- Add the explicit `METransformDrainComplete.0` arm in pump_loop event match, with counter reset.
- Add INFO log for the event.
- After this commit: `mft_drain_after_channel_close_does_not_panic` cleaner output (no more "unhandled event_type" warn for DrainComplete).
- Commit message: `feat(infra): handle METransformDrainComplete with counter reset to prevent post-drain phantom events`

### Phase 4 — Quality gates + smoke prep
- Run all 8 quality gates per #186 (cargo check, clippy with -D warnings, fmt --check via direct invocation per discovery #581, nextest --workspace, deny check, no-default-features check, no-default-features nextest, pnpm test if frontend touched — not in this change, skip).
- Document smoke instructions for user:

```
cargo nextest run -p sm-infra --features hw-encoder --test windows_mft_encode --run-ignored=all --no-capture --no-fail-fast
```

  Expected: 18/18 PASS (16 existing + T-NEW-1 + T-NEW-2).
- User saves transcript via `mem_save(topic_key: "sdd/hw-encoder-mft-rework/smoke-transcript", type: "discovery", content: <invocation + host + ISO 8601 + pass summary + stdout/stderr>)`.
- Commit message: NONE (engram-only) OR a docs/comment commit if any code annotation is added in this phase.

### Phase 5 — Verify gate (BLOCKED_ON_SMOKE)
- Per #186 BLOCKED_ON_SMOKE rule: verify CANNOT issue `APPROVED_FOR_ARCHIVE` until smoke transcript supplied.
- If smoke fails: Decision #7 contingency activates — abort merge, escalate to new change `hw-encoder-mft-async-callback`.
- If smoke passes: verify emits `APPROVED_FOR_ARCHIVE`; archive runs.

### Estimated commit count

3 logical commits (Phase 1 test, Phase 2 pump_loop redesign + extraction, Phase 3 DrainComplete arm). Within the proposal-implied 60–100 LOC envelope for the pump_loop redesign plus ~75 LOC tests = ~175 LOC total — well under the 400-line single-PR budget.

---

## 11. Risks (carry-forward + new)

### Carry-forward from proposal #595

| # | Risk | Design-level mitigation |
|---|------|-------------------------|
| **R1** | NO_WAIT vendor-support unverified empirically on user GPU until apply-phase smoke. | Decision #7 contingency stays primary mitigation. Design adds: T-NEW-1 / T-NEW-2 will FAIL (timeout) cleanly if vendor returns something other than MF_E_NO_EVENTS_AVAILABLE for NO_WAIT — failure is observable, not silent. |
| **R2** | 18 `#[ignore]` smoke tests means BLOCKED_ON_SMOKE will fire on verify; user must supply transcript before archive. | Process risk only. Design Phase 4 explicitly documents the smoke invocation command. |
| **R3** | Decision #3 (single PR) overrides cached `auto-chain`. Diff size must stay within 400-line budget. | Design Phase 4 estimate: ~175 LOC total (~75 src + ~75 tests + ~25 ancillary). Well under 400. Tasks-phase Review Workload Forecast must verify. |
| **R4** | METransformDrainComplete arm added without prior empirical evidence the event is actually emitted. | Design DD3 falls back gracefully if event NEVER arrives: counters are also reset by the next stop() / Drop chain. The arm is INSURANCE, not a critical-path dependency. Smoke will reveal via the new INFO log whether DrainComplete actually fires. |
| **R5** | Decision #8 (default stays `[]`) means this change does not, by itself, restore production HW acceleration. | User-expectation risk only. Design has no mechanism to address — handled at archive time per proposal §10. |

### New design-level risks

| # | Risk | Mitigation |
|---|------|------------|
| **DR-NEW-1** | The five new `windows` crate symbols (`MF_E_NO_EVENTS_AVAILABLE`, `MF_E_SHUTDOWN`, `MF_E_NOTACCEPTING`, `METransformDrainComplete`, `MF_EVENT_FLAG_NO_WAIT`) may not all be re-exported under the currently-enabled `Win32_Media_MediaFoundation` feature in `windows = "0.62.2"`. | Apply Phase 0 anchor explicitly verifies via `cargo check --features hw-encoder` BEFORE writing any pump_loop code. If a symbol is missing, escalate to user (likely needs an extra `windows` feature like `Win32_Media_MediaFoundation_Engine` or a manual constant). NOT a redesign — just a Cargo features fix. |
| **DR-NEW-2** | Vendor-priming `E_UNEXPECTED` recognition uses a string-prefix match on the formatted `EncoderError::EncodeFailed` reason — brittle to format changes in `collect_output`. | Document the contract inline: any change to `collect_output`'s error-formatting must keep the `"ProcessOutput: 0x"` prefix intact. Alternative refactor (typed HRESULT in EncoderError variant) is OUT OF SCOPE per proposal §4 (no `sm-domain` changes). |
| **DR-NEW-3** | COM thread affinity — pump thread is the same that called `CoInitializeEx(MTA)`; all MFT/COM calls remain on this thread. NOT introducing a new synchronization point that could cross apartments. | Design §5.3 documents the unchanged threading model. The new sleep tick is `std::thread::sleep` which does NOT yield COM apartment ownership. |
| **DR-NEW-4** | `debug_assert!(false, ...)` for `MF_E_NOTACCEPTING` will panic the encoder thread in debug builds (test runs included). If the assertion fires during nextest with `--profile dev`, the test process aborts and other parallel tests in the same binary are unaffected (nextest process isolation), but the test author may misread the panic as a test bug rather than a counter-logic bug. | Panic message includes "counter logic wrong" — specific enough for debugging. release builds use ERROR + return for graceful degradation. |
| **DR-NEW-5** | The DD8 change-only DEBUG snapshot might miss intermediate states if multiple events arrive in a single iteration. | Counter mutations occur ONLY at the event arms (one event per iteration; only ONE arm runs per iteration). The snapshot fires at the END of the per-iteration mutation phase, capturing the post-event state. No state can be "between" event arms within an iteration. |
| **DR-NEW-6** | Cross-platform CI matrix: this change is Windows + hw-encoder gated. The 3-OS CI matrix (Linux, macOS, Windows-no-features) does NOT compile or run windows_mft.rs. Verified safe — the existing `#![cfg(all(target_os = "windows", feature = "hw-encoder"))]` gate at line 1 of `windows_mft.rs` and line 21 of `windows_mft_encode.rs` continues to gate. No matrix change required. | Apply Phase 4 includes `cargo check --no-default-features` and `cargo nextest run --workspace --no-default-features` as gates per #186. If either breaks, that is a regression. |

---

## 12. Result Contract

- **status**: done
- **executive_summary**: Locked Pattern B + Fix B as a single pump_loop redesign — counter-based dual-arm state machine with `MF_EVENT_FLAG_NO_WAIT` polling, drain-output-first ordering, top-of-loop stop check (≤ 1 ms latency), explicit `METransformDrainComplete` arm with counter reset (resolves OQ-4 to option (a) — reset and continue), vendor-priming `E_UNEXPECTED` consume-credit policy, `MF_E_NOTACCEPTING` `debug_assert` for counter desync. Two new `#[ignore]` smoke tests for stop deadline (idle + active paths). Cargo defaults unchanged per Decision #8. 3 logical commits within ~175 LOC.
- **artifacts**:
  - `engram://sdd/hw-encoder-mft-rework/design`
  - `openspec/changes/hw-encoder-mft-rework/design.md`
- **next_recommended**: `sdd-tasks` (after `sdd-spec` also returns)
- **risks**:
  - **R1 (carry)**: NO_WAIT vendor-support unverified empirically until smoke; Decision #7 contingency primary.
  - **DR-NEW-1**: Five new `windows` crate symbols may need extra Cargo feature — apply Phase 0 verifies before writing pump_loop code.
  - **DR-NEW-2**: String-prefix match on E_UNEXPECTED is brittle; documented inline contract; alternative refactor out of scope.
  - **R4 (carry)**: DrainComplete event may never fire on some vendor MFTs — DD3 reset is INSURANCE not critical path.
- **skill_resolution**: injected
