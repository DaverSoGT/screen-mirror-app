// init-recovery.test.js — SC-IR-1..8 (init-segment recovery fix)
//
// These tests are RED against the current dist/mse-client.js because onInitFrame
// drops a FRAME_INIT that arrives before the MediaSource reaches readyState='open'
// (no pendingInit queue, no self-arm on ms===null, no teardown discard).
//
// All six scenarios exercise the "init arrives during the setUpMse async gap"
// race that is invisible to the current mock (which auto-opens synchronously).
// The new deferOpen mode in MockMediaSourceCtor (WU-IR-MOCK) makes the race
// reachable by NOT auto-firing sourceopen — tests drive _fireSourceOpen() manually.
//
// SC-IR-1: init during setUpMse gap → queued + applied on sourceopen → recovers
// SC-IR-2: streaming before any media → no permanent starvation
// SC-IR-3: FRAME_INIT with ms===null → self-arms setUpMse exactly once
// SC-IR-4: two pre-open inits → only latest applied (single-slot queue)
// SC-IR-5: REQ-SSR-4 frozen-frame preserved (reconnecting alone does NOT teardown)
// SC-IR-6: stale pendingInit discarded on teardown (session A init can't leak to B)
// SC-IR-7: recovery init arrives before streaming during Stage 1 → tears down stale
//          session, self-arms, segment appended (Guard 3 fix)
// SC-IR-8: Stage-2 teardown invariant — revealReconnectingOverlay MUST call
//          tearDownMse so a recovery FRAME_INIT after Stage 2 enters the self-arm
//          path on a clean slate (no wedge)

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { INIT_HIGH_41, INIT_BASELINE_30 } from '../fixtures/init-segments.js';
import { makeInitFrame, makeMediaSegmentFrame } from '../fixtures/media-segments.js';

// Build a 0x02 status frame: [0x02, ...UTF-8 JSON bytes]
function makeStatusFrame(obj) {
  const json = JSON.stringify(obj);
  const encoded = new TextEncoder().encode(json);
  const frame = new Uint8Array(1 + encoded.length);
  frame[0] = 0x02;
  frame.set(encoded, 1);
  return frame;
}

