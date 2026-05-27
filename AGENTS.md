# Code Review Rules — screen-mirror-app

## JavaScript / TypeScript

- Use `const` for module-level constants; `let` only when reassignment is required.
- No `var`. No `window.*` global state unless required for Tauri IPC surface (`window.__TAURI__`).
- Async functions must use `try/catch` around `invoke()` calls.
- No `window.location.reload()` in any IPC retry path (REQ-NO-RELOAD).
- No `localStorage`/`sessionStorage` writes from auto-retry paths; only explicit user-action handlers may write storage.

## Rust

- No `unwrap()` in production paths; use `?` or explicit `match`/`if let`.
- All `Mutex` locks must be released before any async `.await` point.
- Tauri commands must be registered in `generate_handler![]`.
- No new Tauri commands without corresponding capability entries.

## Tests

- Strict TDD: RED commit (failing tests) MUST precede GREEN commit (implementation).
- Vitest tests use `vi.useFakeTimers()` in `beforeEach` and `vi.useRealTimers()` in `afterEach`.
- `vi.resetModules()` required in `beforeEach` to isolate module-level state between tests.
- No `console.error` or `debugger` in committed code.

## Architecture

- `dist/` files are hand-edited static files — no bundler, no source maps.
- No new npm dependencies without explicit justification.
- No Rust changes for JS-only features.
- Conventional commits only; no `Co-Authored-By` AI attribution.
