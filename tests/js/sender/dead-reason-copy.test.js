// dead-reason-copy.test.js — S-conf1 (CAP-2-v3), sender leg
//
// CAP-2-v3 introduced new terminal dead `reason` tokens (peer_unreachable,
// ice_failed_repeatedly). The dead-modal must render HUMAN copy for these tokens,
// not the raw machine token. Today the sender renders
//   "Connection lost: " + (value.reason || "unknown")
// so peer_unreachable would surface as "Connection lost: peer_unreachable".
//
// This asserts the sender dead-modal (#error, set by the `dead` case in
// dist/sender.js) renders the mapped human copy. Unmapped/absent reasons keep the
// current raw fallback — those are NOT asserted here (behavior unchanged).
//
// RED today: the modal renders the raw token "peer_unreachable" / "ice_failed_repeatedly".

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';

// Helper: encode a JS object as UTF-8 ArrayBuffer (sender.js decodes it at line 83).
function encodeMessage(obj) {
  const text = JSON.stringify(obj);
  return new TextEncoder().encode(text).buffer;
}

describe('sender — human dead-reason copy (S-conf1 / CAP-2-v3)', () => {
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

  it('S-conf1: peer_unreachable renders "The other device is unreachable"', () => {
    ch._dispatch(encodeMessage({ kind: 'dead', reason: 'peer_unreachable' }));

    const errorDiv = document.getElementById('error');
    expect(errorDiv.textContent).toBe('The other device is unreachable');
    expect(errorDiv.textContent).not.toContain('peer_unreachable');
  });

  it('S-conf1: ice_failed_repeatedly renders "The connection failed repeatedly"', () => {
    ch._dispatch(encodeMessage({ kind: 'dead', reason: 'ice_failed_repeatedly' }));

    const errorDiv = document.getElementById('error');
    expect(errorDiv.textContent).toBe('The connection failed repeatedly');
    expect(errorDiv.textContent).not.toContain('ice_failed_repeatedly');
  });

  it('S-conf1: an unmapped reason keeps the raw fallback (behavior unchanged)', () => {
    ch._dispatch(encodeMessage({ kind: 'dead', reason: 'some_other_reason' }));

    const errorDiv = document.getElementById('error');
    expect(errorDiv.textContent).toBe('Connection lost: some_other_reason');
  });

  it('S-conf1: an absent reason keeps the "unknown" fallback (behavior unchanged)', () => {
    ch._dispatch(encodeMessage({ kind: 'dead' }));

    const errorDiv = document.getElementById('error');
    expect(errorDiv.textContent).toBe('Connection lost: unknown');
  });
});
