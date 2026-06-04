import { vi } from 'vitest';

// MockSourceBuffer matches the surface mse-client.js touches:
//   .mode (writable string)            — line 263 sets to 'sequence'
//   .updating (bool)                   — read at lines 159, 175
//   .buffered (TimeRanges-ish)         — read at lines 123, 179
//   .appendBuffer(bytes)               — called at line 162
//   .remove(start, end)                — called at line 180
//   .addEventListener(ev, cb)          — line 264 ('updateend'),
//                                        lines 271, 274 ('error', 'abort')
export function makeSourceBuffer() {
  const listeners = Object.create(null);
  const sb = {
    mode: 'segments',
    updating: false,
    buffered: { length: 0, start: () => 0, end: () => 0 },
    appendBuffer: vi.fn((bytes) => {
      // Record the EXACT bytes (and the byteOffset/byteLength) so tests can
      // assert that the discriminant byte was stripped (B11-S7 regression).
      sb._lastAppend = bytes;
      // Synchronously fire updateend in a microtask so flushQueue can chain.
      queueMicrotask(() => listeners.updateend?.());
    }),
    remove: vi.fn(),
    addEventListener: vi.fn((ev, cb) => { listeners[ev] = cb; }),
    _listeners: listeners,
    _lastAppend: null,
  };
  return sb;
}

// MockMediaSource matches the surface at lines 91, 137-140, 246, 277, 280:
//
// deferOpen (default: false) — opt-in manual-sourceopen mode for init-race tests:
//   - addSourceBuffer throws InvalidStateError when readyState !== 'open'
//     (models real browser behaviour; does NOT affect default mode)
//   - sourceopen is NOT auto-fired on addEventListener; test must call
//     ms._fireSourceOpen() to open the MediaSource manually
//   - Default mode is byte-compatible with prior behaviour (91 baseline stay green)
export function makeMediaSource({ deferOpen = false } = {}) {
  const sb = makeSourceBuffer();
  const listeners = Object.create(null);
  const ms = {
    readyState: 'closed',
    addSourceBuffer: vi.fn((codec) => {
      if (ms.readyState !== 'open') {
        // Model the real browser: addSourceBuffer on a non-open MediaSource throws.
        // In default mode this path is unreachable because readyState is set to
        // 'open' before the sourceopen callback fires (see addEventListener below).
        // In deferOpen mode this enforces the queue contract tested by SC-IR-1..4.
        throw new DOMException('InvalidStateError', 'InvalidStateError');
      }
      ms._lastCodec = codec;
      return sb;
    }),
    endOfStream: vi.fn(),
    addEventListener: vi.fn((ev, cb) => {
      listeners[ev] = cb;
      if (ev === 'sourceopen') {
        if (!deferOpen) {
          // DEFAULT (unchanged): flip readyState to 'open' BEFORE the callback
          // fires so addSourceBuffer does not hit the guard above, then fire
          // async via queueMicrotask — same observable timing as before.
          ms.readyState = 'open';
          queueMicrotask(cb);
        }
        // deferOpen: do NOT auto-fire. Test drives ms._fireSourceOpen() manually.
      }
    }),
    // One-shot manual trigger for deferOpen mode. Sets readyState='open' then
    // fires the stored sourceopen listener. No-op if no listener registered yet.
    _fireSourceOpen: () => {
      ms.readyState = 'open';
      listeners.sourceopen?.();
    },
    _listeners: listeners,
    _sb: sb,
    _lastCodec: null,
  };
  return ms;
}

// Constructor used to stub global `MediaSource`:
//
// MockMediaSourceCtor._deferOpenNext (bool, default false):
//   One-shot flag — if true the NEXT constructed instance uses deferOpen mode.
//   Reset to false after construction so subsequent instances use default mode.
//   Use in tests: MockMediaSourceCtor._deferOpenNext = true; (before the SUT
//   code that creates the MediaSource via new MediaSource()).
export function MockMediaSourceCtor() {
  const ms = makeMediaSource({ deferOpen: MockMediaSourceCtor._deferOpenNext });
  MockMediaSourceCtor._deferOpenNext = false; // one-shot: reset after use
  // Expose the ms instance so tests can access _sb etc via MediaSource._lastInstance
  MockMediaSourceCtor._lastInstance = ms;
  return ms;
}
MockMediaSourceCtor.isTypeSupported = vi.fn((type) => {
  // Default: accept any video/mp4 codec the SUT probes.
  return /^video\/mp4/.test(type);
});
MockMediaSourceCtor._lastInstance = null;
MockMediaSourceCtor._deferOpenNext = false;
