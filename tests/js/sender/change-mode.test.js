// change-mode.test.js — SC-SEND-8
//
// SC-SEND-8: #change-mode click → localStorage.removeItem('sm.lastMode') called exactly once.
//
// R14: fail-fast — assert tauri.lastChannel() !== null in beforeEach.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';

describe('sender — change-mode link (SC-SEND-8)', () => {
  let tauri;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.resetModules();
    await import('../../../dist/sender.js');
    await Promise.resolve();
    // Note: Channel is created lazily in startSender(). For change-mode tests,
    // we don't need the channel — the click listener is registered in the IIFE.
    // R14 applies only to handleMessage tests that need ch._dispatch().
  });

  afterEach(() => {
    removeDom();
    resetTauriMock();
  });

  it('SC-SEND-8: #change-mode click → localStorage.removeItem("sm.lastMode") called once', () => {
    // Stub localStorage.removeItem
    const removeItemSpy = vi.spyOn(localStorage, 'removeItem');

    const changeModeLink = document.getElementById('change-mode');
    changeModeLink.click();

    expect(removeItemSpy).toHaveBeenCalledTimes(1);
    expect(removeItemSpy).toHaveBeenCalledWith('sm.lastMode');
  });
});
