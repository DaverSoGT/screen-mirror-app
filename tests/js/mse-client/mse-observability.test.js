// mse-observability.test.js — SC-MSE-LOG-1..11b
//
// GATE-6 observability bridge tests. Validates that mseLog() helper is wired
// to the correct call sites in dist/mse-client.js and that the [sm-mse] line
// format contract is met for all 8 instrumented signals.
//
// Test seam: `invoke` mock intercepts `invoke("mse_log", { line })` calls.
// `mseLog` is also exported via __SCREEN_MIRROR_TEST_EXPORTS__ for direct
// unit tests (SC-MSE-LOG-10, 11a, 11b).
//
// Design refs: D-PPT6-2..6, reconciliation deltas in tasks-slice6.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

// Helper: make a minimal fake TimeRanges object.
function makeBuffered(ranges) {
  // ranges: array of [start, end] pairs
  return {
    length: ranges.length,
    start: (i) => ranges[i][0],
    end: (i) => ranges[i][1],
  };
}

// Helper: filter invoke mock calls for mse_log commands and return line strings.
function getMseLogLines(tauri) {
  return tauri.invoke.mock.calls
    .filter((c) => c[0] === 'mse_log')
    .map((c) => c[1].line);
}

// ── Shared beforeEach/afterEach setup ────────────────────────────────────────

// Most tests need a fully-initialized MSE client with a SourceBuffer primed.
// We expose this as a shared setup helper called by describe blocks that need it.
async function setupWithSb() {
  installDom();
  const tauri = installTauriMock();
  vi.stubGlobal('MediaSource', MockMediaSourceCtor);
  globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
  vi.useFakeTimers();
  vi.resetModules();
  await import('../../../dist/mse-client.js');
  // Flush sourceopen microtask (queueMicrotask via MockMediaSource).
  await vi.advanceTimersByTimeAsync(0);
  // Flush start_stream resolve chain — may need multiple microtask ticks.
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();

  // Prime with init segment so SourceBuffer is created.
  const ch = tauri.lastChannel();
  const initFrame = makeInitFrame(INIT_HIGH_41);
  ch._dispatch(initFrame.buffer);
  // Flush: init → addSourceBuffer → enqueue → appendBuffer → updateend
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();

  const exports = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  const ms = MockMediaSourceCtor._lastInstance;
  const sb = ms._sb;
  return { tauri, exports, ms, sb };
}

// ── Phase 2 / Phase 3: mseLog helper unit tests ───────────────────────────────

