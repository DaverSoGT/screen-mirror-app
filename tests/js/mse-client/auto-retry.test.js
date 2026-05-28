// auto-retry.test.js — SC-RRE-1..10 (REQ-AUTORETRY-*, REQ-ROLECHANGE-*)
//
// Covers the receiver auto-retry-on-exhaustion cycle (receiver-retry-on-exhaustion).
// All tests use Strict TDD: this file is the RED commit for these behaviours.
//
// SC-RRE-1:  Dead event arms a 30s timer.
// SC-RRE-2:  Auto-retry fires exactly once at 30s (new Channel, correct invoke args).
// SC-RRE-3:  Manual Retry click before 30s cancels the pending timer.
// SC-RRE-4:  Manual Cancel click before 30s cancels the pending timer.
// SC-RRE-5:  Second Dead re-arms timer; only the fresh one fires.
// SC-RRE-6:  streaming status cancels pending timer.
// SC-RRE-7:  reconnecting status cancels pending timer.
// SC-RRE-8:  Auto-retry hides dead-modal when it fires.
// SC-RRE-9:  #dead-role-change present with correct href / role / aria-label.
// SC-RRE-10: Role-change click sets sm.lastMode=sender + cancels auto-retry.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock, MockChannel } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';

// Build a 0x02 status frame: [0x02, ...UTF-8 JSON bytes].
// Mirrors the helper in mse-teardown-setup.test.js.
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

