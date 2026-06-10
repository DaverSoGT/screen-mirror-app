// dead-reason-copy.test.js — S-conf1 (CAP-2-v3), receiver leg
//
// CAP-2-v3 introduced new terminal dead `reason` tokens (peer_unreachable,
// ice_failed_repeatedly). The dead-modal must render HUMAN copy for these tokens,
// not the raw machine token. Today both legs render
//   "Connection lost: " + (payload.reason || "unknown")
// so peer_unreachable would surface as "Connection lost: peer_unreachable".
//
// This asserts the receiver dead-modal (#dead-reason, set by the `dead` case in
// dist/mse-client.js) renders the mapped human copy. Unmapped/absent reasons keep
// the current raw fallback — those are NOT asserted here (behavior unchanged).
//
// RED today: the modal renders the raw token "peer_unreachable" / "ice_failed_repeatedly".

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
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

describe('mse-client — human dead-reason copy (S-conf1 / CAP-2-v3)', () => {
  let tauri;
  let ch;

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
    ch = tauri.lastChannel();
    expect(ch).not.toBeNull();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
  });

  it('S-conf1: peer_unreachable renders "The other device is unreachable"', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'peer_unreachable' }).buffer);
    await Promise.resolve();

    const deadReason = document.getElementById('dead-reason');
    expect(deadReason.textContent).toBe('The other device is unreachable');
    // The raw machine token MUST NOT leak into the user-facing copy.
    expect(deadReason.textContent).not.toContain('peer_unreachable');
  });

  it('S-conf1: ice_failed_repeatedly renders "The connection failed repeatedly"', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' }).buffer);
    await Promise.resolve();

    const deadReason = document.getElementById('dead-reason');
    expect(deadReason.textContent).toBe('The connection failed repeatedly');
    expect(deadReason.textContent).not.toContain('ice_failed_repeatedly');
  });

  it('S-conf1: an unmapped reason keeps the raw fallback (behavior unchanged)', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'dead', reason: 'some_other_reason' }).buffer);
    await Promise.resolve();

    const deadReason = document.getElementById('dead-reason');
    expect(deadReason.textContent).toBe('Connection lost: some_other_reason');
  });

  it('S-conf1: an absent reason keeps the "unknown" fallback (behavior unchanged)', async () => {
    ch._dispatch(makeStatusFrame({ kind: 'dead' }).buffer);
    await Promise.resolve();

    const deadReason = document.getElementById('dead-reason');
    expect(deadReason.textContent).toBe('Connection lost: unknown');
  });
});
