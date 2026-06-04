// overlay-dismiss.test.js — SC-SSR-OVL-RECOVERY
//
// Bug: after a real reconnect recovers via the FRAME_INIT self-arm path
// (onInitFrame Guard 1 → setUpMse), the `reconnecting` overlay stays visible
// over the now-playing video because overlay-hide lives ONLY in handleStatus
// case "streaming" and the self-arm path never sends a "streaming" status frame.
//
// Fix: add a dismissReconnectOverlayOnRecovery() helper called from the <video>
// "playing" event handler, gated on overlayRevealed so it only acts during an
// active Stage-2 reconnect. Idempotent — no effect during normal playback.
//
// SC-SSR-OVL-RECOVERY:
//   GIVEN Stage 2 entered (overlayRevealed === true, overlay.hidden === false)
//   WHEN the <video> "playing" event fires (recovery via self-arm, no "streaming" status)
//   THEN overlay.hidden === true AND overlayRevealed === false (cancelSilentRecovery called)
//
// SC-SSR-OVL-NOOP:
//   GIVEN Stage 2 NOT entered (overlayRevealed === false, overlay.hidden === true)
//   WHEN the <video> "playing" event fires (normal playback)
//   THEN overlay.hidden is still true (no spurious hide; no double-cancel)

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

// Build a 0x02 status frame: [0x02, ...UTF-8 JSON bytes].
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

const THRESHOLD = 10_000;

describe('mse-client — overlay dismiss on video playing event (SC-SSR-OVL-RECOVERY)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    MockMediaSourceCtor._deferOpenNext = false;
    localStorage.clear();
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    ch = tauri.lastChannel();

    // Prime with an init segment so the MSE session is fully active.
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    MockMediaSourceCtor._deferOpenNext = false;
  });

  // ── SC-SSR-OVL-RECOVERY ──────────────────────────────────────────────────────
  // The core regression test.
  //
  // 1. Enter Stage 2 by sending a "reconnecting" frame and advancing the timer
  //    THRESHOLD ms (revealReconnectingOverlay fires: overlay.hidden=false,
  //    overlayRevealed=true, tearDownMse called).
  // 2. Do NOT send a "streaming" status — simulate the self-arm recovery path
  //    where FRAME_INIT triggers setUpMse directly (no status frame).
  // 3. Fire the <video> "playing" event.
  // 4. Assert overlay.hidden === true (overlay dismissed).
  // 5. Assert overlayRevealed === false (sentinel reset via cancelSilentRecovery).
  //
  // RED against current code: the <video> "playing" handler only console.logs
  // and does NOT touch the overlay or the overlayRevealed sentinel.
  it('SC-SSR-OVL-RECOVERY: playing event after Stage-2 entry hides overlay and resets overlayRevealed', async () => {
    const overlay = document.getElementById('reconnecting-overlay');
    expect(overlay).not.toBeNull();

    // ── Step 1: Enter Stage 2 ─────────────────────────────────────────────────
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance to threshold: revealReconnectingOverlay fires.
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();

    // Confirm Stage 2: overlay is visible.
    expect(overlay.hidden).toBe(false);

    // ── Step 2: Simulate self-arm recovery — no "streaming" status frame ──────
    // (The self-arm path: FRAME_INIT arrives with ms===null → onInitFrame Guard 1
    //  → setUpMse → sourceopen → video eventually fires "playing". No
    //  handleStatus("streaming") is sent, so the overlay-hide in case "streaming"
    //  is never triggered. This is the regression scenario.)

    // ── Step 3: Fire the <video> "playing" event ──────────────────────────────
    const videoEl = document.getElementById('player');
    videoEl.dispatchEvent(new Event('playing'));
    await Promise.resolve();

    // ── Step 4: Overlay must now be hidden ────────────────────────────────────
    // RED: current code — <video> "playing" handler only logs; overlay stays visible.
    // GREEN: dismissReconnectOverlayOnRecovery() called → overlay.hidden = true.
    expect(overlay.hidden).toBe(true);

    // ── Step 5: overlayRevealed sentinel must be reset ────────────────────────
    // cancelSilentRecovery() resets overlayRevealed = false (D-SSR-6 reset).
    // We verify this indirectly: after reset, a fresh "reconnecting" frame MUST
    // be able to re-arm the silent-recovery timer (sentinel allows re-arm).
    //
    // Record timer count and fire a fresh reconnecting frame.
    const timerCountBefore = vi.getTimerCount();
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 2 }).buffer);
    await Promise.resolve();

    // If overlayRevealed was properly reset, the arm guard passes and timer count
    // increases by 1. If still true (broken), the guard blocks re-arm (no increase).
    const timerCountAfter = vi.getTimerCount();
    expect(timerCountAfter).toBe(timerCountBefore + 1);
  });

  // ── SC-SSR-OVL-NOOP ──────────────────────────────────────────────────────────
  // Triangulation: "playing" during normal playback (Stage 2 NOT entered) must
  // NOT hide the overlay or trigger any spurious behavior.
  //
  // overlayRevealed is false (Stage 1 never armed or already cancelled via
  // cancelSilentRecovery). The overlay is already hidden. Firing "playing" must
  // leave everything unchanged — the gate `if (!overlayRevealed) return` must fire.
  it('SC-SSR-OVL-NOOP: playing event when overlayRevealed=false does NOT touch overlay (normal playback guard)', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    // No reconnecting status dispatched — overlayRevealed is false,
    // overlay is already hidden (installDom sets it hidden by default).
    expect(overlay.hidden).toBe(true);

    // Fire "playing" as in normal startup playback.
    const videoEl = document.getElementById('player');
    videoEl.dispatchEvent(new Event('playing'));
    await Promise.resolve();

    // Overlay must still be hidden (gate prevents spurious action).
    expect(overlay.hidden).toBe(true);

    // Timer count must not have changed (no cancelSilentRecovery side-effects).
    // (No reconnecting was sent so silentRecoveryTimerId is null — cancel is a no-op.)
    // Verify by dispatching a reconnecting frame; it must arm a NEW timer normally
    // (no pre-empted state from the spurious "playing" call).
    const timerCountBefore = vi.getTimerCount();
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    const timerCountAfter = vi.getTimerCount();
    expect(timerCountAfter).toBe(timerCountBefore + 1);
  });
});
