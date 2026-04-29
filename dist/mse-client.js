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
const VIDEO_EL = document.getElementById("player");
const STATUS_EL = document.getElementById("status");

// Frame discriminant constants (must match FRAME_INIT / FRAME_SEGMENT in stream.rs).
const FRAME_INIT = 0x00;
const FRAME_SEGMENT = 0x01;

function setStatus(msg) {
  if (STATUS_EL) STATUS_EL.textContent = msg;
  console.log("[mse]", msg);
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
  const ms = new MediaSource();
  VIDEO_EL.src = URL.createObjectURL(ms);

  try {
    await new Promise((resolve, reject) => {
      ms.addEventListener("sourceopen", () => resolve(), { once: true });
      ms.addEventListener("error", reject, { once: true });
    });
  } catch (e) {
    setStatus("MediaSource setup failed: " + e);
    return;
  }

  setStatus("MSE ready — awaiting init segment…");

  // ── 2. Sequential append queue + lazy SourceBuffer (R11.5) ────────────────
  let sb = null;
  const pending = [];
  let initReceived = false;

  function enqueue(bytes) {
    pending.push(bytes);
    flushQueue();
  }

  function flushQueue() {
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

  // ── 3. Buffer trim — every 5 s, keep last 30 s (R11.6, OQ-mse-trim-1) ────
  function trimSourceBuffer() {
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
  const trimHandle = setInterval(trimSourceBuffer, 5000);

  // ── 4. Create a Tauri Channel<Bytes> (F-fix-3) ───────────────────────────
  //
  // window.__TAURI__.core.Channel is the Tauri 2 Channel constructor (per
  // dual-mode-shell amendment #339).
  // onmessage receives an ArrayBuffer. Byte 0 is the discriminant:
  //   0x00 = init segment → derive codec, addSourceBuffer, append
  //   0x01 = media segment → append after init
  const Channel = window.__TAURI__.core.Channel;
  const streamChannel = new Channel();

  streamChannel.onmessage = (payload) => {
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
      if (sb !== null) {
        // Already have a SourceBuffer from a previous init — re-init not
        // supported in v1; ignore subsequent init segments.
        console.warn("[mse] additional init segment ignored");
        return;
      }
      // B11 diagnostic: dump the first 128 bytes of the init segment in hex
      // so we can validate the box structure (ftyp magic, moov, tkhd dims,
      // avcC bytes) when MSE rejects appendBuffer with the SourceBuffer
      // "removed from parent media source" error.
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
        setStatus("init segment missing avcC — cannot derive codec");
        return;
      }
      if (!MediaSource.isTypeSupported(derived)) {
        setStatus("FATAL: derived codec not supported: " + derived);
        return;
      }
      try {
        sb = ms.addSourceBuffer(derived);
        sb.mode = "segments"; // R11.4: deterministic timeline
        sb.addEventListener("updateend", flushQueue);
      } catch (e) {
        setStatus("addSourceBuffer failed: " + e);
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
        console.warn("[mse] MediaSource sourceended");
      });
      ms.addEventListener("sourceclose", () => {
        console.warn("[mse] MediaSource sourceclose — readyState=" + ms.readyState);
      });
      setStatus(
        "init segment received (" + (data.length - 1) + " bytes), codec=" + derived
      );
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
    window.__sm_streamActive = true; // R6 — flag set after MSE source attach + start_stream succeeds (amended R9.1)
    setStatus("start_stream invoked — waiting for first IDR…");

    // B11-S8 / B11-S9: fire PLI to force the sender's encoder to produce
    // an IDR on demand. OpenH264 in screen-content mode keeps GOPs very
    // long (relies on scene-change detection) so the receiver is otherwise
    // stuck waiting for the next natural keyframe — observed B11 latency:
    // 100s+. The first attach_stream call typically races the ICE
    // handshake and lands before str0m has a remote peer, so the
    // RequestKeyframe is queued and effectively lost. We retry every 2 s
    // (matching the Rust-side rate limit) until the init segment arrives
    // — at which point the mux thread has decoded an IDR and rendering
    // can begin. The retry loop self-cancels via `initReceived` and a hard
    // 30 s timeout.
    const FIRE_PLI = async () => {
      try {
        await window.__TAURI__.core.invoke("attach_stream");
        console.log("[mse] attach_stream invoked — PLI fired toward sender");
      } catch (e) {
        console.warn("[mse] attach_stream failed:", e);
      }
    };
    FIRE_PLI(); // first fire — likely pre-ICE, may be wasted
    const pliRetryDeadline = Date.now() + 30_000;
    const pliInterval = setInterval(() => {
      if (initReceived || Date.now() > pliRetryDeadline) {
        clearInterval(pliInterval);
        return;
      }
      FIRE_PLI();
    }, 2_000);
  } catch (e) {
    setStatus("start_stream failed: " + e);
    clearInterval(trimHandle);
  }
}

main().catch((e) => setStatus("startup failed: " + e));
