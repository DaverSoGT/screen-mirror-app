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

  // T-S7-15 (updated S10: lead raised to 0.45)
  it('T-S7-15: LIVE_EDGE_STALL_SNAP_LEAD_SEC exported and === 0.45', () => {
    expect(exports.LIVE_EDGE_STALL_SNAP_LEAD_SEC).toBe(0.45);
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
    sb.buffered = makeBuffered(0, 10.320);
    videoEl.currentTime = 10.016;

    videoEl.dispatchEvent(new Event('waiting'));

    const lines = getMseLogLines(tauri);
    // S7-4-SC1: mseLog called exactly once (spec "exactly once")
    expect(lines.length).toBe(1);
    // Gate B threshold update: drift must stay above 0.300 to allow recovery.
    // bufEnd=10.320, ct=10.016, target=10.320-0.450=9.870, drift=0.304.
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=10.016');
    expect(snapLine).toContain('to=9.870');
    expect(snapLine).toContain('drift=0.304');
  });

  // ── T-S7-2: Backward replay-cushion with real drift (S10: lead 0.45) ───────
  it('T-S7-2: backward case with drift >= replay minimum → target=bufEnd−0.450; exact log', () => {
    const bufEnd = 10.320;
    const ct = 10.016;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    const expectedTarget = bufEnd - 0.450; // = 9.850
    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);

    const lines = getMseLogLines(tauri);
    // S7-4-SC1: mseLog called exactly once (spec "exactly once")
    expect(lines.length).toBe(1);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget.toFixed(3));
    expect(snapLine).toContain('drift=' + (bufEnd - ct).toFixed(3));
  });

  // ── T-S7-3: Large drift — target = bufEnd−0.450 NOT −0.200 (S10: lead 0.45) ─
  it('T-S7-3: large drift (7 s) → target = bufEnd−0.450, not −0.200', () => {
    const bufEnd = 10.000;
    const ct = 3.000; // drift = 7
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    const expectedTarget = bufEnd - 0.450; // = 9.550
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

  // ── T-S7-3b: Live-edge waiting must not replay old frames ──────────────────
  it('T-S7-3b: live-edge waiting with backward target → no stall_snap replay', () => {
    const bufEnd = 10.030;
    const ct = bufEnd - 0.020;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;
    overrideProperty(videoEl, 'readyState', { get: () => 2 });

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(ct, 5);
    const lines = getMseLogLines(tauri);
    expect(lines.filter((l) => l.includes('result=stall_snap')).length).toBe(0);
  });

  it('T-S7-3b-boundary: drift exactly 0.300s replay minimum → no stall_snap replay', () => {
    const bufEnd = 10.300;
    const ct = 10.000;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;
    overrideProperty(videoEl, 'readyState', { get: () => 2 });

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(ct, 5);
    expect(exports.getSuppressedGuardCount()).toBe(1);
    const lines = getMseLogLines(tauri);
    expect(lines.filter((l) => l.includes('result=stall_snap')).length).toBe(0);
  });

  it('T-S7-3b-rebound-rs: low-drift backward waiting stays suppressed even if readyState rebounds above 2', () => {
    const bufEnd = 10.300;
    const ct = 10.000;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;
    overrideProperty(videoEl, 'readyState', { get: () => 3 });

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(ct, 5);
    expect(exports.getSuppressedGuardCount()).toBe(1);
    expect(exports.getHardStarveStreak()).toBe(0);
    const lines = getMseLogLines(tauri);
    expect(lines.filter((l) => l.includes('result=stall_snap')).length).toBe(0);
  });

  it('T-S7-3b-past-edge: currentTime past bufEnd → backward stall_snap can recover', () => {
    const bufEnd = 10.030;
    const ct = 10.050;
    const expectedTarget = bufEnd - 0.450;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;
    overrideProperty(videoEl, 'readyState', { get: () => 2 });

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);
    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=10.050');
    expect(snapLine).toContain('to=9.580');
    expect(snapLine).toContain('drift=-0.020');
  });

  it('T-S7-3b-noop-tolerance: near-no-op delta at 0.010s suppresses stall_snap', () => {
    const bufEnd = 10.000;
    const expectedTarget = bufEnd - 0.450;
    const ct = expectedTarget - 0.010;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(ct, 5);
    expect(exports.getSuppressedGuardCount()).toBe(1);
    expect(getMseLogLines(tauri).filter((l) => l.includes('result=stall_snap')).length).toBe(0);
  });

  it('T-S7-3b-noop-tolerance-over: near-no-op delta above 0.010s still executes stall_snap', () => {
    const bufEnd = 10.000;
    const expectedTarget = bufEnd - 0.450;
    const ct = expectedTarget - 0.011;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);
    expect(exports.getSuppressedGuardCount()).toBe(0);
    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget.toFixed(3));
    expect(snapLine).toContain('drift=' + (bufEnd - ct).toFixed(3));
  });

  it('T-S7-3b-noop-tolerance-over-ahead: currentTime 0.011s above target still executes stall_snap', () => {
    const bufEnd = 10.000;
    const expectedTarget = bufEnd - 0.450;
    const ct = expectedTarget + 0.011;
    sb.buffered = makeBuffered(0, bufEnd);
    videoEl.currentTime = ct;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);
    expect(exports.getSuppressedGuardCount()).toBe(0);
    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget.toFixed(3));
    expect(snapLine).toContain('drift=' + (bufEnd - ct).toFixed(3));
  });

  // ── T-S7-3c: Replay guard must not bypass starvation bookkeeping ───────────
  it('T-S7-3c: near-edge suppression increments guards/streak, then 0.301s drift still recovers', () => {
    overrideProperty(videoEl, 'readyState', { get: () => 2 });

    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.010;
    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(10.010, 5);
    expect(exports.getSuppressedGuardCount()).toBe(1);
    expect(exports.getHardStarveStreak()).toBe(1);
    expect(getMseLogLines(tauri).filter((l) => l.includes('result=stall_snap')).length).toBe(0);

    tauri_clearInvoke(tauri);
    sb.buffered = makeBuffered(0, 10.301);
    videoEl.currentTime = 10.000;
    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBeCloseTo(10.301 - 0.450, 5);
    expect(exports.getSuppressedGuardCount()).toBe(1);
    expect(exports.getHardStarveStreak()).toBe(2);
    const lines = getMseLogLines(tauri);
    expect(lines.filter((l) => l.includes('result=stall_snap')).length).toBe(1);
  });

  // ── T-S7-4: G2 — sb.updating=true → silent ────────────────────────────────
  it('T-S7-4: sb.updating=true → no snap, no log (G2)', () => {
    sb.updating = true;
    sb.buffered = makeBuffered(0, 10.030);
    videoEl.currentTime = 10.016;
    const ctBefore = videoEl.currentTime;

    exports.onVideoWaiting();

    expect(videoEl.currentTime).toBe(ctBefore);
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

  // ── T-S7-10: Multi-range — last-range anchor, forward gap-jump (S10: lead 0.45)
  it('T-S7-10: ranges [0,0.15],[2.5,8.0]; ct=0.15 → target=7.550 (forward gap-jump, S10 lead 0.45)', () => {
    sb.buffered = makeBufferedMulti([[0, 0.15], [2.5, 8.0]]);
    videoEl.currentTime = 0.150;

    exports.onVideoWaiting();

    // lastStart=2.5, bufEnd=8.0, target=Math.max(2.5, 8.0−0.450)=7.550
    expect(videoEl.currentTime).toBeCloseTo(7.550, 5);

    const lines = getMseLogLines(tauri);
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=0.150');
    expect(snapLine).toContain('to=7.550');
    expect(snapLine).toContain('drift=7.850');
  });

  // ── T-S7-11: Sliver-only range — Slice 9 no-hole clamp returns null → silent no-op
  // UPDATED Slice 9: range [5.0,5.2] is 200ms < SNAP_SLIVER_MIN_SEC(0.3s) — a sliver.
  // clampSnapTarget returns null (no substantial range to land in) → onVideoWaiting
  // silently returns. Old behavior (Math.max clamp to 5.0) is superseded by Slice 9
  // no-hole guarantee (D-PPT9-B, SNAP_SLIVER_MIN_SEC=0.3). No seek, no log.
  it('T-S7-11: range [5.0,5.2] is a sliver (0.2s < 0.3s) → clampSnapTarget returns null → silent no-op (Slice 9)', () => {
    sb.buffered = makeBuffered(5.0, 5.2);
    videoEl.currentTime = 5.2;
    const ctBefore = videoEl.currentTime;

    exports.onVideoWaiting();

    // Slice 9: sliver range returns null from clamp → no seek
    expect(videoEl.currentTime).toBe(ctBefore);
    const lines = getMseLogLines(tauri);
    expect(lines.filter(l => l.includes('result=stall_snap')).length).toBe(0);
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
    // Setup: ct=10.016, bufEnd=10.320, target=10.320-0.450=9.870, drift=0.304
    const bufEnd = 10.320;
    const ct = 10.016;
    const expectedTarget = (bufEnd - 0.450).toFixed(3); // 9.870
    const expectedDrift  = (bufEnd - ct).toFixed(3);    // 0.304

    sb.buffered = makeBuffered(0, bufEnd);

    // W2: getter returns ct (10.016) on FIRST read only; sentinel (99.999) on any
    // subsequent read. This kills mutants that re-read VIDEO_EL.currentTime inside
    // the catch block — they would capture 99.999 and produce wrong from= values.
    let getterCallCount = 0;
    overrideProperty(videoEl, 'currentTime', {
      get() {
        getterCallCount += 1;
        return getterCallCount === 1 ? ct : 99.999;
      },
      set() { throw new Error('DOM err'); },
    });

    expect(() => exports.onVideoWaiting()).not.toThrow();

    const lines = getMseLogLines(tauri);

    // W1: partition lines by result= value (order-robust). Each result MUST
    // appear EXACTLY once — guards against mock-call pollution/duplication.
    const snapLines  = lines.filter((l) => l.includes('result=stall_snap'));
    const throwLines = lines.filter((l) => l.includes('result=throw'));
    expect(snapLines.length).toBe(1);
    expect(throwLines.length).toBe(1);
    const snapLine  = snapLines[0];
    const throwLine = throwLines[0];

    // W1: assert RELATIVE ORDER — stall_snap MUST come before throw.
    // A mutant that moves the mseLog call after the currentTime assignment
    // emits [throw, stall_snap] instead of [stall_snap, throw] — killed here.
    expect(lines.indexOf(snapLine)).toBeLessThan(lines.indexOf(throwLine));

    // Exact-value field checks on the stall_snap line.
    expect(snapLine).toContain('from=' + ct.toFixed(3));
    expect(snapLine).toContain('to=' + expectedTarget);
    expect(snapLine).toContain('drift=' + expectedDrift);

    // Exact-value field checks on the throw line.
    // Values MUST equal the first-read ct (10.016), not the sentinel (99.999).
    // This kills mutants that re-read VIDEO_EL.currentTime in the catch block.
    expect(throwLine).toContain('from=' + ct.toFixed(3));
    expect(throwLine).toContain('to=' + expectedTarget);
    expect(throwLine).toContain('drift=' + expectedDrift);
  });

  // ── T-S7-14: Idempotency — post-stall-snap seekToLiveEdge no-ops ──────────
  // After onVideoWaiting() snaps currentTime to bufEnd−0.45 (S10 lead), drift becomes
  // ~0.45 s which is ≤ LIVE_EDGE_MAX_DRIFT_SEC (0.5). seekToLiveEdge() must not
  // emit any additional seek line (heartbeat idempotency, NFR-S7-1-SC2).
  it('T-S7-14: post-stall-snap: seekToLiveEdge no-ops (drift 0.45 ≤ 0.5)', () => {
    const bufEnd = 10.330;
    sb.buffered = makeBuffered(0, bufEnd);
    // Set currentTime to a value that triggers the snap.
    videoEl.currentTime = bufEnd - 0.020; // 10.310

    // First: stall-snap fires.
    exports.onVideoWaiting();

    // After snap: currentTime should now be bufEnd−0.45 = 9.880.
    const snapTarget = bufEnd - 0.450;
    // Reflect the post-snap currentTime in the videoEl for seekToLiveEdge to read.
    // The mock videoEl currentTime was set by the handler via assignment.
    // At this point videoEl.currentTime === snapTarget.

    // Clear invoke mock calls so we can track only the seekToLiveEdge call.
    tauri.invoke.mockClear();

    // Now call seekToLiveEdge: drift = bufEnd − snapTarget = 0.45 ≤ 0.5 → no snap.
    exports.seekToLiveEdge();

    // No mse_log calls should have been made (no result=snap emitted).
    const lines = getMseLogLines(tauri);
    expect(lines.some((l) => l.includes('result=snap') && !l.includes('result=stall_snap'))).toBe(false);
  });
});

