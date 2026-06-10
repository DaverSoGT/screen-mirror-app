// honest-reconnect-modal.test.js — SC-WD-S10 (REQ-WD-10), receiver leg
//
// CAP-2-v3: during the bounded retry window the status modal MUST NOT render a
// misleading "attempt X/N" counter. The transport can keep retrying for ~60s
// (issue #62) and the frontend cannot distinguish the supervisor's real retry
// from the post-watchdog wait, so the false "/3" denominator is removed in favour
// of an honest "still trying / waiting for the other device" message.
//
// This first describe asserts ONLY the inline status string (setStatus → #status),
// which is the `reconnecting` case in dist/mse-client.js (~line 457). The deferred
// Stage-2 silent-recovery overlay (revealReconnectingOverlay) is now ALSO count-free
// (CAP-2-v3 FIX-1) — it renders the same honest waiting copy with no "/N". The 2nd
// describe block below (and staged-reconnect.test.js) assert that overlay directly.
//
// RED today: the status string renders "Reconnecting (attempt 1/3)..." → contains "/3".

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
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

describe('mse-client — honest reconnecting modal (SC-WD-S10 / REQ-WD-10)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await Promise.resolve();
    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
  });

  it('SC-WD-S10: reconnecting status does NOT render "/N" and shows an honest waiting message', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    const status = document.getElementById('status');
    // The misleading "/3" (and any "/" + max) MUST be gone.
    expect(status.textContent).not.toContain('/3');
    expect(status.textContent).not.toMatch(/\/\s*\d/);
    // An honest, human-readable waiting message MUST be present.
    expect(status.textContent.toLowerCase()).toContain('waiting for the other device');
  });

  it('SC-WD-S10: a later reconnecting frame (attempt 2/3) still shows no "/N"', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 2, max: 3 }).buffer);
    await Promise.resolve();

    const status = document.getElementById('status');
    expect(status.textContent).not.toContain('/3');
    expect(status.textContent).not.toMatch(/\/\s*\d/);
  });
});

// CAP-2-v3 FIX-1 (R-F extension to the Stage-2 overlay): the prominent reconnecting
// OVERLAY (revealReconnectingOverlay) is the surface the user actually stares at during
// the absent-peer wait, yet it still rendered "Reconnecting (attempt 1/3)..." — the exact
// misleading "/N" the user decided (R-F) to remove. The overlay copy must match the honest
// count-free status line. This asserts ONLY the overlay text; the silent-recovery timer,
// deferred teardown, and terminal dead-modal rendering are untouched (and stay covered by
// staged-reconnect.test.js).
//
// RED today: the overlay renders "Reconnecting (attempt 1/3)..." → contains "/3".
const OVERLAY_THRESHOLD = 10_000; // SILENT_RECOVERY_THRESHOLD_MS

describe('mse-client — honest reconnecting OVERLAY (SC-WD-S10 / FIX-1, Stage-2)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    // Prime an init segment so the MSE session is active before the overlay timer fires.
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
  });

  it('SC-WD-S10: revealed overlay does NOT render "/N" and shows the honest waiting copy', async () => {
    const overlay = document.getElementById('reconnecting-overlay');
    expect(overlay).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    // Advance to the threshold → Stage 2 → overlay revealed.
    await vi.advanceTimersByTimeAsync(OVERLAY_THRESHOLD);
    await Promise.resolve();
    await Promise.resolve();

    expect(overlay.hidden).toBe(false);
    // The misleading "/3" (and any "/" + max) MUST be gone from the overlay.
    expect(overlay.textContent).not.toContain('/3');
    expect(overlay.textContent).not.toMatch(/\/\s*\d/);
    // The overlay MUST carry the honest count-free waiting message.
    expect(overlay.textContent.toLowerCase()).toContain('waiting for the other device');
  });
});
