// Screen Mirror — MSE client (Capability G, B7 / F-fix-3).
//
// Wires <video> + MediaSource + SourceBuffer to a Tauri Channel<Bytes>.
//
// OQ-tauri-emit-1 pivot (F-fix-3):
//   Previously used window.__TAURI__.event.listen("stream/init", ...) which
//   received a JSON Array<number> payload (serde_json encoding of Vec<u8>).
//   At 1080p30 H.264 (~500 KB/segment) this produces 1.5–2 MB of JSON per
//   segment plus a synchronous JSON.parse on the WebView main thread — jank.
//
//   Resolution: frontend creates a Channel via window.__TAURI__.core.Channel,
//   passes it to Rust's start_stream as a command argument. Rust sends
//   InvokeResponseBody::Raw(bytes) — no JSON encoding. The Channel delivers an
//   ArrayBuffer directly to onmessage. The toUint8Array() helper is no longer needed.
//
//   Frame layout (byte 0 = discriminant):
//     0x00 (FRAME_INIT)    = fMP4 init segment (moov box, one per session)
//     0x01 (FRAME_SEGMENT) = fMP4 media segment (moof+mdat, one per GOP)
//
// JS binding (Tauri 2.x with withGlobalTauri: true):
//   window.__TAURI__.core.Channel — constructor for Channel<T>.
//   window.__TAURI__.core.invoke — invoke() function.
//   Anchor on the documented __TAURI__.core.* surface, NOT __TAURI_INTERNALS__.*.
//   Earlier Tauri 2 versions exposed Channel under __TAURI_INTERNALS__; that
//   binding moved to __TAURI__.core in newer 2.x releases (verified 2026-04-28
//   via DevTools probe — see #341 spec-amendments).
//
// No import / require. Plain JS module. R11.7.

// Codec compatibility check — generic Baseline 3.0 string is enough to test
// MSE+H.264 support; the per-stream codec string is derived from the avcC box
// in the actual init segment (see deriveCodecFromInitSegment below). This
// avoids the B11-S4 bug where a hardcoded `avc1.42E01E` codec string was
// rejected by Chromium's MSE parser when the encoder produced a stream at a
// higher level (e.g. 4.0 for 1080p), closing the MediaSource and removing
// the SourceBuffer mid-stream.
const PROBE_CODEC = 'video/mp4; codecs="avc1.42E01E"';

// Live-edge playback snap constants.
// Snap once currentTime falls more than this many seconds behind the live edge.
const LIVE_EDGE_MAX_DRIFT_SEC = 0.5;
// After a snap, sit this far behind the edge (small playable cushion).
const LIVE_EDGE_TARGET_LEAD_SEC = 0.2;
// Auto-retry delay after Dead-state entry (PQ-1). D-RRE-1.
const AUTO_RETRY_DELAY_MS = 30_000;
// Silent recovery threshold: duration of Stage 1 before the reconnecting overlay
// is revealed (Stage 2). Tunable single constant (D-SSR-1). Default 10s gives
// comfortable margin over a healthy ~5s ICE rebuild without immediately alarming
// the user on brief disruptions.
const SILENT_RECOVERY_THRESHOLD_MS = 10_000;
const VIDEO_EL = document.getElementById("player");
const STATUS_EL = document.getElementById("status");

// Module-level auto-retry timer handle. NOT on window, NOT in mseState (D-RRE-1).
// Null when no timer is armed; non-null between Dead-state entry and timer fire/cancel.
let autoRetryTimerId = null;

// Module-level silent-recovery timer handle (D-SSR-2). Null = no Stage-1 window active.
// Non-null = Stage 1 silent window armed, overlay NOT yet shown. Armed ONCE per loss
// episode (null-guard in case "reconnecting" prevents re-arm on attempt 2/3).
let silentRecoveryTimerId = null;

// Sentinel: true once the overlay has been revealed (Stage 2 entered) for this loss
// episode (D-SSR-6). Prevents a post-reveal reconnecting{n} frame from re-arming a
// second silent-recovery timer after the first one fired and nulled silentRecoveryTimerId.
// Reset to false by cancelSilentRecovery() so a new loss episode can arm the timer again.
let overlayRevealed = false;

// Most-recent {attempt, max} from reconnecting frames during Stage 1 (D-SSR-3).
// Updated on every reconnecting frame so the deferred reveal shows the current counter.
// Reset to null by cancelSilentRecovery() (on streaming/dead/retry/cancel).
let pendingReconnectAttempt = null;

// ── Module-level MSE state ───────────────────────────────────────────────────
// Lifted from main() so tearDownMse / setUpMse (called by handleStatus) can
// mutate it without threading closure references through every caller.
// Phase 10 — T10.1: SWAP-MEDIASOURCE pattern (spec §5.2, design §5).
const mseState = {
  /** Active MediaSource instance, or null when torn down. */
  ms: null,
  /** Active SourceBuffer, or null when torn down. */
  sb: null,
  /** The ObjectURL currently assigned to VIDEO_EL.src. */
  objectUrl: null,
  /** Sequential byte append queue (flushed on updateend). */
  pending: [],
  /** True after the first FRAME_INIT has been processed this session. */
  initReceived: false,
  /** True while a live MSE session is active (ms != null && sb ready). */
  active: false,
};

// ── Init-segment recovery state (D-IR-1, D-IR-5) ────────────────────────────
// Single-slot queue for a FRAME_INIT that arrived before the MediaSource
// reached readyState "open". The latest wins — a second pre-open init
// overwrites the first (D-IR-1). Drained on sourceopen (D-IR-2).
// Nulled by tearDownMse so a stale init from session N cannot bleed into
// session N+1 (D-IR-2). Value: { data: Uint8Array, frameBytes: Uint8Array }.
let pendingInit = null;

// Guard: true while a setUpMse() call is in flight (MediaSource constructed
// but sourceopen not yet fired). Prevents the onInitFrame self-arm (D-IR-4)
// from spawning a second concurrent MediaSource when one is already in
// progress (D-IR-5). Reset to false by both the sourceopen and error handlers.
let setUpInFlight = false;

// Generation counter for MSE lifecycle sessions (SC-IR-9: stale-sourceopen guard).
// Incremented each time setUpMse() creates a new MediaSource. The sourceopen and
// error handlers capture their generation at construction time and exit early if
// the current counter no longer matches — a superseded MS's late sourceopen must
// not corrupt the live session's state (the stale-callback wedge).
let mseGeneration = 0;

