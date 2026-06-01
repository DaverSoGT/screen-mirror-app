// mse-teardown-setup.test.js — T10.1: tearDownMse / setUpMse helpers
//
// Spec §5.2 (receiver-side MSE swap):
//
// SC-T10-1 (REWRITTEN — SC-SSR-13): On kind="reconnecting" 0x02 event →
//            tearDownMse is NOT called immediately (deferred behavior per REQ-SSR-4);
//            tearDownMse IS called when kind="streaming" subsequently arrives.
// SC-T10-2 (REWRITTEN): VIDEO_EL.src remains non-empty during the silent window;
//            it is cleared only after streaming arrives (deferred teardown).
// SC-T10-3: On kind="streaming" 0x02 event (after deferred teardown) + subsequent FRAME_INIT →
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

  // SC-T10-1 REWRITE (SC-SSR-13, REQ-SSR-4, REQ-SSR-13):
  // tearDownMse must NOT be called immediately on reconnecting (deferred).
  // It MUST be called exactly once when streaming subsequently arrives.
  // This test is RED under the current code (which calls tearDownMse on reconnecting).
  it('SC-T10-1: kind="reconnecting" → tearDownMse NOT called immediately; called when streaming arrives', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    // Dispatch reconnecting — teardown must be deferred (NOT called yet).
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Under deferred behavior: endOfStream must NOT have been called yet.
    expect(ms.endOfStream).not.toHaveBeenCalled();

    // Now dispatch streaming — deferred teardown fires before setUpMse.
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // After streaming arrives, teardown (endOfStream) must have been called exactly once.
    expect(ms.endOfStream).toHaveBeenCalledTimes(1);
    expect(ms.endOfStream).toHaveBeenCalledWith('decode');
  });

  // SC-T10-2 REWRITE (REQ-SSR-4):
  // VIDEO_EL.src must remain set (non-empty) during the silent window.
  // After streaming arrives, deferred teardown fires: the MediaSource is
  // ended (endOfStream called) proving teardown was deferred, not immediate.
  // This test is RED under the current code (which calls tearDownMse on reconnecting,
  // clearing src and calling endOfStream immediately).
  it('SC-T10-2: kind="reconnecting" → VIDEO_EL.src stays set during silent window; teardown deferred to streaming', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    const videoEl = document.getElementById('player');
    const srcBeforeReconnect = videoEl.src;
    expect(srcBeforeReconnect).toBeTruthy(); // was set by setUpMse

    // Dispatch reconnecting — src must remain set, teardown deferred.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // During silent window: src must still be set (NOT cleared by tearDownMse yet).
    expect(videoEl.src).toBeTruthy();
    expect(videoEl.src).not.toBe('');
    expect(videoEl.src).not.toBe('null');
    // Deferred: endOfStream must NOT have been called yet.
    expect(ms.endOfStream).not.toHaveBeenCalled();

    // Now dispatch streaming — deferred teardown fires before setUpMse.
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // After streaming: teardown must have been called (endOfStream invoked).
    // This confirms teardown was deferred to the streaming path, not immediate.
    expect(ms.endOfStream).toHaveBeenCalledTimes(1);
    expect(ms.endOfStream).toHaveBeenCalledWith('decode');
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
