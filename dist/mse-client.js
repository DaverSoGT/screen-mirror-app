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
//   Resolution: frontend creates a Channel via window.__TAURI_INTERNALS__.Channel,
//   passes it to Rust's start_stream as a command argument. Rust sends
//   InvokeResponseBody::Raw(bytes) — no JSON encoding. The Channel delivers an
//   ArrayBuffer directly to onmessage. The toUint8Array() helper is no longer needed.
//
//   Frame layout (byte 0 = discriminant):
//     0x00 (FRAME_INIT)    = fMP4 init segment (moov box, one per session)
//     0x01 (FRAME_SEGMENT) = fMP4 media segment (moof+mdat, one per GOP)
//
// JS binding verified:
//   window.__TAURI_INTERNALS__.Channel is the constructor available in Tauri 2
//   when withGlobalTauri: true is set in tauri.conf.json. invoke() is available
//   as window.__TAURI__.core.invoke or window.__TAURI_INTERNALS__.invoke.
//
// No import / require. Plain JS module. R11.7.

const CODEC = 'video/mp4; codecs="avc1.42E01E"';
const VIDEO_EL = document.getElementById("player");
const STATUS_EL = document.getElementById("status");

// Frame discriminant constants (must match FRAME_INIT / FRAME_SEGMENT in stream.rs).
const FRAME_INIT = 0x00;
const FRAME_SEGMENT = 0x01;

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

  // ── 4. Create a Tauri Channel<Bytes> (F-fix-3) ───────────────────────────
  //
  // window.__TAURI_INTERNALS__.Channel is the Tauri 2 Channel constructor.
  // The channel is passed to start_stream as a command argument; Rust clones
  // it into the mux thread and calls channel.send(InvokeResponseBody::Raw(bytes)).
  //
  // onmessage receives an ArrayBuffer. Byte 0 is the discriminant:
  //   0x00 = init segment → feed to SourceBuffer first
  //   0x01 = media segment → feed to SourceBuffer after init
  const Channel = window.__TAURI_INTERNALS__.Channel;
  const streamChannel = new Channel();

  streamChannel.onmessage = (payload) => {
    // payload is ArrayBuffer (InvokeResponseBody::Raw path).
    const data = new Uint8Array(payload);
    if (data.length === 0) {
      console.warn("[mse] empty frame received — ignoring");
      return;
    }

    const discriminant = data[0];
    const frameBytes = data.subarray(1).buffer; // strip the discriminant byte

    if (discriminant === FRAME_INIT) {
      setStatus("init segment received (" + (data.length - 1) + " bytes)");
      initReceived = true;
      enqueue(frameBytes);
    } else if (discriminant === FRAME_SEGMENT) {
      if (!initReceived) {
        console.warn("[mse] segment arrived before init — discarding");
        return;
      }
      enqueue(frameBytes);
    } else {
      console.warn("[mse] unknown frame discriminant: 0x" + discriminant.toString(16));
    }
  };

  // ── 5. Invoke start_stream, passing the Channel ref (F-fix-3) ────────────
  //
  // Tauri serializes Channel<T> as a "__CHANNEL__:{id}" string on the JS side,
  // which the Rust CommandArg impl deserialises back into Channel<InvokeResponseBody>.
  try {
    await window.__TAURI__.core.invoke("start_stream", { channel: streamChannel });
    setStatus("start_stream invoked — waiting for first IDR…");
  } catch (e) {
    setStatus("start_stream failed: " + e);
    clearInterval(trimHandle);
  }
}

main().catch((e) => setStatus("startup failed: " + e));