// ── cancelAutoRetry ──────────────────────────────────────────────────────────
// Cancels any pending auto-retry timer. Idempotent — safe to call when no
// timer is armed. MUST be called on every Dead-state exit (PQ-3 invariant,
// D-RRE-3). Six call sites: case "dead" re-entry, case "streaming",
// case "reconnecting", Retry click, Cancel click, role-change click.
function cancelAutoRetry() {
  if (autoRetryTimerId !== null) {
    clearTimeout(autoRetryTimerId);
    autoRetryTimerId = null;
  }
}

// ── cancelSilentRecovery ─────────────────────────────────────────────────────
// Cancels any pending Stage-1 silent-recovery timer. Idempotent (D-SSR-4).
// Mirrors cancelAutoRetry discipline exactly. Four mandatory call sites:
//   case "streaming" top, case "dead" top, Retry click, Cancel click.
// Also resets pendingReconnectAttempt so stale attempt counters are not leaked
// into a subsequent loss episode.
function cancelSilentRecovery() {
  if (silentRecoveryTimerId !== null) {
    clearTimeout(silentRecoveryTimerId);
    silentRecoveryTimerId = null;
  }
  pendingReconnectAttempt = null;
  overlayRevealed = false; // reset sentinel so a new loss episode can arm the timer (D-SSR-6)
}

// ── dismissReconnectOverlayOnRecovery ────────────────────────────────────────
// Called from the <video> "playing" event to dismiss the Stage-2 reconnecting
// overlay when the video resumes playback via the FRAME_INIT self-arm path
// (D-IR-4, onInitFrame Guard 1). That path calls setUpMse() directly without
// sending a handleStatus("streaming") frame, so the overlay-hide in case
// "streaming" is never triggered — the overlay stays visible over the
// now-playing video. This is the additive dismissal seam (SC-SSR-OVL-RECOVERY).
//
// Gate: if (!overlayRevealed) return — no-op during normal playback or Stage 1
// so spurious "playing" events (startup, seek) do not touch overlay state.
// Idempotent: repeated calls are safe (cancelSilentRecovery is idempotent).
function dismissReconnectOverlayOnRecovery() {
  if (!overlayRevealed) return;
  if (reconnectingOverlay) reconnectingOverlay.hidden = true;
  cancelSilentRecovery(); // resets overlayRevealed = false + clears any pending timer
}

// ── revealReconnectingOverlay ────────────────────────────────────────────────
// Timer callback: fires SILENT_RECOVERY_THRESHOLD_MS after the first
// reconnecting frame. Transitions from Stage 1 (silent) to Stage 2 (visible).
// Deferred teardown fires HERE — the last frozen frame was visible throughout
// Stage 1; we teardown now because the overlay will cover the blanked video.
// Uses pendingReconnectAttempt (the LATEST counter, not attempt 1) for text.
// Uses module-scoped reconnectingOverlay (assigned at parse time, always
// available when the timer fires). D-SSR-5.
function revealReconnectingOverlay() {
  silentRecoveryTimerId = null; // null FIRST (mirrors triggerAutoRetry pattern)
  overlayRevealed = true;       // set sentinel: Stage 2 entered (D-SSR-6)
  tearDownMse();                // deferred teardown fires at Stage 2 reveal
  // reconnectingOverlay is the module-scoped variable assigned below at parse
  // time; by the time this timer callback fires it is fully initialized.
  if (reconnectingOverlay && pendingReconnectAttempt) {
    // CAP-2-v3 FIX-1 (R-F extension to the overlay): the Stage-2 overlay is the
    // surface the user actually stares at during the absent-peer wait, so it must
    // NOT render the misleading "/N" denominator. The transport can keep retrying
    // for ~60s (issue #62) and the frontend cannot distinguish the supervisor's
    // real retry from the post-watchdog wait — the same honest, count-free copy as
    // the reconnecting status line is used here. The presence of pendingReconnectAttempt
    // is still the gate (deferred teardown + timer logic unchanged); only the text
    // is count-free.
    reconnectingOverlay.textContent =
      "Reconnecting… waiting for the other device";
    reconnectingOverlay.hidden = false;
  }
}

// Frame discriminant constants (must match FRAME_INIT / FRAME_SEGMENT / FRAME_STATUS
// in stream.rs).
const FRAME_INIT = 0x00;
const FRAME_SEGMENT = 0x01;
const FRAME_STATUS = 0x02;

// ── dispatchChannelMessage ───────────────────────────────────────────────────
// Module-scoped frame dispatcher, extracted from the inline onmessage closure
// that was previously defined only inside main(). Extraction enables the R-6
// fix: triggerRetry() can now bind this same function on the fresh Channel it
// creates, so post-retry frames (FRAME_INIT, FRAME_SEGMENT, FRAME_STATUS) reach
// JS exactly as they did on the initial channel.
//
// R-6 FIX (REQ-SSR-9): the old code bound onmessage once in main() on the
// initial streamChannel; after triggerRetry() constructed a new Channel and
// passed it to retry_session_stream, Rust correctly delivered frames into that
// new Channel, but JS had no onmessage on it — frames were silently dropped.
// Binding dispatchChannelMessage on every Channel instance (initial + retry)
// closes the gap.
//
// The body only references module-scoped identifiers (FRAME_INIT, FRAME_SEGMENT,
// FRAME_STATUS, mseState, onInitFrame, enqueue, handleStatus) — no main()-local
// closure captures, so extraction is behavior-preserving.
function dispatchChannelMessage(payload) {
  // payload is ArrayBuffer (InvokeResponseBody::Raw path).
  const data = new Uint8Array(payload);
  if (data.length === 0) {
    console.warn("[mse] empty frame received — ignoring");
    return;
  }

  const discriminant = data[0];
  // B11-S7: pass the Uint8Array view directly (NOT `.buffer`). `subarray(1)`
  // yields a typed-array view starting at byte 1, but `.buffer` returns the
  // FULL underlying ArrayBuffer ignoring the byteOffset — so appendBuffer
  // would receive the entire payload including byte 0 (the discriminant)
  // and the mp4 box parser at offset 0 would read size=0x00000000 instead
  // of size=0x00000020, closing the MediaSource on init parse failure.
  const frameBytes = data.subarray(1);

  if (discriminant === FRAME_INIT) {
    onInitFrame(data, frameBytes);
  } else if (discriminant === FRAME_SEGMENT) {
    if (!mseState.initReceived) {
      console.warn("[mse] segment arrived before init — discarding");
      return;
    }
    enqueue(frameBytes);
  } else if (discriminant === FRAME_STATUS) {
    // 0x02 — JSON status event from the reconnect supervisor (Phase 8, T8.2).
    // Decode the payload bytes as UTF-8 JSON and forward to handleStatus.
    // Must NOT feed bytes to the SourceBuffer.
    try {
      const json = new TextDecoder().decode(frameBytes);
      const statusPayload = JSON.parse(json);
      handleStatus(statusPayload);
    } catch (e) {
      console.warn("[mse-client] 0x02 frame JSON parse error:", e);
    }
  } else {
    console.warn("[mse] unknown frame discriminant: 0x" + discriminant.toString(16));
  }
}

