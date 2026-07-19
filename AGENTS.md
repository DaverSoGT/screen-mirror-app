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
- For new Rust behavior tests only, a strict-TDD RED commit may contain only the smallest crate-private, unwired production scaffold required for the tests in the same diff to compile. The same diff MUST add direct behavior assertions; the scaffold MUST use the intended final signatures while deliberately providing no working semantics (for example, reservation returns `None`, and commit or cancel operations do not mutate state).
- Such a RED scaffold MUST NOT add public API, dependencies, async or runtime wiring, payload handling, test-local shims, hook or review bypasses, ignored, `should_panic`, or panic-only tests, `todo!()`, unrelated production behavior, or working RED semantics.
- In every allowed case, the intentional runtime failure is required RED evidence and MUST NOT be reported as a defect. Ordinary production changes remain subject to normal review.
- Vitest tests use `vi.useFakeTimers()` in `beforeEach` and `vi.useRealTimers()` in `afterEach`.
- `vi.resetModules()` required in `beforeEach` to isolate module-level state between tests.
- No `console.error` or `debugger` in committed code.

## Architecture

- `dist/` files are hand-edited static files — no bundler, no source maps.
- No new npm dependencies without explicit justification.
- No Rust changes for JS-only features.
- Conventional commits only; no `Co-Authored-By` AI attribution.