describe('mse-client — init-segment recovery (SC-IR-1..8)', () => {
  let tauri;
  let ch;

  beforeEach(async () => {
    installDom();
    tauri = installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    MockMediaSourceCtor._lastInstance = null;
    MockMediaSourceCtor._deferOpenNext = false;
    vi.useFakeTimers();
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();

    ch = tauri.lastChannel();

    // Prime with an init segment so the initial MSE session is fully active.
    const initFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    MockMediaSourceCtor._deferOpenNext = false;
  });

  // ── SC-IR-1 ───────────────────────────────────────────────────────────────────
  // Init arrives DURING the setUpMse async gap (readyState still 'closed').
  // Expected: init is QUEUED (addSourceBuffer NOT called yet), then applied on
  // _fireSourceOpen() → SourceBuffer created, appendBuffer called (recovered).
  //
  // RED today: onInitFrame:572-577 checks ms.readyState !== 'open', warns, and
  // returns — the init is dropped with no queue and no drain on sourceopen.
  it('SC-IR-1: init during setUpMse gap → queued + applied on sourceopen → recovers', async () => {
    // Arm deferred-open for the NEXT MediaSource construction (happens when
    // handleStatus("streaming") calls tearDownMse() then setUpMse()).
    MockMediaSourceCtor._deferOpenNext = true;

    // Dispatch reconnecting then streaming → tearDownMse + setUpMse.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();

    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // Grab the new (deferred-open) MediaSource that setUpMse just created.
    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2).not.toBeNull();
    // Confirm it is deferred: readyState should still be 'closed'
    // (sourceopen has NOT fired yet — no _fireSourceOpen called).
    expect(ms2.readyState).toBe('closed');
    // SourceBuffer must not exist yet.
    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    // Dispatch a fresh FRAME_INIT while the MediaSource is still 'closed'.
    const initFrame2 = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame2.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // addSourceBuffer must NOT have been called yet (init is queued, not applied).
    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    // Now fire sourceopen manually — drain should apply the queued init.
    ms2._fireSourceOpen();
    await Promise.resolve();
    await Promise.resolve();

    // After sourceopen + drain: SourceBuffer must have been created.
    expect(ms2.addSourceBuffer).toHaveBeenCalledTimes(1);
    // appendBuffer must have been called with the init bytes.
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(1);
    // The appended bytes must be the init payload (no discriminant byte).
    const appended = ms2._sb._lastAppend;
    expect(appended).not.toBeNull();
    expect(appended.length).toBe(INIT_HIGH_41.length);
  });

  // ── SC-IR-2 ───────────────────────────────────────────────────────────────────
  // Streaming arrives (setUpMse runs) but the fresh FRAME_INIT is delivered only
  // AFTER setUpMse starts — still pre-open. The receiver must not permanently
  // starve: queued init must be applied on sourceopen.
  //
  // RED today: same drop as SC-IR-1.
  it('SC-IR-2: streaming before any media → init queued pre-open → applied on sourceopen (no starvation)', async () => {
    MockMediaSourceCtor._deferOpenNext = true;

    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2).not.toBeNull();
    expect(ms2.readyState).toBe('closed');

    // Advance several PLI cadences (4s) — init has not arrived yet.
    await vi.advanceTimersByTimeAsync(4_000);
    await Promise.resolve();

    // Now the fresh FRAME_INIT arrives (still pre-open).
    const initFrame2 = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame2.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Still queued — addSourceBuffer not called yet.
    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    // Fire sourceopen → drain.
    ms2._fireSourceOpen();
    await Promise.resolve();
    await Promise.resolve();

    // Recovered: SourceBuffer created and init appended.
    expect(ms2.addSourceBuffer).toHaveBeenCalledTimes(1);
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(1);
  });

  // ── SC-IR-3 ───────────────────────────────────────────────────────────────────
  // FRAME_INIT arrives while mseState.ms === null (teardown happened, no streaming
  // yet). D-IR-4+D-IR-5: onInitFrame must self-arm setUpMse() ONCE (guarded by
  // setUpInFlight) AND window.__sm_streamActive must be true (stream is still live).
  //
  // Setup: prime session, then simulate teardown-only (Stage 2 reveal fires
  // tearDownMse) WITHOUT a subsequent streaming event.
  //
  // RED today: onInitFrame:573-576 logs "no active MediaSource" and returns — no
  // self-arm, no queue.
  it('SC-IR-3: FRAME_INIT with ms===null and stream active → self-arms setUpMse exactly once', async () => {
    // Confirm stream is active after initial prime.
    // window.__sm_streamActive is set true inside main() after start_stream resolves.
    expect(window.__sm_streamActive).toBe(true);

    // Remember first MS instance count.
    const msCountBefore = MockMediaSourceCtor._lastInstance;

    // Force teardown without streaming: advance past threshold (Stage 2 reveal
    // calls tearDownMse). After this, mseState.ms === null.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(10_000); // fire the silent-recovery timer
    await Promise.resolve();
    await Promise.resolve();

    // At this point Stage 2 teardown has fired → ms is null.
    // window.__sm_streamActive is still true (not dead yet).
    expect(window.__sm_streamActive).toBe(true);

    // Record how many times MockMediaSourceCtor has been called so far
    // by counting construction indirectly through _lastInstance changes.
    const msAfterTeardown = MockMediaSourceCtor._lastInstance;
    // After Stage 2 teardown, _lastInstance is the old MS (tearDownMse does not
    // clear _lastInstance — it only nulls mseState.ms).

    // Now dispatch FRAME_INIT with ms===null.
    // The SUT must call setUpMse() → construct a new MS → eventually drain.
    const initFrame2 = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame2.buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // A NEW MediaSource must have been created (self-arm).
    const msAfterSelfArm = MockMediaSourceCtor._lastInstance;
    expect(msAfterSelfArm).not.toBe(msAfterTeardown);
    expect(msAfterSelfArm).not.toBeNull();

    // Dispatch a second FRAME_INIT immediately — the setUpInFlight guard must
    // prevent a second setUpMse() from constructing yet another MS.
    ch._dispatch(initFrame2.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // _lastInstance must still be the same MS (no second construction).
    expect(MockMediaSourceCtor._lastInstance).toBe(msAfterSelfArm);
  });

  // ── SC-IR-4 ───────────────────────────────────────────────────────────────────
  // Two FRAME_INITs arrive before sourceopen. Only the LATEST must be applied
  // (single-slot pendingInit: second init overwrites first).
  //
  // RED today: first init is dropped (no queue), second would also be dropped.
  it('SC-IR-4: two pre-open inits → only latest applied (single-slot queue)', async () => {
    MockMediaSourceCtor._deferOpenNext = true;

    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2.readyState).toBe('closed');

    // First FRAME_INIT (High@4.1) — should be stored in pendingInit.
    const initFrame1 = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrame1.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Second FRAME_INIT (Baseline@3.0) — should OVERWRITE the first in pendingInit.
    const initFrame2 = makeInitFrame(INIT_BASELINE_30);
    ch._dispatch(initFrame2.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Neither should have been applied yet.
    expect(ms2.addSourceBuffer).not.toHaveBeenCalled();

    // Fire sourceopen → drain.
    ms2._fireSourceOpen();
    await Promise.resolve();
    await Promise.resolve();

    // addSourceBuffer must have been called exactly ONCE (not twice).
    expect(ms2.addSourceBuffer).toHaveBeenCalledTimes(1);

    // The codec must correspond to the SECOND init segment (Baseline@3.0:
    // 'video/mp4; codecs="avc1.42E01E"'), not the first (High@4.1).
    expect(ms2._lastCodec).toBe('video/mp4; codecs="avc1.42E01E"');
  });

  // ── SC-IR-5 ───────────────────────────────────────────────────────────────────
  // Regression guard: REQ-SSR-4 frozen-frame behavior must not be broken.
  // A 'reconnecting' status alone must NOT call tearDownMse, NOT blank the video,
  // and must NOT touch pendingInit or the SourceBuffer.
  //
  // This scenario reuses the SC-SSR-8 shape but confirms the new init-recovery
  // code paths did not inadvertently add a teardown on reconnecting.
  it('SC-IR-5: reconnecting alone does NOT teardown, does NOT blank video, does NOT touch pendingInit/sb', async () => {
    const ms = MockMediaSourceCtor._lastInstance;
    expect(ms).not.toBeNull();

    const videoEl = document.getElementById('player');
    const srcBefore = videoEl.src;
    expect(srcBefore).toBeTruthy();

    // SourceBuffer was created during priming.
    expect(ms.addSourceBuffer).toHaveBeenCalledTimes(1);
    const endOfStreamCallsBefore = ms.endOfStream.mock.calls.length;

    // Dispatch reconnecting.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // endOfStream must NOT have been called (no teardown).
    expect(ms.endOfStream.mock.calls.length).toBe(endOfStreamCallsBefore);
    // VIDEO_EL.src must still be set (frozen frame preserved).
    expect(videoEl.src).toBeTruthy();
    expect(videoEl.src).not.toBe('');

    // Advance partway — no teardown must happen before threshold.
    await vi.advanceTimersByTimeAsync(5_000);
    await Promise.resolve();
    expect(ms.endOfStream.mock.calls.length).toBe(endOfStreamCallsBefore);
    expect(videoEl.src).toBeTruthy();
  });

  // ── SC-IR-6 ───────────────────────────────────────────────────────────────────
  // Stale pendingInit from session A must be discarded when tearDownMse runs.
  // Session B's sourceopen must NOT apply session A's queued init.
  //
  // D-IR-2: tearDownMse() must null pendingInit so a stale init cannot bleed.
  //
  // RED today: even if a queue existed, tearDownMse does not null it — the old
  // init would fire on the next sourceopen.
  it('SC-IR-6: stale pendingInit discarded on tearDownMse → session B sourceopen is clean', async () => {
    // ── Session A setup: create a deferred-open MS with a queued (pending) init.
    MockMediaSourceCtor._deferOpenNext = true;

    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    const msA = MockMediaSourceCtor._lastInstance;
    expect(msA.readyState).toBe('closed');

    // Queue session A's init (pre-open, not yet applied).
    const initFrameA = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(initFrameA.buffer);
    await Promise.resolve();
    await Promise.resolve();
    // Queued but not applied.
    expect(msA.addSourceBuffer).not.toHaveBeenCalled();

    // ── Teardown before session A's sourceopen fires: pendingInit must be nulled.
    // Dispatch dead (or streaming again) to force tearDownMse before _fireSourceOpen.
    MockMediaSourceCtor._deferOpenNext = false; // session B uses default (auto-open)
    ch._dispatch(makeStatusFrame({ kind: 'streaming' }).buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();

    // Session B's MS was created auto-open (deferOpenNext was false).
    const msB = MockMediaSourceCtor._lastInstance;
    expect(msB).not.toBe(msA);

    // Session B's MS should have received sourceopen automatically (default mode).
    // The pendingInit from session A must have been nulled by tearDownMse — so
    // session B's sourceopen drain must NOT apply session A's init.
    // Evidence: session B's addSourceBuffer should NOT have been called from the
    // drain path (no pending init to drain). It would only be called if a new
    // FRAME_INIT arrives and the MS is open.
    expect(msB.addSourceBuffer).not.toHaveBeenCalled();

    // Confirm session A's MS was never given a SourceBuffer (stale init discarded).
    expect(msA.addSourceBuffer).not.toHaveBeenCalled();

    // Now dispatch a fresh FRAME_INIT for session B — it must apply normally.
    const initFrameB = makeInitFrame(INIT_BASELINE_30);
    ch._dispatch(initFrameB.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Session B applies its own fresh init correctly.
    expect(msB.addSourceBuffer).toHaveBeenCalledTimes(1);
    expect(msB._lastCodec).toBe('video/mp4; codecs="avc1.42E01E"');
    // Session A still untouched.
    expect(msA.addSourceBuffer).not.toHaveBeenCalled();
  });

  // ── SC-IR-7 ───────────────────────────────────────────────────────────────────
  // Bug: recovery FRAME_INIT arrives BEFORE the "streaming" status during Stage 1
  // of the silent-reconnect window. Guard 3 in onInitFrame sees sb!==null and logs
  // "additional init segment ignored", dropping the ONLY recovery init. Every later
  // FRAME_SEGMENT is silently discarded (mseState.initReceived stays true on a dead
  // MediaSource but mseState.active flips false on sourceended/sourceclose), causing
  // permanent video freeze. No self-recovery without a full app relaunch.
  //
  // Fix: Guard 3 must distinguish a true same-session duplicate (MS open, not
  // reconnecting) from a reconnect-window recovery init (sb!==null but we are in
  // Stage 1 OR the MS is no longer "open"). When in doubt, call tearDownMse() and
  // fall through to Guard 1 self-arm so the proven recovery path handles the init.
  //
  // RED on current code: Guard 3 logs "additional init segment ignored" and returns;
  // no new MediaSource is constructed; subsequent FRAME_SEGMENT is never appended.
  it('SC-IR-7: recovery init arrives before streaming during Stage 1 → tears down stale session, self-arms, segment appended', async () => {
    // ── Step 1: verify the initial primed session is healthy (beforeEach already did this).
    const ms1 = MockMediaSourceCtor._lastInstance;
    expect(ms1).not.toBeNull();
    // sb was created during priming.
    expect(ms1.addSourceBuffer).toHaveBeenCalledTimes(1);
    expect(ms1._sb.appendBuffer).toHaveBeenCalledTimes(1); // init payload appended

    const endOfStreamBefore = ms1.endOfStream.mock.calls.length;

    // ── Step 2: dispatch reconnecting (attempt 1/3) — Stage 1 silent window opens.
    // REQ-SSR-4: tearDownMse must NOT fire; sb stays non-null; MS stays open.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Confirm Stage 1: no teardown yet.
    expect(ms1.endOfStream.mock.calls.length).toBe(endOfStreamBefore);
    // The silent-recovery timer is armed (silentRecoveryTimerId !== null) — that is
    // the reconnect signal Guard 3 must check. We do not inspect it directly here
    // (module-private), but we know it is set because no 10 s have elapsed and
    // cancelSilentRecovery has not been called.

    // ── Step 3: dispatch a recovery FRAME_INIT while sb !== null AND before streaming.
    // This is the RACE condition: Rust emits FRAME_INIT from its keyframe thread
    // before the reconnect supervisor emits the "streaming" status frame.
    const recoveryInitFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(recoveryInitFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve(); // extra tick for setUpMse promise + sourceopen microtask

    // ── Step 4: assert a NEW MediaSource was constructed (self-arm fired).
    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2).not.toBe(ms1);
    expect(ms2).not.toBeNull();

    // The recovery init must have been applied to the new MS (addSourceBuffer called).
    expect(ms2.addSourceBuffer).toHaveBeenCalledTimes(1);
    // The init payload bytes must have been appended to the new SourceBuffer.
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(1);
    const appended = ms2._sb._lastAppend;
    expect(appended).not.toBeNull();
    expect(appended.length).toBe(INIT_HIGH_41.length);

    // ── Step 5: dispatch a FRAME_SEGMENT and confirm it is appended (no discard loop).
    const segFrame = makeMediaSegmentFrame();
    ch._dispatch(segFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // appendBuffer must have been called a second time (once for init, once for segment).
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(2);
  });

  // ── SC-IR-8 ───────────────────────────────────────────────────────────────────
  // Stage-2 teardown invariant: revealReconnectingOverlay MUST call tearDownMse().
  //
  // The Guard 3 fix (SC-IR-7) is only safe because revealReconnectingOverlay sets
  // overlayRevealed=true AND immediately calls tearDownMse() (nulling mseState.sb).
  // If a future refactor separated those two operations, Guard 3's
  // (overlayRevealed && !msOpen) branch would not be reached for Stage 2 — the stale
  // sb would still be non-null and the recovery init would be silently dropped again.
  //
  // This test locks that invariant:
  //   1. Prime a healthy active session (sb !== null, MS open).
  //   2. Dispatch reconnecting (Stage 1) — no teardown must happen.
  //   3. Advance fake timers PAST SILENT_RECOVERY_THRESHOLD_MS so
  //      revealReconnectingOverlay fires (Stage 2 entered).
  //   4. Assert the old MS was torn down (endOfStream called) — Stage 2 fired teardown.
  //   5. Dispatch a recovery FRAME_INIT — self-arm must construct a NEW MediaSource,
  //      apply the init (addSourceBuffer called), and a subsequent segment must append.
  //      This confirms NO WEDGE in the Stage-2 recovery path.
  //
  // FAILURE MODE when tearDownMse is removed from revealReconnectingOverlay:
  //   Step 4 passes (endOfStream not called → expected fail on that assert).
  //   Step 5: onInitFrame sees sb!==null; Guard 3 checks msOpen (still "open" since
  //   MS was never torn down) + reconnecting (overlayRevealed=true but msOpen is still
  //   true, so the (overlayRevealed && !msOpen) branch is false; silentRecoveryTimerId
  //   is null because it already fired). Both reconnect signals are false → takes the
  //   "ignore duplicate" path → recovery init dropped → segment never appended.
  it('SC-IR-8: Stage-2 reveal MUST tear down the stale session so recovery FRAME_INIT enters self-arm path (no wedge)', async () => {
    // ── Step 1: verify the initial primed session is healthy.
    const ms1 = MockMediaSourceCtor._lastInstance;
    expect(ms1).not.toBeNull();
    expect(ms1.addSourceBuffer).toHaveBeenCalledTimes(1);

    const endOfStreamBefore = ms1.endOfStream.mock.calls.length;

    // ── Step 2: dispatch reconnecting — Stage 1 silent window opens, no teardown.
    ch._dispatch(makeStatusFrame({ kind: 'reconnecting', attempt: 1, max: 3 }).buffer);
    await Promise.resolve();
    await Promise.resolve();

    // Confirm Stage 1: no teardown yet.
    expect(ms1.endOfStream.mock.calls.length).toBe(endOfStreamBefore);

    // ── Step 3: advance timers past the threshold — revealReconnectingOverlay fires.
    await vi.advanceTimersByTimeAsync(10_000);
    await Promise.resolve();
    await Promise.resolve();

    // ── Step 4: assert Stage 2 tore down the stale session.
    // revealReconnectingOverlay must have called tearDownMse → endOfStream called on ms1.
    expect(ms1.endOfStream.mock.calls.length).toBeGreaterThan(endOfStreamBefore);

    // ── Step 5: dispatch recovery FRAME_INIT — must NOT be wedged.
    // With the teardown done, mseState.sb === null. Guard 3 does not fire.
    // Guard 1 (ms === null) self-arms setUpMse → constructs a new MS.
    const recoveryInitFrame = makeInitFrame(INIT_HIGH_41);
    ch._dispatch(recoveryInitFrame.buffer);
    await vi.advanceTimersByTimeAsync(0);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve(); // extra tick for setUpMse sourceopen microtask

    // A NEW MediaSource must have been constructed (self-arm).
    const ms2 = MockMediaSourceCtor._lastInstance;
    expect(ms2).not.toBe(ms1);
    expect(ms2).not.toBeNull();

    // The recovery init must have been applied to the new MS.
    expect(ms2.addSourceBuffer).toHaveBeenCalledTimes(1);
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(1);
    const appended = ms2._sb._lastAppend;
    expect(appended).not.toBeNull();
    expect(appended.length).toBe(INIT_HIGH_41.length);

    // ── Step 6: dispatch a FRAME_SEGMENT — must be appended (no discard loop).
    const segFrame = makeMediaSegmentFrame();
    ch._dispatch(segFrame.buffer);
    await Promise.resolve();
    await Promise.resolve();

    // appendBuffer must have been called a second time (init + segment).
    expect(ms2._sb.appendBuffer).toHaveBeenCalledTimes(2);
  });
});