function setStatus(msg) {
  if (STATUS_EL) STATUS_EL.textContent = msg;
  console.log("[mse]", msg);
}

// ── tearDownMse ──────────────────────────────────────────────────────────────
// Spec §5.2 (receiver MSE teardown — T10.1):
//   1. endOfStream("decode") signals a decode error to the browser so it does
//      not attempt to play remaining buffered frames from the stale segment.
//   2. Revoke the ObjectURL and clear VIDEO_EL.src so the element is detached.
//   3. Call VIDEO_EL.load() to reset the element state machine (required by MSE
//      spec before attaching a new MediaSource — Chromium WebView2 tested).
//   4. Reset mseState so the next FRAME_INIT triggers a fresh setUpMse path.
//
// Idempotent: safe to call when no MSE session is active (mseState.ms == null).
function tearDownMse() {
  try {
    if (mseState.ms && mseState.ms.readyState === "open") {
      mseState.ms.endOfStream("decode");
    }
  } catch (_) {
    // endOfStream can throw if readyState transitions concurrently — ignore.
  }
  if (mseState.objectUrl) {
    try { URL.revokeObjectURL(mseState.objectUrl); } catch (_) {}
    mseState.objectUrl = null;
  }
  VIDEO_EL.removeAttribute("src");
  VIDEO_EL.load();
  // Reset all MSE state so the onmessage handler starts fresh.
  mseState.ms = null;
  mseState.sb = null;
  mseState.pending = [];
  mseState.initReceived = false;
  mseState.active = false;
  // D-IR-2: discard any queued init from the torn-down session so it cannot
  // bleed into the next session's sourceopen drain. Also reset setUpInFlight
  // since this teardown ends any in-progress setUpMse (the MediaSource we just
  // nulled will never fire sourceopen again).
  pendingInit = null;
  setUpInFlight = false;
}

// ── setUpMse ─────────────────────────────────────────────────────────────────
// Spec §5.2: re-attach a fresh MediaSource and await sourceopen.
// setUpMse prepares VIDEO_EL for the next init segment; the first FRAME_INIT
// received after setUpMse creates the SourceBuffer (existing lazy-init path).
// If a FRAME_INIT arrived during the async sourceopen gap (before readyState
// became "open"), it was stored in pendingInit (D-IR-1) and is drained here
// on sourceopen (D-IR-2) instead of waiting for a new init that may never come.
//
// Returns a Promise that resolves when sourceopen fires (or rejects on error).
// Called by handleStatus on "streaming" event following a reconnect, and by
// onInitFrame's self-arm fallback when ms===null (D-IR-4).
function setUpMse() {
  setUpInFlight = true; // D-IR-5: mark in-flight before construction
  const ms = new MediaSource();
  // SC-IR-9: capture this session's generation tag immediately after construction.
  // If tearDownMse supersedes this MS before its sourceopen fires, the orphaned
  // callback will see a stale myGen and exit without touching global state.
  const myGen = ++mseGeneration;
  mseState.ms = ms;
  const url = URL.createObjectURL(ms);
  mseState.objectUrl = url;
  VIDEO_EL.src = url;
  mseState.pending = [];
  mseState.sb = null;
  mseState.initReceived = false;
  mseState.active = false;

  return new Promise((resolve, reject) => {
    ms.addEventListener("sourceopen", () => {
      // SC-IR-9: stale-sourceopen guard. A superseded MS's late sourceopen must
      // not corrupt the live session's state. If this generation tag no longer
      // matches the current counter, or mseState.ms has moved on to a different
      // instance, this callback belongs to an orphaned MS — resolve and return
      // without mutating any global state.
      if (mseGeneration !== myGen || mseState.ms !== ms) {
        resolve();
        return;
      }
      setUpInFlight = false; // D-IR-5: clear flag — MS is now open
      mseState.active = true;
      setStatus("MSE ready (reconnect) — awaiting fresh init segment…");
      // D-IR-2: drain a FRAME_INIT that arrived during the async gap. At this
      // point ms.readyState === "open" so addSourceBuffer is safe.
      if (pendingInit !== null && mseState.sb === null) {
        const { data, frameBytes } = pendingInit;
        pendingInit = null;
        applyInit(ms, data, frameBytes);
      }
      resolve();
    }, { once: true });
    ms.addEventListener("error", (e) => {
      // SC-IR-9: same stale-generation guard for the error path.
      if (mseGeneration !== myGen || mseState.ms !== ms) {
        reject(e);
        return;
      }
      setUpInFlight = false; // D-IR-5: clear flag on error too
      reject(e);
    }, { once: true });
  });
}

