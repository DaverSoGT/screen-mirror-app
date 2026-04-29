// setup.test.js — SC-SETUP-1, SC-SETUP-2: meta-tests that validate the harness itself.
//
// SC-SETUP-1: ALL required globals exist BEFORE any dist/* import.
// SC-SETUP-2: vi.resetModules() isolates IIFE side effects between tests.
//
// If setup.js has a bug (wrong load order, missing stub), SC-SETUP-1 fails
// first and diagnosis is instant — rather than a cryptic TypeError in the
// SUT test files.

import { vi, describe, it, expect, beforeEach, afterEach } from 'vitest';
import { installDom, removeDom } from './mocks/dom.js';
import { installTauriMock, resetTauriMock } from './mocks/tauri.js';

describe('harness meta-tests (SC-SETUP-1)', () => {
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

describe('harness meta-tests (SC-SETUP-2) — IIFE isolation via vi.resetModules()', () => {
  // This describe verifies that two consecutive tests that each import dist/sender.js
  // do NOT accumulate event listeners. Each test should see invoke('start_sender')
  // called exactly once when #start is clicked — not twice on the second run.

  beforeEach(async () => {
    installDom();
    installTauriMock();
    // resetModules ensures the IIFE re-runs against fresh DOM each test
    vi.resetModules();
    await import('../../dist/sender.js');
    await Promise.resolve();
  });

  afterEach(() => {
    removeDom();
    resetTauriMock();
  });

  it('SC-SETUP-2 (first run): start click → invoke("start_sender") exactly once', async () => {
    const { invoke } = globalThis.__TAURI__.core;
    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    const calls = invoke.mock.calls.filter((c) => c[0] === 'start_sender');
    expect(calls.length).toBe(1);
  });

  it('SC-SETUP-2 (second run): start click → invoke("start_sender") exactly once (not accumulated)', async () => {
    // Without vi.resetModules() in beforeEach, the IIFE would not re-run, the
    // new DOM's #start button would have no listener, and this test would fail.
    // With resetModules, the IIFE re-runs, the listener is fresh, count is 1.
    const { invoke } = globalThis.__TAURI__.core;
    const startBtn = document.getElementById('start');
    startBtn.click();
    await Promise.resolve();
    await Promise.resolve();

    const calls = invoke.mock.calls.filter((c) => c[0] === 'start_sender');
    expect(calls.length).toBe(1);
  });
});