// ── Slice 8: Snap-Storm Debounce + Effectiveness Guard (T-S8-1..29) ───────────
//
// Harness reuses the Slice-7 setup pattern with one critical addition:
//   vi.useFakeTimers() does NOT advance performance.now() in vitest 2.1 + happy-dom.
//   Instead, tests use vi.spyOn(performance, 'now') via the harness perfNow helper.
//
// Key Slice-8 helpers:
//   h.perfNow(ms)   — set the value that performance.now() will return (spy-based)
//   exports.setLastSnapState({lastSnapAtMs, lastSnapCt, lastSnapBufEnd})
//     — seeds the module-level debounce / effectiveness state
//   exports.getLastSnapState()
//     — reads back the current module-level state (for write-rule assertions)
//   exports.getSuppressedDebounceCount()  — getter fn, reads live module-scope let
//   exports.getSuppressedGuardCount()     — getter fn, reads live module-scope let
//   exports.LIVE_EDGE_STALL_SNAP_DEBOUNCE_MS — must === 300
//   exports.ADV_EPS                          — must === 1e-3
//   exports.tearDownMse                      — for session-cumulative lifetime test
//
// Partition-by-discriminant rule (Cycle-7 lesson): never hard-index mock calls;
// filter by 'event=tick' or 'result=stall_snap' discriminants and assert counts/
// regex-match on the filtered subset.

// ── Shared harness factory for Slice-8 describe blocks ─────────────────────────
// Returns { tauri, exports, videoEl, sb, _restoreFns, overrideProperty, perfNow }
// perfNow(ms) — sets the value performance.now() will return (spy controlled).
async function makeS8Harness() {
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

  const exports = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  const videoEl = document.getElementById('player');
  const sb = MockMediaSourceCtor._lastInstance._sb;

  // Default good state: no pending, not updating, not seeking, healthy buffer.
  sb.updating = false;
  sb.buffered = makeBuffered(0, 10.030);
  videoEl.currentTime = 10.016;
  // readyState is getter-only in happy-dom; override via defineProperty.
  // Default = 4 (HAVE_ENOUGH_DATA) so N2 debounce is NOT bypassed by the
  // hardStarve escape hatch (rs <= 1). Tests that need rs<=1 use overrideProperty.
  const _readyStateOrig = Object.getOwnPropertyDescriptor(videoEl, 'readyState');
  Object.defineProperty(videoEl, 'readyState', { value: 4, configurable: true });

  // performance.now() spy: vi.advanceTimersByTime does NOT advance performance.now()
  // in vitest 2.1 + happy-dom. Use a spy with a controlled value instead.
  let _fakeNow = 0;
  const _perfSpy = vi.spyOn(performance, 'now').mockImplementation(() => _fakeNow);
  function perfNow(ms) { _fakeNow = ms; }

  const _restoreFns = [
    () => _perfSpy.mockRestore(),
    () => {
      if (_readyStateOrig) {
        Object.defineProperty(videoEl, 'readyState', _readyStateOrig);
      }
    },
  ];
  function overrideProperty(obj, prop, descriptor) {
    const orig = Object.getOwnPropertyDescriptor(obj, prop);
    Object.defineProperty(obj, prop, { ...descriptor, configurable: true });
    _restoreFns.push(() => {
      if (orig) {
        Object.defineProperty(obj, prop, orig);
      } else {
        delete obj[prop];
      }
    });
  }

  return { tauri, exports, videoEl, sb, _restoreFns, overrideProperty, perfNow };
}

function teardownS8Harness(h) {
  while (h._restoreFns.length) h._restoreFns.pop()();
  vi.useRealTimers();
  removeDom();
  resetTauriMock();
  delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
}

// Helper: filter mse_log calls whose payload contains 'event=tick'
function getTickLines(tauri) {
  return tauri.invoke.mock.calls
    .filter((c) => c[0] === 'mse_log' && c[1].line.includes('event=tick'))
    .map((c) => c[1].line);
}

// ── T-S8 describe 1: constants & exports ─────────────────────────────────────
describe('stall-snap — Slice 8 constants & exports (T-S8-1, T-S8-29)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-1: LIVE_EDGE_STALL_SNAP_DEBOUNCE_MS exported === 300; getter fns present and return 0 (S8-1-SC1, S8-8-SC1)
  it('T-S8-1: LIVE_EDGE_STALL_SNAP_DEBOUNCE_MS===300; getter fns present and return 0 on fresh load', () => {
    expect(h.exports.LIVE_EDGE_STALL_SNAP_DEBOUNCE_MS).toBe(300);
    expect(typeof h.exports.getSuppressedDebounceCount).toBe('function');
    expect(typeof h.exports.getSuppressedGuardCount).toBe('function');
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
  });

  // T-S8-29: ADV_EPS exported === 1e-3 (S8-8-SC1)
  it('T-S8-29: ADV_EPS exported and === 1e-3', () => {
    expect(h.exports.ADV_EPS).toBe(1e-3);
  });
});

// ── T-S8 describe 2: initial state & first snap ──────────────────────────────
describe('stall-snap — Slice 8 initial state & first snap (T-S8-2, T-S8-25, T-S8-26)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-2: First-ever snap (lastSnapAtMs=-Infinity default): N1+N2 do not fire (S8-2-SC1)
  it('T-S8-2: first snap — lastSnapAtMs=-Infinity — N1+N2 bypass, snap executes; counters both 0', () => {
    // Fresh module: lastSnapCt=-Inf, lastSnapBufEnd=-Inf, lastSnapAtMs=-Inf
    // N1: ct(10.016) > -Inf + 1e-3 = true → passes
    // N2: now(any) - (-Inf) = +Inf >= 300 → not debounced → passes
    // N3: target=9.880 (10.330-0.450) != ct=10.016 → passes (S10 lead=0.45)
    // G6: 10.330-9.880=0.450 >= 0.1 → passes → snap executes
    h.perfNow(100); // arbitrary; any value: Inf elapsed from -Inf baseline
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;

    h.exports.onVideoWaiting();

    // Snap executed: currentTime set to target (S10: bufEnd-0.450=9.880)
    expect(h.videoEl.currentTime).toBeCloseTo(9.880, 5);
    // Counters: zero (nothing was suppressed)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    // Log line emitted
    const snapLines = getMseLogLines(h.tauri).filter((l) => l.includes('result=stall_snap'));
    expect(snapLines.length).toBe(1);
  });

  // T-S8-25: State NOT updated on suppressed snap — write rule (S8-2-SC2)
  it('T-S8-25: state NOT updated on N1-suppressed snap; getLastSnapState unchanged from snap#1', () => {
    // Snap#1: executes at perfNow=100, records lastSnapCt=10.016, lastSnapBufEnd=10.330
    h.perfNow(100);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();
    // After snap#1: lastSnapCt=10.016, lastSnapBufEnd=10.330, lastSnapAtMs=100

    // Trigger N1 suppression: ct and bufEnd NOT advanced past ADV_EPS
    // (ct=10.016 unchanged, bufEnd=10.330 unchanged → N1 fires)
    h.perfNow(500); // well outside debounce window (N2 would pass if N1 didn't fire first)
    tauri_clearInvoke(h.tauri);
    h.exports.onVideoWaiting();

    // N1 should have fired: suppressedGuardCount incremented
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
    // State should be unchanged — use getLastSnapState to verify
    const state = h.exports.getLastSnapState();
    expect(state.lastSnapCt).toBeCloseTo(10.016, 5);
    expect(state.lastSnapBufEnd).toBeCloseTo(10.330, 5);
    expect(state.lastSnapAtMs).toBe(100); // unchanged from snap#1
  });

  // T-S8-26: State IS updated on executed snap (S8-7 / S8-2)
  it('T-S8-26: state IS updated after executed snap#2; getLastSnapState reflects snap#2 values', () => {
    // Snap#1: executes at perfNow=100
    h.perfNow(100);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();

    // Advance perfNow past debounce (400 = 300ms elapsed), advance ct and bufEnd > ADV_EPS
    h.perfNow(500); // 500-100=400 >= 300 → N2 passes
    const bufEnd2 = 10.330 + 0.500; // 10.830
    const ct2 = 10.016 + 0.500;     // 10.516
    h.sb.buffered = makeBuffered(0, bufEnd2);
    h.videoEl.currentTime = ct2;

    h.exports.onVideoWaiting();

    // Snap#2 should have executed — state updated to snap#2 values
    const state = h.exports.getLastSnapState();
    expect(state.lastSnapCt).toBeCloseTo(ct2, 5);
    expect(state.lastSnapBufEnd).toBeCloseTo(bufEnd2, 5);
    expect(state.lastSnapAtMs).toBe(500); // now at snap#2 perfNow
  });
});

// Helper: clear only mse_log calls (doesn't affect other mock state)
function tauri_clearInvoke(tauri) {
  tauri.invoke.mock.calls.length = 0;
}

// ── T-S8 describe 3: N2 debounce window ──────────────────────────────────────
describe('stall-snap — Slice 8 N2 debounce window (T-S8-3..5, T-S8-17)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-3: snap inside 300ms window → N2 fires (S8-3-SC1)
  it('T-S8-3: snap#2 at T+50ms (inside 300ms window) → N2 fires; debounce count = 1', () => {
    // Snap#1: executes at perfNow=0 (lastSnapAtMs=0)
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();

    // Snap#2: perfNow=50 (50ms elapsed < 300ms). Advance ct/bufEnd > ADV_EPS so N1 passes.
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.332);  // bufEnd advanced by 2ms > ADV_EPS=1ms
    h.videoEl.currentTime = 10.018;            // ct advanced by 2ms > ADV_EPS=1ms

    const ctBefore = h.videoEl.currentTime;
    h.exports.onVideoWaiting();

    // N2 fires: no seek, debounce count = 1
    expect(h.videoEl.currentTime).toBe(ctBefore);
    expect(h.exports.getSuppressedDebounceCount()).toBe(1);
  });

  // T-S8-4: snap outside 300ms window → N2 does NOT fire (S8-3-SC2)
  it('T-S8-4: snap#2 at T+350ms (outside window) → N2 does not fire; snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1: lastSnapAtMs=0

    h.perfNow(350); // 350 - 0 = 350 >= 300 → N2 does not fire
    h.sb.buffered = makeBuffered(0, 10.332);
    h.videoEl.currentTime = 10.018;

    h.exports.onVideoWaiting();

    // Snap executes: debounce count unchanged at 0 (S10: target=10.332-0.450)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.332 - 0.450, 5);
  });

  // T-S8-5: snap at exactly 300ms boundary → NOT suppressed (S8-3-SC3)
  it('T-S8-5: snap#2 at T+300ms exactly (boundary) → N2 does not fire; snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1: lastSnapAtMs=0

    h.perfNow(300); // 300 - 0 = 300 >= 300 → NOT suppressed (boundary)
    h.sb.buffered = makeBuffered(0, 10.332);
    h.videoEl.currentTime = 10.018;

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.332 - 0.450, 5);
  });

  // T-S8-17: 3 consecutive N2 suppressions → debounce count = 3 (S8-6-SC1)
  it('T-S8-17: 3 consecutive N2 suppressions in window → getSuppressedDebounceCount() === 3', () => {
    // Snap#1: executes at perfNow=0
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    let bufEnd = 10.330;
    let ct = 10.016;

    // 3 snaps inside 300ms window (perfNow stays < 300), each with ct/bufEnd > ADV_EPS
    for (let i = 0; i < 3; i++) {
      h.perfNow((i + 1) * 30); // 30ms, 60ms, 90ms — all inside 300ms window
      bufEnd += 0.002; // advance by 2ms > ADV_EPS
      ct += 0.002;
      h.sb.buffered = makeBuffered(0, bufEnd);
      h.videoEl.currentTime = ct;
      h.exports.onVideoWaiting();
    }

    expect(h.exports.getSuppressedDebounceCount()).toBe(3);
  });
});

