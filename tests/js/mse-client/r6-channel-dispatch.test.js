// r6-channel-dispatch.test.js — SC-SSR-11, SC-SSR-12 (REQ-SSR-9)
//
// R-6 bug: triggerRetry() creates a new Channel but never binds .onmessage on it.
// The frame dispatcher is bound once in main() on the initial channel only.
// After a retry, Rust delivers frames into the new Channel but JS never receives
// them — they are silently dropped.
//
// SC-SSR-11: After triggerRetry(), the NEW Channel's .onmessage MUST be a function.
// SC-SSR-12: Dispatching a status frame through the new Channel MUST reach handleStatus.
//
// Both tests are RED under current code: triggerRetry() does not bind .onmessage,
// so the new Channel's .onmessage remains null after the call.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';

// Build a 0x02 status frame: [0x02, ...UTF-8 JSON bytes].
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

describe('mse-client — R-6 channel onmessage rebind after triggerRetry (SC-SSR-11, SC-SSR-12)', () => {
  let tauri;
  let initialChannel;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    // Capture the channel that main() created for start_stream.
    initialChannel = tauri.lastChannel();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // ── SC-SSR-11 ────────────────────────────────────────────────────────────────
  // After triggerRetry() constructs a new Channel, its .onmessage MUST be bound
  // to the frame-dispatch handler before invoke() is called.
  // RED under current code: triggerRetry() never binds onmessage on the new Channel.
  it('SC-SSR-11: triggerRetry() binds .onmessage on the newly constructed Channel', async () => {
    // Trigger a dead event so the retry button is visible (dead-modal shown).
    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    initialChannel._dispatch(deadFrame.buffer);
    await Promise.resolve();

    // Click the receiver-retry button — this calls cancelAutoRetry() then triggerRetry().
    const retryBtn = document.getElementById('receiver-retry');
    expect(retryBtn).not.toBeNull();
    retryBtn.dispatchEvent(new Event('click', { bubbles: true }));
    // Flush the async triggerRetry() — two microtask flushes cover the await invoke().
    await Promise.resolve();
    await Promise.resolve();

    // The new Channel is the last one constructed (triggerRetry creates one per call).
    const newChannel = tauri.lastChannel();

    // Sanity: a NEW channel must have been created (not the same as the initial one).
    expect(newChannel).not.toBeNull();
    expect(newChannel).not.toBe(initialChannel);

    // SC-SSR-11 assertion: .onmessage MUST be a function, not null.
    // RED: current code never assigns onmessage on the new Channel.
    expect(typeof newChannel.onmessage).toBe('function');
  });

  // ── SC-SSR-12 ────────────────────────────────────────────────────────────────
  // After triggerRetry() and onmessage is bound, dispatching a status frame
  // through the new Channel MUST call handleStatus (i.e. reach JS via the dispatcher).
  // RED under current code: onmessage is null so dispatch is a no-op.
  it('SC-SSR-12: dispatching a status frame through the post-retry Channel reaches handleStatus', async () => {
    // Trigger dead → click retry → flush.
    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    initialChannel._dispatch(deadFrame.buffer);
    await Promise.resolve();

    const retryBtn = document.getElementById('receiver-retry');
    retryBtn.dispatchEvent(new Event('click', { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();

    const newChannel = tauri.lastChannel();
    expect(newChannel).not.toBe(initialChannel);

    // Spy on console.log to detect the handleStatus log line.
    // handleStatus logs: `[mse-client] status: <kind> <payload>`.
    const consoleSpy = vi.spyOn(console, 'log');

    // Dispatch a streaming frame through the new channel's onmessage.
    // SC-SSR-12: this MUST reach handleStatus.
    const streamingFrame = makeStatusFrame({ kind: 'streaming' });
    // Use _dispatch if onmessage is bound; if it is null this is a no-op (RED path).
    newChannel._dispatch(streamingFrame.buffer);
    await Promise.resolve();

    // SC-SSR-12 assertion: handleStatus must have logged the status event.
    // RED: current code — onmessage is null, _dispatch is a no-op, nothing logged.
    const statusLogs = consoleSpy.mock.calls.filter(
      (call) => typeof call[0] === 'string' && call[0].includes('[mse-client] status:')
    );
    expect(statusLogs.length).toBeGreaterThanOrEqual(1);
    // The logged kind must be 'streaming'.
    const flatArgs = statusLogs.flatMap((c) => c);
    expect(flatArgs).toContain('streaming');
  });
});
