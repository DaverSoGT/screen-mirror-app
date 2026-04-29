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

  // SC-S4-1: High@4.1 avcC box → full MIME codec string
  // Regression guard for B11-S4: mse-client.js:58-75 must return full MIME,
  // not bare avc1. string. Revert mse-client.js:71 to return bare string →
  // this test fails.
  it('SC-S4-1: High@4.1 avcC box yields exact full MIME codec string', () => {
    const { deriveCodecFromInitSegment } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    // AVCC_HIGH_41: profile=0x64, compat=0x00, level=0x29
    // Expected: 'video/mp4; codecs="avc1.640029"'
    expect(deriveCodecFromInitSegment(AVCC_HIGH_41.buffer))
      .toBe('video/mp4; codecs="avc1.640029"');
  });

  // SC-S4-2: Buffer with no avcC → null
  // Regression guard for B11-S4: fallback must be null, not a default codec.
  it('SC-S4-2: buffer with no avcC box yields null', () => {
    const { deriveCodecFromInitSegment } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    // NO_AVCC: contains 'ftyp' + 'isom' but no 'avcC' bytes
    expect(deriveCodecFromInitSegment(NO_AVCC.buffer)).toBeNull();
  });

  // SC-S4-3 (additional baseline): Baseline@3.0 avcC box
  it('SC-S4-3 (additional): Baseline@3.0 avcC box yields correct codec string', () => {
    const { deriveCodecFromInitSegment } = globalThis.__SCREEN_MIRROR_TEST_EXPORTS__;
    // AVCC_BASELINE_30: profile=0x42, compat=0xE0, level=0x1E
    // Expected: 'video/mp4; codecs="avc1.42E01E"'
    expect(deriveCodecFromInitSegment(AVCC_BASELINE_30.buffer))
      .toBe('video/mp4; codecs="avc1.42E01E"');
  });
});
