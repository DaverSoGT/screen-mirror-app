// demux-0x02.test.js — T8.2: 0x02 JSON status frame demuxer
//
// SC-T8-1: A 0x02 frame whose payload is valid UTF-8 JSON must call
//          handleStatus(payload) without feeding bytes to SourceBuffer.
// SC-T8-2: A 0x02 frame with malformed JSON must NOT throw — it must log a
//          warning and silently drop the frame.
// SC-T8-3: A 0x01 frame after a 0x02 frame must still be appended normally
//          (0x02 frames must not corrupt the segmentation state).
// SC-T8-4: A 0x02 frame must NOT trigger appendBuffer on the SourceBuffer.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41 } from '../fixtures/init-segments.js';
import { makeInitFrame, makeMediaSegmentFrame } from '../fixtures/media-segments.js';

// Build a 0x02 frame: [0x02, ...UTF-8 JSON bytes]
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

// Build a 0x02 frame with raw (potentially malformed) bytes after the discriminant.
function makeRawStatusFrame(bytes) {
  const frame = new Uint8Array(1 + bytes.length);
  frame[0] = 0x02;
  frame.set(bytes, 1);
  return frame;
}

describe('mse-client — 0x02 JSON status frame demuxer (T8.2)', () => {
  let tauri;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // Flush sourceopen microtask
    await vi.advanceTimersByTimeAsync(0);
    // Flush start_stream resolve
    await Promise.resolve();

    // Prime with an init segment so SourceBuffer exists
    const ch = tauri.lastChannel();
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Clear appendBuffer calls from init so counts start from zero
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

  it('SC-T8-1: 0x02 frame with valid JSON calls handleStatus — kind is preserved', async () => {
    const consoleSpy = vi.spyOn(console, 'log');
    const ch = tauri.lastChannel();

    const statusPayload = { kind: 'reconnecting', attempt: 1, max: 3 };
    const frame = makeStatusFrame(statusPayload);
    ch._dispatch(frame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // handleStatus must have logged with the kind value
    const statusCalls = consoleSpy.mock.calls.filter(
      (call) => typeof call[0] === 'string' && call[0].includes('[mse-client] status:')
    );
    expect(statusCalls.length).toBeGreaterThanOrEqual(1);
    // The kind must appear in the log
    const flatArgs = statusCalls.flatMap((c) => c);
    expect(flatArgs).toContain('reconnecting');
  });

  it('SC-T8-4: 0x02 frame must NOT call appendBuffer on the SourceBuffer', async () => {
    const ch = tauri.lastChannel();
    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms?._sb;

    const frame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(frame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    expect(sb?.appendBuffer).not.toHaveBeenCalled();
  });

  it('SC-T8-2: 0x02 frame with malformed JSON does not throw — logs warning', async () => {
    const warnSpy = vi.spyOn(console, 'warn');
    const ch = tauri.lastChannel();

    // Broken UTF-8 / not valid JSON
    const bad = new Uint8Array([0x7b, 0x22, 0x62, 0x61, 0x64]); // '{"bad' — unclosed
    const frame = makeRawStatusFrame(bad);

    expect(() => {
      ch._dispatch(frame.buffer);
    }).not.toThrow();

    await Promise.resolve();

    const warnCalls = warnSpy.mock.calls.filter(
      (call) => typeof call[0] === 'string' && call[0].includes('[mse-client]')
    );
    expect(warnCalls.length).toBeGreaterThanOrEqual(1);
  });

  it('SC-T8-3: 0x01 segment after a non-lifecycle 0x02 frame is still appended normally', async () => {
    const ch = tauri.lastChannel();
    const ms = MockMediaSourceCtor._lastInstance;
    const sb = ms?._sb;

    // Send a 0x02 status frame with a kind that does NOT trigger MSE teardown.
    // 'connecting' falls through to the default log-only case in handleStatus,
    // so the SourceBuffer and initReceived flag are untouched.
    // (Note: 'reconnecting'/'dead' now invoke tearDownMse by design — see T10.1 tests.)
    const statusFrame = makeStatusFrame({ kind: 'connecting' });
    ch._dispatch(statusFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Now send a regular media segment
    const segFrame = makeMediaSegmentFrame();
    ch._dispatch(segFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // appendBuffer must have been called exactly once (for the segment, not the status)
    expect(sb?.appendBuffer).toHaveBeenCalledTimes(1);
  });
});
