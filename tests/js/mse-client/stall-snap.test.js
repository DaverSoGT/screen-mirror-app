// stall-snap.test.js — T-S7-1..17: stall-triggered early snap (Slice 7)
//
// Tests for the onVideoWaiting handler that fires on the video element's
// 'waiting' event and immediately snaps to bufEnd−LIVE_EDGE_STALL_SNAP_LEAD_SEC
// of the last buffered range, eliminating the 0.5–1.9 s heartbeat-wait freeze.
//
// Harness pattern mirrors live-edge.test.js: installDom, installTauriMock,
// MockMediaSourceCtor, init-prime, exports binding.
// Guard/path tests call exports.onVideoWaiting() directly; registration test
// dispatches new Event('waiting') on videoEl.
// Multi-range buffered uses a file-local inline helper { length, start, end }.
// Object.defineProperty overrides use configurable:true and are restored in afterEach.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

// File-local helper: build a fake TimeRanges-like object with N ranges.
// Usage: makeBufferedMulti([[s0,e0],[s1,e1],...])
function makeBufferedMulti(ranges) {
  return {
    length: ranges.length,
    start(i) { return ranges[i][0]; },
    end(i)   { return ranges[i][1]; },
  };
}

// File-local helper: single-range shorthand.
function makeBuffered(start, end) {
  return makeBufferedMulti([[start, end]]);
}

// ── Helpers for reading mse_log lines from tauri mock ─────────────────────────
// mseLog calls invoke("mse_log", { line }) — extract the `line` field.
function getMseLogLines(tauri) {
  return tauri.invoke.mock.calls
    .filter((c) => c[0] === 'mse_log')
    .map((c) => c[1].line);
}