describe('mse-client — mseLog helper unit tests (SC-MSE-LOG-9, 10, 11a, 11b)', () => {
  let tauri;
  let exports;

  beforeEach(async () => {
    ({ tauri, exports } = await setupWithSb());
    // Reset invoke spy after init so init side-effects don't pollute assertions.
    tauri.invoke.mockClear();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // SC-MSE-LOG-9: successful appendBuffer path → zero mse_log invoke calls.
  // This test is trivially green until call-site instrumentation lands;
  // it becomes the regression guard once call sites are instrumented.
  it('SC-MSE-LOG-9: no invoke("mse_log") on successful appendBuffer', async () => {
    const ch = tauri.lastChannel();
    // Push a media segment (0x01 prefix) — appendBuffer should succeed.
    const segment = new Uint8Array([0x01, 0xAA, 0xBB, 0xCC]);
    ch._dispatch(segment.buffer);
    await Promise.resolve();
    await Promise.resolve();

    const mseLogCalls = getMseLogLines(tauri);
    expect(mseLogCalls.length).toBe(0);
  });

  // SC-MSE-LOG-10: mseLog with __TAURI__ undefined → no throw, console.log called.
  // RED: fails until mseLog is exported via __SCREEN_MIRROR_TEST_EXPORTS__.
  it('SC-MSE-LOG-10: mseLog with no __TAURI__ does not throw; console.log receives "[sm-mse] " prefix', () => {
    const { mseLog } = exports;
    const consoleSpy = vi.spyOn(console, 'log').mockImplementation(() => {});
    const savedTauri = globalThis.__TAURI__;
    globalThis.__TAURI__ = undefined;

    try {
      expect(() => mseLog('test-line')).not.toThrow();
      const smMseCalls = consoleSpy.mock.calls.filter(
        (c) => typeof c[0] === 'string' && c[0].startsWith('[sm-mse] ')
      );
      expect(smMseCalls.length).toBeGreaterThanOrEqual(1);
      expect(smMseCalls[0][0]).toBe('[sm-mse] test-line');
    } finally {
      globalThis.__TAURI__ = savedTauri;
      consoleSpy.mockRestore();
    }
  });

  // SC-MSE-LOG-11a: console receives "[sm-mse] " + line; invoke receives bare line.
  // RED: fails until mseLog is exported.
  it('SC-MSE-LOG-11a: console gets "[sm-mse] "+line prefix; invoke("mse_log") receives bare line', async () => {
    const { mseLog } = exports;
    const consoleSpy = vi.spyOn(console, 'log').mockImplementation(() => {});

    mseLog('event=tick ct=1.000');
    await Promise.resolve();

    const consoleSmMse = consoleSpy.mock.calls.filter(
      (c) => typeof c[0] === 'string' && c[0].startsWith('[sm-mse] ')
    );
    expect(consoleSmMse.length).toBeGreaterThanOrEqual(1);
    expect(consoleSmMse[0][0]).toBe('[sm-mse] event=tick ct=1.000');

    const mseLogCalls = getMseLogLines(tauri);
    expect(mseLogCalls.length).toBeGreaterThanOrEqual(1);
    expect(mseLogCalls[0]).toBe('event=tick ct=1.000'); // bare line, no prefix
    consoleSpy.mockRestore();
  });

  // SC-MSE-LOG-11b: invoke rejection is swallowed — .catch guard is called.
  // Mutation-resistant: a spy wraps the rejected promise; if .catch is removed
  // from mseLog the spy is never invoked and the test fails.
  it('SC-MSE-LOG-11b: invoke("mse_log") rejection is swallowed — .catch guard is attached to the returned promise', async () => {
    const { mseLog } = exports;

    // Build a spy promise: the underlying promise is rejected, but .catch is
    // intercepted so we can assert the guard was actually attached.
    let catchCalled = false;
    const rejection = Promise.reject(new Error('IPC failure'));
    const spyPromise = {
      catch: (fn) => {
        catchCalled = true;
        return rejection.catch(fn);
      },
    };
    // Suppress the unhandled rejection on our side — the .catch inside mseLog
    // handles it; we only need a safety drain here for the test environment.
    rejection.catch(() => {});
    tauri.invoke.mockReturnValueOnce(spyPromise);

    expect(() => mseLog('event=test')).not.toThrow();
    // Allow the microtask queue to settle so the .catch chain runs.
    await Promise.resolve();
    await Promise.resolve();

    // The .catch guard in mseLog MUST have been called on the returned promise.
    expect(catchCalled).toBe(true);
  });
});

// ── Phase 5: Call-site tests (RED before instrumentation) ─────────────────────

describe('mse-client — GATE-6 call-site tests (SC-MSE-LOG-1..8)', () => {
  let tauri;
  let exports;
  let sb;
  let videoEl;

  beforeEach(async () => {
    ({ tauri, exports, sb } = await setupWithSb());
    videoEl = document.getElementById('player');
    // Reset invoke spy so init-segment side effects don't pollute assertions.
    tauri.invoke.mockClear();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // SC-MSE-LOG-1: QuotaExceededError in flushQueue → two invoke("mse_log") calls.
  // First line: event=append_quota; second: event=append_error name=QuotaExceededError.
  // pending is counted AFTER unshift (off-by-one guard: the chunk is re-queued).
  it('SC-MSE-LOG-1: QuotaExceededError → two mse_log calls (append_quota + append_error)', async () => {
    // Make appendBuffer throw QuotaExceededError.
    sb.appendBuffer.mockImplementationOnce(() => {
      const e = new DOMException('QuotaExceededError', 'QuotaExceededError');
      throw e;
    });
    sb.updating = false;

    // Push a media segment to trigger flushQueue.
    const ch = tauri.lastChannel();
    const segment = new Uint8Array([0x01, 0xDE, 0xAD]);
    ch._dispatch(segment.buffer);
    await Promise.resolve();
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    expect(lines.length).toBeGreaterThanOrEqual(2);

    const quotaLine = lines.find((l) => l.startsWith('event=append_quota'));
    expect(quotaLine).toBeTruthy();
    // Exact: the chunk is re-queued via pending.unshift BEFORE counting, so
    // pending reflects the retry. Off-by-one mutant (read length before unshift)
    // would emit pending=0 here — pin the exact post-unshift value to kill it.
    expect(quotaLine).toMatch(/(^| )pending=1( |$)/);
    expect(quotaLine).toMatch(/buffered=/);

    const errorLine = lines.find((l) => l.startsWith('event=append_error'));
    expect(errorLine).toBeTruthy();
    expect(errorLine).toMatch(/name=QuotaExceededError/);
    // The append_error line follows the quota branch's unshift; pending is still 1.
    expect(errorLine).toMatch(/(^| )pending=1( |$)/);
  });

  // SC-MSE-LOG-2: Non-quota appendBuffer throw → one mse_log call.
  // event=append_error name=InvalidStateError sb_updating=false.
  it('SC-MSE-LOG-2: non-quota appendBuffer throw → one mse_log with event=append_error', async () => {
    sb.appendBuffer.mockImplementationOnce(() => {
      const e = new DOMException('InvalidStateError', 'InvalidStateError');
      throw e;
    });
    sb.updating = false;

    const ch = tauri.lastChannel();
    const segment = new Uint8Array([0x01, 0xBE, 0xEF]);
    ch._dispatch(segment.buffer);
    await Promise.resolve();
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const errorLines = lines.filter((l) => l.startsWith('event=append_error'));
    expect(errorLines.length).toBe(1);
    expect(errorLines[0]).toMatch(/name=InvalidStateError/);
    expect(errorLines[0]).toMatch(/sb_updating=false/);

    // No append_quota line for non-quota errors.
    expect(lines.some((l) => l.startsWith('event=append_quota'))).toBe(false);
  });

  // SC-MSE-LOG-3: trimSourceBuffer — two sides.
  // (a) buffered.start(0) >= cutoff → action=noop.
  // (b) buffered.start(0) < cutoff → action=remove AND sb.remove called.
  it('SC-MSE-LOG-3: trimSourceBuffer emits action=noop when start>=cutoff; action=remove when start<cutoff', () => {
    // trimSourceBuffer is not exported — exercise it via the 5s trim setInterval.

    const videoEl = document.getElementById('player');
    videoEl.currentTime = 100; // currentTime=100 → cutoff=70

    // Side (a): buffered.start(0) >= cutoff → noop
    sb.updating = false;
    sb.buffered = makeBuffered([[80, 95]]); // start=80 >= cutoff=70 → noop
    tauri.invoke.mockClear();

    vi.advanceTimersByTime(5000); // fires the 5s trim interval

    const linesA = getMseLogLines(tauri);
    const trimLinesA = linesA.filter((l) => l.startsWith('event=trim'));
    expect(trimLinesA.length).toBeGreaterThanOrEqual(1);
    expect(trimLinesA[0]).toMatch(/action=noop/);
    expect(sb.remove).not.toHaveBeenCalled();

    // Side (b): buffered.start(0) < cutoff → remove
    videoEl.currentTime = 100;
    sb.buffered = makeBuffered([[10, 95]]); // start=10 < cutoff=70 → remove
    tauri.invoke.mockClear();
    sb.remove.mockClear();

    vi.advanceTimersByTime(5000);

    const linesB = getMseLogLines(tauri);
    const trimLinesB = linesB.filter((l) => l.startsWith('event=trim'));
    expect(trimLinesB.length).toBeGreaterThanOrEqual(1);
    expect(trimLinesB[0]).toMatch(/action=remove/);
    expect(sb.remove).toHaveBeenCalled();
  });

  // SC-MSE-LOG-3b: trimSourceBuffer with sb.updating=true → action=busy log.
  // Frozen contract D-PPT6-3 #3: busy guard must be logged, not silently skipped.
  // RED: currently fails — busy guard returns silently without logging.
  it('SC-MSE-LOG-3b: trimSourceBuffer with sb.updating=true → mse_log event=trim action=busy', () => {
    const videoEl = document.getElementById('player');
    videoEl.currentTime = 100;

    // sb exists but is currently updating — the busy guard fires.
    sb.updating = true;
    sb.buffered = makeBuffered([[10, 95]]);
    tauri.invoke.mockClear();

    vi.advanceTimersByTime(5000);

    const lines = getMseLogLines(tauri);
    const busyLines = lines.filter((l) => l.startsWith('event=trim'));
    expect(busyLines.length).toBeGreaterThanOrEqual(1);
    expect(busyLines[0]).toMatch(/action=busy/);
    // N/A fields must carry "-" sentinel per NFR-2, not be omitted.
    expect(busyLines[0]).toMatch(/cutoff=-/);
    expect(busyLines[0]).toMatch(/buf_start=-/);
    expect(busyLines[0]).toMatch(/name=-/);
  });

  // SC-MSE-LOG-3c: N/A sentinel "-" present in remove/noop/throw trim lines.
  // Frozen contract D-PPT6-3: absent values use "-", not field omission.
  // RED: currently fails — name= field absent on remove/noop; cutoff/buf_start absent on throw.
  it('SC-MSE-LOG-3c: trim remove/noop lines carry name="-" sentinel; throw line carries cutoff="-" and buf_start="-"', () => {
    const videoEl = document.getElementById('player');
    videoEl.currentTime = 100;

    // noop path — name field must be "-"
    sb.updating = false;
    sb.buffered = makeBuffered([[80, 95]]);
    tauri.invoke.mockClear();
    vi.advanceTimersByTime(5000);
    const noopLines = getMseLogLines(tauri).filter((l) => l.includes('action=noop'));
    expect(noopLines.length).toBeGreaterThanOrEqual(1);
    expect(noopLines[0]).toMatch(/name=-/);

    // remove path — name field must be "-"
    sb.buffered = makeBuffered([[10, 95]]);
    tauri.invoke.mockClear();
    sb.remove.mockClear();
    vi.advanceTimersByTime(5000);
    const removeLines = getMseLogLines(tauri).filter((l) => l.includes('action=remove'));
    expect(removeLines.length).toBeGreaterThanOrEqual(1);
    expect(removeLines[0]).toMatch(/name=-/);

    // throw path — sb.remove throws (SourceBuffer detached after remove call begins);
    // cutoff=- and buf_start=- sentinels must appear in the throw trim line.
    sb.buffered = makeBuffered([[10, 95]]); // start < cutoff → remove branch
    sb.remove.mockImplementationOnce(() => {
      throw new DOMException('InvalidStateError', 'InvalidStateError');
    });
    tauri.invoke.mockClear();
    vi.advanceTimersByTime(5000);
    const throwLines = getMseLogLines(tauri).filter((l) => l.includes('action=throw'));
    expect(throwLines.length).toBeGreaterThanOrEqual(1);
    expect(throwLines[0]).toMatch(/cutoff=-/);
    expect(throwLines[0]).toMatch(/buf_start=-/);
  });

  // SC-MSE-LOG-4: seekToLiveEdge — two sides.
  // (a) After instrumentation: result=guard_backward logged when target<=currentTime AND drift>0.5.
  //     Note: with LIVE_EDGE_TARGET_LEAD_SEC=0.2 and LIVE_EDGE_MAX_DRIFT_SEC=0.5, guard_backward
  //     requires bufEnd-lead<=ct AND bufEnd-ct>0.5 — mathematically impossible with fixed constants.
  //     Instead we test via snap path (drift>0.5, target>ct) and the silence path (drift<=0.5).
  // (b) drift <= 0.5 → zero mse_log seek lines (silence guard — D-PPT6-4).
  it('SC-MSE-LOG-4: seekToLiveEdge snap path logged; drift<=0.5 → zero seek mse_log lines (silence)', () => {
    const { seekToLiveEdge } = exports;

    // Side (b) first: drift <= 0.5 → silence (guard_drift, no log per D-PPT6-4).
    sb.updating = false;
    // bufEnd=5.3, currentTime=5.0 → drift=0.3 <= 0.5 → guard_drift → silence.
    sb.buffered = makeBuffered([[0, 5.3]]);
    videoEl.currentTime = 5.0;
    tauri.invoke.mockClear();

    seekToLiveEdge();

    const silenceLines = getMseLogLines(tauri).filter((l) =>
      l.startsWith('event=seek') || l.startsWith('event=guard')
    );
    expect(silenceLines.length).toBe(0);

    // Side (a): snap path — drift > 0.5, target > currentTime → result=snap logged.
    // bufEnd=8, currentTime=1 → drift=7 > 0.5; target=7.8 > 1 → snap.
    sb.buffered = makeBuffered([[0, 8]]);
    videoEl.currentTime = 1;
    tauri.invoke.mockClear();

    seekToLiveEdge();

    return Promise.resolve().then(() => {
      const snapLines = getMseLogLines(tauri).filter((l) => l.startsWith('event=seek'));
      expect(snapLines.length).toBeGreaterThanOrEqual(1);
      expect(snapLines[0]).toMatch(/result=snap/);
      expect(snapLines[0]).toMatch(/from=/);
      expect(snapLines[0]).toMatch(/to=/);
      expect(snapLines[0]).toMatch(/drift=/);
    });
  });

  // SC-MSE-LOG-5: heartbeat 2s tick → mse_log with event=tick, exact field values.
  // Exact assertions (not presence-only) so design-claimed mutants cannot survive:
  // hardcoding sb_updating=false, dropping a field, or fudging ct/buffered all fail.
  it('SC-MSE-LOG-5: 2s tick fires → mse_log event=tick with exact ct/sb_updating/pending/buffered values', async () => {
    sb.updating = false;
    sb.buffered = makeBuffered([[0, 3]]);
    // currentTime is settable on the mock video element from happy-dom.
    videoEl.currentTime = 1.5;

    tauri.invoke.mockClear();
    await vi.advanceTimersByTimeAsync(2000);
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const tickLines = lines.filter((l) => l.startsWith('event=tick'));
    expect(tickLines.length).toBeGreaterThanOrEqual(1);

    const tick = tickLines[0];
    // Exact values the fixture sets — not /\d/ presence-only.
    expect(tick).toMatch(/(^| )ct=1\.500( |$)/);
    expect(tick).toMatch(/(^| )paused=/);
    expect(tick).toMatch(/ rs=\d/);   // anchored: must not match ms_rs= substring
    expect(tick).toMatch(/(^| )pending=0( |$)/);     // queue drained after init
    expect(tick).toMatch(/(^| )sb_updating=false( |$)/);
    expect(tick).toMatch(/(^| )ms_rs=/);
    expect(tick).toMatch(/(^| )buffered=\[0\.000→3\.000\]( |$)/);
  });

  // SC-MSE-LOG-5b: sb_updating reflects the live SourceBuffer state — a tick with
  // sb.updating=true MUST emit sb_updating=true. Kills the hardcode-false mutant
  // that SC-MSE-LOG-5's false-case alone cannot catch.
  it('SC-MSE-LOG-5b: 2s tick with sb.updating=true → mse_log event=tick sb_updating=true', async () => {
    sb.updating = true;
    sb.buffered = makeBuffered([[0, 3]]);
    videoEl.currentTime = 2.0;

    tauri.invoke.mockClear();
    await vi.advanceTimersByTimeAsync(2000);
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const tickLines = lines.filter((l) => l.startsWith('event=tick'));
    expect(tickLines.length).toBeGreaterThanOrEqual(1);
    expect(tickLines[0]).toMatch(/(^| )sb_updating=true( |$)/);
  });

  // SC-MSE-LOG-6: SourceBuffer "error" event → mse_log with event=sb_error.
  // Existing console.error still called.
  it('SC-MSE-LOG-6: SourceBuffer "error" event → mse_log event=sb_error; console.error untouched', () => {
    const consoleSpy = vi.spyOn(console, 'error').mockImplementation(() => {});
    tauri.invoke.mockClear();

    // Fire the "error" listener on the SourceBuffer.
    const fakeEvent = { type: 'error' };
    sb._listeners.error?.(fakeEvent);

    const lines = getMseLogLines(tauri);
    const sbErrorLines = lines.filter((l) => l.startsWith('event=sb_error'));
    expect(sbErrorLines.length).toBeGreaterThanOrEqual(1);
    expect(sbErrorLines[0]).toMatch(/type=error/);

    // console.error must still have been called.
    expect(consoleSpy).toHaveBeenCalled();
    consoleSpy.mockRestore();
  });

  // SC-MSE-LOG-7: onWindowError handler → mse_log with event=js_error, msg LAST.
  // Tested via direct handler invocation (happy-dom limitation per D-PPT6-5).
  it('SC-MSE-LOG-7: onWindowError({message,filename,lineno,colno}) → mse_log event=js_error, msg last, truncated at 200', () => {
    const { onWindowError } = exports;
    tauri.invoke.mockClear();

    const longMsg = 'X'.repeat(300);
    onWindowError({ message: longMsg, filename: 'app.js', lineno: 42, colno: 7 });
    return Promise.resolve().then(() => {
      const lines = getMseLogLines(tauri);
      const jsErrorLines = lines.filter((l) => l.startsWith('event=js_error'));
      expect(jsErrorLines.length).toBeGreaterThanOrEqual(1);
      const line = jsErrorLines[0];
      expect(line).toMatch(/src=app\.js/);
      expect(line).toMatch(/line=42/);
      expect(line).toMatch(/col=7/);
      // msg must be last and truncated to 200 chars.
      const msgMatch = line.match(/msg=(.+)$/);
      expect(msgMatch).toBeTruthy();
      expect(msgMatch[1].length).toBeLessThanOrEqual(200);
      // The msg field must come AFTER src/line/col fields.
      const msgIdx = line.indexOf('msg=');
      const srcIdx = line.indexOf('src=');
      expect(msgIdx).toBeGreaterThan(srcIdx);
    });
  });

  // SC-MSE-LOG-8: onUnhandledRejection({reason}) → mse_log event=unhandled_rejection,
  // reason truncated at 200 chars.
  it('SC-MSE-LOG-8: onUnhandledRejection({reason}) → mse_log event=unhandled_rejection reason truncated at 200', () => {
    const { onUnhandledRejection } = exports;
    tauri.invoke.mockClear();

    const longReason = 'R'.repeat(300);
    onUnhandledRejection({ reason: longReason });
    return Promise.resolve().then(() => {
      const lines = getMseLogLines(tauri);
      const rejLines = lines.filter((l) => l.startsWith('event=unhandled_rejection'));
      expect(rejLines.length).toBeGreaterThanOrEqual(1);
      const line = rejLines[0];
      expect(line).toMatch(/reason=/);
      const reasonMatch = line.match(/reason=(.+)$/);
      expect(reasonMatch).toBeTruthy();
      expect(reasonMatch[1].length).toBeLessThanOrEqual(200);
    });
  });

  // SC-MSE-LOG-12: detached-SourceBuffer `buffered` getter throws InvalidStateError.
  //
  // Per the MSE spec, reading SourceBuffer.buffered on a detached/removed
  // SourceBuffer throws InvalidStateError. bufferedSummary MUST read sb.buffered
  // INSIDE its own try/catch (D-PPT6-3 signature bufferedSummary(sb)) so the getter
  // throw is swallowed and emitted as buffered=-, and NO exception escapes the
  // exception-safe channel pump (flushQueue quota + append_error paths) or the
  // bare setInterval tick body (which would otherwise fire window.onerror →
  // onWindowError → self-inflicted event=js_error spam polluting GATE-6).
  //
  // Install a throwing getter that replaces the plain `buffered` data property.
  function installThrowingBuffered(target) {
    Object.defineProperty(target, 'buffered', {
      configurable: true,
      get() {
        throw new DOMException('SourceBuffer detached', 'InvalidStateError');
      },
    });
  }

  // (a) flushQueue quota branch: throwing getter → append_quota carries buffered=-,
  //     append_error carries buffered=-, and dispatching the segment does NOT throw.
  it('SC-MSE-LOG-12a: detached buffered getter in flushQueue quota branch → buffered=- on append_quota + append_error, no throw escapes', async () => {
    sb.appendBuffer.mockImplementationOnce(() => {
      throw new DOMException('QuotaExceededError', 'QuotaExceededError');
    });
    sb.updating = false;
    installThrowingBuffered(sb);

    const ch = tauri.lastChannel();
    const segment = new Uint8Array([0x01, 0xDE, 0xAD]);
    // The channel onmessage pump is exception-safe: a getter throw escaping
    // bufferedSummary would propagate out of flushQueue → enqueue → this dispatch.
    expect(() => ch._dispatch(segment.buffer)).not.toThrow();
    await Promise.resolve();
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const quotaLine = lines.find((l) => l.startsWith('event=append_quota'));
    expect(quotaLine).toBeTruthy();
    expect(quotaLine).toMatch(/(^| )buffered=-( |$)/);

    const errorLine = lines.find((l) => l.startsWith('event=append_error'));
    expect(errorLine).toBeTruthy();
    expect(errorLine).toMatch(/name=QuotaExceededError/);
    expect(errorLine).toMatch(/(^| )buffered=-( |$)/);
  });

  // (b) flushQueue non-quota append_error branch: throwing getter → buffered=-,
  //     and dispatching the segment does NOT throw.
  it('SC-MSE-LOG-12b: detached buffered getter in flushQueue append_error branch → buffered=-, no throw escapes', async () => {
    sb.appendBuffer.mockImplementationOnce(() => {
      throw new DOMException('InvalidStateError', 'InvalidStateError');
    });
    sb.updating = false;
    installThrowingBuffered(sb);

    const ch = tauri.lastChannel();
    const segment = new Uint8Array([0x01, 0xBE, 0xEF]);
    expect(() => ch._dispatch(segment.buffer)).not.toThrow();
    await Promise.resolve();
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const errorLines = lines.filter((l) => l.startsWith('event=append_error'));
    expect(errorLines.length).toBe(1);
    expect(errorLines[0]).toMatch(/name=InvalidStateError/);
    expect(errorLines[0]).toMatch(/(^| )buffered=-( |$)/);
    // Non-quota path must not have produced an append_quota line.
    expect(lines.some((l) => l.startsWith('event=append_quota'))).toBe(false);
  });

  // (c) heartbeat tick body: throwing getter → tick line carries buffered=-, and
  //     NO exception escapes the bare setInterval body (which would fire
  //     window.onerror → onWindowError → a self-inflicted event=js_error line).
  it('SC-MSE-LOG-12c: detached buffered getter in 2s tick → buffered=-, no throw escapes tick body (no self-inflicted js_error)', async () => {
    sb.updating = false;
    installThrowingBuffered(sb);
    videoEl.currentTime = 1.5;

    tauri.invoke.mockClear();
    // advanceTimersByTimeAsync rejects if the timer callback throws synchronously;
    // a getter escaping bufferedSummary in the tick body would surface here.
    await expect(vi.advanceTimersByTimeAsync(2000)).resolves.not.toThrow();
    await Promise.resolve();

    const lines = getMseLogLines(tauri);
    const tickLines = lines.filter((l) => l.startsWith('event=tick'));
    expect(tickLines.length).toBeGreaterThanOrEqual(1);
    expect(tickLines[0]).toMatch(/(^| )buffered=-( |$)/);
    // The tick body must NOT have produced a self-inflicted js_error line.
    expect(lines.some((l) => l.startsWith('event=js_error'))).toBe(false);
  });
});