// ── triggerRetry / triggerAutoRetry ─────────────────────────────────────────
// Shared retry implementation (D-RRE-4). Called by both the manual Retry
// button and the auto-retry timer callback. Replicates the original click
// handler semantics: hide dead-modal, create a new Channel, invoke
// retry_session_stream. Does NOT call stop_stream (that would clear
// restart_cache — I-NR-2 invariant). Does NOT call location.reload()
// (REQ-NO-RELOAD).
// R-6 FIX (REQ-SSR-9): binds dispatchChannelMessage on the new Channel BEFORE
// invoke() so Rust-delivered frames (FRAME_INIT, FRAME_SEGMENT, FRAME_STATUS)
// reach JS after retry. Previously onmessage was never set here — frames were
// silently dropped on the manual-Retry and 30s-auto-retry paths.
async function triggerRetry() {
  if (deadModal) deadModal.hidden = true;
  const invoke = window.__TAURI__?.core?.invoke;
  const Channel = window.__TAURI__?.core?.Channel;
  if (invoke && Channel) {
    try {
      const channel = new Channel();
      channel.onmessage = dispatchChannelMessage; // R-6: bind before invoke
      await invoke("retry_session_stream", { channel });
    } catch (e) {
      console.warn("[mse-client] retry_session_stream failed:", e);
    }
  }
}

// Auto-retry timer callback. Nulls the timer ID before calling triggerRetry()
// so that any cancelAutoRetry() call racing with triggerRetry() is a no-op
// (I-12 invariant: idempotent cancel after fire). D-RRE-4.
function triggerAutoRetry() {
  autoRetryTimerId = null;
  triggerRetry();
}

// ── handleStatus ─────────────────────────────────────────────────────────────
// Handle a decoded 0x02 JSON status payload from the Rust reconnect supervisor.
// Spec §5.2, T10.1: routes reconnecting/dead/streaming lifecycle events to
// tearDownMse / setUpMse and shows/hides the viewer overlay/modal.
//
// Phase 9 (Batch 6 CRITICAL-1): also shows/hides the reconnecting-overlay and
// dead-modal elements added to viewer.html (spec §5.4).
//
// receiver-retry-on-exhaustion (D-RRE-2, D-RRE-3):
//   • case "dead" arms AUTO_RETRY_DELAY_MS setTimeout after showing the modal.
//   • All three branches call cancelAutoRetry() at their top (PQ-3 invariant).
//   • The auto-retry fires triggerAutoRetry() → triggerRetry() (D-RRE-4).

const reconnectingOverlay = document.getElementById("reconnecting-overlay");
const deadModal = document.getElementById("dead-modal");
const deadReasonEl = document.getElementById("dead-reason");
const receiverRetryBtn = document.getElementById("receiver-retry");
const receiverCancelBtn = document.getElementById("receiver-cancel");

// Retry: call retry_session_stream IPC which handles stop+restart internally,
// reusing the active Tauri channel. This avoids window.location.reload() which
// would reset all JS state including the IPC channel reference (REQ-B2,
// REQ-NO-RELOAD). The backend command mirrors retry_session on the sender side.
// cancelAutoRetry() is prepended (PQ-3 invariant, D-RRE-3 call site 4).
// cancelSilentRecovery() clears any Stage-1 timer still pending (D-SSR-4).
if (receiverRetryBtn) {
  receiverRetryBtn.addEventListener("click", async function () {
    cancelAutoRetry();
    cancelSilentRecovery();
    await triggerRetry();
  });
}

// Cancel: stop the stream and return to idle (no reload).
// cancelAutoRetry() is prepended (PQ-3 invariant, D-RRE-3 call site 5).
// cancelSilentRecovery() clears any Stage-1 timer still pending (D-SSR-4).
if (receiverCancelBtn) {
  receiverCancelBtn.addEventListener("click", async function () {
    cancelAutoRetry();
    cancelSilentRecovery();
    if (deadModal) deadModal.hidden = true;
    const invoke = window.__TAURI__?.core?.invoke;
    if (invoke) {
      try { await invoke("stop_stream"); } catch (_) {}
    }
    window.__sm_streamActive = false;
    setStatus("Stopped");
  });
}

// Role-change affordance: navigate to sender.html from the dead-modal.
// cancelAutoRetry() is called first (PQ-3 invariant, D-RRE-3 call site 6).
// localStorage write sets sm.lastMode so cold-relaunch boots into sender mode.
// Wraps write in try/catch (R-8 mitigation). Does NOT preventDefault — lets
// the native <a href> navigate. __sm_streamActive is already false (set in
// case "dead"), so the beforeunload guard at viewer.html:125 is a no-op.
// D-RRE-5 (PQ-4 LOCK).
const deadRoleChangeEl = document.getElementById("dead-role-change");
if (deadRoleChangeEl) {
  deadRoleChangeEl.addEventListener("click", function () {
    cancelAutoRetry();
    try { localStorage.setItem("sm.lastMode", "sender"); } catch (_) {}
    if (deadModal) deadModal.hidden = true;
  });
}

// S-conf1 (CAP-2-v3): map terminal dead `reason` tokens to human-readable copy.
// CAP-2-v3 introduced new machine tokens (peer_unreachable, ice_failed_repeatedly)
// that would otherwise leak raw into the dead-modal as "Connection lost: peer_unreachable".
// Only mapped tokens get bespoke copy; any other/absent reason keeps the existing
// "Connection lost: <reason|unknown>" fallback (behavior unchanged). Kept symmetric
// with dist/sender.js.
const DEAD_REASON_COPY = {
  peer_unreachable: "The other device is unreachable",
  ice_failed_repeatedly: "The connection failed repeatedly",
};

function humanDeadReason(reason) {
  if (reason && Object.prototype.hasOwnProperty.call(DEAD_REASON_COPY, reason)) {
    return DEAD_REASON_COPY[reason];
  }
  return "Connection lost: " + (reason || "unknown");
}

