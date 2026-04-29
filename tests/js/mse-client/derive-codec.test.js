// derive-codec.test.js — B1 seam detection + B2 codec derivation unit tests.
//
// SC-B1-SEAM: asserts the test export seam exposes deriveCodecFromInitSegment.
// SC-S4-1, SC-S4-2: codec derivation from bare avcC fixtures.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from '../mocks/dom.js';
import { installTauriMock, resetTauriMock } from '../mocks/tauri.js';
import { MockMediaSourceCtor } from '../mocks/media-source.js';
import { AVCC_HIGH_41, AVCC_BASELINE_30, NO_AVCC } from '../fixtures/avcc.js';

describe('mse-client — seam: deriveCodecFromInitSegment export (B1)', () => {
  beforeEach(async () => {
    // 1. DOM first — module top-level reads document.getElementById
    installDom();
    // 2. Globals — Tauri + MediaSource
    installTauriMock();
    vi.stubGlobal('MediaSource', MockMediaSourceCtor);
    // 3. Test-export bag
    globalThis.__SCREEN_MIRROR_TEST_EXPORTS__ = {};
    // 4. Fake timers BEFORE import (setInterval at line 331 must be captured)
    vi.useFakeTimers();
    // 5. Module reset + dynamic import
    vi.resetModules();
    await import('../../../dist/mse-client.js');
    // 6. Flush sourceopen microtask
    await vi.advanceTimersByTimeAsync(0);
  });

  afterEach(() => {
    vi.useRealTimers();
    removeDom();
    resetTauriMock();
    delete globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
  });

  it('exposes deriveCodecFromInitSegment via __SCREEN_MIRROR_TEST_EXPORTS__ after import', () => {
    expect(globalThis.__SCREEN_MIRROR_TEST_EXPORTS__.deriveCodecFromInitSegment)
      .toBeTypeOf('function');
  });
});
