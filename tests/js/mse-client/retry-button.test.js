// retry-button.test.js — SC-B2-001, SC-B2-002, SC-NO-RELOAD-001 (REQ-B2, REQ-NO-RELOAD)
//
// SC-B2-001: Static check — no window.location.reload() in Retry code path.
//            After module import, parse the Retry handler source and assert zero
//            occurrences of "location.reload" in the receiver-retry click path.
//
// SC-B2-002: Behaviour check — Retry button click invokes
//            invoke("retry_session_stream") and does NOT call
//            window.location.reload().
//
// SC-NO-RELOAD-001: Same negative assertion as SC-B2-002 but expressed as a
//                   standalone spy to be resilient against future renames.
//
// REQ-NO-RELOAD (spec §2 REQ-NO-RELOAD): window.location.reload() MUST NOT
// appear in ANY Retry code path. This test is the CI static gate.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';

describe('mse-client — receiver Retry button (SC-B2-001, SC-B2-002, SC-NO-RELOAD-001)', () => {
  let tauri;

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
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('SC-B2-001: Retry handler calls invoke("retry_session_stream") not reload', async () => {
    // Spy on window.location.reload to detect any call.
    // happy-dom's location.reload may not be spyable via vi.spyOn directly,
    // so we stub it via vi.stubGlobal on the reload property.
    const reloadSpy = vi.fn();
    vi.stubGlobal('location', { ...globalThis.location, reload: reloadSpy });

    const retryBtn = document.getElementById('receiver-retry');
    expect(retryBtn).not.toBeNull();

    // Reset invoke mock call history before click.
    tauri.invoke.mockClear();

    // Dispatch click event on the Retry button.
    retryBtn.dispatchEvent(new Event('click', { bubbles: true }));

    // Flush async handlers (the click handler is async).
    await Promise.resolve();
    await Promise.resolve();

    // THEN: invoke must have been called with 'retry_session_stream'.
    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBeGreaterThanOrEqual(1);

    // THEN: window.location.reload must NOT have been called (REQ-NO-RELOAD).
    expect(reloadSpy).not.toHaveBeenCalled();
  });

  it('SC-B2-002: invoke("stop_stream") is NOT called separately by Retry (retry_session_stream handles stop internally)', async () => {
    const reloadSpy = vi.fn();
    vi.stubGlobal('location', { ...globalThis.location, reload: reloadSpy });

    const retryBtn = document.getElementById('receiver-retry');
    expect(retryBtn).not.toBeNull();

    tauri.invoke.mockClear();

    retryBtn.dispatchEvent(new Event('click', { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();

    // The new path only calls retry_session_stream — NOT a separate stop_stream.
    // This confirms the IPC is the single entrypoint (D-2 design).
    const stopCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'stop_stream'
    );
    expect(stopCalls.length).toBe(0);

    // Must not reload.
    expect(reloadSpy).not.toHaveBeenCalled();
  });

  it('SC-NO-RELOAD-001: window.location.reload does not appear in Retry path (static module check)', async () => {
    // Load the raw source text of mse-client.js and locate the receiver-retry
    // event handler. Assert that "location.reload" does not appear in the handler.
    const fs = await import('fs');
    const path = await import('path');
    const src = fs.readFileSync(
      path.resolve('./dist/mse-client.js'),
      'utf8'
    );

    // Find the receiverRetryBtn addEventListener block.
    // We search for the handler registration and extract a window of text.
    const retryHandlerMatch = src.match(
      /receiverRetryBtn\.addEventListener\s*\([^)]+function[^{]*\{[\s\S]*?\}\s*\)\s*;/
    );

    if (retryHandlerMatch) {
      expect(retryHandlerMatch[0]).not.toContain('location.reload');
    } else {
      // If the regex did not match (unlikely — would indicate the handler was
      // restructured), fall back to a coarser check: assert the full source
      // near the word 'receiver-retry' has no reload call.
      const retryIdx = src.indexOf('receiver-retry');
      expect(retryIdx).toBeGreaterThan(-1);
      // 500-char window after 'receiver-retry' occurrence
      const window500 = src.slice(retryIdx, retryIdx + 500);
      expect(window500).not.toContain('location.reload');
    }
  });
});