// ── T-S8 describe 4: N2 escape hatch (rs<=1) ──────────────────────────────────
describe('stall-snap — Slice 8 N2 escape hatch rs<=1 (T-S8-6..9, T-S8-24)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-6: rs=1 inside window, N1 passes → N2 bypassed, snap executes (S8-3-SC4 / S10 2-strike)
  // S10 update: 2-strike gate requires streak>=2 for bypass. Pre-seed streak=1 so this
  // second call reaches streak=2 and still bypasses (hatch remediation C1).
  it('T-S8-6: rs=1 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1 at perfNow=0; lastSnapAtMs=0

    // perfNow=50: 50ms elapsed < 300ms (inside window); hardStarve=true → bypasses N2
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.018; // ct advanced > ADV_EPS → N1 passes
    Object.defineProperty(h.videoEl, 'readyState', { value: 1, configurable: true });
    h.exports.setHardStarveStreak(1); // pre-seed: this call → streak 1→2 → bypass fires

    h.exports.onVideoWaiting();

    // N2 bypassed: snap executes, debounce count unchanged at 0 (drift=0.312 > 0.300)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.450, 5);
  });

  // T-S8-7: rs=1 inside window, NO ct/bufEnd progress → N1 fires first (S8-3-SC5)
  it('T-S8-7: rs=1 inside window, no ct/bufEnd progress → N1 fires; suppressedGuardCount++; N2 NOT reached', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1; records ct=10.016, bufEnd=10.030

    h.perfNow(50); // inside 300ms window
    // ct and bufEnd NOT advanced (still at snap#1 baseline)
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    Object.defineProperty(h.videoEl, 'readyState', { value: 1, configurable: true });

    h.exports.onVideoWaiting();

    // N1 fires first: suppressedGuardCount incremented; N2 NOT reached
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
  });

  // T-S8-8 (UPDATED S10 — 2-strike hatch): rs=2 inside window, ct/bufEnd advanced → N2 BYPASSED
  // S10 update: 2-strike gate requires streak>=2. Pre-seed streak=1 so this call
  // reaches streak=2 and still bypasses (C1 remediation). Lead also updated to 0.45.
  it('T-S8-8: rs=2 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, snap executes (Slice 9 rs<=2 hatch; S10 2-strike pre-seed)', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50); // inside 300ms
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.018; // ct advanced > ADV_EPS → N1 passes
    Object.defineProperty(h.videoEl, 'readyState', { value: 2, configurable: true }); // rs=2 IS escape hatch
    h.exports.setHardStarveStreak(1); // pre-seed: this call → streak 1→2 → bypass fires

    h.exports.onVideoWaiting();

    // N2 bypassed: snap executes, debounce count unchanged at 0 (drift=0.312 > 0.300)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.450, 5);
  });

  // T-S8-9: rs=0 inside window, ct/bufEnd advanced → N2 bypassed (rs=0 <= 2 hardStarve) (S8-3-SC7)
  // S10 update: 2-strike gate requires streak>=2. Pre-seed streak=1 so this call
  // reaches streak=2 and still bypasses (C1 remediation). Lead also updated to 0.45.
  it('T-S8-9: rs=0 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50); // inside 300ms
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.018;
    Object.defineProperty(h.videoEl, 'readyState', { value: 0, configurable: true }); // rs=0 <= 2 hardStarve
    h.exports.setHardStarveStreak(1); // pre-seed: this call → streak 1→2 → bypass fires

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.450, 5);
  });

  // T-S8-24: rs=1 inside window, no progress → N1 fires (suppressedGuardCount++), NOT N2 (S8-7-SC3)
  it('T-S8-24: rs=1 inside window, no ct/bufEnd progress → N1 fires (suppressedGuard++); N2 NOT reached (suppressedDebounce unchanged)', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1 records state

    h.perfNow(50); // inside 300ms
    // No progress: ct and bufEnd at same values as snap#1
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    Object.defineProperty(h.videoEl, 'readyState', { value: 1, configurable: true });

    const guardBefore = h.exports.getSuppressedGuardCount();
    const debounceBefore = h.exports.getSuppressedDebounceCount();

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(guardBefore + 1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(debounceBefore); // unchanged
  });
});

// ── T-S8 describe 5: N1 effectiveness guard ──────────────────────────────────
describe('stall-snap — Slice 8 N1 effectiveness guard (T-S8-10..14, T-S8-18)', () => {
  let h;

  beforeEach(async () => {
    h = await makeS8Harness();
    // Seed state: lastSnapCt=5.000, lastSnapBufEnd=5.300; lastSnapAtMs=0
    // perfNow set to 350 so N2 sees elapsed=350-0=350>=300 (outside window) — N2 will not fire.
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 5.000, lastSnapBufEnd: 5.300 });
    h.perfNow(350); // outside 300ms debounce window
    // Set a good buffer geometry (will be overridden per test)
    h.sb.buffered = makeBuffered(0, 10.030);
  });
  afterEach(() => teardownS8Harness(h));

  // T-S8-10: only ct advanced > ADV_EPS → N1 passes, snap executes (S8-4-SC1)
  // Keep drift above the low-drift replay guard while holding bufEnd fixed.
  it('T-S8-10: lastSnapCt=4.996, lastSnapBufEnd=5.300; ct=4.998 (advanced), bufEnd=5.300 (unchanged) → N1 passes', () => {
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 4.996, lastSnapBufEnd: 5.300 });
    h.sb.buffered = makeBuffered(0, 5.300); // bufEnd unchanged from baseline
    h.videoEl.currentTime = 4.998;          // ct=4.998 > 4.996+0.001=4.997 ✓
    // Geometry: target=Math.max(0, 5.300-0.450)=4.850; drift=0.302 > 0.300 → replay guard stays open

    h.exports.onVideoWaiting();

    // N1 passed: snap executes; guardCount unchanged at 0
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(4.850, 5);
  });

  // T-S8-11: only bufEnd advanced > ADV_EPS → N1 passes, snap executes (S8-4-SC2)
  // S10: target=5.302-0.450=4.852; G6: 5.302-4.852=0.450 >= 0.1 ✓
  it('T-S8-11: lastSnapCt=5.000, lastSnapBufEnd=5.300; ct=5.000 (unchanged), bufEnd=5.302 (advanced) → N1 passes', () => {
    h.sb.buffered = makeBuffered(0, 5.302); // bufEnd=5.302 > 5.300+0.001=5.301 ✓
    h.videoEl.currentTime = 5.000;          // ct=5.000, unchanged (NOT advanced)
    // target=Math.max(0, 5.302-0.450)=4.852; ct=5.000 != target → N3 passes
    // G6: 5.302-4.852=0.450 >= 0.1 → passes

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(4.852, 5);
  });

  // T-S8-12: neither ct nor bufEnd advanced > ADV_EPS → N1 fires (S8-4-SC3)
  it('T-S8-12: ct=5.000 (unchanged), bufEnd=5.300 (unchanged) → N1 fires; suppressedGuardCount++', () => {
    h.sb.buffered = makeBuffered(0, 5.300); // bufEnd=5.300 NOT > 5.300+0.001
    h.videoEl.currentTime = 5.000;          // ct=5.000 NOT > 5.000+0.001

    const ctBefore = h.videoEl.currentTime;
    h.exports.onVideoWaiting();

    expect(h.videoEl.currentTime).toBe(ctBefore); // no seek
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
  });

  // T-S8-13: both ct and bufEnd advanced > ADV_EPS → N1 passes (S8-4-SC4)
  // S10: target=5.360-0.450=4.910; G6: 5.360-4.910=0.450 >= 0.1 ✓
  it('T-S8-13: ct=5.050 (advanced), bufEnd=5.360 (advanced) → N1 passes, snap executes', () => {
    h.sb.buffered = makeBuffered(0, 5.360); // bufEnd=5.360 > 5.300+0.001 ✓
    h.videoEl.currentTime = 5.050;          // ct=5.050 > 5.000+0.001 ✓
    // target=Math.max(0, 5.360-0.450)=4.910; ct=5.050 != 4.910 → N3 passes
    // G6: 5.360-4.910=0.450 >= 0.1 → passes

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(4.910, 5);
  });

  // T-S8-14: ct sub-epsilon (0.0005 advance) → N1 fires (S8-4-SC5)
  it('T-S8-14: ct=5.0005 (sub-epsilon: 5.0005 > 5.001 is false), bufEnd=5.300 unchanged → N1 fires', () => {
    h.sb.buffered = makeBuffered(0, 5.300); // bufEnd=5.300, unchanged
    h.videoEl.currentTime = 5.0005;         // 5.0005 NOT > 5.001 (ADV_EPS=0.001)

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(1);
  });

  // T-S8-18: 2 consecutive N1 suppressions → suppressedGuardCount === 2 (S8-6-SC2)
  it('T-S8-18: 2 N1 suppressions → getSuppressedGuardCount() === 2', () => {
    h.sb.buffered = makeBuffered(0, 5.300);
    h.videoEl.currentTime = 5.000; // neither advanced

    h.exports.onVideoWaiting(); // N1 fires: guard=1
    h.exports.onVideoWaiting(); // N1 fires again: guard=2

    expect(h.exports.getSuppressedGuardCount()).toBe(2);
    expect(h.exports.getSuppressedDebounceCount()).toBe(0); // debounce unaffected
  });

  // T-S8-31: N1 epsilon boundary — ct half. Advance EXACTLY ADV_EPS → N1 fires.
  // N1 uses strict `>`: ct > lastSnapCt + ADV_EPS. At exact equality the comparison
  // is FALSE → advanced=false → guard++. The `>`→`>=` mutant would make it TRUE
  // (N1 passes, snap executes, guard unchanged) — killed by the guard++ assertion.
  // Equality holds BY CONSTRUCTION: baseline + the exported ADV_EPS, so floating-point
  // representation is identical on both sides of the comparison.
  it('T-S8-31: ct advanced by EXACTLY ADV_EPS (ct === lastSnapCt + ADV_EPS), bufEnd unchanged → N1 fires (strict > boundary); kills >→>=', () => {
    const eps        = h.exports.ADV_EPS; // exact same value the production guard uses
    const baseCt     = 5.000;
    const baseBufEnd = 5.300;
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: baseCt, lastSnapBufEnd: baseBufEnd });
    h.perfNow(350); // outside 300ms window — isolates N1 from N2

    // bufEnd held at baseline (no progress); ct exactly at the boundary.
    h.sb.buffered = makeBuffered(0, baseBufEnd);
    h.videoEl.currentTime = baseCt + eps; // === lastSnapCt + ADV_EPS → `>` is FALSE

    const guardBefore    = h.exports.getSuppressedGuardCount();
    const debounceBefore = h.exports.getSuppressedDebounceCount();
    const ctBefore       = h.videoEl.currentTime;

    h.exports.onVideoWaiting();

    // Strict `>`: at exact equality N1 fires. `>=` mutant would let it pass.
    expect(h.exports.getSuppressedGuardCount()).toBe(guardBefore + 1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(debounceBefore); // N2 not reached
    expect(h.videoEl.currentTime).toBe(ctBefore); // no seek
  });

  // T-S8-32: N1 epsilon boundary — bufEnd half (symmetric to T-S8-31).
  // bufEnd === lastSnapBufEnd + ADV_EPS, ct held at baseline. Strict `>` → FALSE on
  // both disjuncts → advanced=false → guard++. The `>`→`>=` mutant on the bufEnd
  // disjunct would make it TRUE — killed by the guard++ assertion.
  it('T-S8-32: bufEnd advanced by EXACTLY ADV_EPS (bufEnd === lastSnapBufEnd + ADV_EPS), ct unchanged → N1 fires (strict > boundary); kills >→>=', () => {
    const eps        = h.exports.ADV_EPS;
    const baseCt     = 5.000;
    const baseBufEnd = 5.300;
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: baseCt, lastSnapBufEnd: baseBufEnd });
    h.perfNow(350); // outside 300ms window — isolates N1 from N2

    // ct held at baseline (no progress); bufEnd exactly at the boundary.
    h.sb.buffered = makeBuffered(0, baseBufEnd + eps); // === lastSnapBufEnd + ADV_EPS → `>` is FALSE
    h.videoEl.currentTime = baseCt;

    const guardBefore    = h.exports.getSuppressedGuardCount();
    const debounceBefore = h.exports.getSuppressedDebounceCount();
    const ctBefore       = h.videoEl.currentTime;

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(guardBefore + 1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(debounceBefore); // N2 not reached
    expect(h.videoEl.currentTime).toBe(ctBefore); // no seek
  });
});

// ── T-S8 describe 6: N3 no-op kill ───────────────────────────────────────────
describe('stall-snap — Slice 8 N3 no-op kill (T-S8-15..16)', () => {
  let h;

  beforeEach(async () => {
    h = await makeS8Harness();
    // Seed state with baseline far from test geometry; perfNow=350 → N2 sees elapsed=350>=300 (outside window)
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 0.000, lastSnapBufEnd: 0.000 });
    h.perfNow(350); // outside 300ms debounce window
  });
  afterEach(() => teardownS8Harness(h));

  // T-S8-15: target === ct → N3 fires (S8-5-SC1)
  // S10: ct=9.550, bufEnd=10.000, lastRangeStart=0.000
  // target = Math.max(0.000, 10.000-0.450) = 9.55 === ct (IEEE 754 exact)
  it('T-S8-15: target === ct (9.550 === 9.550, IEEE 754 exact, S10 lead=0.45) → N3 fires; no seek; suppressedGuardCount++', () => {
    h.sb.buffered = makeBuffered(0, 10.000); // lastRangeStart=0, bufEnd=10.000
    h.videoEl.currentTime = 9.550;           // ct = 9.55; target = 10.000-0.450 = 9.55 === ct

    const ctBefore = h.videoEl.currentTime;
    h.exports.onVideoWaiting();

    expect(h.videoEl.currentTime).toBe(ctBefore); // no seek — N3 fired
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some((l) => l.includes('result=stall_snap'))).toBe(false);
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
  });

  // T-S8-16: target !== ct → N3 does not fire, snap executes (S8-5-SC2)
  // S10: target = 10.330-0.450=9.880, ct=10.016 != 9.880 → N3 does not fire
  it('T-S8-16: target !== ct (10.016 vs 9.880) → N3 does not fire; snap executes', () => {
    h.sb.buffered = makeBuffered(0, 10.330); // target = 9.880 (S10: 10.330-0.450)
    h.videoEl.currentTime = 10.016;          // ct = 10.016 != 9.580

    h.exports.onVideoWaiting();

    expect(h.videoEl.currentTime).toBeCloseTo(9.880, 5);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
  });
});

