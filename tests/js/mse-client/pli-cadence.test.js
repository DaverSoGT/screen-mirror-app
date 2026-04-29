// pli-cadence.test.js — SC-S9-1, SC-S9-2, SC-S10-1 (B11-S9/S10 regression guard)
//
// SC-S9-1: 2 total attach_stream calls at 2000 ms (1 immediate + 1 cadence tick)
// SC-S9-2: 6 total attach_stream calls at 10000 ms (1 + 5 ticks)
// SC-S10-1: 4 total calls at 6000 ms with NO media segments dispatched
//
// Regression for B11-S9/S10: mse-client.js:331 must have a permanent
// setInterval(FIRE_PLI, 2000). Removing or gating it by initReceived → fails.
//
// CRITICAL (R12): vi.useFakeTimers() MUST be called BEFORE await import(...)
// because main() fires setInterval at line 331 synchronously within the first
// await that resolves (the sourceopen microtask). If fake timers are activated
// AFTER import, setInterval escapes to real timers and count assertions fail.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';

describe('mse-client — PLI cadence permanent (SC-S9-1, SC-S9-2, SC-S10-1)', () => {
  let tauri;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    // R12: fake timers BEFORE import — the setInterval at line 331 must be
    // captured by fake timers. If done after import, interval runs on real timers.
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // Flush sourceopen microtask so main() runs past line 137
    await vi.advanceTimersByTimeAsync(0);
    // Flush start_stream resolve (microtask — NOT timer)
    await Promise.resolve();
    // At this point: FIRE_PLI() immediate call has fired (count=1),
    // and setInterval(FIRE_PLI, 2000) is registered under fake timers.
  });

  afterEach(() => {
    // SC-SETUP-3: restore real timers so intervals don't bleed into next test
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // Helper to count attach_stream calls
  function attachStreamCount() {
    return tauri.invoke.mock.calls.filter((c) => c[0] === 'attach_stream').length;
  }

  it('SC-S9-1: 2 total attach_stream calls at 2000 ms (1 immediate + 1 tick)', async () => {
    // At this point: 1 immediate call already fired
    expect(attachStreamCount()).toBe(1);

    // Advance 2000 ms → 1 cadence tick fires FIRE_PLI async → +1
    await vi.advanceTimersByTimeAsync(2000);
    // Flush the microtask from the async FIRE_PLI arrow function
    await Promise.resolve();

    expect(attachStreamCount()).toBe(2);
  });

  it('SC-S9-2: 6 total attach_stream calls at 10000 ms (1 + 5 ticks)', async () => {
    // At this point: 1 immediate call
    expect(attachStreamCount()).toBe(1);

    // Advance 10000 ms → 5 cadence ticks (at 2000, 4000, 6000, 8000, 10000 ms)
    await vi.advanceTimersByTimeAsync(10000);
    await Promise.resolve();

    expect(attachStreamCount()).toBe(6);
  });

  it('SC-S10-1: PLI fires regardless of media segments — 4 calls at 6000 ms with no segments', async () => {
    // No media segments dispatched — cadence must still fire
    expect(attachStreamCount()).toBe(1);

    // Advance 6000 ms → 3 cadence ticks (at 2000, 4000, 6000 ms)
    await vi.advanceTimersByTimeAsync(6000);
    await Promise.resolve();

    // 1 immediate + 3 ticks = 4 total
    expect(attachStreamCount()).toBe(4);
  });
});
