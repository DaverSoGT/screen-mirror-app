// attach-stream.test.js — SC-S8-1, SC-S8-2 (B11-S8 regression guard)
//
// SC-S8-1: invoke('attach_stream') fires after start_stream resolves.
// SC-S8-2: invoke('attach_stream') NOT called if start_stream rejects.
//
// Regression for B11-S8: mse-client.js:324,330 must call invoke('attach_stream'),
// NOT invoke('request_keyframe'). Reverting or removing that call → SC-S8-1 fails.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';

describe('mse-client — attach_stream invocation (SC-S8-1, SC-S8-2)', () => {
  let tauri;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    // R12: fake timers BEFORE import
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // Flush sourceopen microtask
    await vi.advanceTimersByTimeAsync(0);
    // Flush start_stream resolve
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('SC-S8-1: invoke("attach_stream") is called at least once after start_stream resolves', () => {
    // start_stream resolves (mockResolvedValue(undefined)) — the immediate
    // FIRE_PLI() call at mse-client.js:330 must have fired by now.
    const attachCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'attach_stream'
    );
    expect(attachCalls.length).toBeGreaterThanOrEqual(1);
  });

  it('SC-S8-1 variant: the exact string "attach_stream" is used (not "request_keyframe")', () => {
    // Verify the exact invocation string — prevents regression to 'request_keyframe'
    const invocations = tauri.invoke.mock.calls.map((call) => call[0]);
    expect(invocations).toContain('attach_stream');
    expect(invocations).not.toContain('request_keyframe');
  });

  it('SC-S8-2: invoke("attach_stream") NOT called when start_stream rejects', async () => {
    // This test needs its own setup with start_stream rejecting
    // Teardown is handled by afterEach; we need a fresh import
    vi.useRealTimers();
    removeDom();
    resetTauriMock();

    // Re-setup with a rejecting start_stream
    installDom();
    const freshTauri = installTauriMock();
    freshTauri.invoke.mockImplementation(async (cmd) => {
      if (cmd === 'start_stream') throw new Error('connection refused');
      return undefined;
    });
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    // Flush the rejection
    await Promise.resolve();
    await Promise.resolve();

    // Count attach_stream calls — must be 0
    const attachCalls = freshTauri.invoke.mock.calls.filter(
      (call) => call[0] === 'attach_stream'
    );
    expect(attachCalls.length).toBe(0);
  });
});
