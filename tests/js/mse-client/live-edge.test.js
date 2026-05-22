// live-edge.test.js — LE-1..LE-4 (stream-periodic-freeze / Part Live-edge)
//
// Verifies the live-edge snap logic inserted into the existing 2000ms heartbeat.
//
// Scenarios covered:
//   LE-S1: drift > LIVE_EDGE_MAX_DRIFT_SEC && playing → currentTime snaps to end - LIVE_EDGE_TARGET_LEAD_SEC
//   LE-S2: drift < threshold → no snap
//   LE-S3: video paused → no snap even at large drift
//   LE-S4: snap target would land before buffered.start(last) → no snap (in-range guard)
//
// CRITICAL (R12 pattern): vi.useFakeTimers() MUST be called BEFORE await import(...)
// because main() fires setInterval at line ~419 synchronously on the sourceopen
// microtask. If fake timers are activated AFTER import, setInterval escapes to
// real timers and advanceTimersByTimeAsync won't fire it.
//
// Sequence mode: sb.mode = 'sequence' stays — live-edge uses buffered.end - currentTime,
// NOT tfdt (design Decision (d)).

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { makeSourceBuffer, MockMediaSourceCtor } from '../mocks/media-source.js';

// ─── Helpers ─────────────────────────────────────────────────────────────────

/**
 * Build a controllable MockSourceBuffer where buffered.length, start(), end()
 * return values we can set before each assertion.
 */
function makeControllableSourceBuffer(initialEnd = 0, initialStart = 0) {
  const sb = makeSourceBuffer();
  // Patch buffered to be controllable
  let _end = initialEnd;
  let _start = initialStart;
  Object.defineProperty(sb, 'buffered', {
    configurable: true,
    get() {
      return {
        length: _end > _start ? 1 : 0,
        start: (i) => (i === 0 ? _start : 0),
        end: (i) => (i === 0 ? _end : 0),
      };
    },
  });
  sb._setBuffered = (start, end) => { _start = start; _end = end; };
  return sb;
}

/**
 * Build a custom MediaSource that returns our controllable SB.
 */
function makeControllableMediaSource(sb) {
  const ms = {
    readyState: 'closed',
    addSourceBuffer: vi.fn((_codec) => {
      ms.readyState = 'open';
      return sb;
    }),
    endOfStream: vi.fn(),
    addEventListener: vi.fn((ev, cb) => {
      if (ev === 'sourceopen') queueMicrotask(cb);
    }),
    _sb: sb,
  };
  return ms;
}

function makeControllableMediaSourceCtor(sb) {
  function Ctor() {
    const ms = makeControllableMediaSource(sb);
    Ctor._lastInstance = ms;
    return ms;
  }
  Ctor.isTypeSupported = vi.fn(() => true);
  Ctor._lastInstance = null;
  return Ctor;
}

// ─── Suite ───────────────────────────────────────────────────────────────────

