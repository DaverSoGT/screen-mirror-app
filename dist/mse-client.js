// Screen Mirror — MSE client (Capability G, B7).
//
// Wires <video> + MediaSource + SourceBuffer to Tauri event stream.
// Listens for "stream/init" and "stream/segment" events emitted from Rust.
//
// OQ-tauri-emit-1 resolution:
//   Tauri 2 app.emit() with Vec<u8> payload serializes as Array<number> on the
//   JS side (serde_json encoding). The toUint8Array() helper wraps it into a
//   Uint8Array before appendBuffer. Acceptable for V1 (LAN streaming).
//
// No import / require. Plain JS module. Depends on window.__TAURI__ (global
// Tauri API injected by withGlobalTauri: true in tauri.conf.json).
//
// R11.7: no npm, no bundler, no node_modules.

const CODEC = 'video/mp4; codecs="avc1.42E01E"';
const VIDEO_EL = document.getElementById("player");
const STATUS_EL = document.getElementById("status");

function setStatus(msg) {
  if (STATUS_EL) STATUS_EL.textContent = msg;
  console.log("[mse]", msg);
}

async function main() {
  // ── R11.2: codec compatibility check ─────────────────────────────────────
  if (!("MediaSource" in window) || !MediaSource.isTypeSupported(CODEC)) {
    const err =
      "FATAL: MSE / H.264 baseline 3.0 not supported. " +
      "Install Media Feature Pack (Windows N/KN edition).";
    setStatus(err);
    if (STATUS_EL) STATUS_EL.style.color = "red";
    return;
  }

  // ── 1. Wire <video> to a fresh MediaSource ────────────────────────────────
  const ms = new MediaSource();
  VIDEO_EL.src = URL.createObjectURL(ms);

  let sb;
  try {
    sb = await new Promise((resolve, reject) => {
      ms.addEventListener(
        "sourceopen",
        () => {
          try {
            const buf = ms.addSourceBuffer(CODEC);
            buf.mode = "segments"; // R11.4: deterministic timeline
            resolve(buf);
          } catch (e) {
            reject(e);
          }
        },
        { once: true }
      );
      ms.addEventListener("error", reject, { once: true });
    });
  } catch (e) {
    setStatus("MediaSource setup failed: " + e);
    return;
  }

  setStatus("MSE ready — awaiting init segment…");

  // ── 2. Sequential append queue (R11.5) ────────────────────────────────────
  const pending = [];
  let initReceived = false;

  function enqueue(bytes) {
    pending.push(bytes);
    flushQueue();
  }

  function flushQueue() {
    if (sb.updating || pending.length === 0) return;
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

  sb.addEventListener("updateend", flushQueue);

  // ── 3. Buffer trim — every 5 s, keep last 30 s (R11.6, OQ-mse-trim-1) ────
  function trimSourceBuffer() {
    if (sb.updating || !VIDEO_EL) return;
    const cur = VIDEO_EL.currentTime;
    const cutoff = Math.max(0, cur - 30);
    if (sb.buffered.length > 0 && sb.buffered.start(0) < cutoff) {
      try {
        sb.remove(sb.buffered.start(0), cutoff);
      } catch (e) {
        console.warn("[mse] trim failed", e);
      }
    }
  }
  const trimHandle = setInterval(trimSourceBuffer, 5000);

  // ── 4. Tauri event subscriptions (R11.3) ──────────────────────────────────
  const { listen } = window.__TAURI__.event;

  await listen("stream/init", (event) => {
    // OQ-tauri-emit-1: payload is Array<number> from serde_json → wrap to Uint8Array.
    const init = toUint8Array(event.payload);
    setStatus("init segment received (" + init.byteLength + " bytes)");
    initReceived = true;
    enqueue(init.buffer);
  });

  await listen("stream/segment", (event) => {
    if (!initReceived) {
      console.warn("[mse] segment arrived before init — discarding");
      return;
    }
    const seg = toUint8Array(event.payload);
    enqueue(seg.buffer);
  });

  // ── 5. Invoke start_stream after sourceopen ───────────────────────────────
  // R-D3: frontend calls start_stream after MSE is ready to consume segments.
  try {
    await window.__TAURI__.core.invoke("start_stream");
    setStatus("start_stream invoked — waiting for first IDR…");
  } catch (e) {
    setStatus("start_stream failed: " + e);
    clearInterval(trimHandle);
  }
}

/**
 * Convert a Tauri 2 emit payload to Uint8Array.
 *
 * Tauri 2 app.emit() with Vec<u8> → JSON Array<number> on the JS side.
 * Channel<T> API would deliver a Uint8Array directly (V2 migration path).
 *
 * @param {number[]|Uint8Array} payload
 * @returns {Uint8Array}
 */
function toUint8Array(payload) {
  if (payload instanceof Uint8Array) return payload;
  return new Uint8Array(payload);
}

main().catch((e) => setStatus("startup failed: " + e));
