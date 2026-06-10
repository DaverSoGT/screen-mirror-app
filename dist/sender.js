// sender.js — Tauri IPC wiring for the sender page.
//
// Handles start/stop/restart lifecycle and renders status + button label
// changes received over the Channel<InvokeResponseBody>.
//
// Channel binding: __TAURI__.core.Channel (per dual-mode-shell amendment #339,
// commit f6fc389 — NOT __TAURI_INTERNALS__).
//
// Phase 9 additions:
// - "reconnecting" event: show transient status, hide Retry/Cancel.
// - "dead" event: show error + Retry/Cancel buttons.
//   Retry: invokes retry_session (Phase 11 stub — falls back to start_sender).
//   Cancel: invokes stop_sender.

(function () {
  const { invoke, Channel } = window.__TAURI__.core;

  const startBtn = document.getElementById("start");
  const statusDiv = document.getElementById("status");
  const errorDiv = document.getElementById("error");
  const changeModeLink = document.getElementById("change-mode");
  // Phase 9: Retry and Cancel buttons for dead-session UI.
  // These elements may be absent from older HTML pages — guard with ?. calls.
  const retryBtn = document.getElementById("retry");
  const cancelBtn = document.getElementById("cancel");
  // hw-encoder-backend-disclosure: backend label span (hidden by default).
  const encoderBackendEl = document.getElementById("encoder-backend");

  // "idle" | "running" | "restart" | "reconnecting" | "dead"
  let senderMode = "idle";

  // ── Backend label helpers (DD4) ──────────────────────────────────────────────

  // Canonical vocabulary → human-readable display mapping (R10, DD4).
  const BACKEND_LABELS = {
    hw_nvenc: "HW (NVENC)",
    hw_intel_qsv: "HW (Intel QSV)",
    hw_amd: "HW (AMD)",
    hw_unknown: "HW (unknown)",
    sw_openh264: "SW (OpenH264)",
    sw_fake: "SW (fake)",
  };

  function backendLabel(key) {
    return BACKEND_LABELS[key] ?? "Encoder: " + (key || "?");
  }

  /** Show the encoder backend label with the given token. */
  function renderBackend(key) {
    if (!encoderBackendEl) return;
    if (!key) {
      clearBackend();
      return;
    }
    encoderBackendEl.textContent = backendLabel(key);
    encoderBackendEl.hidden = false;
  }

  /** Hide and clear the encoder backend label. */
  function clearBackend() {
    if (!encoderBackendEl) return;
    encoderBackendEl.hidden = true;
    encoderBackendEl.textContent = "";
  }

  // ── Phase 9 helpers ──────────────────────────────────────────────────────────

  function showDeadButtons() {
    if (retryBtn) retryBtn.style.display = "";
    if (cancelBtn) cancelBtn.style.display = "";
  }

  function hideDeadButtons() {
    if (retryBtn) retryBtn.style.display = "none";
    if (cancelBtn) cancelBtn.style.display = "none";
  }

  // S-conf1 (CAP-2-v3): map terminal dead `reason` tokens to human-readable copy.
  // CAP-2-v3 introduced new machine tokens (peer_unreachable, ice_failed_repeatedly)
  // that would otherwise leak raw into the dead-modal as "Connection lost: peer_unreachable".
  // Only mapped tokens get bespoke copy; any other/absent reason keeps the existing
  // "Connection lost: <reason|unknown>" fallback (behavior unchanged). Kept symmetric
  // with dist/mse-client.js.
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

  // ── Channel message handler ──────────────────────────────────────────────────

  function handleMessage(value) {
    switch (value.kind) {
      case "status":
        statusDiv.textContent = value.value;
        errorDiv.textContent = "";
        break;
      case "connecting":
        statusDiv.textContent = "Connecting...";
        errorDiv.textContent = "";
        hideDeadButtons();
        break;
      case "streaming":
        statusDiv.textContent = "Streaming";
        errorDiv.textContent = "";
        hideDeadButtons();
        senderMode = "running";
        // One-shot backend disclosure (DD3, R11): invoke once per session start.
        invoke("sender_diagnostics")
          .then(function (s) { renderBackend(s.backend_name); })
          .catch(function () {});
        break;
      case "reconnecting":
        // CAP-2-v3 (REQ-WD-10): honest count-free copy. The bounded retry window can
        // last up to ~60s (issue #62); the frontend cannot distinguish the supervisor's
        // real retry from the post-watchdog wait, so the misleading "attempt X/max"
        // denominator is removed in favour of a clear "still waiting" message.
        statusDiv.textContent = "Reconnecting… waiting for the viewer";
        errorDiv.textContent = "";
        hideDeadButtons();
        senderMode = "reconnecting";
        break;
      case "dead":
        // All reconnect attempts exhausted — show error + Retry/Cancel (AC-7).
        errorDiv.textContent = humanDeadReason(value.reason);
        statusDiv.textContent = "Disconnected";
        showDeadButtons();
        senderMode = "dead";
        window.__sm_streamActive = false;
        clearBackend();
        break;
      case "peer_lost":
        // V1-incompatible-peer path only (transient ICE failure now goes
        // through the reconnect supervisor, not here). Kept for backwards compat.
        statusDiv.textContent = "Disconnected";
        hideDeadButtons();
        clearBackend();
        break;
      case "stopped":
        statusDiv.textContent = "Not connected";
        startBtn.textContent = "Start streaming";
        senderMode = "idle";
        window.__sm_streamActive = false;
        hideDeadButtons();
        clearBackend();
        break;
      case "button":
        startBtn.textContent = value.label;
        if (value.label === "Restart") {
          senderMode = "restart";
        } else if (value.label === "Stop streaming") {
          senderMode = "running";
        }
        break;
      case "error":
        errorDiv.textContent = value.message;
        startBtn.textContent = "Start streaming";
        senderMode = "idle";
        window.__sm_streamActive = false;
        hideDeadButtons();
        break;
      case "failed":
        errorDiv.textContent = value.reason || "Unknown error";
        startBtn.textContent = "Start streaming";
        senderMode = "idle";
        window.__sm_streamActive = false;
        hideDeadButtons();
        break;
    }
  }

  // ── Start sender ─────────────────────────────────────────────────────────────

  async function startSender() {
    errorDiv.textContent = "";
    statusDiv.textContent = "Connecting...";
    startBtn.textContent = "Stop streaming";
    senderMode = "running";
    // Clear any stale backend label from the previous session (DD6).
    clearBackend();

    const channel = new Channel();
    channel.onmessage = function (payload) {
      // payload is the raw bytes from InvokeResponseBody::Raw.
      // The sender encodes JSON as UTF-8 bytes; decode and parse.
      try {
        let value;
        if (payload instanceof ArrayBuffer || ArrayBuffer.isView(payload)) {
          const text = new TextDecoder().decode(payload);
          value = JSON.parse(text);
        } else if (typeof payload === "string") {
          value = JSON.parse(payload);
        } else {
          value = payload;
        }
        handleMessage(value);
      } catch (e) {
        console.error("[sender] channel message parse error:", e, payload);
      }
    };

    try {
      await invoke("start_sender", {
        channel,
        udpPort: null,
        serviceName: null,
      });
      window.__sm_streamActive = true;
    } catch (err) {
      console.error("[sender] start_sender failed:", err);
      const msg =
        typeof err === "object" && err !== null
          ? JSON.stringify(err)
          : String(err);
      errorDiv.textContent = msg;
      startBtn.textContent = "Start streaming";
      statusDiv.textContent = "Not connected";
      senderMode = "idle";
      window.__sm_streamActive = false;
    }
  }

  // ── Stop sender ──────────────────────────────────────────────────────────────

  async function stopSender() {
    try {
      await invoke("stop_sender");
    } catch (err) {
      console.error("[sender] stop_sender failed:", err);
    }
    startBtn.textContent = "Start streaming";
    statusDiv.textContent = "Not connected";
    errorDiv.textContent = "";
    senderMode = "idle";
    window.__sm_streamActive = false;
  }

  // ── Phase 9: Retry button handler ───────────────────────────────────────────
  // Retry invokes retry_session (Phase 11). Until Phase 11 lands, falls back to
  // start_sender with cached params (TODO Phase 11: swap to retry_session).

  if (retryBtn) {
    retryBtn.addEventListener("click", async function () {
      hideDeadButtons();
      senderMode = "idle";
      // Phase 11: invoke retry_session — reads cached params from SenderBridge,
      // tears down residue, re-enters Connecting state (AC-8).
      const channel = new Channel();
      channel.onmessage = function (payload) {
        try {
          let value;
          if (payload instanceof ArrayBuffer || ArrayBuffer.isView(payload)) {
            const text = new TextDecoder().decode(payload);
            value = JSON.parse(text);
          } else if (typeof payload === "string") {
            value = JSON.parse(payload);
          } else {
            value = payload;
          }
          handleMessage(value);
        } catch (e) {
          console.error("[sender] retry channel message parse error:", e, payload);
        }
      };
      try {
        await invoke("retry_session", { channel });
        senderMode = "running";
        window.__sm_streamActive = true;
      } catch (err) {
        console.error("[sender] retry_session failed:", err);
        const msg =
          typeof err === "object" && err !== null
            ? JSON.stringify(err)
            : String(err);
        const errorDiv = document.getElementById("error");
        if (errorDiv) errorDiv.textContent = msg;
        senderMode = "idle";
        window.__sm_streamActive = false;
      }
    });
  }

  // ── Phase 9: Cancel button handler ──────────────────────────────────────────
  // Cancel invokes stop_sender to return to idle state (spec §5.1).

  if (cancelBtn) {
    cancelBtn.addEventListener("click", async function () {
      hideDeadButtons();
      await stopSender();
    });
  }

  // ── Button click handler ─────────────────────────────────────────────────────

  startBtn.addEventListener("click", async function () {
    if (senderMode === "running") {
      await stopSender();
    } else if (senderMode === "restart") {
      await stopSender();
      await startSender();
    } else {
      await startSender();
    }
  });

  // ── Change-mode link ─────────────────────────────────────────────────────────

  changeModeLink.addEventListener("click", function () {
    localStorage.removeItem("sm.lastMode");
  });
})();