function handleStatus(payload) {
  console.log("[mse-client] status:", payload.kind, payload);
  switch (payload.kind) {
    case "reconnecting":
      // Cancel any pending auto-retry (PQ-3 invariant, D-RRE-3 call site 1).
      cancelAutoRetry();
      // Capture most-recent attempt/max for the deferred overlay reveal (D-SSR-3).
      pendingReconnectAttempt = { attempt: payload.attempt, max: payload.max };
      // CAP-2-v3 (REQ-WD-10): honest count-free copy. The bounded retry window can last
      // up to ~60s (issue #62) and the frontend cannot distinguish the supervisor's real
      // retry from the post-watchdog wait, so the misleading "attempt X/max" denominator
      // is removed. The deferred Stage-2 silent-recovery overlay keeps its own counter.
      setStatus("Reconnecting… waiting for the other device");
      // DO NOT call tearDownMse() here — deferred to Stage 2 reveal or streaming/dead
      // so the last frozen video frame stays visible during the silent window (REQ-SSR-4).
      // DO NOT show the overlay yet — Stage 1 is silent (REQ-SSR-3).
      // Arm one-shot total-elapsed timer ONLY on the FIRST reconnecting frame of this
      // episode. Two-part guard (D-SSR-6):
      //   1. silentRecoveryTimerId === null: prevents re-arm while Stage 1 is still active
      //      (subsequent reconnecting{2,3} frames cannot reset the 10s window).
      //   2. !overlayRevealed: prevents re-arm AFTER Stage 2 entry — once the overlay is
      //      revealed (timer fired, silentRecoveryTimerId nulled itself), a later
      //      reconnecting{n} must NOT start a new 10s window (REQ-SSR-2, D-SSR-6).
      //      overlayRevealed is reset by cancelSilentRecovery() (streaming/dead/retry) so
      //      a genuinely new loss episode can arm the timer again.
      if (silentRecoveryTimerId === null && !overlayRevealed) {
        silentRecoveryTimerId = setTimeout(
          revealReconnectingOverlay,
          SILENT_RECOVERY_THRESHOLD_MS
        );
      }
      if (deadModal) deadModal.hidden = true;
      break;
    case "dead":
      // Cancel any prior auto-retry before re-arming (PQ-3 invariant, D-RRE-3 call site 3).
      // Handles both second-Dead re-entry and the normal first-entry (idempotent).
      cancelAutoRetry();
      // Cancel the Stage-1 silent-recovery timer if still active (D-SSR-9, REQ-SSR-7).
      // Fast-exhaustion edge: dead arrives before 10s → silent overlay never shown.
      // Stage-2 edge: dead arrives after reveal → cancel is a no-op (timer already null).
      cancelSilentRecovery();
      // All reconnect attempts exhausted — show dead-session modal with Retry/Cancel.
      setStatus("Disconnected — session lost");
      tearDownMse();
      window.__sm_streamActive = false;
      if (reconnectingOverlay) reconnectingOverlay.hidden = true;
      if (deadModal) {
        if (deadReasonEl) {
          deadReasonEl.textContent = humanDeadReason(payload.reason);
        }
        deadModal.hidden = false;
      }
      // Arm the single bounded auto-retry timer (PQ-1, PQ-2, D-RRE-2).
      autoRetryTimerId = setTimeout(triggerAutoRetry, AUTO_RETRY_DELAY_MS);
      break;
    case "streaming":
      // Cancel any pending auto-retry (PQ-3 invariant, D-RRE-3 call site 2).
      cancelAutoRetry();
      // Cancel Stage-1 silent-recovery timer (D-SSR-8, REQ-SSR-5, REQ-SSR-8).
      // Silent success path: streaming arrived before 10s → overlay was never shown.
      // Post-Stage-2 path: streaming arrived after reveal → cancel is a no-op (timer
      // already null) but we still hide the overlay below.
      cancelSilentRecovery();
      // Reconnect supervisor reports the rebuild succeeded. Prepare a fresh MSE
      // session so the next FRAME_INIT can re-initialize the SourceBuffer.
      if (reconnectingOverlay) reconnectingOverlay.hidden = true;
      if (deadModal) deadModal.hidden = true;
      // Deferred teardown: MSE was kept alive during Stage 1 to hold the frozen frame.
      // tearDownMse() MUST be called here BEFORE setUpMse() to end the stale MediaSource
      // and give setUpMse a clean slate (REQ-SSR-4, REQ-SSR-5, D-SSR-8, R-SSR-3).
      tearDownMse();
      setUpMse().catch((e) => {
        console.error("[mse-client] setUpMse failed after reconnect:", e);
      });
      break;
    default:
      // Other status events (connected, stopped) — log only.
      break;
  }
}

// Scan an fMP4 init segment for the `avcC` box and synthesize the matching
// codec string. The avcC payload (after the 4-byte "avcC" tag) is:
//   [0] configurationVersion (1)
//   [1] AVCProfileIndication
//   [2] profile_compatibility
//   [3] AVCLevelIndication
// These three bytes drive the codec string `avc1.PPCCLL`. Returns null if no
// avcC box is found in the buffer.
function deriveCodecFromInitSegment(buf) {
  const view = new Uint8Array(buf);
  for (let i = 0; i + 8 < view.length; i++) {
    if (
      view[i] === 0x61 && // 'a'
      view[i + 1] === 0x76 && // 'v'
      view[i + 2] === 0x63 && // 'c'
      view[i + 3] === 0x43 // 'C'
    ) {
      const profile = view[i + 5];
      const compat = view[i + 6];
      const level = view[i + 7];
      const hex = (b) => b.toString(16).padStart(2, "0").toUpperCase();
      return 'video/mp4; codecs="avc1.' + hex(profile) + hex(compat) + hex(level) + '"';
    }
  }
  return null;
}

// ── Append queue helpers ─────────────────────────────────────────────────────
// These operate on mseState (module-level) so tearDownMse / setUpMse can reset
// the queue without needing closure references. Phase 10 — T10.1.

function enqueue(bytes) {
  mseState.pending.push(bytes);
  flushQueue();
}

function flushQueue() {
  const sb = mseState.sb;
  const pending = mseState.pending;
  if (!sb || sb.updating || pending.length === 0) return;
  const next = pending.shift();
  try {
    sb.appendBuffer(next);
  } catch (e) {
    if (e.name === "QuotaExceededError") {
      trimSourceBuffer();
      pending.unshift(next); // retry after trim
    } else {
      console.error("[mse] appendBuffer error", e);
    }
  }
}

function trimSourceBuffer() {
  const sb = mseState.sb;
  if (!sb || sb.updating || !VIDEO_EL) return;
  try {
    const cur = VIDEO_EL.currentTime;
    const cutoff = Math.max(0, cur - 30);
    if (sb.buffered.length > 0 && sb.buffered.start(0) < cutoff) {
      sb.remove(sb.buffered.start(0), cutoff);
    }
  } catch (e) {
    // Buffered/remove can throw if SourceBuffer detached — log once, don't spam.
    console.warn("[mse] trim skipped", e.name);
  }
}

