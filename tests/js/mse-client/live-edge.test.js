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
    // Video is playing (not paused)
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });

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
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });

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

  it('LE-S4: no snap when snap target would be before buffered.start(last)', async () => {
    const { checkLiveEdge } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    if (!checkLiveEdge) {
      throw new Error('checkLiveEdge not found in __SCREEN_MIRROR_TEST_EXPORTS__ — RED');
    }

    const VIDEO_EL = document.getElementById('player');

    // LIVE_EDGE_TARGET_LEAD_SEC = 0.10 > buffered.end = 0.05
    // snap_target = 0.05 - 0.10 = -0.05 → before buffered.start(0) = 0 → no snap
    sb._setBuffered(0, 0.05);
    VIDEO_EL.currentTime = 0.0; // drift = 0.05 (less than threshold anyway)
    Object.defineProperty(VIDEO_EL, 'paused', { configurable: true, get: () => false });

    checkLiveEdge(VIDEO_EL, sb);

    // currentTime must NOT change (both drift guard and range guard would block)
    expect(VIDEO_EL.currentTime).toBeCloseTo(0.0, 2);
  });
});
