// staged-reconnect.test.js — SC-SSR-1..10 (REQ-SSR-1..8)
//
// Covers the 3-stage silent-reconnect timer gate.
// All tests are RED (failing) until the production implementation in
// dist/mse-client.js is updated (WU-CONST through WU-STREAMING).
//
// SC-SSR-1:  No overlay shown before SILENT_RECOVERY_THRESHOLD_MS.
// SC-SSR-2:  Overlay revealed at exactly SILENT_RECOVERY_THRESHOLD_MS.
// SC-SSR-3:  Silent success: streaming before threshold → overlay never shown.
// SC-SSR-4:  Timer NOT re-armed on subsequent reconnecting frames (total-elapsed singleton).
// SC-SSR-5:  Overlay shows most-recent attempt/max when timer fires.
// SC-SSR-6:  Timer cancelled on dead → overlay never shown, dead-modal appears.
// SC-SSR-7:  Dead-modal + auto-retry still fires when dead arrives after Stage 2.
// SC-SSR-8:  tearDownMse deferred during silent window; called when timer fires.
// SC-SSR-9:  tearDownMse called before setUpMse on streaming arrival.
// SC-SSR-10: No double-arm on repeated reconnecting (only 1 pending silent timer).

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

// The threshold value the implementation must define.
const THRESHOLD = 10_000;

