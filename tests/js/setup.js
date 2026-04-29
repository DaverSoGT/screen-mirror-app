// tests/js/setup.js — vitest setupFiles entry
//
// This file runs ONCE per test FILE, BEFORE any module in that file imports.
// It installs ALL global stubs that dist/mse-client.js and dist/sender.js
// need at module parse time:
//
//   - dist/mse-client.js:38-39  reads VIDEO_EL + STATUS_EL from the DOM
//   - dist/mse-client.js:338    calls main() which reads __TAURI__ + MediaSource
//   - dist/sender.js:10         reads window.__TAURI__.core at IIFE eval
//   - dist/sender.js:12-15      reads DOM elements at IIFE eval
//
// NOTE: DOM elements are NOT installed here — each test installs its own
// fresh DOM fragment in beforeEach (installDom from mocks/dom.js). If DOM
// elements were installed here they would be shared across tests in the same
// file, causing click-handler accumulation (R5 / SC-SETUP-2).

import { vi } from 'vitest';
import { MockChannel } from './mocks/tauri.js';
import { MockMediaSourceCtor } from './mocks/media-source.js';

// 1. Install Tauri globals
//    Channel must be a real constructor (sender.js:76 does `new Channel()`).
globalThis.__TAURI__ = {
  core: {
    invoke: vi.fn().mockResolvedValue(undefined),
    Channel: MockChannel,
  },
};

// 2. Install MediaSource stub
//    happy-dom does not implement MediaSource; the SUT probes it at startup.
globalThis.MediaSource = MockMediaSourceCtor;

// 3. Stub URL.createObjectURL / revokeObjectURL
//    happy-dom's implementation is unreliable; stub deterministically.
if (!globalThis.URL) globalThis.URL = {};
globalThis.URL.createObjectURL = vi.fn(() => 'blob:happy-dom://stub');
globalThis.URL.revokeObjectURL = vi.fn();

// 4. Initialise the test-export bag
//    dist/mse-client.js seam appends exports here after import.
globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
