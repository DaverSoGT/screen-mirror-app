// codec-passthrough.test.js — SC-S4-3, SC-S4-4
//
// SC-S4-3: derived codec is passed to ms.addSourceBuffer (NOT hardcoded).
// SC-S4-4: when init segment has no avcC, addSourceBuffer is NOT called.
//
// These tests exercise the full FRAME_INIT path via the Channel mock and
// require the beforeEach pattern: DOM → Tauri → MediaSource → fake timers →
// resetModules → dynamic import → flush microtask.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41, INIT_MISSING_AVCC } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

describe('mse-client — codec passthrough (SC-S4-3, SC-S4-4)', () => {
  let tauri;

  beforeEach(async () => {
    // 1. DOM first — module top-level reads document.getElementById
    installDom();
    // 2. Globals — Tauri + MediaSource
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    // 3. Test-export bag
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    // 4. Fake timers BEFORE import (R12: setInterval at line 331 must be fake)
    vi.useFakeTimers();
    // 5. Module reset + dynamic import
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // 6. Flush the sourceopen queueMicrotask so main() can advance past line 137
    //    and reach the start_stream await at line 304.
    await vi.advanceTimersByTimeAsync(0);
    // 7. Flush start_stream resolve (mockResolvedValue is a microtask)
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('SC-S4-3: dispatching FRAME_INIT with avcC calls addSourceBuffer with full MIME codec', async () => {
    const ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    // Dispatch init frame: discriminant 0x00 + INIT_HIGH_41 bytes
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);

    // Flush any queued microtasks
    await Promise.resolve();

    // addSourceBuffer must be called exactly once with the full MIME codec
    expect(ms.addSourceBuffer).toHaveBeenCalledTimes(1);
    expect(ms.addSourceBuffer).toHaveBeenCalledWith('video/mp4; codecs="avc1.640029"');

    // Must NOT be called with the old hardcoded baseline codec
    expect(ms.addSourceBuffer).not.toHaveBeenCalledWith('video/mp4; codecs="avc1.42E01E"');
  });

  it('SC-S4-4: dispatching FRAME_INIT without avcC does NOT call addSourceBuffer', async () => {
    const ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    // Dispatch init frame with no avcC
    const initFrame = makeInitFrame(INIT_MISSING_AVCC);
    ch._dispatch(initFrame.buffer);

    await Promise.resolve();

    // addSourceBuffer must NOT be called
    expect(ms.addSourceBuffer).not.toHaveBeenCalled();

    // Status must contain 'missing avcC'
    const statusEl = document.getElementById('status');
    expect(statusEl.textContent).toMatch(/missing avcC/i);
  });
});
