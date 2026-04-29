// handle-message.test.js — SC-SEND-1 through SC-SEND-5
//
// Tests for handleMessage() which is called via channel.onmessage.
// handleMessage is NOT exported — it's reached via ch._dispatch(payload)
// where payload is a JSON-encoded ArrayBuffer (matching the real Tauri path).
//
// R14: Add fail-fast assertion in beforeEach that ch = tauri.lastChannel()
// is not null — if the IIFE didn't register a channel, all asserts pass vacuously.
//
// SC-SEND-1: kind='streaming' → status='Streaming', error=''
// SC-SEND-2: kind='stopped' → button='Start streaming', __sm_streamActive=false
// SC-SEND-3: kind='error' with message → error=message, button reset
// SC-SEND-4: kind='failed' with reason → error=reason, button reset
// SC-SEND-5: kind='button' with label → button=label

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';

// Helper: encode a JS object as UTF-8 ArrayBuffer (sender.js decodes it at line 83)
function encodeMessage(obj) {
  const text = JSON.stringify(obj);
  return new TextEncoder().encode(text).buffer;
}

describe('sender — handleMessage (SC-SEND-1 through SC-SEND-5)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    // 1. DOM first (IIFE reads elements at module parse time)
    installDom();
    // 2. Tauri mock
    tauri = installTauriMock();
    // 3. No fake timers needed for sender.js (no setInterval)
    // 4. Reset modules + dynamic import so IIFE runs fresh
    vi.resetModules();
    await import('../../../dist/sender.js');
    // Flush any microtasks from import
    await Promise.resolve();

    // The Channel is created lazily inside startSender() — it is NOT created
    // at IIFE eval time. We must trigger startSender() via a click to create it.
    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve(); // flush the async click handler
    await Promise.resolve(); // flush invoke('start_sender') microtask

    // R14: fail-fast — if the channel was not registered, tests pass vacuously
    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    // Reset invoke spy so tests start with a clean call count
    tauri.invoke.mockClear();
    tauri.invoke.mockResolvedValue(undefined);
  });

  afterEach(() => {
    removeDom();
    resetTauriMock();
  });

  it('SC-SEND-1: kind="streaming" → status="Streaming", error=""', () => {
    ch._dispatch(encodeMessage({ kind: 'streaming' }));

    const statusDiv = document.getElementById('status');
    const errorDiv = document.getElementById('error');
    expect(statusDiv.textContent).toBe('Streaming');
    expect(errorDiv.textContent).toBe('');
  });

  it('SC-SEND-2: kind="stopped" → button="Start streaming", __sm_streamActive=false', () => {
    // First simulate running state
    ch._dispatch(encodeMessage({ kind: 'button', label: 'Stop streaming' }));
    window.__sm_streamActive = true;

    // Now stop
    ch._dispatch(encodeMessage({ kind: 'stopped' }));

    const startBtn = document.getElementById('start');
    expect(startBtn.textContent).toBe('Start streaming');
    expect(window.__sm_streamActive).toBe(false);

    const statusDiv = document.getElementById('status');
    expect(statusDiv.textContent).toBe('Not connected');
  });

  it('SC-SEND-3: kind="error" → error=message, button reset, __sm_streamActive=false', () => {
    ch._dispatch(encodeMessage({ kind: 'error', message: 'test error' }));

    const errorDiv = document.getElementById('error');
    const startBtn = document.getElementById('start');
    expect(errorDiv.textContent).toBe('test error');
    expect(startBtn.textContent).toBe('Start streaming');
    expect(window.__sm_streamActive).toBe(false);
  });

  it('SC-SEND-4: kind="failed" → error=reason, button reset', () => {
    ch._dispatch(encodeMessage({ kind: 'failed', reason: 'connection refused' }));

    const errorDiv = document.getElementById('error');
    const startBtn = document.getElementById('start');
    expect(errorDiv.textContent).toBe('connection refused');
    expect(startBtn.textContent).toBe('Start streaming');
  });

  it('SC-SEND-5: kind="button" with label → button text updated', () => {
    ch._dispatch(encodeMessage({ kind: 'button', label: 'Stop streaming' }));

    const startBtn = document.getElementById('start');
    expect(startBtn.textContent).toBe('Stop streaming');
  });
});
