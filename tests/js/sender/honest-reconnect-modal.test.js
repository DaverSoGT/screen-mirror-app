// honest-reconnect-modal.test.js — SC-WD-S10 (REQ-WD-10), sender leg
//
// CAP-2-v3: during the bounded retry window the sender status modal MUST NOT render
// a misleading "attempt X/N" counter. The false "/3" denominator is replaced with an
// honest "still trying / waiting for the viewer" message. Only the `reconnecting`
// case status string changes; the `dead` frame and all other behavior are unchanged.
//
// RED today: the status renders "Reconnecting (attempt 1/3)…" → contains "/3".

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';

// Encode a JS object as UTF-8 ArrayBuffer (sender.js decodes it at line ~83).
function encodeMessage(obj) {
  const text = JSON.stringify(obj);
  return new TextEncoder().encode(text).buffer;
}

describe('sender — honest reconnecting modal (SC-WD-S10 / REQ-WD-10)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/sender.js');
    await Promise.resolve();

    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();
    tauri.invoke.mockClear();
    tauri.invoke.mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
  });

  it('SC-WD-S10: reconnecting status does NOT render "/N" and shows an honest waiting message', () => {
    ch._dispatch(encodeMessage({ kind: 'reconnecting', attempt: 1, max: 3 }));

    const status = document.getElementById('status');
    expect(status.textContent).not.toContain('/3');
    expect(status.textContent).not.toMatch(/\/\s*\d/);
    expect(status.textContent.toLowerCase()).toContain('waiting for the viewer');
  });

  it('SC-WD-S10: a later reconnecting frame (attempt 2/3) still shows no "/N"', () => {
    ch._dispatch(encodeMessage({ kind: 'reconnecting', attempt: 2, max: 3 }));

    const status = document.getElementById('status');
    expect(status.textContent).not.toContain('/3');
    expect(status.textContent).not.toMatch(/\/\s*\d/);
  });
});