describe('mse-client — live-edge snap (LE-S1..LE-S4)', () => {
  let tauri;
  let sb;
  let MSCtor;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();

    sb = makeControllableSourceBuffer(0, 0);
    MSCtor = makeControllableMediaSourceCtor(sb);

    vi.stubGlobal('MediaSource', MSCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};

    // R12: fake timers BEFORE import
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');

    // Flush sourceopen microtask → main() runs past the MediaSource await
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    // At this point the heartbeat setInterval (2000ms) is registered under fake timers.
    // mseState.sb is null until the first init frame — live-edge reads mseState.sb.
    // We need to inject mseState.sb manually via the seam, OR trigger a real init frame.
    // Simplest approach: set mseState directly if exported, otherwise use the seam.
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // ─── LE-S1: drift above threshold → snap fires ─────────────────────────────

  it('LE-S1: currentTime snaps to buffered.end - LIVE_EDGE_TARGET_LEAD_SEC when drift exceeds threshold', async () => {
    // Expose the live-edge check function via the test seam
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      // If seam not yet wired, skip (expected RED until production code adds it)
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED (seam not yet wired)');
    }

    const VIDEO_EL = document.getElementById('player');

    // Set up: buffered.end = 5.0s, currentTime = 2.0s → drift = 3.0s > 0.30 threshold
    sb._setBuffered(0, 5.0);
    VIDEO_EL.currentTime = 2.0;
    // Video is playing normally: not paused, not ended, readyState = HAVE_ENOUGH_DATA (4).
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'ended', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'readyState', { configurable: true, get: () => 4 });

    checkLiveEdge(VIDEO_EL, sb);

    // Expected: currentTime snapped to 5.0 - 0.10 = 4.90
    expect(VIDEO_EL.currentTime).toBeCloseTo(4.90, 2);
  });

  // ─── LE-S2: drift below threshold → no snap ─────────────────────────────────

  it('LE-S2: no snap when drift is below LIVE_EDGE_MAX_DRIFT_SEC', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED');
    }

    const VIDEO_EL = document.getElementById('player');

    // drift = 0.1s < 0.30 threshold → no snap
    sb._setBuffered(0, 1.5);
    VIDEO_EL.currentTime = 1.4;
    // Video is playing normally: not paused, not ended, readyState = HAVE_ENOUGH_DATA (4).
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'ended', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'readyState', { configurable: true, get: () => 4 });

    checkLiveEdge(VIDEO_EL, sb);

    // currentTime must NOT change
    expect(VIDEO_EL.currentTime).toBeCloseTo(1.4, 2);
  });

  // ─── LE-S3: paused → no snap ─────────────────────────────────────────────────

  it('LE-S3: no snap when video is paused even with large drift', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED');
    }

    const VIDEO_EL = document.getElementById('player');

    // drift = 3.0s > threshold, but video is PAUSED
    sb._setBuffered(0, 5.0);
    VIDEO_EL.currentTime = 2.0;
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => true });

    checkLiveEdge(VIDEO_EL, sb);

    // currentTime must NOT change
    expect(VIDEO_EL.currentTime).toBeCloseTo(2.0, 2);
  });

  // ─── LE-S4: snap target before buffered.start(last) → no snap ─────────────
  //
  // REQ-LE-3: if snapTarget = (lastEnd - LIVE_EDGE_TARGET_LEAD_SEC) < buffered.start(last),
  // do NOT seek (snap would land in un-buffered space).
  //
  // Setup rationale:
  //   - buffered range = [100.55 → 100.60] (a tiny tail segment, e.g. after GC)
  //   - currentTime = 0.0  →  drift = 100.60 - 0.0 = 100.60 > 0.30
  //     The drift guard (line ~397) passes — execution reaches the range guard.
  //   - snapTarget  = 100.60 - 0.10 = 100.50
  //     100.50 < 100.55 (bufStart) → range guard fires → return, no snap.
  //
  // Falsification: removing `if (snapTarget < buf.start(last)) return;` from
  // checkLiveEdge would set currentTime = 100.50, causing this assertion to fail.

  it('LE-S4: no snap when snap target would be before buffered.start(last)', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED');
    }

    const VIDEO_EL = document.getElementById('player');

    // buffered = [100.55 → 100.60]: tiny tail segment (e.g. after buffer eviction).
    // snapTarget = 100.60 - 0.10 = 100.50, which is below bufStart 100.55.
    sb._setBuffered(100.55, 100.60);
    VIDEO_EL.currentTime = 0.0; // drift = 100.60s > 0.30 → drift guard passes
    // Video is playing normally so all other guards pass — only the range guard fires.
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'ended', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'readyState', { configurable: true, get: () => 4 });

    checkLiveEdge(VIDEO_EL, sb);

    // Range guard must block the snap — currentTime must NOT change.
    expect(VIDEO_EL.currentTime).toBeCloseTo(0.0, 2);
  });

  // ─── LE-S5: ended → no snap (REQ-LE-5) ────────────────────────────────────
  //
  // REQ-LE-5: the live-edge snap SHALL only execute when the video element is
  // not paused and is not in a "waiting" or "ended" state.
  //
  // Setup: drift = 3.0s > 0.30 threshold (would snap under the old paused-only
  // guard), but video.ended = true. The new guard MUST suppress the snap.
  //
  // Falsification: removing the `if (videoEl.ended) return;` line from
  // checkLiveEdge would allow the snap to fire and set currentTime = 4.90,
  // failing the assertion that currentTime remains 2.0.

  it('LE-S5: no snap when video.ended is true even with large drift (REQ-LE-5)', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED (seam not wired)');
    }

    const VIDEO_EL = document.getElementById('player');

    // drift = 3.0s > threshold — would snap without the ended guard.
    sb._setBuffered(0, 5.0);
    VIDEO_EL.currentTime = 2.0;
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'ended', { configurable: true, get: () => true });

    checkLiveEdge(VIDEO_EL, sb);

    // ended guard must suppress the snap.
    expect(VIDEO_EL.currentTime).toBeCloseTo(2.0, 2);
  });

  // ─── LE-S6: low readyState (waiting/buffering) → no snap (REQ-LE-5) ───────
  //
  // REQ-LE-5: the live-edge snap SHALL NOT execute while the video element is
  // in a "waiting" state. The HTMLMediaElement "waiting" event fires when
  // readyState drops below HAVE_FUTURE_DATA (3). Accordingly, the guard
  // MUST suppress the snap when readyState < 3.
  //
  // Setup: drift = 3.0s > 0.30 (would snap), video not paused, not ended, but
  // readyState = 2 (HAVE_CURRENT_DATA — element has current frame but no future
  // data; this is the state that fires the "waiting" event). New guard blocks it.
  //
  // Falsification: removing `if (videoEl.readyState < HAVE_FUTURE_DATA) return;`
  // from checkLiveEdge would let currentTime be set to 4.90 and fail this test.

  it('LE-S6: no snap when readyState < HAVE_FUTURE_DATA (waiting state, REQ-LE-5)', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED (seam not wired)');
    }

    const VIDEO_EL = document.getElementById('player');

    // drift = 3.0s > threshold — would snap without the readyState guard.
    sb._setBuffered(0, 5.0);
    VIDEO_EL.currentTime = 2.0;
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });
    Object.defineProperty(VIDEO_EL, 'ended', { configurable: true, get: () => false });
    // readyState = 2 (HAVE_CURRENT_DATA): the "waiting" state per HTMLMediaElement spec.
    Object.defineProperty(VIDEO_EL, 'readyState', { configurable: true, get: () => 2 });

    checkLiveEdge(VIDEO_EL, sb);

    // readyState guard must suppress the snap.
    expect(VIDEO_EL.currentTime).toBeCloseTo(2.0, 2);
  });
});
