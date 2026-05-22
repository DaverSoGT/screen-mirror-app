// live-edge.test.js — SC-LIVE-EDGE-1..4: receiver live-edge snap guard
//
// SC-LIVE-EDGE-1: When drift > LIVE_EDGE_MAX_DRIFT_SEC and queue is drained,
//                 seekToLiveEdge() sets currentTime to bufEnd - LIVE_EDGE_TARGET_LEAD_SEC.
// SC-LIVE-EDGE-2: When drift <= LIVE_EDGE_MAX_DRIFT_SEC, currentTime is NOT changed.
// SC-LIVE-EDGE-3: It never seeks backward (target <= currentTime → no change).
// SC-LIVE-EDGE-4: It does nothing while mseState.pending.length > 0 or sb.updating
//                 (burst not fully appended yet).

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame } from '../fixtures/media-segments.js';

// Helper: build a fake TimeRanges with a single range [start, end].
function makeBuffered(start, end) {
  return { length: 1, start: () => start, end: () => end };
}

describe('mse-client — live-edge seek logic (SC-LIVE-EDGE-1..4)', () => {
  let tauri;
  let exports;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // Flush sourceopen microtask so main() proceeds past the await.
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    // Prime with an init segment so SourceBuffer exists.
    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    // Flush: init → addSourceBuffer → enqueue → appendBuffer → updateend
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

  // ── SC-LIVE-EDGE-1 ────────────────────────────────────────────────────────
  // Drift > threshold + queue drained → snap currentTime to bufEnd - lead.
  it('SC-LIVE-EDGE-1: snaps currentTime to live edge when drift exceeds threshold', () => {
    const { seekToLiveEdge, LIVE_EDGE_MAX_DRIFT_SEC, LIVE_EDGE_TARGET_LEAD_SEC } = exports;

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // Simulate: buffered = [0 → 8], currentTime = 1 (drift = 7s — far behind live).
    sb.updating = false;
    sb.buffered = makeBuffered(0, 8);

    const videoEl = document.getElementById('player');
    videoEl.currentTime = 1;

    seekToLiveEdge();

    const expectedTarget = 8 - LIVE_EDGE_TARGET_LEAD_SEC;
    expect(videoEl.currentTime).toBeCloseTo(expectedTarget, 5);
  });

  // ── SC-LIVE-EDGE-2 ────────────────────────────────────────────────────────
  // Drift <= threshold → do nothing.
  it('SC-LIVE-EDGE-2: does NOT seek when drift is within LIVE_EDGE_MAX_DRIFT_SEC', () => {
    const { seekToLiveEdge, LIVE_EDGE_MAX_DRIFT_SEC } = exports;

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // Drift = 0.3 which is below the 0.5 threshold.
    const bufEnd = 5.3;
    const currentTime = 5.0;
    sb.updating = false;
    sb.buffered = makeBuffered(0, bufEnd);

    const videoEl = document.getElementById('player');
    videoEl.currentTime = currentTime;

    seekToLiveEdge();

    // Must NOT have moved.
    expect(videoEl.currentTime).toBe(currentTime);
  });

  // ── SC-LIVE-EDGE-3 ────────────────────────────────────────────────────────
  // target = bufEnd - lead <= currentTime → never seek backward.
  it('SC-LIVE-EDGE-3: does NOT seek backward when target <= currentTime', () => {
    const { seekToLiveEdge } = exports;

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // bufEnd=5.3, lead=0.2 → target=5.1, currentTime=5.25 → target < currentTime.
    // Drift = 5.3 - 5.25 = 0.05, which is also below threshold — but we want to test
    // the backward-seek guard specifically. Use a large drift but currentTime already
    // past the target so only the backward guard applies.
    // bufEnd=10, lead=0.2 → target=9.8, currentTime=9.9 → target < currentTime.
    const bufEnd = 10;
    const currentTimeVal = 9.9; // already past the target of 9.8
    sb.updating = false;
    sb.buffered = makeBuffered(0, bufEnd);

    const videoEl = document.getElementById('player');
    videoEl.currentTime = currentTimeVal;

    seekToLiveEdge();

    // currentTime must NOT have changed.
    expect(videoEl.currentTime).toBeCloseTo(currentTimeVal, 5);
  });

  // ── SC-LIVE-EDGE-4a ───────────────────────────────────────────────────────
  // sb.updating = true → do nothing (burst in progress).
  it('SC-LIVE-EDGE-4a: does nothing while sb.updating is true', () => {
    const { seekToLiveEdge } = exports;

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    sb.updating = true;
    sb.buffered = makeBuffered(0, 8);

    const videoEl = document.getElementById('player');
    videoEl.currentTime = 1;

    seekToLiveEdge();

    expect(videoEl.currentTime).toBe(1);
  });

  // ── SC-LIVE-EDGE-4b ───────────────────────────────────────────────────────
  // mseState.pending.length > 0 → do nothing (chunks still queued).
  it('SC-LIVE-EDGE-4b: does nothing while there are pending chunks in the queue', async () => {
    const { seekToLiveEdge } = exports;

    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms._sb;

    // Force sb.updating=true so the next appendBuffer call keeps it busy,
    // which causes enqueue to push to pending without flushing.
    sb.updating = true;
    sb.buffered = makeBuffered(0, 8);

    // Push a fake chunk directly into the pending queue by dispatching a media
    // segment while sb.updating=true — flushQueue will push to pending and return.
    // We use a raw Uint8Array for simplicity.
    const { makeMediaSegmentFrame } = await import('../fixtures/media-segments.js');
    const segFrame = makeMediaSegmentFrame();
    const ch = tauri.lastChannel();
    ch._dispatch(segFrame.buffer);
    await Promise.resolve();

    // Now simulate the scenario: sb is not updating anymore but pending still has items.
    sb.updating = false;

    const videoEl = document.getElementById('player');
    videoEl.currentTime = 1;

    seekToLiveEdge();

    // Must NOT have seeked — pending queue is not empty.
    expect(videoEl.currentTime).toBe(1);
  });
});