// ── T-S8 describe 7: counter monotonicity & telemetry ────────────────────────
describe('stall-snap — Slice 8 counter monotonicity & telemetry (T-S8-19..21, T-S8-28)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-19: mixed suppressions — counters monotonic and per-type accurate (S8-6-SC3)
  it('T-S8-19: 3 N2 suppressions then 2 N1 suppressions then 1 N2 → debounce=4, guard=2 (monotonic)', () => {
    let bufEnd = 10.330;
    let ct = 10.016;
    let perfT = 0;

    // Snap#1 at perfNow=0
    h.perfNow(perfT);
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    // 3 N2 suppressions inside window (perfNow 20/40/60 — all inside 300ms from t=0)
    for (let i = 0; i < 3; i++) {
      perfT += 20;
      h.perfNow(perfT); // 20ms, 40ms, 60ms — inside 300ms window
      bufEnd += 0.002;
      ct += 0.002;
      h.sb.buffered = makeBuffered(0, bufEnd);
      h.videoEl.currentTime = ct;
      h.exports.onVideoWaiting();
    }
    expect(h.exports.getSuppressedDebounceCount()).toBe(3);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);

    // Snap#2: perfNow=500 (outside 300ms window from t=0=60ms last snap check)
    perfT = 500;
    h.perfNow(perfT);
    bufEnd += 0.500;
    ct += 0.500;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // snap#2 executes (updates baseline to new ct/bufEnd); lastSnapAtMs=500

    // 2 N1 suppressions: no progress past the new baseline (ct/bufEnd unchanged)
    perfT = 1000;
    h.perfNow(perfT); // outside 300ms from snap#2
    h.exports.onVideoWaiting(); // N1 fires
    h.exports.onVideoWaiting(); // N1 fires again
    expect(h.exports.getSuppressedGuardCount()).toBe(2);

    // Snap#3: advance ct/bufEnd, outside window from snap#2
    bufEnd += 0.500;
    ct += 0.500;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // snap#3 executes; lastSnapAtMs=1000

    // 1 more N2 suppression: inside 300ms from snap#3 (perfNow=1000), ct/bufEnd advanced
    perfT = 1050; // 1050 - 1000 = 50ms < 300ms
    h.perfNow(perfT);
    bufEnd += 0.002;
    ct += 0.002;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // N2 fires

    expect(h.exports.getSuppressedDebounceCount()).toBe(4);
    expect(h.exports.getSuppressedGuardCount()).toBe(2);
  });

  // T-S8-20: tick line emits suppressed_debounce and suppressed_guard in correct position (S8-6-SC4)
  // NOTE (Slice 9): the old $-anchor is relaxed because watchdog_rescues=N is now the trailing field.
  it('T-S8-20: tick line matches /suppressed_debounce=(\\d+) suppressed_guard=(\\d+) watchdog_rescues=(\\d+)$/ with correct values', async () => {
    // Set up known counter values: 2 N2 suppressions (debounce=2) + 1 N1 suppression (guard=1)
    let bufEnd = 10.330;
    let ct = 10.016;
    let perfT = 0;

    h.perfNow(perfT);
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // snap#1

    // 2 N2 suppressions inside window
    for (let i = 0; i < 2; i++) {
      perfT += 20;
      h.perfNow(perfT); // 20ms, 40ms — inside 300ms window
      bufEnd += 0.002;
      ct += 0.002;
      h.sb.buffered = makeBuffered(0, bufEnd);
      h.videoEl.currentTime = ct;
      h.exports.onVideoWaiting();
    }
    // Snap#2: outside debounce window
    perfT = 500;
    h.perfNow(perfT);
    bufEnd += 0.500;
    ct += 0.500;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // snap#2 executes

    // 1 N1 suppression: outside debounce window, no ct/bufEnd progress
    perfT = 1000;
    h.perfNow(perfT);
    h.exports.onVideoWaiting(); // N1 fires

    expect(h.exports.getSuppressedDebounceCount()).toBe(2);
    expect(h.exports.getSuppressedGuardCount()).toBe(1);

    // Trigger heartbeat tick (2s interval via fake timer — vi.advanceTimersByTimeAsync works for setInterval)
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const tickLines = getTickLines(h.tauri);
    expect(tickLines.length).toBeGreaterThan(0);
    const lastTick = tickLines[tickLines.length - 1];

    // Must match trailing regex (watchdog_rescues=N is now the last field after suppressed_guard)
    expect(lastTick).toMatch(/suppressed_debounce=(\d+) suppressed_guard=(\d+) watchdog_rescues=(\d+)$/);
    // Extract values and compare with getter
    const match = lastTick.match(/suppressed_debounce=(\d+) suppressed_guard=(\d+) watchdog_rescues=(\d+)$/);
    expect(parseInt(match[1], 10)).toBe(h.exports.getSuppressedDebounceCount());
    expect(parseInt(match[2], 10)).toBe(h.exports.getSuppressedGuardCount());
    // suppressed_debounce= appears after buffered=
    expect(lastTick.indexOf('buffered=')).toBeLessThan(lastTick.indexOf('suppressed_debounce='));
    // suppressed_guard= appears after suppressed_debounce=
    expect(lastTick.indexOf('suppressed_debounce=')).toBeLessThan(lastTick.indexOf('suppressed_guard='));
  });

  // T-S8-21: tick line has fields even when counters are 0 (S8-6-SC5)
  // NOTE (Slice 9): watchdog_rescues=N is now the trailing field; relax the $-anchor to include it.
  it('T-S8-21: no suppressions → tick line ends with suppressed_debounce=0 suppressed_guard=0 watchdog_rescues=0', async () => {
    // No snaps, no suppressions — counters at 0
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const tickLines = getTickLines(h.tauri);
    expect(tickLines.length).toBeGreaterThan(0);
    const lastTick = tickLines[tickLines.length - 1];
    expect(lastTick).toMatch(/suppressed_debounce=0 suppressed_guard=0 watchdog_rescues=0$/);
  });

  // T-S8-28: counters persist across tearDownMse (NOT reset to 0) (S8-6-SC6)
  it('T-S8-28: counters persist across tearDownMse — NOT reset to 0', () => {
    let bufEnd = 10.330;
    let ct = 10.016;
    let perfT = 0;

    // Snap#1 at perfNow=0
    h.perfNow(perfT);
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    // 5 N2 suppressions inside window (perfNow stays inside 300ms from t=0)
    for (let i = 0; i < 5; i++) {
      perfT += 20;
      h.perfNow(perfT); // 20,40,60,80,100ms — all inside 300ms
      bufEnd += 0.002;
      ct += 0.002;
      h.sb.buffered = makeBuffered(0, bufEnd);
      h.videoEl.currentTime = ct;
      h.exports.onVideoWaiting();
    }
    // Snap#2: outside debounce window, update baseline
    perfT = 500;
    h.perfNow(perfT);
    bufEnd += 0.500;
    ct += 0.500;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = ct;
    h.exports.onVideoWaiting(); // snap#2; lastSnapAtMs=500

    // 3 N1 suppressions: no ct/bufEnd progress
    perfT = 1000;
    h.perfNow(perfT); // outside 300ms from snap#2
    h.exports.onVideoWaiting();
    h.exports.onVideoWaiting();
    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(5);
    expect(h.exports.getSuppressedGuardCount()).toBe(3);

    // Tear down MSE session (simulates reconnect)
    h.exports.tearDownMse();

    // Counters must still be at same values — NOT reset by teardown
    expect(h.exports.getSuppressedDebounceCount()).toBe(5);
    expect(h.exports.getSuppressedGuardCount()).toBe(3);
  });
});

// ── T-S8 describe 8: guard ordering ──────────────────────────────────────────
describe('stall-snap — Slice 8 guard ordering (T-S8-22..23)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-22: G1 fires before N1 is evaluated (S8-7-SC1)
  it('T-S8-22: G1 (mseState.sb=null) fires before N1; neither counter incremented', () => {
    // G1 fires before N1 is evaluated — no time manipulation needed for this test
    // (initial lastSnapAtMs=-Inf → debounce would pass anyway, but G1 fires first)
    const origSb = h.exports.mseState.sb;
    h.exports.mseState.sb = null;

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);

    h.exports.mseState.sb = origSb;
  });

  // T-S8-23: G6 fires after N1/N2/N3 pass — G6 remains SILENT (S8-7-SC2)
  it('T-S8-23: G6 (sliver: bufEnd-target < 0.1) fires after N1/N2/N3 pass; neither counter incremented', () => {
    // Seed state far from current geometry to ensure N1 passes
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 0.000, lastSnapBufEnd: 0.000 });
    h.perfNow(350); // outside debounce window (350-0=350>=300)

    // G6 sliver geometry: target=Math.max(5.0, 4.75)=5.0; ct=5.04; target!=ct (N3 passes)
    // cushion = 5.05-5.0 = 0.05 < 0.1 → G6 fires
    h.sb.buffered = makeBuffered(5.0, 5.05);
    h.videoEl.currentTime = 5.04;

    h.exports.onVideoWaiting();

    // G6 silent: neither counter incremented
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
  });

  // T-S8-30: N1 EFFECTIVENESS runs BEFORE N2 DEBOUNCE — kills the N1<->N2 swap mutant.
  // The discriminating scenario: rs>=2 (NOT the hardStarve escape hatch) + inside the
  // 300ms debounce window + neither ct nor bufEnd advanced past the last EXECUTED snap
  // baseline. Under correct N1-first ordering, N1 fires (guard++) and returns before N2
  // is reached, so debounce stays 0. Under the swap, N2 would fire first (debounce++)
  // and guard would stay 0 — caught by the exact-delta partition asserts below.
  it('T-S8-30: rs>=2 + inside debounce window + no ct/bufEnd progress → N1 fires first (guard++), N2 NOT reached (debounce unchanged); no seek', () => {
    // Seed last-snap baseline directly via the state seam. lastSnapAtMs=100 so that
    // perfNow elapsed (100ms below) sits INSIDE the 300ms debounce window.
    const baseAtMs   = 100;
    const baseCt     = 5.000;
    const baseBufEnd = 5.300;
    h.exports.setLastSnapState({ lastSnapAtMs: baseAtMs, lastSnapCt: baseCt, lastSnapBufEnd: baseBufEnd });

    // now - lastSnapAtMs = 200 - 100 = 100ms < 300ms → N2 WOULD fire if reached.
    h.perfNow(baseAtMs + 100);

    // rs>=2 (NOT escape hatch): rs=3 (HAVE_FUTURE_DATA). hardStarve=false → N2 not bypassed.
    h.overrideProperty(h.videoEl, 'readyState', { value: 3 });

    // Hold ct and bufEnd at the EXACT baseline values — no progress past ADV_EPS.
    // N1 sees advanced=false → fires and returns before N2 is evaluated.
    h.sb.buffered = makeBuffered(0, baseBufEnd);
    h.videoEl.currentTime = baseCt;

    const guardBefore    = h.exports.getSuppressedGuardCount();
    const debounceBefore = h.exports.getSuppressedDebounceCount();
    const ctBefore       = h.videoEl.currentTime;

    h.exports.onVideoWaiting();

    // N1-first: guard incremented by exactly 1, debounce unchanged (N2 never reached).
    // Under the N1<->N2 swap mutant the deltas invert (debounce++, guard unchanged).
    expect(h.exports.getSuppressedGuardCount()).toBe(guardBefore + 1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(debounceBefore);
    // No seek occurred — currentTime untouched.
    expect(h.videoEl.currentTime).toBe(ctBefore);
  });
});

// ── T-S8 describe 9: getter export contract ──────────────────────────────────
describe('stall-snap — Slice 8 getter export contract (T-S8-27)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S8-27: getter functions return live state (not snapshot-at-0) (S8-8-SC2)
  it('T-S8-27: getSuppressedDebounceCount() reads live state after N2 suppression; getSuppressedGuardCount() independent', () => {
    // Snap#1 at perfNow=0
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    // Trigger one N2 suppression: perfNow=50 (inside 300ms), ct/bufEnd advanced > ADV_EPS
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.332);
    h.videoEl.currentTime = 10.018;
    h.exports.onVideoWaiting(); // N2 fires

    // getSuppressedDebounceCount() must return 1 (live state, not snapshot-at-0)
    expect(h.exports.getSuppressedDebounceCount()).toBe(1);
    // getSuppressedGuardCount() must return 0 (independent counter, unchanged)
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
  });
});

// ═══════════════════════════════════════════════════════════════════════════════
// SLICE 9 — Gap-stranding freeze-fix: watchdog + no-hole clamp + rs<=2 + telemetry
// T-S9-* tests (RED before GREEN per strict TDD; all FAIL until GREEN implementation)
// ═══════════════════════════════════════════════════════════════════════════════

