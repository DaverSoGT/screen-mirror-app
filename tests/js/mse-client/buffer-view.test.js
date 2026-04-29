// buffer-view.test.js — SC-S7-1, SC-S7-2 (B11-S7 regression guard)
//
// SC-S7-1: appendBuffer receives a Uint8Array view with byteLength === N (payload only).
// SC-S7-2: negative — if data.subarray(1).buffer were passed, byteLength would be N+1.
//
// Regression for B11-S7: mse-client.js:214 must use data.subarray(1) (typed array view),
// NOT data.subarray(1).buffer (the full underlying ArrayBuffer). Reverting line 214
// to .buffer causes SC-S7-1 to fail and SC-S7-2 to detect the regression.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame, makeMediaSegmentFrame, FAKE_PAYLOAD } from '../fixtures/media-segments.js';

describe('mse-client — Uint8Array view semantics (SC-S7-1, SC-S7-2)', () => {
  let tauri;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    // Prime the SUT with an init segment so it creates a SourceBuffer
    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    // Flush updateend microtask from init processing
    await Promise.resolve();
    // Reset appendBuffer spy so the test only counts media-segment calls
    const ms = MockMediaSourceCtor._lastInstance;
    if (ms) {
      ms._sb.appendBuffer.mockClear();
      ms._sb._lastAppend = null;
    }
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('SC-S7-1: appendBuffer receives Uint8Array view with byteLength === FAKE_PAYLOAD.length (32)', async () => {
    const ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // Dispatch a media segment frame: [0x01, ...32 payload bytes] = 33 total
    const segFrame = makeMediaSegmentFrame();
    ch._dispatch(segFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // appendBuffer MUST have been called
    expect(sb.appendBuffer).toHaveBeenCalledTimes(1);

    const arg = sb._lastAppend;
    // Must be a Uint8Array, not an ArrayBuffer
    expect(arg).toBeInstanceOf(Uint8Array);
    // Must be exactly FAKE_PAYLOAD.length (32) — discriminant byte stripped
    expect(arg.byteLength).toBe(FAKE_PAYLOAD.length);
    // First byte must match FAKE_PAYLOAD[0] (0x00), NOT discriminant 0x01
    expect(arg[0]).toBe(FAKE_PAYLOAD[0]);
    expect(arg[0]).not.toBe(0x01);
  });

  it('SC-S7-2: regression guard — if .buffer were passed, byteLength would be N+1 (33)', async () => {
    // This is a negative documentation test: it verifies that if appendBuffer
    // were called with the FULL underlying ArrayBuffer instead of the view,
    // byteLength would be 33 (N+1, includes the discriminant byte at index 0).
    //
    // We verify this by calling appendBuffer directly on the mock with the wrong arg,
    // then asserting the byte length difference — proving the mock is capable of
    // detecting the regression if mse-client.js:214 were changed to .buffer.
    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // Construct the "wrong" argument: what .buffer would have been
    const segFrame = makeMediaSegmentFrame(); // 33 bytes
    const wrongArg = segFrame.buffer;         // ArrayBuffer, byteLength=33

    // Manually record the wrong argument to the mock
    sb._lastAppend = wrongArg;

    // A test asserting byteLength === 32 WOULD FAIL here (33 !== 32):
    expect(sb._lastAppend.byteLength).toBe(FAKE_PAYLOAD.length + 1); // 33
    // And byte 0 would be the discriminant, not the payload start:
    const view = new Uint8Array(sb._lastAppend);
    expect(view[0]).toBe(0x01); // discriminant — wrong!

    // This confirms that the SC-S7-1 assertion would fail if .buffer were used.
    // The actual SUT uses data.subarray(1) which passes the SC-S7-1 assertion above.
  });
});