describe('stall-snap — constants & exports (T-S7-15..17)', () => {
  let exports;

  beforeEach(async () => {
    installDom();
    const tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    exports = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // T-S7-15
  it('T-S7-15: LIVE_EDGE_STALL_SNAP_LEAD_SEC exported and === 0.3', () => {
    expect(exports.LIVE_EDGE_STALL_SNAP_LEAD_SEC).toBe(0.3);
  });

  // T-S7-16
  it('T-S7-16: LIVE_EDGE_STALL_MIN_CUSHION_SEC exported and === 0.1', () => {
    expect(exports.LIVE_EDGE_STALL_MIN_CUSHION_SEC).toBe(0.1);
  });

  // T-S7-17
  it('T-S7-17: onVideoWaiting exported and is a function', () => {
    expect(typeof exports.onVideoWaiting).toBe('function');
  });
});

describe('stall-snap — handler behavior (T-S7-1..14)', () => {
  let tauri;
  let exports;
  let videoEl;
  let sb;

  // Track per-test defineProperty overrides for afterEach restore.
  const _restoreFns = [];

  function overrideProperty(obj, prop, descriptor) {
    const originalDescriptor = Object.getOwnPropertyDescriptor(obj, prop);
    Object.defineProperty(obj, prop, { ...descriptor, configurable: true });
    _restoreFns.push(() => {
      if (originalDescriptor) {
        Object.defineProperty(obj, prop, originalDescriptor);
      } else {
        delete obj[prop];
      }
    });
  }

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

    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    exports = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    videoEl = document.getElementById('player');
    sb = MockMediaSourceCtor._lastInstance._sb;

    // Default good state for guard tests: not updating, no pending, not seeking.
    sb.updating = false;
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;
  });

  afterEach(() => {
    // Restore all per-test property overrides in reverse order.
    while (_restoreFns.length) _restoreFns.pop()();
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // ── T-S7-1: Registration — dispatchEvent('waiting') triggers snap ─────────
  it('T-S7-1: dispatchEvent(waiting) fires snap + log result=stall_snap', () => {
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;

    videoEl.dispatchEvent(new Event('waiting'));

    const lines = getMseLogLines(tauri);
    expect(lines.some((l) => l.includes('result=stall_snap'))).toBe(true);
  });

  // ── T-S7-2: Dominant backward replay-cushion case ─────────────────────────
  it('T-S7-2: dominant backward case — ct=bufEnd−0.020 → target=bufEnd−0.300; exact log', () => {
    const bufEnd = 10.030;
    const ct = bufEnd - 0.020; // = 10.010
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    const expectedTarget = bufEnd - 0.300; // = 9.730
    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);

    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget.toFixed(3));
    expect(snapLine).toContain('drift=' + (bufEnd - ct).toFixed(3));
  });

  // ── T-S7-3: Large drift — target = bufEnd−0.300 NOT −0.200 ───────────────
  it('T-S7-3: large drift (7 s) → target = bufEnd−0.300, not −0.200', () => {
    const bufEnd = 10.000;
    const ct = 3.000; // drift = 7
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    const expectedTarget = bufEnd - 0.300; // = 9.700
    // Must NOT be bufEnd−0.200 = 9.800
    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);
    expect(videoEl.currentTime).not.toBeCloseTo(bufEnd - 0.200, 5);

    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget.toFixed(3));
    expect(snapLine).toContain('drift=' + (bufEnd - ct).toFixed(3));
  });

  // ── T-S7-4: G2 — sb.updating=true → silent ────────────────────────────────
  it('T-S7-4: sb.updating=true → no snap, no log (G2)', () => {
    sb.updating = true;
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;
    const ctBefore = videoEl.currentTime;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
    const lines = getMseLogLines(tauri);
    expect(lines.some((l) => l.includes('mse_log') || l.includes('result=stall_snap'))).toBe(false);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);
  });

  // ── T-S7-5: G2 — pending queue non-empty → silent ─────────────────────────
  it('T-S7-5: pending.length > 0 → no snap, no log (G2)', () => {
    sb.updating = false;
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;
    const ctBefore = videoEl.currentTime;

    // Push a dummy entry into mseState.pending.
    exports.mseState.pending.push(new ArrayBuffer(4));

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);

    // Cleanup pending.
    exports.mseState.pending.length = 0;
  });

  // ── T-S7-6: G3 — seeking=true → silent ────────────────────────────────────
  it('T-S7-6: VIDEO_EL.seeking=true → no snap, no log (G3)', () => {
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;
    const ctBefore = videoEl.currentTime;

    overrideProperty(videoEl, 'seeking', { get: () => true });

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);
  });

  // ── T-S7-7: G1 — mseState.sb=null → silent ────────────────────────────────
  it('T-S7-7: mseState.sb=null → silent no-op, no throw (G1)', () => {
    const origSb = exports.mseState.sb;
    exports.mseState.sb = null;
    const ctBefore = videoEl.currentTime;

    expect(() => exports.onVideoWaiting()).not.toThrow();
    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);

    exports.mseState.sb = origSb;
  });

  // ── T-S7-8: G5 — buffered empty (length=0) → silent ──────────────────────
  it('T-S7-8: buffered.length=0 → no snap, no log (G5)', () => {
    sb.buffered = makeBufferedMulti([]);
    videoEl.currentTime = 10.016;
    const ctBefore = videoEl.currentTime;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);
  });

  // ── T-S7-9: G4 — buffered getter throws → swallowed ──────────────────────
  it('T-S7-9: sb.buffered getter throws → exception swallowed; no log; no uncaught (G4)', () => {
    const ctBefore = videoEl.currentTime;

    overrideProperty(sb, 'buffered', {
      get() { throw new DOMException('', 'InvalidStateError'); },
    });

    expect(() => exports.onVideoWaiting()).not.toThrow();
    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);
  });

  // ── T-S7-10: Multi-range — last-range anchor, forward gap-jump ────────────
  it('T-S7-10: ranges [0,0.15],[2.5,8.0]; ct=0.15 → target=7.700 (forward gap-jump)', () => {
    sb.buffered = makeBufferedMulti([[0, 0.15], [2.5, 8.0]]);
    videoEl.currentTime = 0.150;

    exports.onVideoWaiting();

    // lastStart=2.5, bufEnd=8.0, target=Math.max(2.5, 8.0−0.3)=7.700
    expect(videoEl.currentTime).toBeCloseTo(7.700, 5);

    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=0.150');
    expect(snapLine).toContain('to=7.700');
    expect(snapLine).toContain('drift=7.850');
  });

  // ── T-S7-11: Clamp to range start (narrow last range) ────────────────────
  it('T-S7-11: range [5.0,5.2]; ct=5.2 → target clamped to 5.000; cushion=0.200≥0.1 → snaps', () => {
    // lastStart=5.0, bufEnd=5.2, bufEnd−0.3=4.9 < lastStart=5.0 → clamp to 5.0
    // cushion = bufEnd−target = 5.2−5.0 = 0.200 ≥ 0.1 → passes G6
    sb.buffered = makeBuffered(5.0, 5.2);
    videoEl.currentTime = 5.2;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(5.000, 5);

    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
  });

  // ── T-S7-12: G6 — cushion guard, sliver range → silent ───────────────────
  it('T-S7-12: range [5.0,5.05]; ct=5.04 → cushion=0.05<0.1 → silent no-op (G6)', () => {
    // target = Math.max(5.0, 5.05−0.3) = Math.max(5.0, 4.75) = 5.0
    // cushion = 5.05−5.0 = 0.05 < 0.1 → G6 fires
    sb.buffered = makeBuffered(5.0, 5.05);
    videoEl.currentTime = 5.04;
    const ctBefore = videoEl.currentTime;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
    expect(tauri.invoke.mock.calls.filter(([cmd]) => cmd === 'mse_log').length).toBe(0);
  });

  // ── T-S7-13: currentTime setter throws → result=throw line from locals ─────
  it('T-S7-13: currentTime setter throws → result=throw from locals; no re-reads; no uncaught', () => {
    // Setup: ct=10.016, bufEnd=10.030, target=9.730, drift=0.014
    const bufEnd = 10.030;
    const ct = 10.016;
    const expectedTarget = (bufEnd - 0.300).toFixed(3); // 9.730
    const expectedDrift  = (bufEnd - ct).toFixed(3);    // 0.014

    sb.buffered = makeBuffered(0, bufEnd);

    // Override currentTime: getter returns ct, setter throws.
    overrideProperty(videoEl, 'currentTime', {
      get: () => ct,
      set() { throw new Error('DOM err'); },
    });

    expect(() => exports.onVideoWaiting()).not.toThrow();

    const lines = getMseLogLines(tauri);
    const snapLine  = lines.find((l) => l.includes('result=stall_snap'));
    const throwLine = lines.find((l) => l.includes('result=throw'));

    // First log: result=stall_snap
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget);
    expect(snapLine).toContain('drift=' + expectedDrift);

    // Second log: result=throw, same values from locals (no getter re-reads).
    expect(throwLine).toBeDefined();
    expect(throwLine).toContain('from=' + ct.toFixed(3));
    expect(throwLine).toContain('to=' + expectedTarget);
    expect(throwLine).toContain('drift=' + expectedDrift);
  });

  // ── T-S7-14: Idempotency — post-stall-snap seekToLiveEdge no-ops ──────────
  // After onVideoWaiting() snaps currentTime to bufEnd−0.3, the drift becomes
  // ~0.3 s which is ≤ LIVE_EDGE_MAX_DRIFT_SEC (0.5). seekToLiveEdge() must not
  // emit any additional seek line (heartbeat idempotency, NFR-S7-1-SC2).
  it('T-S7-14: post-stall-snap: seekToLiveEdge no-ops (drift 0.3 ≤ 0.5)', () => {
    const bufEnd = 10.330;
    sb.buffered = makeBuffered(0, bufEnd);
    // Set currentTime to a value that triggers the snap.
    videoEl.currentTime = bufEnd - 0.020; // 10.310

    // First: stall-snap fires.
    exports.onVideoWaiting();

    // After snap: currentTime should now be bufEnd−0.3 = 10.030.
    const snapTarget = bufEnd - 0.300;
    // Reflect the post-snap currentTime in the videoEl for seekToLiveEdge to read.
    // The mock videoEl currentTime was set by the handler via assignment.
    // At this point videoEl.currentTime === snapTarget.

    // Clear invoke mock calls so we can track only the seekToLiveEdge call.
    tauri.invoke.mockClear();

    // Now call seekToLiveEdge: drift = bufEnd − snapTarget = 0.3 ≤ 0.5 → no snap.
    exports.seekToLiveEdge();

    // No mse_log calls should have been made (no result=snap emitted).
    const lines = getMseLogLines(tauri);
    expect(lines.some((l) => l.includes('result=snap') && !l.includes('result=stall_snap'))).toBe(false);
  });
});