// ── seekToLiveEdge ───────────────────────────────────────────────────────────
// Snaps playback to the live edge when currentTime falls too far behind the
// newest buffered content. Called on every updateend (once the burst is fully
// appended) and on the 2 s heartbeat as a safety net.
//
// Guard order:
//   1. No SourceBuffer, or it is still updating (burst in progress) → wait.
//   2. Pending queue not empty (more chunks about to be appended) → wait.
//      This ensures ONE seek per burst, not one per chunk.
//   3. A seek is already in flight → skip to avoid re-entrant seeks.
//   4. buffered is empty → nothing to snap to.
//   5. Drift ≤ LIVE_EDGE_MAX_DRIFT_SEC → already close enough, do nothing.
//   6. target ≤ currentTime → never seek backward.
function seekToLiveEdge() {
  const sb = mseState.sb;
  if (!sb || sb.updating || mseState.pending.length > 0) return;
  if (VIDEO_EL.seeking) return;
  let bufEnd;
  try {
    const buf = sb.buffered;
    if (buf.length === 0) return;
    bufEnd = buf.end(buf.length - 1);
  } catch (e) {
    return;
  }
  const drift = bufEnd - VIDEO_EL.currentTime;
  if (drift <= LIVE_EDGE_MAX_DRIFT_SEC) return;
  const target = bufEnd - LIVE_EDGE_TARGET_LEAD_SEC;
  if (target <= VIDEO_EL.currentTime) return;
  console.log(
    "[mse] live-edge seek: " + VIDEO_EL.currentTime.toFixed(3) +
    " → " + target.toFixed(3) +
    " (drift was " + drift.toFixed(3) + "s)"
  );
  try {
    VIDEO_EL.currentTime = target;
  } catch (e) {
    console.warn("[mse] live-edge seek failed", e);
  }
}

// ── applyInit — commit an init segment to an open MediaSource (D-IR-3) ───────
// Extracted verbatim from the original onInitFrame body. Requires:
//   ms.readyState === "open" (addSourceBuffer throws otherwise — R-IR-7)
//   mseState.sb === null (called only when creating a fresh SourceBuffer)
// Called by onInitFrame (normal path) and by the setUpMse sourceopen drain
// (D-IR-2 — when a FRAME_INIT arrived before sourceopen and was queued).
function applyInit(ms, data, frameBytes) {
  // B11 diagnostic: dump the first 128 bytes of the init segment in hex.
  const initBytes = new Uint8Array(frameBytes);
  const previewLen = Math.min(128, initBytes.length);
  const hex = Array.from(initBytes.subarray(0, previewLen))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join(" ");
  console.log(
    "[mse] init segment hex (first " + previewLen + "/" + initBytes.length + " bytes):\n" + hex
  );

  const derived = deriveCodecFromInitSegment(frameBytes);
  if (!derived) {
    // W-2: permanent failure — loud error so operator sees an unrecoverable session
    // in the console, not just the status bar. Re-queueing would not help: the same
    // init would fail identically (no avcC box cannot be fixed by retrying).
    const msg = "init segment missing avcC — cannot derive codec";
    setStatus(msg);
    console.error("[mse] applyInit permanent failure:", msg);
    return;
  }
  if (!MediaSource.isTypeSupported(derived)) {
    // W-2: permanent failure — same rationale as above.
    const msg = "FATAL: derived codec not supported: " + derived;
    setStatus(msg);
    console.error("[mse] applyInit permanent failure:", msg);
    return;
  }
  let sb;
  try {
    sb = ms.addSourceBuffer(derived);
    // B11-S12: 'sequence' instead of 'segments'.
    //
    // 'segments' honours each segment's tfdt absolute timestamp on the
    // MSE timeline. With a screen-capture pipeline whose effective frame
    // rate fluctuates (Windows Graphics Capture only emits a frame on
    // visible-content change → 1-5 fps on a static desktop, 30 fps on a
    // moving one) the muxer produces non-contiguous buffered ranges
    // ([0→0.1], gap, [2.8→2.95], gap, …) and the <video> element stalls
    // at the first gap with readyState=HAVE_CURRENT_DATA waiting for
    // contiguous data that never arrives.
    //
    // 'sequence' tells MSE to append each segment immediately after the
    // last in append order, ignoring per-segment tfdt. Playback stays
    // continuous regardless of capture-rate variability — exactly what
    // a live screen mirror wants. Latency is unchanged (~2 s, driven by
    // the IDR cadence).
    sb.mode = "sequence";
    sb.addEventListener("updateend", flushQueue);
    sb.addEventListener("updateend", seekToLiveEdge);
    mseState.sb = sb; // write into module-level state (heartbeat + tearDownMse)
  } catch (e) {
    // Primary warn (keep existing): visible in operator console.
    setStatus("addSourceBuffer failed: " + e);
    // Secondary hardening (SC-IR-9 defense-in-depth): reset broken state so the
    // pipeline can self-heal. Without this reset, mseState.sb stays null and
    // mseState.initReceived stays false after a throw (e.g. InvalidStateError on a
    // CLOSED MS), but mseState.active may already be true from a prior stale
    // sourceopen — the pipeline wedges permanently. Re-queuing into pendingInit
    // gives a later/current sourceopen drain the chance to re-apply the init.
    mseState.sb = null;
    mseState.initReceived = false;
    // Re-queue this init so the next sourceopen drain can re-apply it.
    // { data, frameBytes } are in scope as applyInit parameters.
    pendingInit = { data, frameBytes };
    return;
  }
  // Surface SourceBuffer error and updateend events so MSE failures are visible.
  sb.addEventListener("error", (e) => {
    console.error("[mse] SourceBuffer error event", e);
  });
  sb.addEventListener("abort", () => {
    console.warn("[mse] SourceBuffer abort event");
  });
  ms.addEventListener("sourceended", () => {
    // W-1: reset active so a subsequent FRAME_INIT takes Guard 2 (queued) instead
    // of the happy path → addSourceBuffer on a non-open MS → throw → drop.
    mseState.active = false;
    console.warn("[mse] MediaSource sourceended");
  });
  ms.addEventListener("sourceclose", () => {
    // W-1: same defense-in-depth reset as sourceended.
    mseState.active = false;
    console.warn("[mse] MediaSource sourceclose — readyState=" + ms.readyState);
  });
  setStatus(
    "init segment received (" + (data.length - 1) + " bytes), codec=" + derived
  );
  mseState.initReceived = true;
  enqueue(frameBytes);
}

