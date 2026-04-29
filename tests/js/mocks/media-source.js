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
export function makeMediaSource() {
  const sb = makeSourceBuffer();
  const listeners = Object.create(null);
  const ms = {
    readyState: 'closed',
    addSourceBuffer: vi.fn((codec) => {
      ms._lastCodec = codec;
      ms.readyState = 'open';
      return sb;
    }),
    endOfStream: vi.fn(),
    addEventListener: vi.fn((ev, cb) => {
      listeners[ev] = cb;
      // sourceopen must fire async after construction so the
      // `await new Promise(resolve => ms.addEventListener('sourceopen', resolve, {once:true}))`
      // at line 137 resolves. queueMicrotask (NOT setTimeout) so fake timers
      // do not block it.
      if (ev === 'sourceopen') queueMicrotask(cb);
    }),
    _listeners: listeners,
    _sb: sb,
    _lastCodec: null,
  };
  return ms;
}

// Constructor used to stub global `MediaSource`:
export function MockMediaSourceCtor() {
  const ms = makeMediaSource();
  // Expose the ms instance so tests can access _sb etc via MediaSource._lastInstance
  MockMediaSourceCtor._lastInstance = ms;
  return ms;
}
MockMediaSourceCtor.isTypeSupported = vi.fn((type) => {
  // Default: accept any video/mp4 codec the SUT probes.
  return /^video\/mp4/.test(type);
});
MockMediaSourceCtor._lastInstance = null;
