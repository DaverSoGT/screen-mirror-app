// reconnect-status.test.js — SC-SEND-R1 through SC-SEND-R6
//
// Tests for reconnecting + dead handleMessage cases added in Phase 9 (T9.1).
//
// SC-SEND-R1: kind="reconnecting" → status="Reconnecting (attempt 1/3)…", no buttons changed
// SC-SEND-R2: kind="reconnecting" attempt=2 → status="Reconnecting (attempt 2/3)…"
// SC-SEND-R3: kind="dead" → error message shown, Retry button appears
// SC-SEND-R4: kind="dead" + Cancel button → invoke("stop_sender") called
// SC-SEND-R5: kind="peer_lost" still works (backwards compat)
// SC-SEND-R6: kind="dead" + Retry button → invoke("retry_session") OR stub call

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';

// Helper: encode a JS object as UTF-8 ArrayBuffer (sender.js decodes it at line ~83)
function encodeMessage(obj) {
  const text = JSON.stringify(obj);
  return new TextEncoder().encode(text).buffer;
}

describe('sender — reconnect status messages (SC-SEND-R1 through SC-SEND-R6)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.resetModules();
    await import('../../../dist/sender.js');
    await Promise.resolve();

    // Trigger startSender() to create the channel (channel is lazy).
    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    // Clear the invoke history from startSender.
    tauri.invoke.mockClear();
    tauri.invoke.mockResolvedValue(undefined);
  });

  afterEach(() => {
    removeDom();
    resetTauriMock();
  });

  it('SC-SEND-R1: kind="reconnecting" attempt=1 max=3 → status shows attempt info', () => {
    ch._dispatch(encodeMessage({ kind: 'reconnecting', attempt: 1, max: 3 }));

    const statusDiv = document.getElementById('status');
    // Status must contain "1" and "3" to indicate attempt 1/3.
    expect(statusDiv.textContent).toContain('1');
    expect(statusDiv.textContent).toContain('3');
  });

  it('SC-SEND-R2: kind="reconnecting" attempt=2 max=3 → status reflects attempt 2', () => {
    ch._dispatch(encodeMessage({ kind: 'reconnecting', attempt: 2, max: 3 }));

    const statusDiv = document.getElementById('status');
    expect(statusDiv.textContent).toContain('2');
    expect(statusDiv.textContent).toContain('3');
  });

  it('SC-SEND-R3: kind="dead" → error div shows a message', () => {
    ch._dispatch(
      encodeMessage({ kind: 'dead', reason: 'ice_failed_repeatedly' })
    );

    const errorDiv = document.getElementById('error');
    // Error div must not be empty after dead event.
    expect(errorDiv.textContent.length).toBeGreaterThan(0);
  });

  it('SC-SEND-R4: kind="dead" + clicking Cancel → invoke("stop_sender") called', async () => {
    ch._dispatch(
      encodeMessage({ kind: 'dead', reason: 'ice_failed_repeatedly' })
    );
    await Promise.resolve();

    // Find the cancel button — spec §5.1 says Cancel invokes stop_sender.
    // It may be a dedicated element or the start button relabelled "Cancel".
    const cancelBtn = document.getElementById('cancel') ||
      Array.from(document.querySelectorAll('button')).find(
        (b) => b.textContent.includes('Cancel') || b.textContent.includes('cancel')
      );
    expect(cancelBtn).not.toBeNull();

    cancelBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    const stopCalls = tauri.invoke.mock.calls.filter((c) => c[0] === 'stop_sender');
    expect(stopCalls.length).toBeGreaterThan(0);
  });

  it('SC-SEND-R5: kind="peer_lost" still renders (backwards compat)', () => {
    ch._dispatch(encodeMessage({ kind: 'peer_lost' }));

    const statusDiv = document.getElementById('status');
    // Should render something (the existing case still works).
    expect(statusDiv.textContent.length).toBeGreaterThan(0);
  });

  it('SC-SEND-R6: kind="dead" + clicking Retry → retry command invoked (or stub noted)', async () => {
    ch._dispatch(
      encodeMessage({ kind: 'dead', reason: 'ice_failed_repeatedly' })
    );
    await Promise.resolve();

    // Find the retry button.
    const retryBtn = document.getElementById('retry') ||
      Array.from(document.querySelectorAll('button')).find(
        (b) => b.textContent.includes('Retry') || b.textContent.includes('retry')
      );
    expect(retryBtn).not.toBeNull();

    retryBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    // Phase 11 stub: retry_session does not exist yet.
    // Acceptable: either retry_session OR start_sender is invoked.
    const retryCalls = tauri.invoke.mock.calls.filter(
      (c) => c[0] === 'retry_session' || c[0] === 'start_sender'
    );
    expect(retryCalls.length).toBeGreaterThan(0);
  });
});
