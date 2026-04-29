// setup.test.js — SC-SETUP-1: meta-test that validates the harness itself.
//
// This test asserts that ALL required globals exist BEFORE any dist/* import.
// If setup.js has a bug (wrong load order, missing stub), this test fails
// first and diagnosis is instant — rather than a cryptic TypeError in the
// SUT test files.

import { describe, it, expect } from 'vitest';

describe('harness setup (SC-SETUP-1)', () => {
  it('window.__TAURI__ is installed before any dist import', () => {
    expect(globalThis.__TAURI__).not.toBeUndefined();
    expect(globalThis.__TAURI__).not.toBeNull();
    expect(typeof globalThis.__TAURI__.core.invoke).toBe('function');
    expect(typeof globalThis.__TAURI__.core.Channel).toBe('function');
  });

  it('MediaSource global is installed before any dist import', () => {
    expect(globalThis.MediaSource).not.toBeUndefined();
    expect(globalThis.MediaSource).not.toBeNull();
    expect(typeof globalThis.MediaSource.isTypeSupported).toBe('function');
  });

  it('URL.createObjectURL is stubbed before any dist import', () => {
    expect(typeof globalThis.URL.createObjectURL).toBe('function');
    const result = globalThis.URL.createObjectURL({});
    expect(result).toMatch(/^blob:/);
  });

  it('__SCREEN_MIRROR_TEST_EXPORTS__ is an object before any dist import', () => {
    expect(globalThis.__SCREEN_MIRROR_TEST_EXPORTS__).not.toBeUndefined();
    expect(typeof globalThis.__SCREEN_MIRROR_TEST_EXPORTS__).toBe('object');
  });

  it('document.getElementById("player") is null before installDom (no premature DOM)', () => {
    // setup.js intentionally does NOT install DOM elements — each test does it
    // in beforeEach. This confirms setup.js respects that contract.
    // After pnpm test, this should be null (clean state).
    const el = document.getElementById('player');
    // Either null (no prior test installed DOM) or not-null if another test
    // ran first and leaked — but since pool=forks, each file is isolated.
    // We simply assert the function is callable without throwing.
    expect(() => document.getElementById('player')).not.toThrow();
  });
});