// ── T2-A: clampSnapTarget unit tests ─────────────────────────────────────────
describe('clampSnapTarget — no-hole clamp unit (T-S9-C1..C8)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-C1: rawTarget inside a substantial range → returns rawTarget unchanged (pass-through)
  it('T-S9-C1: rawTarget inside a substantial range → returns rawTarget unchanged', () => {
    const buf = makeBufferedMulti([[0, 5.0], [6.0, 10.0]]);
    const result = h.exports.clampSnapTarget(buf, 6.5, 8.0);
    expect(result).toBeCloseTo(8.0, 10);
  });

  // T-S9-C2: rawTarget in a gap → returns buf.start(i) of next substantial range
  it('T-S9-C2: rawTarget in a gap → returns start of next substantial range', () => {
    const buf = makeBufferedMulti([[0, 2.0], [4.0, 8.0]]);
    // rawTarget=3.0 is in gap [2.0, 4.0]
    const result = h.exports.clampSnapTarget(buf, 1.5, 3.0);
    expect(result).toBeCloseTo(4.0, 10);
  });

  // T-S9-C3: GATE-8 exact geometry
  it('T-S9-C3: GATE-8 exact geometry [[0,0.261],[1.895,2.239]] ct=1.761 rawTarget=1.761 → returns 1.895', () => {
    const buf = makeBufferedMulti([[0, 0.261], [1.895, 2.239]]);
    const result = h.exports.clampSnapTarget(buf, 1.761, 1.761);
    expect(result).toBeCloseTo(1.895, 5);
  });

  // T-S9-C4: next forward range is a sliver (<0.3s) → skip sliver, return next substantial range start
  it('T-S9-C4: next forward range is a sliver (<0.3s) → skip sliver, return next substantial range start', () => {
    // gap from 2.0→3.0; sliver [3.0, 3.2] (0.2s < 0.3s); substantial [4.0, 8.0]
    const buf = makeBufferedMulti([[0, 2.0], [3.0, 3.2], [4.0, 8.0]]);
    const result = h.exports.clampSnapTarget(buf, 1.5, 2.5);
    expect(result).toBeCloseTo(4.0, 10);
  });

  // T-S9-C5: no forward substantial range → last-substantial fallback
  it('T-S9-C5: no forward substantial range → last substantial range start fallback', () => {
    // buf has one substantial range [0, 5.0]; rawTarget=6.0 is past it
    const buf = makeBufferedMulti([[0, 5.0]]);
    const result = h.exports.clampSnapTarget(buf, 0.5, 6.0);
    expect(result).toBeCloseTo(0.0, 10);
  });

  // T-S9-C6: empty buf → returns null
  it('T-S9-C6: empty buf (buf.length===0) → returns null', () => {
    const buf = makeBufferedMulti([]);
    const result = h.exports.clampSnapTarget(buf, 1.0, 2.0);
    expect(result).toBeNull();
  });

  // T-S9-C7: all ranges are slivers, no substantial range → returns null
  it('T-S9-C7: all ranges are slivers (< SNAP_SLIVER_MIN_SEC=0.3s), no substantial range → returns null', () => {
    const buf = makeBufferedMulti([[0, 0.1], [1.0, 1.2], [2.0, 2.25]]);
    const result = h.exports.clampSnapTarget(buf, 0.05, 1.5);
    expect(result).toBeNull();
  });

  // T-S9-C8: rawTarget in a sliver range → treated as gap, forwards to next substantial range
  it('T-S9-C8: rawTarget inside a sliver range → treated as gap, returns next substantial range start', () => {
    // sliver [0, 0.2]; gap; substantial [1.0, 5.0]
    const buf = makeBufferedMulti([[0, 0.2], [1.0, 5.0]]);
    // rawTarget=0.1 is inside the sliver
    const result = h.exports.clampSnapTarget(buf, 0.05, 0.1);
    expect(result).toBeCloseTo(1.0, 10);
  });

  // T-S9-11 (S9-3-SC4): EXACT-0.300 boundary — a range of duration EXACTLY SNAP_SLIVER_MIN_SEC
  // is SUBSTANTIAL (selectable / pass-through), NOT a sliver. The clamp uses `e - s >= 0.3`;
  // this pins the `>=` boundary and KILLS the `>=` → `>` mutant (which would treat a 0.300
  // range as a sliver and reject it). Two complementary cases:
  //   (a) rawTarget INSIDE the exact-0.3 range → pass-through (returns rawTarget unchanged).
  //   (b) rawTarget in a gap, exact-0.3 range AHEAD → selected as the forward substantial range.
  // NOTE: durations must be float-EXACT 0.3 for this boundary to be meaningful — many decimal
  // pairs (e.g. 5.3-5.0) evaluate to 0.2999999999999998 in IEEE-754 and would read as slivers.
  // [0,0.3] and [0.3,0.6] both subtract to exactly 0.3, so `>=0.3` is true while `>0.3` is false.
  it('T-S9-11: range of duration EXACTLY 0.300 is SUBSTANTIAL (selectable / pass-through), killing the >=→> mutant', () => {
    // (a) pass-through: rawTarget 0.1 inside the exact-0.300 range [0, 0.3].
    //     Correct (>=0.3 substantial): returns 0.1. Mutant (>0.3 sliver): would skip to 1.0.
    const bufA = makeBufferedMulti([[0, 0.3], [1.0, 2.0]]);
    const passThrough = h.exports.clampSnapTarget(bufA, 0.05, 0.1);
    expect(passThrough).toBeCloseTo(0.1, 10); // accepted as-is — exact-0.3 range is substantial

    // (b) forward-select: rawTarget 0.05 inside sliver [0,0.1]; the exact-0.300 range [0.3, 0.6]
    //     must be chosen as the next substantial range start (not skipped as a sliver).
    //     Correct (>=0.3): returns 0.3. Mutant (>0.3): [0.3,0.6] read as sliver → null.
    const bufB = makeBufferedMulti([[0, 0.1], [0.3, 0.6]]);
    const forwardSelect = h.exports.clampSnapTarget(bufB, 0.02, 0.05);
    expect(forwardSelect).toBeCloseTo(0.3, 10); // exact-0.3 range selected, not skipped
  });
});

// ── T2-B: watchdog 2-tick stuck threshold ────────────────────────────────────
describe('watchdog — 2-tick stuck threshold (T-S9-W1..W6)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-W1: 1 stuck tick → NO rescue
  it('T-S9-W1: 1 stuck tick (stuckTicks starts at 0, no-progress + data ahead, 1 tick) → NO rescue', async () => {
    // arm: watchdogStuckTicks=0 (default), set up stuck scenario
    h.exports.setWatchdogState({ watchdogStuckTicks: 0, watchdogLastTickCt: 5.0 });
    // ct below watchdogLastTickCt + WATCHDOG_PROGRESS_EPS(0.5)
    h.videoEl.currentTime = 5.0; // no meaningful progress (5.0 - 5.0 = 0 < 0.5)
    // data substantially ahead: bufEnd = 10.0 > ct + 0.5
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // stuckTicks incremented to 1, but threshold is 2 → no rescue
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(false);
  });

  // T-S9-W2: 2 stuck ticks → rescue fires
  it('T-S9-W2: 2 stuck ticks (pre-arm stuckTicks=1, no-progress + data ahead, 1 tick) → rescue fires', async () => {
    // pre-arm to stuckTicks=1 via setWatchdogState
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0; // no progress (0 < WATCHDOG_PROGRESS_EPS=0.5)
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]); // substantial data ahead

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // stuckTicks reaches 2 → rescue fires
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore + 1);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(true);
  });

  // T-S9-W3: ct progress resets stuck counter
  it('T-S9-W3: ct progress (>= WATCHDOG_PROGRESS_EPS=0.5) resets stuckTicks to 0, no rescue', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    // ct advanced by 0.6 > WATCHDOG_PROGRESS_EPS(0.5) → "progressed"
    h.videoEl.currentTime = 5.6;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // progress detected → stuckTicks reset to 0, no rescue
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(false);
    // getWatchdogState shows stuckTicks reset
    expect(h.exports.getWatchdogState().watchdogStuckTicks).toBe(0);
  });

  // T-S9-W4: no data-ahead resets counter
  it('T-S9-W4: no data-ahead (bufEnd <= ct + WATCHDOG_DATA_AHEAD_SEC=0.5) resets stuckTicks to 0, no rescue', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    // bufEnd = 5.3 which is NOT > ct(5.0) + 0.5 → no data ahead
    h.sb.buffered = makeBufferedMulti([[0, 5.3]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    await vi.advanceTimersByTimeAsync(2000);

    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    expect(h.exports.getWatchdogState().watchdogStuckTicks).toBe(0);
  });

  // T-S9-W5: empty buffered resets counter and sentinel
  it('T-S9-W5: empty buf resets stuckTicks=0 and watchdogLastTickCt=-Infinity, no rescue', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.sb.buffered = makeBufferedMulti([]); // empty

    const rescuesBefore = h.exports.getWatchdogRescues();
    await vi.advanceTimersByTimeAsync(2000);

    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    expect(h.exports.getWatchdogState().watchdogStuckTicks).toBe(0);
    expect(h.exports.getWatchdogState().watchdogLastTickCt).toBe(-Infinity);
  });

  // T-S9-W6: rescue resets stuckTicks; subsequent cycle needs 2 more ticks to re-rescue
  it('T-S9-W6: after rescue, stuckTicks=0; next stuck tick does NOT immediately re-rescue', async () => {
    // First rescue: pre-arm to threshold
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    await vi.advanceTimersByTimeAsync(2000); // rescue fires, stuckTicks reset to 0

    const rescuesAfterFirst = h.exports.getWatchdogRescues();
    expect(rescuesAfterFirst).toBeGreaterThan(0);

    // Now simulate another tick with no-progress from the rescue landing position
    // The watchdog should NOT re-rescue on this tick (stuckTicks was just reset to 0 → now 1)
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000); // stuckTicks=0 → increments to 1, NOT rescue yet

    // No additional rescue on this tick
    expect(h.exports.getWatchdogRescues()).toBe(rescuesAfterFirst);
  });

  // T-S9-W7 (reconcile #7): clamp-null / sliver-only-ahead → NO rescue AND stuckTicks NOT reset.
  // Geometry: all-sliver buffer [[0,0.2],[0.5,0.7],[1.0,1.2]] (every range < SNAP_SLIVER_MIN_SEC=0.3),
  //   ct=0.0 stuck, pre-armed to threshold (stuckTicks=1, lastTickCt=0.0).
  // Tick path: dataAhead = bufEnd(1.2) > ct(0.0)+WATCHDOG_DATA_AHEAD_SEC(0.5) ✓; no progress
  //   → stuckTicks 1→2 → enters rescue block → clampSnapTarget returns null (no substantial range
  //   to land in) → wTarget===null → NO seek, NO counter reset.
  // Invariant: the watchdog must NOT silently swallow the stuck state — stuckTicks stays at 2 so
  // the next tick (once a substantial range arrives) retries immediately instead of re-counting
  // from zero. Pins the no-rescue-on-null branch AND the no-reset retry behavior.
  it('T-S9-W7: only slivers ahead (clamp null) → NO watchdog_snap fires AND stuckTicks NOT reset (retries next tick)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 0.0 });
    h.videoEl.currentTime = 0.0; // stuck — no progress
    h.sb.buffered = makeBufferedMulti([[0, 0.2], [0.5, 0.7], [1.0, 1.2]]); // all slivers, data ahead

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // No rescue: clampSnapTarget returned null.
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(false);
    // stuckTicks NOT reset (reached 2, no rescue → stays armed so the next tick retries).
    expect(h.exports.getWatchdogState().watchdogStuckTicks).toBe(2);
  });

  // T-S9-W8 (FIX-6): watchdog forward-only guard. clampSnapTarget resolves to a value <= ct
  //   (via the last-substantial fallback / only-behind geometry) → the `wTarget > wCt` guard
  //   MUST block the rescue. Pins forward-only and KILLS dropping `wTarget > wCt`.
  // Geometry: substantial range BEHIND ct [0,5.0] plus a trailing sliver [8.0,8.2]; ct=7.0.
  //   dataAhead = bufEnd(8.2) > ct(7.0)+0.5 ✓; pre-armed stuck (no progress) → reaches rescue block.
  //   wRaw = bufEnd(8.2) - LIVE_EDGE_TARGET_LEAD_SEC(0.45) = 7.75 → in the gap [5.0,8.0] below the
  //   sliver [8.0,8.2] (which is < SNAP_SLIVER_MIN_SEC=0.3 anyway) → no forward substantial
  //   range → last-substantial fallback → start of [0,5.0] = 0.0, which is <= ct(7.0).
  //   Forward-only guard (wTarget > wCt) → 0.0 > 7.0 is false → NO rescue.
  // If `wTarget > wCt` is dropped the watchdog would seek BACKWARD to 0.0 → this test FAILS.
  it('T-S9-W8: clamp resolves <= ct (only-behind substantial range) → forward-only guard blocks rescue (kills dropping wTarget>wCt)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 7.0 });
    h.videoEl.currentTime = 7.0; // stuck — no progress
    h.sb.buffered = makeBufferedMulti([[0, 5.0], [8.0, 8.2]]); // substantial behind, sliver ahead

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // No rescue: clamp target (0.0) is <= ct (7.0) → forward-only guard blocks.
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(false);
  });
});