describe('mse-client — staged silent reconnect timer gate (SC-SSR-1..10)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
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
  });

  // ── SC-SSR-1 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-2, REQ-SSR-3: no overlay shown before threshold.
  it('SC-SSR-1: reconnecting-overlay stays hidden before SILENT_RECOVERY_THRESHOLD_MS', async () => {
    const overlay = document.getElementById('reconnecting-overlay');
    expect(overlay).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance to just before the threshold.
    await vi.advanceTimersByTimeAsync(THRESHOLD - 1);
    await Promise.resolve();

    // Overlay must still be hidden.
    expect(overlay.hidden).toBe(true);
  });

  // ── SC-SSR-2 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-6: overlay revealed exactly at threshold.
  it('SC-SSR-2: reconnecting-overlay revealed at SILENT_RECOVERY_THRESHOLD_MS', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance to exactly the threshold.
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();

    // Overlay must now be visible.
    expect(overlay.hidden).toBe(false);
    // Text must include attempt counter.
    expect(overlay.textContent).toMatch(/1/);
    expect(overlay.textContent).toMatch(/3/);
  });

  // ── SC-SSR-3 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-5: silent success — streaming before threshold → overlay never shown.
  it('SC-SSR-3: streaming before threshold → overlay never shown, tearDown + setUp called', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance partway (before threshold).
    await vi.advanceTimersByTimeAsync(THRESHOLD - 2000);
    await Promise.resolve();

    // Dispatch streaming before timer fires.
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // Overlay must remain hidden (silent success — never revealed).
    expect(overlay.hidden).toBe(true);

    // Advance well past the original threshold to confirm the timer was cancelled.
    await vi.advanceTimersByTimeAsync(THRESHOLD + 5000);
    await Promise.resolve();

    // Still hidden — timer was cancelled before it could fire.
    expect(overlay.hidden).toBe(true);
  });

  // ── SC-SSR-4 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-2: total-elapsed singleton — subsequent reconnecting frames must NOT
  // re-arm the timer. Overlay reveals at THRESHOLD from the FIRST frame, not reset.
  it('SC-SSR-4: timer NOT re-armed on reconnecting{2/3} — total-elapsed singleton', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    // First reconnecting frame arms the timer at t=0.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance 2s — second reconnecting frame arrives.
    await vi.advanceTimersByTimeAsync(2_000);
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 2, max: 3 }).buffer);
    await Promise.resolve();

    // At t=THRESHOLD-1 (still counting from the FIRST frame) — must still be hidden.
    await vi.advanceTimersByTimeAsync(THRESHOLD - 2_000 - 1);
    await Promise.resolve();
    expect(overlay.hidden).toBe(true);

    // At t=THRESHOLD (from the first frame, total 10s elapsed) — must reveal.
    await vi.advanceTimersByTimeAsync(1);
    await Promise.resolve();
    await Promise.resolve();
    expect(overlay.hidden).toBe(false);
  });

  // ── SC-SSR-5 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-6: overlay shows most-recent attempt counter when timer fires.
  it('SC-SSR-5: overlay shows most-recent attempt/max when timer fires', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    // First frame: attempt 1/3.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Second frame at t=2000: attempt 2/3.
    await vi.advanceTimersByTimeAsync(2_000);
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 2, max: 3 }).buffer);
    await Promise.resolve();

    // Timer fires at THRESHOLD from first frame.
    await vi.advanceTimersByTimeAsync(THRESHOLD - 2_000);
    await Promise.resolve();
    await Promise.resolve();

    // Overlay text must reference attempt 2 (the most recent payload), not attempt 1.
    expect(overlay.hidden).toBe(false);
    expect(overlay.textContent).toMatch(/2/);
    expect(overlay.textContent).toMatch(/3/);
  });

  // ── SC-SSR-6 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-7: dead before threshold → timer cancelled, dead-modal shown, overlay hidden.
  it('SC-SSR-6: dead before threshold → timer cancelled, dead-modal shown, overlay stays hidden', async () => {
    const overlay = document.getElementById('reconnecting-overlay');
    const deadModal = document.getElementById('dead-modal');

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance 5s — timer still pending.
    await vi.advanceTimersByTimeAsync(5_000);

    // Dead arrives before threshold.
    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Overlay must remain hidden.
    expect(overlay.hidden).toBe(true);
    // Dead-modal must be visible.
    expect(deadModal.hidden).toBe(false);

    // Advance well past the original threshold — silent timer must have been cancelled.
    await vi.advanceTimersByTimeAsync(THRESHOLD + 5_000);
    await Promise.resolve();
    expect(overlay.hidden).toBe(true);
  });

  // ── SC-SSR-7 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-7: dead after Stage 2 (overlay visible) → overlay hidden, dead-modal shown,
  // 30s auto-retry armed.
  it('SC-SSR-7: dead after Stage 2 (overlay visible) → overlay hidden, dead-modal shown', async () => {
    const overlay = document.getElementById('reconnecting-overlay');
    const deadModal = document.getElementById('dead-modal');

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance past threshold → Stage 2 (overlay visible).
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();
    expect(overlay.hidden).toBe(false);

    // Dead arrives in Stage 2.
    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Overlay must be hidden again.
    expect(overlay.hidden).toBe(true);
    // Dead-modal must be visible.
    expect(deadModal.hidden).toBe(false);

    // 30s auto-retry must be armed: advance to just before it fires — no invoke yet.
    tauri.invoke.mockClear();
    await vi.advanceTimersByTimeAsync(29_999);
    await Promise.resolve();
    const retryCallsBefore = tauri.invoke.mock.calls.filter(c => c[0] === 'retry_session_stream');
    expect(retryCallsBefore.length).toBe(0);

    // Advance 1 more ms → auto-retry fires.
    await vi.advanceTimersByTimeAsync(1);
    await Promise.resolve();
    await Promise.resolve();
    const retryCallsAfter = tauri.invoke.mock.calls.filter(c => c[0] === 'retry_session_stream');
    expect(retryCallsAfter.length).toBe(1);
  });

  // ── SC-SSR-8 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-4: tearDownMse deferred — NOT called on reconnecting; called when timer fires.
  it('SC-SSR-8: tearDownMse NOT called on reconnecting; called when timer fires (Stage 2)', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Immediately after reconnecting: endOfStream must NOT have been called.
    expect(ms.endOfStream).not.toHaveBeenCalled();

    // Advance to threshold → timer fires → teardown happens.
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();

    // Now teardown must have been called exactly once.
    expect(ms.endOfStream).toHaveBeenCalledTimes(1);
    expect(ms.endOfStream).toHaveBeenCalledWith('decode');
  });

  // ── SC-SSR-9 ─────────────────────────────────────────────────────────────────
  // REQ-SSR-5: streaming before threshold → tearDownMse called exactly once before setUpMse.
  it('SC-SSR-9: streaming before threshold → tearDownMse called once before setUpMse', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // No teardown yet.
    expect(ms.endOfStream).not.toHaveBeenCalled();

    // Streaming arrives before threshold.
    await vi.advanceTimersByTimeAsync(THRESHOLD - 3000);
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // tearDown must have been called exactly once.
    expect(ms.endOfStream).toHaveBeenCalledTimes(1);
    expect(ms.endOfStream).toHaveBeenCalledWith('decode');

    // setUpMse must have been called (new MediaSource created by MockMediaSourceCtor).
    const newMs = MockMediaSourceCtor._lastInstance;
    expect(newMs).not.toBe(ms);
    expect(newMs).not.toBeNull();
  });

  // ── SC-SSR-10 ────────────────────────────────────────────────────────────────
  // REQ-SSR-2: only ONE pending silent-gate timer after multiple reconnecting frames.
  it('SC-SSR-10: only one pending silent-gate timer after multiple reconnecting frames', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Count pending timers after first reconnecting frame.
    // Note: the module import at t=0 may have armed a start_stream invoke timer;
    // we need to count only the silent-recovery timer. We check that a SECOND
    // reconnecting frame does NOT add another timer (count stays the same).
    const timerCountAfterFirst = vi.getTimerCount();
    expect(timerCountAfterFirst).toBeGreaterThanOrEqual(1);

    // Second reconnecting frame.
    await vi.advanceTimersByTimeAsync(2_000);
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 2, max: 3 }).buffer);
    await Promise.resolve();

    // Timer count must not have increased (no second timer created).
    const timerCountAfterSecond = vi.getTimerCount();
    expect(timerCountAfterSecond).toBe(timerCountAfterFirst);
  });

  // ── SC-SSR-11 ────────────────────────────────────────────────────────────────
  // D-SSR-6 sentinel: after the overlay is revealed (Stage 2, timer fires and
  // silentRecoveryTimerId is nulled), a subsequent reconnecting{n} frame must NOT
  // re-arm the silent-recovery timer. Also verifies that after the episode ends
  // (streaming → cancelSilentRecovery resets overlayRevealed), a fresh loss
  // episode CAN arm the timer and reveal the overlay again.
  //
  // RED against current code: current guard is only `silentRecoveryTimerId === null`
  // which is true after the timer fires → post-reveal reconnecting{n} re-arms.
  // GREEN requires the overlayRevealed sentinel (part 1) and its reset in
  // cancelSilentRecovery (part 2).
  it('SC-SSR-11: overlayRevealed sentinel blocks post-reveal re-arm; episode reset allows fresh arm (D-SSR-6)', async () => {
    const overlay = document.getElementById('reconnecting-overlay');

    // ── Part 1: post-reveal reconnecting must NOT re-arm ──────────────────────
    // Episode 1, attempt 1: arm the timer.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance to threshold: timer fires, overlay revealed, silentRecoveryTimerId = null.
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();
    expect(overlay.hidden).toBe(false); // Stage 2 confirmed

    // Record the timer count after Stage 2 reveal (the silent-recovery timer is gone).
    const timerCountAfterReveal = vi.getTimerCount();

    // Send a post-reveal reconnecting{2} frame. With the sentinel this must NOT
    // re-arm the timer; without the sentinel it WILL re-arm (current code → RED).
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 2, max: 3 }).buffer);
    await Promise.resolve();

    const timerCountAfterPostRevealReconnect = vi.getTimerCount();
    // Sentinel assertion: timer count must NOT increase after post-reveal reconnecting.
    expect(timerCountAfterPostRevealReconnect).toBe(timerCountAfterReveal);

    // Advance another full threshold to prove no second reveal/teardown fires.
    const msAfterReveal = MockMediaSourceCtor._lastInstance;
    const endOfStreamCallsAfterReveal = msAfterReveal.endOfStream.mock.calls.length;
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();
    expect(msAfterReveal.endOfStream.mock.calls.length).toBe(endOfStreamCallsAfterReveal);

    // ── Part 2: episode reset via streaming → new episode can arm ─────────────
    // End episode 1 via streaming (cancelSilentRecovery resets overlayRevealed).
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();
    // Overlay must be hidden again (streaming hides it).
    expect(overlay.hidden).toBe(true);

    // Episode 2, attempt 1: NEW reconnecting on the SAME channel.
    // With the sentinel reset, the timer must re-arm → overlay reveals at THRESHOLD.
    const timerCountBeforeEpisode2 = vi.getTimerCount();
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 2 }).buffer);
    await Promise.resolve();

    const timerCountAfterEpisode2Arm = vi.getTimerCount();
    // Timer must have been armed for episode 2 (count increased by 1).
    expect(timerCountAfterEpisode2Arm).toBe(timerCountBeforeEpisode2 + 1);

    // Advance to threshold: episode 2 overlay must reveal.
    await vi.advanceTimersByTimeAsync(THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();
    expect(overlay.hidden).toBe(false);
  });
});
