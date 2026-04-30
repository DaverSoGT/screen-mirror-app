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

// Frame discriminant constants (must match FRAME_INIT / FRAME_SEGMENT / FRAME_STATUS
// in stream.rs).
const FRAME_INIT = 0x00;
const FRAME_SEGMENT = 0x01;
const FRAME_STATUS = 0x02;

function setStatus(msg) {
  if (STATUS_EL) STATUS_EL.textContent = msg;
  console.log("[mse]", msg);
}

// Handle a decoded 0x02 JSON status payload from the Rust reconnect supervisor.
// Phase 7 will wire this to actual UI state; for now it logs so the console
// shows reconnect progress during development.
function handleStatus(payload) {
  console.log("[mse-client] status:", payload.kind, payload);
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

  // Holder for the (lazy) SourceBuffer so the diagnostic heartbeat can read
  // the latest reference once it's been created on init-segment arrival.
  const sbRef = { sb: null };

  // B11 diagnostic: surface video-element lifecycle so we can tell whether
  // segments reach the decoder and whether playback starts. Without these,
  // a "black <video>" is ambiguous between "no data appended", "decoder
  // rejects data", "<video> never starts playing", and "<video> plays but
  // every frame is black".
  ["loadedmetadata", "loadeddata", "canplay", "playing", "stalled", "waiting", "error", "emptied"].forEach((ev) => {
    VIDEO_EL.addEventListener(ev, () => {
      console.log(
        "[video]",
        ev,
        "readyState=" + VIDEO_EL.readyState,
        "networkState=" + VIDEO_EL.networkState,
        "currentTime=" + VIDEO_EL.currentTime.toFixed(3),
        "paused=" + VIDEO_EL.paused,
        "videoWidth=" + VIDEO_EL.videoWidth,
        "videoHeight=" + VIDEO_EL.videoHeight,
        VIDEO_EL.error ? "error.code=" + VIDEO_EL.error.code + " msg=" + VIDEO_EL.error.message : ""
      );
    });
  });
  // Heartbeat: report buffered ranges + currentTime every 2 s while a SB exists.
  setInterval(() => {
    if (!sbRef.sb) return;
    const ranges = [];
    try {
      const buf = sbRef.sb.buffered;
      for (let i = 0; i < buf.length; i++) {
        ranges.push("[" + buf.start(i).toFixed(3) + "→" + buf.end(i).toFixed(3) + "]");
      }
    } catch (_) {}
    console.log(
      "[video.tick] currentTime=" + VIDEO_EL.currentTime.toFixed(3),
      "paused=" + VIDEO_EL.paused,
      "buffered=" + (ranges.join(",") || "<none>"),
      "ms.readyState=" + ms.readyState
    );
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
        sbRef.sb = sb; // expose to the diagnostic heartbeat (B11)
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
    } else if (discriminant === FRAME_STATUS) {
      // 0x02 — JSON status event from the reconnect supervisor (Phase 8, T8.2).
      // Decode the payload bytes as UTF-8 JSON and forward to handleStatus.
      // Must NOT feed bytes to the SourceBuffer.
      try {
        const json = new TextDecoder().decode(frameBytes);
        const payload = JSON.parse(json);
        handleStatus(payload);
      } catch (e) {
        console.warn("[mse-client] 0x02 frame JSON parse error:", e);
      }
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
  Object.assign(globalThis.__SCREEN_MIRROR_TEST_EXPORTS__, { deriveCodecFromInitSegment });
}