// ── T2-C: watchdog forces through sb.updating ────────────────────────────────
describe('watchdog — forces through sb.updating (T-S9-U1, T-S9-U2)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-U1: sb.updating=true, watchdog pre-armed → rescue fires anyway
  it('T-S9-U1: sb.updating=true, pre-armed watchdog → rescue fires (no sb.updating guard in watchdog)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    h.sb.updating = true; // SourceBuffer is updating

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // Watchdog fires despite sb.updating (no sb.updating guard)
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore + 1);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(true);
  });

  // T-S9-U2: sb.updating=true blocks seekToLiveEdge but NOT watchdog (contrast test)
  it('T-S9-U2: sb.updating=true blocks seekToLiveEdge but watchdog is the only path that unblocks', async () => {
    // seekToLiveEdge direct call: should be blocked by sb.updating
    h.sb.updating = true;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    h.videoEl.currentTime = 5.0;
    const ctBefore = h.videoEl.currentTime;

    h.exports.seekToLiveEdge();

    // seekToLiveEdge blocked: currentTime unchanged
    expect(h.videoEl.currentTime).toBe(ctBefore);
  });
});

// ── T2-D: watchdog respects VIDEO_EL.seeking ─────────────────────────────────
describe('watchdog — respects VIDEO_EL.seeking (T-S9-SK1)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-SK1: VIDEO_EL.seeking=true → watchdog does NOT rescue
  it('T-S9-SK1: VIDEO_EL.seeking=true with pre-armed watchdog → NO rescue (watchdog respects seeking guard)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    // Override seeking to true
    h.overrideProperty(h.videoEl, 'seeking', { get: () => true });

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // No rescue: seeking guard prevents double-seek
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(false);
  });

  // T-S9-SK2 (FIX-7, judgment R2): no-double-seek in the SAME tick, pinned at the
  //   seekToLiveEdge SEEKING guard (L842) — NOT the drift gate (L853).
  //   The watchdog runs BEFORE seekToLiveEdge in the heartbeat. The watchdog rescue's
  //   currentTime assignment flips seeking=true; the subsequent seekToLiveEdge() call
  //   (guard: `if (VIDEO_EL.seeking) return`) MUST be a no-op — it must return at L842
  //   BEFORE reaching the drift gate or any log line.
  // Geometry (FAR-BACK landing so the drift gate canNOT mask the seeking guard):
  //   buf [[0,4.0],[8.0,12.0],[19.85,20.0]], wCt=5.0, watchdogLastTickCt=5.0.
  //   Watchdog: dataAhead = bufEnd(20.0) > wCt(5.0)+0.5 ✓; progressed = 5.0 > 5.0+0.5 false →
  //     stuck 1→2 ✓. wRaw = bufEnd(20.0) - LIVE_EDGE_TARGET_LEAD_SEC(0.45) = 19.55 → in gap
  //     [12.0, 19.85]; forward range [19.85,20.0] is a sliver (0.15 < 0.3) → step-4 fallback →
  //     last substantial range start = [8.0,12.0].start = 8.0. forward-only 8.0 > 5.0 ✓ → rescue
  //     lands ct=8.0. POST-RESCUE drift = bufEnd(20.0) - 8.0 = 12.0 > LIVE_EDGE_MAX_DRIFT_SEC(0.5),
  //     so the drift gate does NOT fire — the seeking guard is the ONLY thing that can suppress
  //     the second seek.
  //   The rescue assignment flips seeking=true (real-element seam below); seekToLiveEdge then
  //   returns at the L842 seeking guard, BEFORE the drift computation, so it emits NO seek line.
  // Setup: override the currentTime SETTER so any assignment flips seeking=true (mimicking the
  //   real <video> element starting a seek), via the existing overrideProperty/defineProperty seam.
  // Mutation kill (verified): removing the L842 seeking guard lets seekToLiveEdge proceed past it;
  //   with drift 12.0 > 0.5 the drift gate passes and seekToLiveEdge emits a SECOND seek line
  //   (result=guard_backward, since target 8.0 <= new ct 8.0). With the guard present that line is
  //   absent. Asserting ZERO result=snap AND ZERO result=guard_backward pins the seeking guard.
  it('T-S9-SK2: watchdog rescue lands far behind live (drift>0.5) and flips seeking=true → seekToLiveEdge bails at the L842 seeking guard, NO second seek line', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.sb.buffered = makeBufferedMulti([[0, 4.0], [8.0, 12.0], [19.85, 20.0]]);

    // Backing store for currentTime; the setter flips seeking=true on assignment (real-element seam).
    let _ct = 5.0;
    let _seeking = false;
    h.overrideProperty(h.videoEl, 'seeking', { get: () => _seeking, configurable: true });
    h.overrideProperty(h.videoEl, 'currentTime', {
      get: () => _ct,
      set: (v) => { _ct = v; _seeking = true; }, // assignment starts a seek
      configurable: true,
    });

    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const logLines = getMseLogLines(h.tauri);
    // Watchdog rescue fired exactly once, landing far behind live (step-4 fallback → 8.000).
    const watchdogLines = logLines.filter(l => l.includes('result=watchdog_snap'));
    expect(watchdogLines.length).toBe(1);
    expect(watchdogLines[0]).toMatch(/ to=8\.000 /);
    // seekToLiveEdge produced NO second seek line of ANY kind: it returned at the seeking guard
    // (L842) BEFORE the drift gate. With the seeking guard removed it would emit a second line
    // (drift 12.0 > 0.5 passes the drift gate), so both of these pin the seeking guard.
    expect(logLines.some(l => l.includes('result=snap'))).toBe(false);
    expect(logLines.some(l => l.includes('result=guard_backward'))).toBe(false);
  });
});

// ── T2-E: watchdog bypasses N1/N2 ────────────────────────────────────────────
describe('watchdog — bypasses N1/N2 effectiveness+debounce (T-S9-N1, T-S9-N2)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-N1: N2 debounce would fire (lastSnapAt within 300ms) but watchdog fires anyway
  it('T-S9-N1: lastSnapAt within 300ms window (N2 would suppress) but watchdog rescue still fires', async () => {
    // Simulate N2 debounce scenario: set lastSnapAtMs to recent time (50ms ago)
    h.perfNow(50); // current perf.now = 50ms
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 5.0, lastSnapBufEnd: 10.0 });
    // Arm watchdog at threshold
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // Watchdog fires despite N2 debounce being active
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore + 1);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(true);
  });

  // T-S9-N2: N1 effectiveness would fire (no ct/bufEnd progress vs lastSnap*) but watchdog fires anyway
  it('T-S9-N2: no ct/bufEnd progress vs lastSnap* (N1 would suppress) but watchdog rescue still fires', async () => {
    h.perfNow(0);
    // Set lastSnap* to exact same ct/bufEnd as current state (N1 would suppress)
    h.exports.setLastSnapState({ lastSnapAtMs: 0, lastSnapCt: 5.0, lastSnapBufEnd: 10.0 });
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    // Watchdog structurally bypasses N1/N2 (they live in onVideoWaiting, not heartbeat)
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore + 1);
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some(l => l.includes('result=watchdog_snap'))).toBe(true);
  });
});

// ── T2-F: no-hole guarantee on all 3 snap paths ──────────────────────────────
describe('no-hole guarantee — all 3 snap paths (T-S9-H1..H3)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // ── GEOMETRY NOTE (judgment R2; S10 lead-raise update) ────────────────────────
  // All three paths now compute rawTarget = bufEnd - LEAD with a SINGLE lead value:
  // LIVE_EDGE_TARGET_LEAD_SEC === LIVE_EDGE_STALL_SNAP_LEAD_SEC === 0.45 (D-PPT10-A).
  // The clamp's step-3 (forward-cross) branch requires a SUBSTANTIAL forward range
  // (span >= SNAP_SLIVER_MIN_SEC = 0.3) that STARTs after rawTarget and ENDs at or before
  // bufEnd. Such a range must fit a >= 0.3 span into a window of width < LEAD before bufEnd:
  // under the old 0.2/0.3 leads (window < 0.3) that was IMPOSSIBLE; under the 0.45 lead the
  // window is < 0.45, so a [0.3, 0.45)-span trailing range CAN now trigger step-3 at
  // integration — it is NO LONGER provably unreachable. The step-3 branch is pinned directly
  // at unit level by T-S9-C2/C3/C4 (which pass rawTarget explicitly, decoupled from bufEnd-LEAD).
  // H1/H2/H3 below DELIBERATELY use a trailing sliver (span 0.1 < 0.3) so step-3 is skipped and
  // the clamp resolves via step-4 (last-substantial fallback). For these gap geometries the
  // exact landing (start of [9.5,10.0] = 9.500) is LEAD-VERSION-INDEPENDENT: regardless of
  // whether rawTarget is 11.5 (old 0.2 lead) or 11.25 (0.45 lead), rawTarget lands in the same
  // gap [10.0, 11.6], the trailing range is a sliver, and step-4 dominates → 9.500. The geometry
  // keeps the fallback landing ~2.2 s behind live, keeping the clamp load-bearing without the
  // artificial far-behind framing.

  // T-S9-H1: watchdog path — rawTarget MUST land in a GAP so the clamp is load-bearing.
  // Geometry: [[0,0.5],[9.5,10.0],[11.6,11.7]], wCt=5.0.
  //   wRaw = bufEnd(11.7) - LIVE_EDGE_TARGET_LEAD_SEC(0.45) = 11.25 → in gap [10.0, 11.6].
  //   clampSnapTarget: forward range [11.6,11.7] is a sliver (0.1 < 0.3) → skipped; no forward
  //   substantial range → last-substantial fallback → start of [9.5,10.0] = 9.500.
  // Preconditions: dataAhead = bufEnd(11.7) > wCt(5.0)+WATCHDOG_DATA_AHEAD_SEC(0.5) ✓;
  //   progressed = wCt(5.0) > watchdogLastTickCt(5.0)+WATCHDOG_PROGRESS_EPS(0.5) false → stuck
  //   1→2 ✓; forward-only 9.5 > 5.0 ✓. Post-snap drift = 11.7 - 9.5 = 2.2 s (tightened).
  // Strict no-hole pin: to= must EXACTLY equal 9.500 (lead-version-independent: rawTarget lands in
  //   the same gap and step-4 dominates). With the 0.45 lead a raw-passthrough mutant fires
  //   (11.25 > 5.0) and lands in the gap → to=11.250. The negative-guard assertion below pins the
  //   LIVE reachable clamp-bypass value (11.250) under the 0.45 lead, so it is load-bearing: a
  //   raw-passthrough mutant emits to=11.250 and the not.toMatch fails. The positive to=9.500 match
  //   and toBeCloseTo(9.500) pin the real clamped landing — lead-independent.
  it('T-S9-H1: watchdog rawTarget in gap [[0,0.5],[9.5,10.0],[11.6,11.7]] ct=5.0 → rescue to=9.500 (step-4 clamp redirect, NOT gap value 11.250)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 0.5], [9.5, 10.0], [11.6, 11.7]]);

    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const logLines = getMseLogLines(h.tauri);
    const watchdogLine = logLines.find(l => l.includes('result=watchdog_snap'));
    // The seek line MUST exist (unconditional — no guard).
    expect(watchdogLine).toBeTruthy();
    // Exact no-hole pin: clamp redirected the in-gap rawTarget (11.25) to the substantial range start.
    expect(watchdogLine).toMatch(/ to=9\.500 /);
    expect(watchdogLine).not.toMatch(/ to=11\.250 /);
    const toMatch = watchdogLine.match(/to=(\d+\.\d+)/);
    expect(parseFloat(toMatch[1])).toBeCloseTo(9.500, 5);
  });

  // T-S9-H2: seekToLiveEdge path — rawTarget MUST land in a GAP so the clamp is load-bearing.
  // Geometry: [[0,0.5],[9.5,10.0],[11.6,11.7]], ct=1.0.
  //   rawTarget = bufEnd(11.7) - LIVE_EDGE_TARGET_LEAD_SEC(0.45) = 11.25 → in gap [10.0, 11.6].
  //   clampSnapTarget: forward range [11.6,11.7] is a sliver (0.1 < 0.3) → skipped; no forward
  //   substantial range → last-substantial fallback → start of [9.5,10.0] = 9.500.
  // Preconditions: drift = bufEnd(11.7) - ct(1.0) = 10.7 > LIVE_EDGE_MAX_DRIFT_SEC(0.5) ✓;
  //   forward-only target 9.5 > ct 1.0 ✓. Post-snap drift = 11.7 - 9.5 = 2.2 s (tightened).
  // Strict no-hole pin: to= must EXACTLY equal 9.500 (lead-version-independent: rawTarget lands in
  //   the same gap and step-4 dominates). With the 0.45 lead a raw-passthrough mutant lands at
  //   to=11.250. The negative-guard assertion below pins the LIVE reachable clamp-bypass value
  //   (11.250) under the 0.45 lead, so it is load-bearing: a raw-passthrough mutant emits to=11.250
  //   and the not.toMatch fails. The positive to=9.500 match and the toBeCloseTo(9.500) check pin
  //   the real clamped landing, which are lead-independent.
  it('T-S9-H2: seekToLiveEdge rawTarget in gap [[0,0.5],[9.5,10.0],[11.6,11.7]] ct=1.0 → snap to=9.500 (step-4 clamp redirect, NOT gap value 11.250)', () => {
    h.sb.buffered = makeBufferedMulti([[0, 0.5], [9.5, 10.0], [11.6, 11.7]]);
    h.videoEl.currentTime = 1.0;
    h.tauri.invoke.mock.calls.length = 0;

    h.exports.seekToLiveEdge();

    const logLines = getMseLogLines(h.tauri);
    const snapLine = logLines.find(l => l.includes('result=snap'));
    // The seek line MUST exist (unconditional — no guard).
    expect(snapLine).toBeTruthy();
    // Exact no-hole pin: clamp redirected the in-gap rawTarget (11.25) to the substantial range start.
    expect(snapLine).toMatch(/ to=9\.500 /);
    expect(snapLine).not.toMatch(/ to=11\.250 /);
    const toMatch = snapLine.match(/to=(\d+\.\d+)/);
    expect(parseFloat(toMatch[1])).toBeCloseTo(9.500, 5);
  });

  // T-S9-H3: onVideoWaiting path — rawTarget MUST land in a GAP so the clamp is load-bearing.
  // Geometry: [[0,0.5],[9.5,10.0],[11.6,11.7]], ct=1.0.
  //   rawTarget = bufEnd(11.7) - LIVE_EDGE_STALL_SNAP_LEAD_SEC(0.45) = 11.25 → in gap [10.0, 11.6].
  //   clampSnapTarget: forward range [11.6,11.7] is a sliver (0.1 < 0.3) → skipped; no forward
  //   substantial range → last-substantial fallback → start of [9.5,10.0] = 9.500.
  // Preconditions: N1 (fresh lastSnap*=-Infinity → advanced ✓); N2 (lastSnapAtMs=-Infinity →
  //   not debounced ✓); N3 target(9.5) !== ct(1.0) ✓; G6 cushion = bufEnd(11.7)-target(9.5) =
  //   2.2 >= LIVE_EDGE_STALL_MIN_CUSHION_SEC(0.1) ✓. Post-snap drift = 2.2 s (tightened).
  // Strict no-hole pin: to= must EXACTLY equal 9.500 (lead-version-independent: rawTarget lands in
  //   the same gap and step-4 dominates). With the 0.45 lead a raw-passthrough mutant lands at
  //   to=11.250. The negative-guard assertion below pins the LIVE reachable clamp-bypass value
  //   (11.250) under the 0.45 lead, so it is load-bearing: a raw-passthrough mutant emits to=11.250
  //   and the not.toMatch fails. The load-bearing pins are also the positive to=9.500 match and
  //   toBeCloseTo(9.500) — lead-independent.
  it('T-S9-H3: onVideoWaiting rawTarget in gap [[0,0.5],[9.5,10.0],[11.6,11.7]] ct=1.0 → stall_snap to=9.500 (step-4 clamp redirect, NOT gap value 11.250)', () => {
    h.sb.buffered = makeBufferedMulti([[0, 0.5], [9.5, 10.0], [11.6, 11.7]]);
    h.videoEl.currentTime = 1.0;
    h.perfNow(0);
    h.tauri.invoke.mock.calls.length = 0;

    h.exports.onVideoWaiting();

    const logLines = getMseLogLines(h.tauri);
    const stall = logLines.find(l => l.includes('result=stall_snap'));
    // The seek line MUST exist (unconditional — no guard).
    expect(stall).toBeTruthy();
    // Exact no-hole pin: clamp redirected the in-gap rawTarget (11.25) to the substantial range start.
    expect(stall).toMatch(/ to=9\.500 /);
    expect(stall).not.toMatch(/ to=11\.250 /);
    const toMatch = stall.match(/to=(\d+\.\d+)/);
    expect(parseFloat(toMatch[1])).toBeCloseTo(9.500, 5);
  });
});

