// sender.js — Tauri IPC wiring for the sender page.
//
// Handles start/stop/restart lifecycle and renders status + button label
// changes received over the Channel<InvokeResponseBody>.
//
// Channel binding: __TAURI__.core.Channel (per dual-mode-shell amendment #339,
// commit f6fc389 — NOT __TAURI_INTERNALS__).

(function () {
  const { invoke, Channel } = window.__TAURI__.core;

  const startBtn = document.getElementById("start");
  const statusDiv = document.getElementById("status");
  const errorDiv = document.getElementById("error");
  const changeModeLink = document.getElementById("change-mode");

  // "idle" | "running" | "restart"
  let senderMode = "idle";

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
        break;
      case "streaming":
        statusDiv.textContent = "Streaming";
        errorDiv.textContent = "";
        break;
      case "peer_lost":
        statusDiv.textContent = "Disconnected";
        break;
      case "stopped":
        statusDiv.textContent = "Not connected";
        startBtn.textContent = "Start streaming";
        senderMode = "idle";
        window.__sm_streamActive = false;
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
        break;
      case "failed":
        errorDiv.textContent = value.reason || "Unknown error";
        startBtn.textContent = "Start streaming";
        senderMode = "idle";
        window.__sm_streamActive = false;
        break;
    }
  }

  // ── Start sender ─────────────────────────────────────────────────────────────

  async function startSender() {
    errorDiv.textContent = "";
    statusDiv.textContent = "Connecting...";
    startBtn.textContent = "Stop streaming";
    senderMode = "running";

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
