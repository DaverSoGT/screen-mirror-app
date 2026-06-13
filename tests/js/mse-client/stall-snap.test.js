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
    // S7-4-SC1: mseLog called exactly once (spec "exactly once")
    expect(lines.length).toBe(1);
    // Same geometry as T-S7-2: bufEnd=10.030, ct=10.016, target=9.730, drift=0.014.
    const snapLine = lines.find((l) => l.includes('result=stall_snap'));
    expect(snapLine).toBeDefined();
    expect(snapLine).toContain('from=10.016');
    expect(snapLine).toContain('to=9.730');
    expect(snapLine).toContain('drift=0.014');
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
    // S7-4-SC1: mseLog called exactly once (spec "exactly once")
    expect(lines.length).toBe(1);
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
    // Geometry: range [5.0,5.2], ct=5.2 → target=Math.max(5.0,4.9)=5.000;
    // drift = bufEnd − ct = 5.2 − 5.2 = 0.000.
    expect(snapLine).toContain('from=5.200');
    expect(snapLine).toContain('to=5.000');
    expect(snapLine).toContain('drift=0.000');
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
    // N3: target=9.730 != ct=10.016 → passes
    // G6: 10.030-9.730=0.300 >= 0.1 → passes → snap executes
    h.perfNow(100); // arbitrary; any value: Inf elapsed from -Inf baseline
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;

    h.exports.onVideoWaiting();

    // Snap executed: currentTime set to target
    expect(h.videoEl.currentTime).toBeCloseTo(9.730, 5);
    // Counters: zero (nothing was suppressed)
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    // Log line emitted
    const snapLines = getMseLogLines(h.tauri).filter((l) => l.includes('result=stall_snap'));
    expect(snapLines.length).toBe(1);
  });

  // T-S8-25: State NOT updated on suppressed snap — write rule (S8-2-SC2)
  it('T-S8-25: state NOT updated on N1-suppressed snap; getLastSnapState unchanged from snap#1', () => {
    // Snap#1: executes at perfNow=100, records lastSnapCt=10.016, lastSnapBufEnd=10.030
    h.perfNow(100);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();
    // After snap#1: lastSnapCt=10.016, lastSnapBufEnd=10.030, lastSnapAtMs=100

    // Trigger N1 suppression: ct and bufEnd NOT advanced past ADV_EPS
    // (ct=10.016 unchanged, bufEnd=10.030 unchanged → N1 fires)
    h.perfNow(500); // well outside debounce window (N2 would pass if N1 didn't fire first)
    tauri_clearInvoke(h.tauri);
    h.exports.onVideoWaiting();

    // N1 should have fired: suppressedGuardCount incremented
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
    // State should be unchanged — use getLastSnapState to verify
    const state = h.exports.getLastSnapState();
    expect(state.lastSnapCt).toBeCloseTo(10.016, 5);
    expect(state.lastSnapBufEnd).toBeCloseTo(10.030, 5);
    expect(state.lastSnapAtMs).toBe(100); // unchanged from snap#1
  });

  // T-S8-26: State IS updated on executed snap (S8-7 / S8-2)
  it('T-S8-26: state IS updated after executed snap#2; getLastSnapState reflects snap#2 values', () => {
    // Snap#1: executes at perfNow=100
    h.perfNow(100);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();

    // Advance perfNow past debounce (400 = 300ms elapsed), advance ct and bufEnd > ADV_EPS
    h.perfNow(500); // 500-100=400 >= 300 → N2 passes
    const bufEnd2 = 10.030 + 0.500; // 10.530
    const ct2 = 10.016 + 0.300;    // 10.316
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
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting();

    // Snap#2: perfNow=50 (50ms elapsed < 300ms). Advance ct/bufEnd > ADV_EPS so N1 passes.
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.032);  // bufEnd advanced by 2ms > ADV_EPS=1ms
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
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1: lastSnapAtMs=0

    h.perfNow(350); // 350 - 0 = 350 >= 300 → N2 does not fire
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;

    h.exports.onVideoWaiting();

    // Snap executes: debounce count unchanged at 0
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.032 - 0.300, 5);
  });

  // T-S8-5: snap at exactly 300ms boundary → NOT suppressed (S8-3-SC3)
  it('T-S8-5: snap#2 at T+300ms exactly (boundary) → N2 does not fire; snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1: lastSnapAtMs=0

    h.perfNow(300); // 300 - 0 = 300 >= 300 → NOT suppressed (boundary)
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.032 - 0.300, 5);
  });

  // T-S8-17: 3 consecutive N2 suppressions → debounce count = 3 (S8-6-SC1)
  it('T-S8-17: 3 consecutive N2 suppressions in window → getSuppressedDebounceCount() === 3', () => {
    // Snap#1: executes at perfNow=0
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    let bufEnd = 10.030;
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

  // T-S8-6: rs=1 inside window, N1 passes → N2 bypassed, snap executes (S8-3-SC4)
  it('T-S8-6: rs=1 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1 at perfNow=0; lastSnapAtMs=0

    // perfNow=50: 50ms elapsed < 300ms (inside window); hardStarve=true → bypasses N2
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018; // ct advanced > ADV_EPS → N1 passes
    Object.defineProperty(h.videoEl, 'readyState', { value: 1, configurable: true });

    h.exports.onVideoWaiting();

    // N2 bypassed: snap executes, debounce count unchanged at 0
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.032 - 0.300, 5);
  });

  // T-S8-7: rs=1 inside window, NO ct/bufEnd progress → N1 fires first (S8-3-SC5)
  it('T-S8-7: rs=1 inside window, no ct/bufEnd progress → N1 fires; suppressedGuardCount++; N2 NOT reached', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1; records ct=10.016, bufEnd=10.030

    h.perfNow(50); // inside 300ms window
    // ct and bufEnd NOT advanced (still at snap#1 baseline)
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    Object.defineProperty(h.videoEl, 'readyState', { value: 1, configurable: true });

    h.exports.onVideoWaiting();

    // N1 fires first: suppressedGuardCount incremented; N2 NOT reached
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
  });

  // T-S8-8: rs=2 inside window → N2 applies (rs=2 NOT escape hatch) (S8-3-SC6)
  it('T-S8-8: rs=2 inside 300ms window, ct/bufEnd advanced → N2 fires (rs=2 is NOT escape hatch)', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50); // inside 300ms
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;
    Object.defineProperty(h.videoEl, 'readyState', { value: 2, configurable: true }); // rs=2 NOT escape hatch

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(1); // N2 applied
  });

  // T-S8-9: rs=0 inside window, ct/bufEnd advanced → N2 bypassed (rs=0 <= 1) (S8-3-SC7)
  it('T-S8-9: rs=0 inside 300ms window, ct/bufEnd advanced > ADV_EPS → N2 bypassed, snap executes', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1

    h.perfNow(50); // inside 300ms
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;
    Object.defineProperty(h.videoEl, 'readyState', { value: 0, configurable: true }); // rs=0 <= 1

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedDebounceCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(10.032 - 0.300, 5);
  });

  // T-S8-24: rs=1 inside window, no progress → N1 fires (suppressedGuardCount++), NOT N2 (S8-7-SC3)
  it('T-S8-24: rs=1 inside window, no ct/bufEnd progress → N1 fires (suppressedGuard++); N2 NOT reached (suppressedDebounce unchanged)', () => {
    h.perfNow(0);
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // snap#1 records state

    h.perfNow(50); // inside 300ms
    // No progress: ct and bufEnd at same values as snap#1
    h.sb.buffered = makeBuffered(0, 10.030);
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
  it('T-S8-10: lastSnapCt=5.000, lastSnapBufEnd=5.300; ct=5.002 (advanced), bufEnd=5.300 (unchanged) → N1 passes', () => {
    h.sb.buffered = makeBuffered(0, 5.300); // bufEnd=5.300, unchanged from baseline
    h.videoEl.currentTime = 5.002;          // ct=5.002 > 5.000+0.001=5.001 ✓
    // Geometry: target=Math.max(0, 5.300-0.300)=5.000; ct=5.002 != target → N3 passes
    // G6: 5.300-5.000=0.300 >= 0.1 → passes

    h.exports.onVideoWaiting();

    // N1 passed: snap executes; guardCount unchanged at 0
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(5.000, 5);
  });

  // T-S8-11: only bufEnd advanced > ADV_EPS → N1 passes, snap executes (S8-4-SC2)
  it('T-S8-11: lastSnapCt=5.000, lastSnapBufEnd=5.300; ct=5.000 (unchanged), bufEnd=5.302 (advanced) → N1 passes', () => {
    h.sb.buffered = makeBuffered(0, 5.302); // bufEnd=5.302 > 5.300+0.001=5.301 ✓
    h.videoEl.currentTime = 5.000;          // ct=5.000, unchanged (NOT advanced)
    // target=Math.max(0, 5.302-0.300)=5.002; ct=5.000 != target → N3 passes
    // G6: 5.302-5.002=0.300 >= 0.1 → passes

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(5.002, 5);
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
  it('T-S8-13: ct=5.050 (advanced), bufEnd=5.360 (advanced) → N1 passes, snap executes', () => {
    h.sb.buffered = makeBuffered(0, 5.360); // bufEnd=5.360 > 5.300+0.001 ✓
    h.videoEl.currentTime = 5.050;          // ct=5.050 > 5.000+0.001 ✓
    // target=Math.max(0, 5.360-0.300)=5.060; ct=5.050 != 5.060 → N3 passes
    // G6: 5.360-5.060=0.300 >= 0.1 → passes

    h.exports.onVideoWaiting();

    expect(h.exports.getSuppressedGuardCount()).toBe(0);
    expect(h.videoEl.currentTime).toBeCloseTo(5.060, 5);
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
  // ct=9.700, bufEnd=10.000, lastRangeStart=0.000
  // target = Math.max(0.000, 10.000-0.300) = 9.7 === ct (IEEE 754 exact)
  it('T-S8-15: target === ct (9.700 === 9.700, IEEE 754 exact) → N3 fires; no seek; suppressedGuardCount++', () => {
    h.sb.buffered = makeBuffered(0, 10.000); // lastRangeStart=0, bufEnd=10.000
    h.videoEl.currentTime = 9.700;           // ct = 9.7; target = 10.000-0.300 = 9.7 === ct

    const ctBefore = h.videoEl.currentTime;
    h.exports.onVideoWaiting();

    expect(h.videoEl.currentTime).toBe(ctBefore); // no seek — N3 fired
    const logLines = getMseLogLines(h.tauri);
    expect(logLines.some((l) => l.includes('result=stall_snap'))).toBe(false);
    expect(h.exports.getSuppressedGuardCount()).toBe(1);
  });

  // T-S8-16: target !== ct → N3 does not fire, snap executes (S8-5-SC2)
  it('T-S8-16: target !== ct (10.016 vs 9.730) → N3 does not fire; snap executes', () => {
    h.sb.buffered = makeBuffered(0, 10.030); // target = 9.730
    h.videoEl.currentTime = 10.016;          // ct = 10.016 != 9.730

    h.exports.onVideoWaiting();

    expect(h.videoEl.currentTime).toBeCloseTo(9.730, 5);
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
    let bufEnd = 10.030;
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
  it('T-S8-20: tick line matches regex /suppressed_debounce=(\\d+) suppressed_guard=(\\d+)$/ with correct values', async () => {
    // Set up known counter values: 2 N2 suppressions (debounce=2) + 1 N1 suppression (guard=1)
    let bufEnd = 10.030;
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

    // Must match trailing regex
    expect(lastTick).toMatch(/suppressed_debounce=(\d+) suppressed_guard=(\d+)$/);
    // Extract values and compare with getter
    const match = lastTick.match(/suppressed_debounce=(\d+) suppressed_guard=(\d+)$/);
    expect(parseInt(match[1], 10)).toBe(h.exports.getSuppressedDebounceCount());
    expect(parseInt(match[2], 10)).toBe(h.exports.getSuppressedGuardCount());
    // suppressed_debounce= appears after buffered=
    expect(lastTick.indexOf('buffered=')).toBeLessThan(lastTick.indexOf('suppressed_debounce='));
    // suppressed_guard= appears after suppressed_debounce=
    expect(lastTick.indexOf('suppressed_debounce=')).toBeLessThan(lastTick.indexOf('suppressed_guard='));
  });

  // T-S8-21: tick line has fields even when counters are 0 (S8-6-SC5)
  it('T-S8-21: no suppressions → tick line ends with suppressed_debounce=0 suppressed_guard=0', async () => {
    // No snaps, no suppressions — counters at 0
    h.tauri.invoke.mock.calls.length = 0;
    await vi.advanceTimersByTimeAsync(2000);

    const tickLines = getTickLines(h.tauri);
    expect(tickLines.length).toBeGreaterThan(0);
    const lastTick = tickLines[tickLines.length - 1];
    expect(lastTick).toMatch(/suppressed_debounce=0 suppressed_guard=0$/);
  });

  // T-S8-28: counters persist across tearDownMse (NOT reset to 0) (S8-6-SC6)
  it('T-S8-28: counters persist across tearDownMse — NOT reset to 0', () => {
    let bufEnd = 10.030;
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
    Object.defineProperty(h.videoEl, 'readyState', { value: 3, configurable: true });

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
    h.sb.buffered = makeBuffered(0, 10.030);
    h.videoEl.currentTime = 10.016;
    h.exports.onVideoWaiting(); // lastSnapAtMs=0

    // Trigger one N2 suppression: perfNow=50 (inside 300ms), ct/bufEnd advanced > ADV_EPS
    h.perfNow(50);
    h.sb.buffered = makeBuffered(0, 10.032);
    h.videoEl.currentTime = 10.018;
    h.exports.onVideoWaiting(); // N2 fires

    // getSuppressedDebounceCount() must return 1 (live state, not snapshot-at-0)
    expect(h.exports.getSuppressedDebounceCount()).toBe(1);
    // getSuppressedGuardCount() must return 0 (independent counter, unchanged)
    expect(h.exports.getSuppressedGuardCount()).toBe(0);
  });
});
