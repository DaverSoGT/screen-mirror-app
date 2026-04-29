// vitest.config.js — root config for the js-test-harness.
//
// Tests exercise dist/*.js EXACTLY as shipped (no transpile, no bundle).
// happy-dom provides a JSDOM-like Window for module-level DOM queries
// (mse-client.js:38-39 and sender.js:12-15) which run at script parse time.
//
// setupFiles runs ONCE per test FILE, BEFORE any module import in that file
// resolves. This is the only safe place to install globalThis.__TAURI__,
// MediaSource, and __SCREEN_MIRROR_TEST_EXPORTS__ — once dist/mse-client.js
// is imported, line 38 (VIDEO_EL) and line 338 (main()) fire immediately and
// any missing global throws synchronously at parse time.

import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'happy-dom',
    setupFiles: ['./tests/js/setup.js'],
    include: ['tests/js/**/*.test.js'],
    // Do NOT use globals: true — explicit imports of vi/describe/it/expect
    // make test files self-documenting and survive future vitest renames.
    globals: false,
    // restoreMocks resets vi.fn() spies between tests (call counts, return
    // values). Pairs with vi.resetModules() in beforeEach to enforce per-test
    // isolation. unstubGlobals lets us track which globals each test stubbed.
    restoreMocks: true,
    unstubGlobals: true,
    // No threads — happy-dom isolation per-file is good enough; threads add
    // memory overhead and gain little for ~9 small test files.
    pool: 'forks',
    coverage: {
      provider: 'v8',
      include: ['dist/sender.js', 'dist/mse-client.js'],
      reporter: ['text', 'lcov'],
      reportsDirectory: './coverage',
      // PQ-4 LOCKED A: track-only, no thresholds gate this change.
      // Once we have a baseline, follow-up `js-coverage-gate` adds:
      //   thresholds: { lines: N, functions: N, branches: N, statements: N }
    },
  },
});
