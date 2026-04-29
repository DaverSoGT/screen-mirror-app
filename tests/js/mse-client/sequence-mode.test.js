// sequence-mode.test.js — SC-S12-1, SC-S12-2 (B11-S12 regression guard)
//
// SC-S12-1: sb.mode is 'sequence' BEFORE the first appendBuffer call.
// SC-S12-2: removing sb.mode = 'sequence' at mse-client.js:263 → test fails.
//
// Regression for B11-S12: SourceBuffer default mode is 'segments' (MSE spec).
// mse-client.js:263 must set sb.mode = 'sequence' after addSourceBuffer to
// ensure live capture playback stays continuous across capture-rate variability.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { makeSourceBuffer, MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

describe('mse-client — SourceBuffer mode=sequence (SC-S12-1, SC-S12-2)', () => {
  let tauri;
  // Capture the mode value at the time appendBuffer is first called
  let modeAtFirstAppend;

  beforeEach(async () => {
    modeAtFirstAppend = undefined;

    installDom();
    tauri = installTauriMock();

    // Use a custom MediaSource factory that instruments the SourceBuffer
    // to capture sb.mode at the exact moment appendBuffer is first called.
    const sb = makeSourceBuffer();
    const originalAppendBuffer = sb.appendBuffer;
    sb.appendBuffer = vi.fn((bytes) => {
      // Capture mode at call time (not after)
      if (modeAtFirstAppend === undefined) {
        modeAtFirstAppend = sb.mode;
      }
      return originalAppendBuffer.call(sb, bytes);
    });
    sb._lastAppend = null;

    // Build a custom MockMediaSource that returns our instrumented sb
    const customMs = {
      readyState: 'closed',
      addSourceBuffer: vi.fn((codec) => {
        customMs._lastCodec = codec;
        customMs.readyState = 'open';
        return sb;
      }),
      endOfStream: vi.fn(),
      addEventListener: vi.fn((ev, cb) => {
        if (ev === 'sourceopen') queueMicrotask(cb);
      }),
      _sb: sb,
      _lastCodec: null,
    };

    // Custom ctor returns our instrumented ms
    function CustomMSCtor() { return customMs; }
    CustomMSCtor.isTypeSupported = vi.fn(() => true);
    CustomMSCtor._lastInstance = customMs;

    vi.stubGlobal('MediaSource', CustomMSCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    // Dispatch an init segment to trigger addSourceBuffer + sb.mode assignment
    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    // Flush: init processing → addSourceBuffer → sb.mode = 'sequence' → enqueue → appendBuffer
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('SC-S12-1: sb.mode is "sequence" at the time appendBuffer is first called', () => {
    // modeAtFirstAppend is captured inside the instrumented appendBuffer spy
    // This verifies the assignment at mse-client.js:263 happens BEFORE appendBuffer
    expect(modeAtFirstAppend).toBe('sequence');
  });

  it('SC-S12-2: regression — default mode is "segments" (MSE spec default)', () => {
    // This test documents that without the assignment at mse-client.js:263,
    // mode would remain 'segments'. If modeAtFirstAppend were 'segments',
    // the SC-S12-1 test above would fail — proving the assignment is needed.
    // Here we verify the mock default starts at 'segments' before assignment.
    const freshSb = makeSourceBuffer();
    expect(freshSb.mode).toBe('segments');
  });
});