// ── onInitFrame — resilient SourceBuffer creation on FRAME_INIT (D-IR-1..5) ──
// Extracted from main() so setUpMse() reconnects can also receive a fresh init.
// Reads mseState.ms (current MediaSource) and writes mseState.sb via applyInit.
//
// Recovery behavior (the HW Procedure B fix):
// - Guard 1 (ms === null): queue the init in pendingInit and self-arm setUpMse
//   ONCE (guarded by setUpInFlight). The setUpMse sourceopen drain (D-IR-2)
//   applies pendingInit when the MS opens. Previously: silent drop + no retry.
// - Guard 2 (ms exists but readyState !== "open"): queue in pendingInit; drain
//   on sourceopen. Real browsers throw on addSourceBuffer here (InvalidStateError);
//   queueing avoids the throw-and-drop. Previously: fell through to addSourceBuffer
//   → threw → caught → dropped; no retry.
// - Guard 3 (sb !== null, same session): same-session duplicate init; ignored as
//   before (tearDownMse always nulls sb for a new session).
function onInitFrame(data, frameBytes) {
  // Guard 3: already have a SourceBuffer — but distinguish a true same-session
  // duplicate from a recovery init that raced ahead during a reconnect.
  //
  // Original invariant: "a genuinely new session always went through tearDownMse
  // (which nulls sb), so sb!==null here means duplicate." This breaks during the
  // deferred-teardown reconnect window: handleStatus("reconnecting") deliberately
  // does NOT call tearDownMse so the last frozen frame stays visible (REQ-SSR-4).
  // The Rust mux emits FRAME_INIT from its own keyframe-driven thread, which can
  // race AHEAD of the "streaming" status frame. The recovery init then hits this
  // guard with sb!==null while we are still in Stage 1 of the silent-reconnect
  // window, causing it to be dropped and permanently wedging the pipeline.
  //
  // Fix: keep the ignore path ONLY for a genuine duplicate on a healthy session
  // (MS open AND no reconnect in progress). Otherwise call tearDownMse() and fall
  // through so Guard 1 self-arms setUpMse and queues the init via pendingInit —
  // the proven recovery path (D-IR-4).
  if (mseState.sb !== null) {
    const msOpen = !!(mseState.ms && mseState.ms.readyState === "open");
    // Detect a reconnect in progress via module-scoped signals:
    //   silentRecoveryTimerId !== null → Stage 1 silent window is active (timer armed
    //   by handleStatus("reconnecting"), not yet fired, overlay not yet shown).
    //   overlayRevealed && !msOpen → Stage 2 was entered (revealReconnectingOverlay
    //   called tearDownMse + set overlayRevealed), but self-arm has not yet re-opened
    //   the MS. Once self-arm completes and sb is set again, msOpen is true and
    //   overlayRevealed is still true until cancelSilentRecovery fires — but that
    //   re-entry case means we're in a healthy new session, not in a reconnect race.
    // Together these two conditions cover the races without false-positives on the
    // "second FRAME_INIT after self-arm recovery" case (SC-IR-3 invariant).
    const reconnecting = silentRecoveryTimerId !== null || (overlayRevealed && !msOpen);
    if (msOpen && !reconnecting) {
      // True same-session duplicate on a healthy, non-reconnecting session — ignore.
      console.warn("[mse] additional init segment ignored");
      return;
    }
    // Recovery init during a reconnect window (or stale MS): tear down the stale
    // session and fall through to Guard 1 to self-arm a fresh setUpMse.
    tearDownMse();
  }

  const ms = mseState.ms;

  // Guard 1: no active MediaSource yet. Queue the init and self-arm setUpMse
  // so the sourceopen drain can apply it. Only self-arm when the stream is still
  // live (window.__sm_streamActive) and no setUpMse is already in flight (D-IR-5).
  if (!ms) {
    pendingInit = { data, frameBytes };
    if (!setUpInFlight && window.__sm_streamActive) {
      setUpMse().catch((e) => {
        console.error("[mse-client] setUpMse (self-arm) failed:", e);
      });
    }
    return;
  }

  // Guard 2: MediaSource exists but sourceopen has not yet fired (mseState.active
  // is false while the async sourceopen gap is open). Queue the init (latest wins)
  // and wait for the setUpMse sourceopen drain (D-IR-2). Previously the code fell
  // through to addSourceBuffer which threw when readyState !== "open" (real browser
  // behaviour) and dropped the init with no retry.
  // Using mseState.active (set in BOTH the main() and setUpMse() sourceopen handlers)
  // rather than ms.readyState directly, because some test stubs only flip readyState
  // inside addSourceBuffer itself (the original mock pattern). mseState.active is the
  // canonical "sourceopen has fired and addSourceBuffer is safe" signal.
  if (!mseState.active) {
    pendingInit = { data, frameBytes };
    return;
  }

  // Happy path: MS is open and no SourceBuffer yet — apply immediately.
  applyInit(ms, data, frameBytes);
}