describe('mse-client — auto-retry on Dead (SC-RRE-1..10)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    localStorage.clear();
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    ch = tauri.lastChannel();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  // ── SC-RRE-1 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-1: after handleStatus({kind:"dead"}), a single 30s timer is armed.
  // Verified by checking that invoke("retry_session_stream") is NOT called at 29 999ms
  // (the timer has not fired yet).
  it('SC-RRE-1: arms a single 30s timer on dead status — no invoke before 30s', async () => {
    tauri.invoke.mockClear();

    // Dispatch a dead status frame to the channel's onmessage.
    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    // Advance to just before the timer fires.
    await vi.advanceTimersByTimeAsync(29_999);
    await Promise.resolve();

    // The auto-retry must NOT have fired yet.
    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBe(0);
  });

  // ── SC-RRE-2 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-2 / REQ-AUTORETRY-8: timer fires exactly once at 30s,
  // creating a new Channel and calling invoke("retry_session_stream", { channel }).
  it('SC-RRE-2: auto-retry fires exactly once after 30s (new Channel, correct invoke signature)', async () => {
    tauri.invoke.mockClear();
    const channelCountBefore = MockChannel._registry.size; // should be 1 (initial)

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    // Advance exactly to the 30s mark.
    await vi.advanceTimersByTimeAsync(30_000);
    // Flush async callbacks from the timer handler.
    await Promise.resolve();
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    // Must have fired exactly once.
    expect(retryCalls.length).toBe(1);
    // Must have been called with a Channel instance as the channel argument.
    expect(retryCalls[0][1]).toBeDefined();
    expect(retryCalls[0][1].channel).toBeInstanceOf(MockChannel);

    // A NEW Channel must have been constructed for the auto-retry.
    expect(MockChannel._registry.size).toBeGreaterThan(channelCountBefore);

    // Advance another 30s — the timer must NOT fire again (single-fire invariant).
    await vi.advanceTimersByTimeAsync(30_000);
    await Promise.resolve();
    const retryCallsAfterDouble = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCallsAfterDouble.length).toBe(1);
  });

  // ── SC-RRE-3 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-3: manual Retry click cancels the pending auto-retry timer.
  it('SC-RRE-3: manual Retry click cancels pending auto-retry', async () => {
    tauri.invoke.mockClear();

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    // Advance 5s (timer still pending).
    await vi.advanceTimersByTimeAsync(5_000);

    // Click the manual Retry button.
    const retryBtn = document.getElementById('receiver-retry');
    retryBtn.dispatchEvent(new Event('click', { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();

    // Advance well past the original 30s mark.
    await vi.advanceTimersByTimeAsync(60_000);
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    // Exactly one call: the manual retry. The auto-retry timer must have been cancelled.
    expect(retryCalls.length).toBe(1);
  });

  // ── SC-RRE-4 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-4: manual Cancel click cancels the pending auto-retry timer.
  it('SC-RRE-4: manual Cancel click cancels pending auto-retry', async () => {
    tauri.invoke.mockClear();

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(5_000);

    const cancelBtn = document.getElementById('receiver-cancel');
    cancelBtn.dispatchEvent(new Event('click', { bubbles: true }));
    await Promise.resolve();
    await Promise.resolve();

    // Advance well past 30s.
    await vi.advanceTimersByTimeAsync(60_000);
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    // Cancel must NOT trigger a retry; auto-retry timer must be cancelled too.
    expect(retryCalls.length).toBe(0);
  });

  // ── SC-RRE-5 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-5: second Dead entry cancels prior timer and schedules a fresh one.
  it('SC-RRE-5: second Dead re-arms timer; only fresh timer fires', async () => {
    tauri.invoke.mockClear();

    const deadFrame1 = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame1.buffer);
    await Promise.resolve();

    // Advance 5s — first timer is running (armed at t=0, fires at t=30 000).
    await vi.advanceTimersByTimeAsync(5_000);

    // Second Dead event at t=5 000.
    const deadFrame2 = makeStatusFrame({ kind: 'dead', reason: 'connection_lost' });
    ch._dispatch(deadFrame2.buffer);
    await Promise.resolve();

    // At t=5 000+29 999=34 999, the fresh timer should NOT have fired yet.
    await vi.advanceTimersByTimeAsync(29_999);
    await Promise.resolve();

    let retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBe(0);

    // At t=5 000+30 000=35 000 the fresh timer fires.
    await vi.advanceTimersByTimeAsync(1);
    await Promise.resolve();
    await Promise.resolve();

    retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    // Only the fresh timer fires — the stale one was cancelled.
    expect(retryCalls.length).toBe(1);
  });

  // ── SC-RRE-6 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-6: streaming status cancels pending timer.
  it('SC-RRE-6: streaming status cancels pending auto-retry', async () => {
    tauri.invoke.mockClear();

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(10_000);

    const streamingFrame = makeStatusFrame({ kind: 'streaming' });
    ch._dispatch(streamingFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Advance well past 30s from the Dead event.
    await vi.advanceTimersByTimeAsync(60_000);
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBe(0);
  });

  // ── SC-RRE-7 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-7: reconnecting status cancels pending timer.
  it('SC-RRE-7: reconnecting status cancels pending auto-retry', async () => {
    tauri.invoke.mockClear();

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(10_000);

    const reconnectingFrame = makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 });
    ch._dispatch(reconnectingFrame.buffer);
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(60_000);
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBe(0);
  });

  // ── SC-RRE-8 ────────────────────────────────────────────────────────────────
  // REQ-AUTORETRY-1: when the auto-retry fires, it hides the dead-modal.
  it('SC-RRE-8: auto-retry hides dead-modal when it fires', async () => {
    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    // Dead-modal should be visible now.
    const deadModal = document.getElementById('dead-modal');
    expect(deadModal.hidden).toBe(false);

    // Let auto-retry fire.
    await vi.advanceTimersByTimeAsync(30_000);
    await Promise.resolve();
    await Promise.resolve();

    // Dead-modal must be hidden after auto-retry.
    expect(deadModal.hidden).toBe(true);
  });

  // ── SC-RRE-9 ────────────────────────────────────────────────────────────────
  // REQ-ROLECHANGE-1 / REQ-ROLECHANGE-3 / NFR-3:
  // #dead-role-change present in DOM with correct href, role, aria-label.
  it('SC-RRE-9: #dead-role-change present with correct href, role, aria-label', () => {
    const el = document.getElementById('dead-role-change');

    expect(el).not.toBeNull();
    expect(el.getAttribute('href')).toBe('./sender.html');
    expect(el.getAttribute('role')).toBe('button');
    // Non-empty accessible name (NFR-3).
    const label = el.getAttribute('aria-label');
    expect(label).toBeTruthy();
    expect(label.length).toBeGreaterThan(0);
  });

  // ── SC-RRE-10 ───────────────────────────────────────────────────────────────
  // REQ-ROLECHANGE-2: role-change click writes sm.lastMode=sender (SC-NFR-1 embedded:
  // auto-retry path must NOT write localStorage; role-change path MUST write it).
  // Also verifies the click cancels the auto-retry timer.
  it('SC-RRE-10: role-change click sets sm.lastMode=sender + cancels auto-retry', async () => {
    tauri.invoke.mockClear();

    const deadFrame = makeStatusFrame({ kind: 'dead', reason: 'ice_failed_repeatedly' });
    ch._dispatch(deadFrame.buffer);
    await Promise.resolve();

    await vi.advanceTimersByTimeAsync(5_000);

    // Spy on localStorage for the SC-NFR-1 assertion (no writes from auto-retry).
    // At this point, localStorage should be clean.
    expect(localStorage.getItem('sm.lastMode')).toBeNull();

    // Click the role-change affordance.
    const roleChangeEl = document.getElementById('dead-role-change');
    roleChangeEl.dispatchEvent(new Event('click', { bubbles: true }));
    await Promise.resolve();

    // localStorage must have been set to "sender".
    expect(localStorage.getItem('sm.lastMode')).toBe('sender');

    // Advance well past 30s — the auto-retry must have been cancelled.
    await vi.advanceTimersByTimeAsync(60_000);
    await Promise.resolve();

    const retryCalls = tauri.invoke.mock.calls.filter(
      (call) => call[0] === 'retry_session_stream'
    );
    expect(retryCalls.length).toBe(0);
  });
});