// ── T2-G: rs<=2 escape hatch + re-storm defense ──────────────────────────────
describe('rs<=2 escape hatch (Mechanism C) + re-storm defense (T-S9-R1..R3)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-R1: rs=2 inside 300ms window WITH progress → executes (was suppressed under Slice 8 rs<=1)
  // S10 update: 2-strike gate requires streak>=2. Pre-seed streak=1 so this call
  // reaches streak=2 and still bypasses (C1 remediation). Lead updated to 0.45.
  it('T-S9-R1: rs=2 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, stall_snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50); // inside 300ms
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.019; // advanced > ADV_EPS → N1 passes
    Object.defineProperty(h.videoEl, 'readyState', { value: 2, configurable: true }); // rs=2 escape hatch
    h.exports.setHardStarveStreak(1); // pre-seed: this call → streak 1→2 → bypass fires

    h.exports.onVideoWaiting();

    // N2 bypassed (rs=2 IS escape hatch with streak>=2): snap executes (drift=0.311 > 0.300)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.450, 5);
  });

  // T-S9-R2: rs=2, NO ct/bufEnd progress → N1 fires (re-storm defense intact)
  it('T-S9-R2: rs=2 inside window, NO ct/bufEnd progress → N1 fires (suppressedGuardCount++), no execute', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1, records ct=10.016, bufEnd=10.030

    h.perfNow(50);
    // ct and bufEnd NOT advanced (still at snap#1 baseline) → N1 fires
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    Object.defineProperty(h.videoEl, 'readyState', { value: 2, configurable: true });

    h.exports.onVideoWaiting();

    // N1 fired first; no execute (re-storm defense)
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
  });

  // T-S9-R3: rs=3 inside 300ms → still debounced by N2 (N2 bypass only for rs<=2)
  it('T-S9-R3: rs=3 inside 300ms window, ct/bufEnd advanced → N2 applies (rs=3 not escape hatch)', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.332);
    h.videoEl.currentTime = 10.018;
    // rs=3 is NOT the escape hatch; readyState default is 4, set to 3 explicitly
    Object.defineProperty(h.videoEl, 'readyState', { value: 3, configurable: true });
    // Seed streak=0 so the next line pins that a hardStarve=false (rs>2) call MUST NOT increment.
    h.exports.setHardStarveStreak(0);

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(1); // N2 still fires
    // REQ-S10-OW3 (direct pin): the increment is `if (hardStarve)`-guarded, so an rs=3
    // (hardStarve=false) invocation must NOT accrue a strike. Mutating the guard to
    // `if (true)` would make this rs=3 call increment → streak 0→1 → this fails.
    expect(h.exports.getHardStarveStreak()).toBe(0);
  });
});

// ── T2-H: watchdog telemetry ──────────────────────────────────────────────────
describe('watchdog telemetry — watchdog_snap signal + watchdog_rescues counter (T-S9-T1..T5)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-T1: rescue log line matches expected format
  it('T-S9-T1: rescue emits log line matching /event=seek result=watchdog_snap from=N to=N drift=N/', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const logLines = getMseLogLines(h.tauri);
    const watchdogLine = logLines.find(l => l.includes('result=watchdog_snap'));
    expect(watchdogLine).toBeTruthy();
    expect(watchdogLine).toMatch(/event=seek result=watchdog_snap from=\d+\.\d+ to=\d+\.\d+ drift=\d+\.\d+/);
    // to= value must be > ct (forward-only)
    const toMatch = watchdogLine.match(/to=(\d+\.\d+)/);
    expect(toMatch).toBeTruthy();
    expect(parseFloat(toMatch[1])).toBeGreaterThan(5.0);
  });

  // T-S9-T1b (FIX-5 / S10 update): EXACT to=/drift= pin. Single substantial range [0,10], ct=5.0.
  //   wRaw = bufEnd(10.0) - LIVE_EDGE_TARGET_LEAD_SEC(0.45) = 9.55 → inside [0,10] → pass-through.
  //   wDrift = bufEnd(10.0) - ct(5.0) = 5.0.
  // Pins to=9.550 AND drift=5.000 EXACTLY. Kills a mutated lead in `wRaw = wBufEnd -
  // LIVE_EDGE_TARGET_LEAD_SEC` (any other lead shifts to= off 9.550) and a mutated drift formula.
  it('T-S9-T1b: watchdog rescue emits EXACT to=9.550 drift=5.000 for buf [[0,10]] lead 0.45 ct 5.0 (kills mutated lead; S10)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const logLines = getMseLogLines(h.tauri);
    const watchdogLine = logLines.find(l => l.includes('result=watchdog_snap'));
    expect(watchdogLine).toBeTruthy();
    expect(watchdogLine).toMatch(/event=seek result=watchdog_snap from=5\.000 to=9\.550 drift=5\.000/);
  });

  // T-S9-T2: monotonic counter — 3 rescues → getWatchdogRescues() === 3
  it('T-S9-T2: 3 sequential rescues → getWatchdogRescues() monotonically increases to 3', async () => {
    for (let i = 0; i < 3; i++) {
      h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
      h.videoEl.currentTime = 5.0;
      h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
      await vi.advanceTimersByTimeAsync(2000);
    }
    expect(h.exports.getWatchdogRescues()).toBe(3);
  });

  // T-S9-T3: watchdog_rescues is the trailing field on tick lines
  it('T-S9-T3: tick line trailing field matches /watchdog_rescues=(\\d+)$/ equaling getWatchdogRescues()', async () => {
    // Fire a rescue
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    await vi.advanceTimersByTimeAsync(2000);

    const rescueCount = h.exports.getWatchdogRescues();
    expect(rescueCount).toBeGreaterThan(0);

    // Fire a clean tick (watchdog won't re-rescue since stuckTicks reset)
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const tickLines = getTickLines(h.tauri);
    expect(tickLines.length).toBeGreaterThan(0);
    const lastTick = tickLines[tickLines.length - 1];
    expect(lastTick).toMatch(/watchdog_rescues=(\d+)$/);
    const m = lastTick.match(/watchdog_rescues=(\d+)$/);
    expect(parseInt(m[1], 10)).toBe(h.exports.getWatchdogRescues());
  });

  // T-S9-T4: getWatchdogRescues() persists across tearDownMse
  it('T-S9-T4: watchdogRescues persists across tearDownMse (module-scope, not mseState-scoped)', async () => {
    // Rescue once
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    await vi.advanceTimersByTimeAsync(2000);

    const rescuesBefore = h.exports.getWatchdogRescues();
    expect(rescuesBefore).toBeGreaterThan(0);

    // tearDownMse and re-setup
    h.exports.tearDownMse();

    // Counter unchanged after teardown
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore);
  });

  // T-S9-T5: WATCHDOG_PROGRESS_EPS=0.5 is meaningful — small creep reads as stuck
  it('T-S9-T5: ct creeping 0.013s/tick (< WATCHDOG_PROGRESS_EPS=0.5) → reads as stuck, watchdog fires', async () => {
    // Simulate GATE-8 freeze creep: 0.013s/tick
    // Pre-arm: lastTickCt=5.0, stuckTicks=1
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.013; // advanced only 0.013s < WATCHDOG_PROGRESS_EPS(0.5) → stuck
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesBefore = h.exports.getWatchdogRescues();
    await vi.advanceTimersByTimeAsync(2000);

    // 0.013 < 0.5 → still "stuck" → rescue fires on this tick (stuckTicks 1→2)
    expect(h.exports.getWatchdogRescues()).toBe(rescuesBefore + 1);

    // Counter-test: ct increment of 0.6s/tick (> WATCHDOG_PROGRESS_EPS) → progresses, no rescue
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.6; // 0.6 > 0.5 → progressed
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    const rescuesAfterFirstSet = h.exports.getWatchdogRescues();
    await vi.advanceTimersByTimeAsync(2000);

    // No rescue: ct considered progressing
    expect(h.exports.getWatchdogRescues()).toBe(rescuesAfterFirstSet);
  });
});

// ── T2-I: Slice 9 seam exports and constants ─────────────────────────────────
describe('Slice 9 seam exports and constants (T-S9-E1..E8)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S9-E1: WATCHDOG_STUCK_TICKS === 2
  it('T-S9-E1: WATCHDOG_STUCK_TICKS exported === 2', () => {
    expect(h.exports.WATCHDOG_STUCK_TICKS).toBe(2);
  });

  // T-S9-E2: WATCHDOG_PROGRESS_EPS === 0.5
  it('T-S9-E2: WATCHDOG_PROGRESS_EPS exported === 0.5', () => {
    expect(h.exports.WATCHDOG_PROGRESS_EPS).toBe(0.5);
  });

  // T-S9-E3: WATCHDOG_DATA_AHEAD_SEC === 0.5
  it('T-S9-E3: WATCHDOG_DATA_AHEAD_SEC exported === 0.5', () => {
    expect(h.exports.WATCHDOG_DATA_AHEAD_SEC).toBe(0.5);
  });

  // T-S9-E4: SNAP_SLIVER_MIN_SEC === 0.3
  it('T-S9-E4: SNAP_SLIVER_MIN_SEC exported === 0.3', () => {
    expect(h.exports.SNAP_SLIVER_MIN_SEC).toBe(0.3);
  });

  // T-S9-E5: getWatchdogRescues is a function that returns a number
  it('T-S9-E5: getWatchdogRescues is a function returning a number', () => {
    expect(typeof h.exports.getWatchdogRescues).toBe('function');
    expect(typeof h.exports.getWatchdogRescues()).toBe('number');
  });

  // T-S9-E6: setWatchdogState and getWatchdogState are functions
  it('T-S9-E6: setWatchdogState and getWatchdogState are both functions', () => {
    expect(typeof h.exports.setWatchdogState).toBe('function');
    expect(typeof h.exports.getWatchdogState).toBe('function');
  });

  // T-S9-E7: clampSnapTarget is a function
  it('T-S9-E7: clampSnapTarget is a function', () => {
    expect(typeof h.exports.clampSnapTarget).toBe('function');
  });

  // T-S9-E8: getWatchdogRescues returns live value (not snapshot-at-0)
  it('T-S9-E8: getWatchdogRescues returns live value — rescue once → getter returns 1 (not snapshotted 0)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);
    await vi.advanceTimersByTimeAsync(2000);

    expect(h.exports.getWatchdogRescues()).toBe(1);
  });
});