async function main() {
  // ── R11.2: probe MSE+H.264 support generically ───────────────────────────
  if (!("MediaSource" in window) || !MediaSource.isTypeSupported(PROBE_CODEC)) {
    const err =
      "FATAL: MSE / H.264 not supported. " +
      "Install Media Feature Pack (Windows N/KN edition).";
    setStatus(err);
    if (STATUS_EL) STATUS_EL.style.color = "red";
    return;
  }

  // ── 1. Wire <video> to a fresh MediaSource (sourceopen but no SourceBuffer yet) ──
  // SourceBuffer creation is deferred until the first init segment arrives so
  // we can derive the precise codec string from its avcC box (B11-S4 fix).
  //
  // Phase 10: state stored in mseState so tearDownMse / setUpMse can mutate it.
  const ms = new MediaSource();
  const objectUrl = URL.createObjectURL(ms);
  mseState.ms = ms;
  mseState.objectUrl = objectUrl;
  VIDEO_EL.src = objectUrl;

  // B11 diagnostic: surface video-element lifecycle so we can tell whether
  // segments reach the decoder and whether playback starts. Without these,
  // a "black <video>" is ambiguous between "no data appended", "decoder
  // rejects data", "<video> never starts playing", and "<video> plays but
  // every frame is black".
  ["loadedmetadata", "loadeddata", "canplay", "playing", "stalled", "waiting", "error", "emptied"].forEach((ev) => {
    VIDEO_EL.addEventListener(ev, () => {
      const curMs = mseState.ms;
      console.log(
        "[video]",
        ev,
        "readyState=" + VIDEO_EL.readyState,
        "networkState=" + VIDEO_EL.networkState,
        "currentTime=" + VIDEO_EL.currentTime.toFixed(3),
        "paused=" + VIDEO_EL.paused,
        "videoWidth=" + VIDEO_EL.videoWidth,
        "videoHeight=" + VIDEO_EL.videoHeight,
        VIDEO_EL.error ? "error.code=" + VIDEO_EL.error.code + " msg=" + VIDEO_EL.error.message : "",
        curMs ? "ms.readyState=" + curMs.readyState : ""
      );
    });
  });
  // Additive: dismiss the Stage-2 reconnecting overlay when the video actually
  // resumes playback, gated on overlayRevealed so it only fires during an active
  // Stage-2 reconnect (no effect on normal playback). Handles the FRAME_INIT
  // self-arm recovery path (D-IR-4) that bypasses handleStatus("streaming").
  VIDEO_EL.addEventListener("playing", dismissReconnectOverlayOnRecovery);
  // Heartbeat: report buffered ranges + currentTime every 2 s while a SB exists.
  // Reads from mseState (updated by tearDownMse / setUpMse / onInitFrame).
  setInterval(() => {
    const sb = mseState.sb;
    const curMs = mseState.ms;
    if (!sb) return;
    const ranges = [];
    try {
      const buf = sb.buffered;
      for (let i = 0; i < buf.length; i++) {
        ranges.push("[" + buf.start(i).toFixed(3) + "→" + buf.end(i).toFixed(3) + "]");
      }
    } catch (_) {}
    console.log(
      "[video.tick] currentTime=" + VIDEO_EL.currentTime.toFixed(3),
      "paused=" + VIDEO_EL.paused,
      "buffered=" + (ranges.join(",") || "<none>"),
      curMs ? "ms.readyState=" + curMs.readyState : "ms=null"
    );
    seekToLiveEdge();
  }, 2000);

  try {
    await new Promise((resolve, reject) => {
      ms.addEventListener("sourceopen", () => resolve(), { once: true });
      ms.addEventListener("error", reject, { once: true });
    });
  } catch (e) {
    setStatus("MediaSource setup failed: " + e);
    return;
  }

  mseState.active = true;
  setStatus("MSE ready — awaiting init segment…");

  // ── 2. Buffer trim — every 5 s, keep last 30 s (R11.6, OQ-mse-trim-1) ────
  const trimHandle = setInterval(trimSourceBuffer, 5000);

  // ── 3. Create a Tauri Channel<Bytes> (F-fix-3) ───────────────────────────
  //
  // window.__TAURI__.core.Channel is the Tauri 2 Channel constructor (per
  // dual-mode-shell amendment #339).
  // onmessage receives an ArrayBuffer. Byte 0 is the discriminant:
  //   0x00 = init segment → derive codec, addSourceBuffer, append
  //   0x01 = media segment → append after init
  const Channel = window.__TAURI__.core.Channel;
  const streamChannel = new Channel();

  // R-6 FIX: use the module-scoped dispatchChannelMessage so triggerRetry()
  // can bind the same function on any future Channel — see dispatchChannelMessage
  // and triggerRetry() comments above.
  streamChannel.onmessage = dispatchChannelMessage;

  // ── 4. Invoke start_stream, passing the Channel ref (F-fix-3) ────────────
  //
  // Tauri serializes Channel<T> as a "__CHANNEL__:{id}" string on the JS side,
  // which the Rust CommandArg impl deserialises back into Channel<InvokeResponseBody>.
  try {
    await window.__TAURI__.core.invoke("start_stream", { channel: streamChannel });
    window.__sm_streamActive = true; // R6 — flag set after MSE source attach + start_stream succeeds (amended R9.1)
    setStatus("start_stream invoked — waiting for first IDR…");

    // B11-S8 / B11-S9 / B11-S10: fire PLI on a permanent 2 s cadence.
    //
    // S8 added the call. S9 retried it until init arrived. S10 keeps
    // retrying FOREVER while the session is alive because the fMP4 muxer
    // only flushes a media segment on the NEXT keyframe — it accumulates
    // P-frames until then. OpenH264 in screen-content mode (used for the
    // sender pipeline) only emits IDRs on scene changes by default, so a
    // static or slowly-changing desktop produces a single startup IDR and
    // then nothing. Without periodic PLIs the SourceBuffer receives the
    // init segment and zero media segments — exactly the 'black rectangle
    // with buffered=<none>' symptom observed in B11. attach_stream is
    // rate-limited to 1 PLI per 2 s on the Rust side so this single
    // cadence drives one IDR every ~2 s, giving v1 demo a steady ~2 s
    // worst-case latency between captured frame and visible frame.
    const FIRE_PLI = async () => {
      try {
        await window.__TAURI__.core.invoke("attach_stream");
        console.log("[mse] attach_stream invoked — PLI fired toward sender");
      } catch (e) {
        console.warn("[mse] attach_stream failed:", e);
      }
    };
    FIRE_PLI(); // first fire — likely pre-ICE, may be wasted
    setInterval(FIRE_PLI, 2_000); // permanent cadence — drives periodic IDRs
  } catch (e) {
    setStatus("start_stream failed: " + e);
    clearInterval(trimHandle);
  }
}

main().catch((e) => setStatus("startup failed: " + e));

// TEST EXPORT SEAM — do not remove. See tests/js/setup.test.js for rationale.
// In the production webview, globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ is
// undefined → the `if` short-circuits and this block is a byte-equivalent
// no-op (zero parser/runtime cost; no observable behavior change).
if (globalThis.__SCREEN_MIRROR_TEST_EXPORTS__) {
  Object.assign(globalThis.__SCREEN_MIRROR_TEST_EXPORTS__, {
    deriveCodecFromInitSegment,
    seekToLiveEdge,
    LIVE_EDGE_MAX_DRIFT_SEC,
    LIVE_EDGE_TARGET_LEAD_SEC,
    mseState, // D-IR-8: expose for init-recovery drain assertions
  });
}
