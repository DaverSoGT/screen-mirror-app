// mse-teardown-setup.test.js — T10.1: tearDownMse / setUpMse helpers
//
// Spec §5.2 (receiver-side MSE swap):
//
// SC-T10-1: On kind="reconnecting" 0x02 event → MediaSource.endOfStream("decode") called
// SC-T10-2: On kind="reconnecting" 0x02 event → VIDEO_EL.src becomes null/empty after teardown
// SC-T10-3: On kind="streaming" 0x02 event (after teardown) + subsequent FRAME_INIT →
//            a fresh MediaSource is created (NOT the same instance as before teardown)
// SC-T10-4: On kind="dead" 0x02 event → endOfStream("decode") called (freeze last frame)
// SC-T10-5: tearDownMse must not throw even if called before any MSE session is active

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

// Build a 0x02 status frame: [0x02, ...UTF-8 JSON bytes]
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

describe('mse-client — tearDownMse / setUpMse (T10.1)', () => {
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

    // Prime with an init segment so the MSE session is fully active
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

  it('SC-T10-1: kind="reconnecting" → MediaSource.endOfStream("decode") is called', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    expect(ms.endOfStream).toHaveBeenCalledWith('decode');
  });

  it('SC-T10-2: kind="reconnecting" → VIDEO_EL.src is cleared after teardown', async () => {
    const videoEl = document.getElementById('player');
    const originalSrc = videoEl.src;
    expect(originalSrc).toBeTruthy(); // was set by setUpMse

    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // After teardown, src must be null or empty string
    expect(videoEl.src == null || videoEl.src === '' || videoEl.src === 'null').toBe(true);
  });

  it('SC-T10-3: kind="streaming" after teardown + FRAME_INIT → fresh MediaSource created', async () => {
    const firstMs = MockMediaSourceCtor._lastInstance;

    // Trigger reconnecting → teardown
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Trigger streaming → setUpMse
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // Deliver a fresh init segment → completes setUpMse
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    const secondMs = MockMediaSourceCtor._lastInstance;
    // A NEW MediaSource instance must have been created
    expect(secondMs).not.toBe(firstMs);
    expect(secondMs).not.toBeNull();
  });

  it('SC-T10-4: kind="dead" → endOfStream("decode") called (freeze last frame)', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    expect(ms.endOfStream).toHaveBeenCalledWith('decode');
  });

  it('SC-T10-5: kind="reconnecting" before any init segment does not throw', async () => {
    // Fresh page load — no MSE session active yet
    removeDom();
    installDom();
    tauri = installTauriMock();
    vi.resetModules();
    MockMediaSourceCtor._lastInstance = null;
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    const freshCh = tauri.lastChannel();

    expect(() => {
      freshCh._dispatch(
        makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer
      );
    }).not.toThrow();
  });
});