// ═══════════════════════════════════════════════════════════════════════════════
// SLICE 10 — Steady-State Catch-Up Stutter Fix: Lead-Raise 0.45 + rs<=2 2-Strike
// T-S10-* tests (RED before GREEN per strict TDD; all FAIL until GREEN implementation)
// ═══════════════════════════════════════════════════════════════════════════════

// ── T-S10-const: new constants + seam exports ────────────────────────────────
describe('S10 constants and seam exports (T-S10-const)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S10-const-1: HARDSTARVE_STRIKE_TICKS exported === 2
  it('T-S10-const-1: HARDSTARVE_STRIKE_TICKS exported and === 2', () => {
    expect(h.exports.HARDSTARVE_STRIKE_TICKS).toBe(2);
  });

  // T-S10-const-2: LIVE_EDGE_TARGET_LEAD_SEC === 0.45
  it('T-S10-const-2: LIVE_EDGE_TARGET_LEAD_SEC exported and === 0.45', () => {
    expect(h.exports.LIVE_EDGE_TARGET_LEAD_SEC).toBe(0.45);
  });

  // T-S10-const-3: LIVE_EDGE_STALL_SNAP_LEAD_SEC === 0.45
  it('T-S10-const-3: LIVE_EDGE_STALL_SNAP_LEAD_SEC exported and === 0.45', () => {
    expect(h.exports.LIVE_EDGE_STALL_SNAP_LEAD_SEC).toBe(0.45);
  });

  // T-S10-const-4: LIVE_EDGE_MAX_DRIFT_SEC === 0.5 (unchanged sanity pin)
  it('T-S10-const-4: LIVE_EDGE_MAX_DRIFT_SEC unchanged === 0.5', () => {
    expect(h.exports.LIVE_EDGE_MAX_DRIFT_SEC).toBe(0.5);
  });

  // T-S10-const-5: getHardStarveStreak is a function (not bare primitive)
  it('T-S10-const-5: getHardStarveStreak exported as a function (not bare primitive)', () => {
    expect(typeof h.exports.getHardStarveStreak).toBe('function');
  });

  // T-S10-const-6: getHardStarveStreak() returns 0 on fresh module load
  it('T-S10-const-6: getHardStarveStreak() === 0 on fresh module (initialized to 0)', () => {
    expect(h.exports.getHardStarveStreak()).toBe(0);
  });

  // T-S10-const-7: setHardStarveStreak is a function
  it('T-S10-const-7: setHardStarveStreak exported as a function', () => {
    expect(typeof h.exports.setHardStarveStreak).toBe('function');
  });
});

// ── T-S10-STK: 2-strike state machine ────────────────────────────────────────
describe('S10 2-strike state machine (T-S10-STK-*)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S10-STK-1: single 1-shot rs=2 dip (streak 0→1) → N2 NOT bypassed
  // Core fix assertion: a transient 1-shot rs=2 edge-dip is now debounced (was bypassed in S9).
  it('T-S10-STK-1: single rs=2 dip inside 300ms window (streak 0→1) → N2 fires, snap suppressed; streak=1', () => {
    // snap#1 at perfNow=0 (establishes baseline, streak stays at 0 after snap#1: rs=4 default)
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1; readyState=4 (default) → hardStarve=false, no increment

    // 50ms later (inside 300ms debounce window): ct/bufEnd advance (N1 passes), rs=2
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.332);
    h.videoEl.currentTime = 10.018; // set ct to 10.018 for N1 to pass
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });
    // streak is 0 before this call; this is the 1-shot transient dip
    const ctBefore2ndCall = h.videoEl.currentTime; // 10.018 — the test manually set it

    h.exports.onVideoWaiting();

    // 2-strike gate: !(true && 1>=2) = !(false) = true AND inside window → N2 fires, snap suppressed
    expect(h.exports.getSuppressedDebounceCount()).toBe(1);
    expect(h.videoEl.currentTime).toBe(ctBefore2ndCall); // currentTime unchanged (10.018, not snapped)
    expect(h.exports.getHardStarveStreak()).toBe(1);   // streak incremented to 1
  });

  // T-S10-STK-2: persisted rs=2 (streak pre-seeded to 1) → streak=2, N2 bypass fires
  // Genuine starvation recovers fast: on the 2nd consecutive rs=2 call the bypass fires.
  it('T-S10-STK-2: seed streak=1, rs=2 inside window, ct/bufEnd advanced → streak=2, N2 bypass, snap at bufEnd-0.45', () => {
    // snap#1 to set baseline
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    // seed streak=1 (simulates one prior rs=2 event having already occurred)
    h.exports.setHardStarveStreak(1);

    // 50ms later: ct/bufEnd advance (N1 passes), rs=2 again → streak 1→2 → bypass fires
    h.perfNow(50);
    const bufEnd2 = 10.330;
    h.sb.buffered = makeBuffered(0, bufEnd2);
    h.videoEl.currentTime = 10.018;
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });

    const debounceBeforeSnap2 = h.exports.getSuppressedDebounceCount();
    h.exports.onVideoWaiting();

    // 2-strike gate: !(true && 2>=2) = !(true) = false → bypass; snap fires
    expect(h.exports.getSuppressedDebounceCount()).toBe(debounceBeforeSnap2); // unchanged (bypass, no suppression)
    expect(h.videoEl.currentTime).toBeCloseTo(bufEnd2 - 0.45, 5);
    expect(h.exports.getHardStarveStreak()).toBe(2);
  });

  // T-S10-STK-reset-positive: heartbeat tick at rs=4 resets streak to 0
  it('T-S10-STK-reset-positive: setHardStarveStreak(2), heartbeat tick with readyState=4 → getHardStarveStreak()===0', async () => {
    h.exports.setHardStarveStreak(2);
    // Override readyState to 4 (player recovered) for the heartbeat to observe
    h.overrideProperty(h.videoEl, 'readyState', { value: 4, configurable: true });
    h.sb.buffered = makeBuffered(0, 10.030);

    await vi.advanceTimersByTimeAsync(2000); // one heartbeat tick

    expect(h.exports.getHardStarveStreak()).toBe(0);
  });

  // T-S10-STK-reset-negative: heartbeat tick at rs=2 does NOT reset streak (still starving)
  it('T-S10-STK-reset-negative: setHardStarveStreak(2), heartbeat tick with readyState=2 → streak stays 2 (pins >2 boundary)', async () => {
    h.exports.setHardStarveStreak(2);
    // Override readyState to 2 (still starving)
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });
    h.sb.buffered = makeBuffered(0, 10.030);

    await vi.advanceTimersByTimeAsync(2000); // one heartbeat tick

    expect(h.exports.getHardStarveStreak()).toBe(2); // NOT reset: 2 is NOT > 2
  });

  // T-S10-STK-no-reset-on-snap: snap path does NOT touch the streak
  it('T-S10-STK-no-reset-on-snap: seed streak=2, fire bypassing rs=2 waiting → snap executes AND streak stays >=2', () => {
    // snap#1 to set baseline
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    // seed streak=2 (already at bypass threshold)
    h.exports.setHardStarveStreak(2);

    // fire a bypassing rs=2 waiting inside window (N1 passes → bypass fires → snap executes)
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.018;
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });

    h.exports.onVideoWaiting(); // snap executes (streak=2→3 due to increment, but NOT reset)

    // snap path (L946-960) NEVER touches hardStarveStreak. Exact pin: seed 2 + exactly one
    // post-N1 increment on this bypassing rs=2 snap = 3. Pins BOTH "no reset" AND "exactly
    // one increment" — a deterministic value, not a loose floor.
    expect(h.exports.getHardStarveStreak()).toBe(3);
    // also verify the snap actually executed (currentTime changed to bufEnd-0.45)
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.45, 5);
  });

  // T-S10-STK-after-N1: N1 fires (no progress) → streak NOT incremented
  // Pins increment-after-N1; kills the increment-before-N1 mutant.
  it('T-S10-STK-after-N1: rs=2 with no ct/bufEnd progress → N1 fires AND getHardStarveStreak() unchanged at 0', () => {
    // snap#1 to establish baseline
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1; streak stays 0 (rs=4 default)

    // No progress: same ct and bufEnd as snap#1 baseline
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.330); // bufEnd unchanged
    h.videoEl.currentTime = 10.016;          // ct unchanged
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });

    const guardBefore = h.exports.getSuppressedGuardCount();
    h.exports.onVideoWaiting(); // N1 fires (not advanced)

    // N1 fired: suppressedGuardCount incremented; streak NOT incremented (increment is AFTER N1)
    expect(h.exports.getSuppressedGuardCount()).toBe(guardBefore + 1);
    expect(h.exports.getHardStarveStreak()).toBe(0); // unchanged: increment never reached
  });
});

// ── T-S10-REG: regression guards ─────────────────────────────────────────────
describe('S10 regression guards (T-S10-REG-*)', () => {
  let h;

  beforeEach(async () => { h = await makeS8Harness(); });
  afterEach(() => teardownS8Harness(h));

  // T-S10-REG-1: persistent rs=2 stall recovers on the 2nd consecutive rs=2 call
  // Pins Gate-10 criterion K1: no extended stall; 2nd strike → snap executes.
  it('T-S10-REG-1: persistent rs=2 stall — snap#1 at t=0, 2nd rs=2 call (streak=2) → snap executes at bufEnd-0.45', () => {
    // snap#1 establishes baseline (streak=0, rs=4 default → no increment on snap#1)
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    // 1st rs=2 call inside window: ct/bufEnd advance (N1 passes), streak 0→1, N2 suppresses
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;
    h.overrideProperty(h.videoEl, 'readyState', { value: 2, configurable: true });
    h.exports.onVideoWaiting(); // streak→1, N2 fires (suppressed)

    // 2nd rs=2 call: ct/bufEnd advance again (N1 passes), streak 1→2 → bypass fires → snap executes
    h.perfNow(100);
    h.sb.buffered = makeBuffered(0, 10.330);
    h.videoEl.currentTime = 10.020;

    h.exports.onVideoWaiting(); // streak→2, bypass, snap

    // Recovery within 1 extra tick; snap at bufEnd-0.45=10.330-0.45=9.880
    expect(h.videoEl.currentTime).toBeCloseTo(10.330 - 0.45, 5);
    expect(h.exports.getHardStarveStreak()).toBe(2);
  });

  // T-S10-REG-2: watchdog backstop with new 0.45 lead — exact to=9.550 for buf[[0,10]], ct=5
  it('T-S10-REG-2: watchdog snap with lead 0.45 → exact to=9.550 drift=5.000 for buf [[0,10]] ct=5.0 (pins new lead)', async () => {
    h.exports.setWatchdogState({ watchdogStuckTicks: 1, watchdogLastTickCt: 5.0 });
    h.videoEl.currentTime = 5.0;
    h.sb.buffered = makeBufferedMulti([[0, 10.0]]);

    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000); // one heartbeat tick → watchdog fires

    const logLines = getMseLogLines(h.tauri);
    const watchdogLine = logLines.find(l => l.includes('result=watchdog_snap'));
    expect(watchdogLine).toBeTruthy();
    // wRaw = 10.0 - 0.45 = 9.55 → inside [0,10] → pass-through → to=9.550
    expect(watchdogLine).toMatch(/event=seek result=watchdog_snap from=5\.000 to=9\.550 drift=5\.000/);
  });

  // T-S10-IDP1: post-snap drift < drift gate → immediate re-snap is silent no-op
  it('T-S10-IDP1: seekToLiveEdge after snap sets ct to bufEnd-0.45 → drift=0.45 < 0.5 → no second snap', () => {
    const bufEnd = 10.0;
    h.sb.buffered = makeBuffered(0, bufEnd);
    h.videoEl.currentTime = 3.0; // drift=7.0 > 0.5 → snap fires

    h.exports.seekToLiveEdge(); // snap#1: ct → 10.0-0.45=9.55

    // drift now = bufEnd - ct = 10.0 - 9.55 = 0.45 < 0.5 → drift gate fires
    h.tauri.invoke.mockClear();
    h.exports.seekToLiveEdge(); // must be silent no-op

    const lines = getMseLogLines(h.tauri);
    expect(lines.some(l => l.includes('result=snap'))).toBe(false); // no second snap
  });
});
