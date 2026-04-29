// buttons.test.js — SC-SEND-6, SC-SEND-7
//
// SC-SEND-6: fresh import + start button click → invoke('start_sender') called
//            with { channel, udpPort: null, serviceName: null }
// SC-SEND-7: running state + start button click → invoke('stop_sender') called
//
// R14: fail-fast — assert tauri.lastChannel() !== null in beforeEach.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock, MockChannel } from '../mocks/tauri.js';

// Helper: encode a JS object as UTF-8 ArrayBuffer
function encodeMessage(obj) {
  const text = JSON.stringify(obj);
  return new TextEncoder().encode(text).buffer;
}

describe('sender — button click handlers (SC-SEND-6, SC-SEND-7)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.resetModules();
    await import('../../../dist/sender.js');
    await Promise.resolve();
    // Channel is created lazily inside startSender() — no channel exists yet
    // after module load; the R14 check is deferred to individual tests.
  });

  afterEach(() => {
    removeDom();
    resetTauriMock();
  });

  it('SC-SEND-6: idle state + start click → invoke("start_sender") with channel, nulls', async () => {
    const startBtn = document.getElementById('start');
    startBtn.click();
    // Allow the async click handler to run
    await Promise.resolve();
    await Promise.resolve();

    // R14: fail-fast after click that creates the channel
    const clickCh = tauri.lastChannel();
    expect(clickCh).not.toBeNull();

    // start_sender must have been called
    const startCalls = tauri.invoke.mock.calls.filter((c) => c[0] === 'start_sender');
    expect(startCalls.length).toBe(1);

    const [, args] = startCalls[0];
    expect(args).toMatchObject({
      udpPort: null,
      serviceName: null,
    });
    // Channel must be a MockChannel instance
    expect(args.channel).toBeInstanceOf(MockChannel);
  });

  it('SC-SEND-7: running state + start click → invoke("stop_sender")', async () => {
    // First click creates channel and starts sender
    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    // R14: fail-fast
    const ch = tauri.lastChannel();
    expect(ch).not.toBeNull();

    // Set running state via channel message
    ch._dispatch(new TextEncoder().encode(JSON.stringify({ kind: 'button', label: 'Stop streaming' })).buffer);
    await Promise.resolve();

    // Second click should stop
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    const stopCalls = tauri.invoke.mock.calls.filter((c) => c[0] === 'stop_sender');
    expect(stopCalls.length).toBe(1);
  });
});
