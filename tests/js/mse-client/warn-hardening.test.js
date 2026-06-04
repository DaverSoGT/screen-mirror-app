// warn-hardening.test.js — SC-W1-1, SC-W2-1, SC-W2-2
//
// W-1: mseState.active not reset on sourceclose/sourceended (stale-active edge).
//   When the sourceclose handler fires (applyInit registers it after a successful
//   SourceBuffer creation), mseState.active stays stale-true. The fix: reset
//   mseState.active = false in the sourceclose and sourceended handlers.
//
// W-2: applyInit failure paths drop silently — no console.error.
//   Permanent failures (null codec, unsupported codec) call setStatus() but do NOT
//   call console.error. The operator cannot distinguish a silent drop from normal
//   operation. Fix: add console.error to each permanent failure path so failures
//   are loud in the browser console.
//   No re-queue applies: all three failure paths in applyInit (null codec,
//   unsupported codec, addSourceBuffer throw) are permanent for the current init
//   segment. Re-queueing the same invalid init would fail again identically.
//
// SC-W1-1: sourceclose fires (no tearDownMse) → mseState.active reset to false
// SC-W2-1: applyInit null-codec path → console.error surfaced
// SC-W2-2: applyInit unsupported-codec path → console.error surfaced

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41, INIT_MISSING_AVCC } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

describe('mse-client — warn-hardening (SC-W1-1, SC-W2-1, SC-W2-2)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    MockMediaSourceCtor._deferOpenNext = false;
    MockMediaSourceCtor.isTypeSupported.mockReset();
    MockMediaSourceCtor.isTypeSupported.mockImplementation((type) =>
      /^video\/mp4/.test(type)
    );
    // Re-initialize export bag so the seam is available after vi.resetModules().
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    ch = tauri.lastChannel();

    // Prime: dispatch a valid FRAME_INIT so the initial MSE session is fully
    // active (mseState.active=true, sb created, sourceclose listener registered).
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    MockMediaSourceCtor._deferOpenNext = false;
    MockMediaSourceCtor.isTypeSupported.mockReset();
    MockMediaSourceCtor.isTypeSupported.mockImplementation((type) =>
      /^video\/mp4/.test(type)
    );
  });

  // ── SC-W1-1 ──────────────────────────────────────────────────────────────────
  // After applyInit runs (SourceBuffer created, sourceclose listener registered),
  // fire the sourceclose listener WITHOUT going through tearDownMse. With the fix,
  // mseState.active must be reset to false so any subsequent FRAME_INIT takes
  // Guard 2 (queued) instead of the happy path → addSourceBuffer throws → drop.
  //
  // The testable invariant is: mseState.active === false after sourceclose handler.
  //
  // RED today: the sourceclose handler only calls console.warn; active stays true.
  it('SC-W1-1: sourceclose handler resets mseState.active to false (defense-in-depth)', async () => {
    // After priming, the initial MS has active=true, sb set, and the sourceclose
    // listener registered (applyInit ran and called ms.addEventListener("sourceclose")).
    const ms1 = MockMediaSourceCtor._lastInstance;
    expect(ms1).not.toBeNull();

    // Confirm sourceclose listener was registered by applyInit.
    expect(ms1._listeners.sourceclose).toBeDefined();

    const exports = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    // Confirm active is true (normal live session).
    expect(exports.mseState.active).toBe(true);

    // Fire sourceclose WITHOUT tearDownMse (simulates browser-side MS close
    // without the normal streaming→tearDownMse state-machine path).
    ms1._listeners.sourceclose();

    // RED: active stays true (handler only logs; no reset).
    // GREEN: active is false (handler resets mseState.active = false).
    expect(exports.mseState.active).toBe(false);
  });

  // ── SC-W2-1 ──────────────────────────────────────────────────────────────────
  // applyInit with a null codec (init segment has no avcC box) must surface a
  // console.error so the operator sees a clear error-level signal in the console.
  // Currently the failure only calls setStatus() → console.log (INFO level), which
  // is easy to miss. The session is unrecoverable; operators need an ERROR signal.
  //
  // RED today: no console.error call on the null-codec path.
  it('SC-W2-1: applyInit with null codec (INIT_MISSING_AVCC) → console.error surfaced', async () => {
    // Dispatch streaming → tearDownMse + fresh setUpMse → new open MS.
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2.readyState).toBe('open');

    const errorSpy = vi.spyOn(console, 'error');

    // Dispatch a FRAME_INIT with no avcC box → deriveCodec returns null →
    // first failure branch in applyInit ("init segment missing avcC — cannot
    // derive codec").
    const badInitFrame = makeInitFrame(INIT_MISSING_AVCC);
    ch._dispatch(badInitFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // addSourceBuffer must NOT have been called (failure before that point).
    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    // Status element must reflect the failure.
    const statusEl = document.getElementById('status');
    expect(statusEl.textContent).toContain('missing avcC');

    // RED: console.error NOT called (only console.log via setStatus).
    // GREEN: console.error called; combined arguments mention avcC, codec, or init.
    const errorCalls = errorSpy.mock.calls.filter((call) =>
      call.some((arg) => typeof arg === 'string' && /avcC|codec|init/i.test(arg))
    );
    expect(errorCalls.length).toBeGreaterThanOrEqual(1);
  });

  // ── SC-W2-2 ──────────────────────────────────────────────────────────────────
  // applyInit with an unsupported codec (isTypeSupported returns false for the
  // derived codec) must surface a console.error. Same rationale as SC-W2-1.
  //
  // RED today: no console.error call on the unsupported-codec path.
  it('SC-W2-2: applyInit with unsupported codec (isTypeSupported=false) → console.error surfaced', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2.readyState).toBe('open');

    // Make isTypeSupported reject the derived codec (return false for all calls
    // from this point — the module-startup PROBE_CODEC check ran at import time,
    // so subsequent calls are from applyInit for the actual derived codec string).
    MockMediaSourceCtor.isTypeSupported.mockImplementation(() => false);

    const errorSpy = vi.spyOn(console, 'error');

    // Dispatch a valid FRAME_INIT (INIT_HIGH_41 has avcC so deriveCodec succeeds),
    // but isTypeSupported now returns false → second failure path in applyInit
    // ("FATAL: derived codec not supported: …").
    const initFrame2 = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame2.buffer);
    await Promise.resolve();
    await Promise.resolve();

    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    const statusEl = document.getElementById('status');
    expect(statusEl.textContent).toContain('not supported');

    // RED: console.error NOT called.
    // GREEN: console.error called; the combined arguments mention codec or support.
    const errorCalls = errorSpy.mock.calls.filter((call) =>
      call.some((arg) => typeof arg === 'string' && /codec|supported|FATAL/i.test(arg))
    );
    expect(errorCalls.length).toBeGreaterThanOrEqual(1);
  });
});
