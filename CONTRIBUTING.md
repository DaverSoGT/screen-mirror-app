# Contributing

## JavaScript Tests

The `tests/js/` directory contains a vitest + happy-dom regression test suite
for `dist/sender.js` and `dist/mse-client.js`. These tests guard the five bug
classes fixed in B11 (codec string format, buffer view semantics, PLI cadence,
SourceBuffer sequence mode, and attach_stream invocation).

### Prerequisites

Node.js 20+ and pnpm (enabled via Corepack):

```sh
corepack enable
```

### Running tests

```sh
# Run all tests once (no coverage)
pnpm test:watch

# Run all tests once with v8 coverage report
pnpm test

# Alias — same as pnpm test
pnpm test:coverage
```

Coverage output is written to `coverage/` (gitignored). The `coverage/lcov.info`
file is produced on every `pnpm test` run and consumed by the `js-test` CI job.

### Test structure

```
tests/js/
  setup.js                  # Global stubs: __TAURI__, MediaSource, DOM elements
  setup.test.js             # SC-SETUP-1/2: validates harness bootstrap
  mocks/
    dom.js                  # installDom() / removeDom()
    media-source.js         # MockMediaSource, MockSourceBuffer
    tauri.js                # MockChannel, installTauriMock()
  fixtures/
    avcc.js                 # Hand-crafted avcC binary fixtures
    init-segments.js        # ftyp+moof init segment fixtures
    media-segments.js       # moof+mdat media segment fixtures
  mse-client/
    derive-codec.test.js    # SC-S4-1/2: codec pure-function unit tests
    codec-passthrough.test.js # SC-S4-3/4: addSourceBuffer integration
    buffer-view.test.js     # SC-S7-1/2: Uint8Array view regression guard
    attach-stream.test.js   # SC-S8-1/2: attach_stream invocation guard
    pli-cadence.test.js     # SC-S9-1/2, SC-S10-1: PLI permanent cadence
    sequence-mode.test.js   # SC-S12-1/2: SourceBuffer mode=sequence guard
  sender/
    handle-message.test.js  # SC-SEND-1..5: handleMessage DOM mutations
    buttons.test.js         # SC-SEND-6/7: start/stop button handlers
    change-mode.test.js     # SC-SEND-8: change-mode localStorage.removeItem
```

### Adding a new test

1. Create a new `.test.js` file under `tests/js/mse-client/` or `tests/js/sender/`.
2. Follow the `beforeEach` pattern in an existing test (DOM install → Tauri mock →
   `vi.useFakeTimers()` if needed → `vi.resetModules()` → `await import('../../../dist/...js')` → flush microtasks).
3. Run `pnpm test` to verify.
