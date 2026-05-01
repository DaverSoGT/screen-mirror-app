# Skill Registry — screen-mirror-app

**Generated**: 2026-04-29
**Project root**: `C:\Users\Usuario\Desktop\screen-mirror-app`
**Stack**: Tauri v2 (Rust backend, `src-tauri/`) + JS frontend with Vitest, pnpm, lefthook
**Persistence**: file (`.atl/skill-registry.md`) + engram (`topic_key: skill-registry`)

**Delegator use only.** Any agent that launches sub-agents reads this registry to resolve compact rules, then injects them directly into sub-agent prompts. Sub-agents do NOT read this registry or individual SKILL.md files.

See `_shared/skill-resolver.md` for the full resolution protocol.

---

## Project Skills (workspace)

These ship under `.claude/skills/` (and a mirror under `.agents/skills/` from `skills-lock.json`). Project-level wins over user-level on dedupe.

| Trigger | Skill | Path |
|---------|-------|------|
| Tauri v2 / `src-tauri` / `invoke` / `emit` / `capabilities.json` / commands & IPC | tauri-v2 | `.claude/skills/tauri-v2/SKILL.md` |
| Writing or reviewing Rust, ownership/error handling, Cargo, clippy, tests | rust-best-practices | `.claude/skills/rust-best-practices/SKILL.md` |
| Vitest tests, mocking, coverage, fixtures, filtering | vitest | `.claude/skills/vitest/SKILL.md` |
| Node.js architecture, framework selection, async/security/validation principles | nodejs-best-practices | `.claude/skills/nodejs-best-practices/SKILL.md` |
| Express/Fastify backends, middleware, error handling, REST/GraphQL APIs | nodejs-backend-patterns | `.claude/skills/nodejs-backend-patterns/SKILL.md` |

## User Skills (global)

| Trigger | Skill | Path |
|---------|-------|------|
| Creating PRs, branch naming, conventional commits, issue-first workflow | branch-pr | `~/.claude/skills/branch-pr/SKILL.md` |
| Creating GitHub issues, bug reports, feature requests, maintainer approval | issue-creation | `~/.claude/skills/issue-creation/SKILL.md` |
| Adversarial dual review ("judgment day", "juzgar", "doble review") | judgment-day | `~/.claude/skills/judgment-day/SKILL.md` |
| Creating new AI agent skills, documenting patterns for AI | skill-creator | `~/.claude/skills/skill-creator/SKILL.md` |
| Go tests, Bubbletea TUI testing, teatest, golden files | go-testing | `~/.claude/skills/go-testing/SKILL.md` |

## Plugin Skills (installed)

| Trigger | Skill | Path |
|---------|-------|------|
| Building distinctive web UIs, frontend components/pages | frontend-design | `~/.claude/plugins/cache/claude-plugins-official/frontend-design/unknown/skills/frontend-design/SKILL.md` |
| ALWAYS ACTIVE — engram persistent memory protocol | engram-memory | `~/.claude/plugins/cache/engram/engram/0.1.0/skills/memory/SKILL.md` |

> Note: The Go-testing skill is unlikely to apply here (no Go code in this Tauri/Rust/JS project). Listed for completeness; sub-agents touching `.go` files would still resolve it.

---

## Compact Rules

Pre-digested rules per skill. Delegators copy matching blocks into sub-agent prompts as `## Project Standards (auto-resolved)`.

### tauri-v2
- Register every command in `tauri::generate_handler![cmd1, cmd2, ...]` — unregistered commands silently fail at runtime
- All app logic lives in `src-tauri/src/lib.rs`; `main.rs` is a thin passthrough; mark `pub fn run()` with `#[cfg_attr(mobile, tauri::mobile_entry_point)]`
- Async commands MUST take owned types (`String`), never borrowed (`&str`) — borrow across await is a compile error
- Command args must `serde::Deserialize`; returns and error types must `serde::Serialize`
- Tauri v2 denies all by default — add capabilities to `src-tauri/capabilities/default.json`; every plugin feature needs an explicit permission string
- Use `Mutex<T>` for shared state via `.manage()`; `State<'_, Mutex<T>>` in commands; types must match `.manage()` exactly or it panics
- Frontend uses `@tauri-apps/api/core` (v2 `invoke`/`Channel`), NOT `@tauri-apps/api/tauri` (v1)
- Use `tauri::WebviewWindow` and `app.get_webview_window("label")`; the v1 `app.get_window()` API is removed
- IPC patterns: `invoke` (req/resp), `emit`/`listen` (events), `Channel<T>` (high-frequency typed streaming)
- Mobile: `rustup target add aarch64-linux-android ...`; gate desktop-only APIs with `#[cfg(desktop)]` / `#[cfg(mobile)]`

### rust-best-practices
- Prefer `&T`/`&str`/`&[T]` over owned types in params unless ownership transfers; avoid `.clone()` unless required
- Return `Result<T, E>` for fallible operations; never `unwrap()`/`expect()` outside tests; use `?` over match chains
- `thiserror` for library/typed errors; `anyhow` for binaries only — never expose `anyhow::Error` from a library API
- Run `cargo clippy --all-targets --all-features --locked -- -D warnings`; watch `redundant_clone`, `large_enum_variant`, `needless_collect`
- Prefer `#[expect(clippy::lint)]` with justification over `#[allow(...)]`
- Tests: descriptive names (`process_should_return_error_when_input_empty`), one assertion per test, doc tests for public API
- Generics (static dispatch) for hot paths; `dyn Trait` only for heterogeneous collections; box at API boundaries, not internally
- `//` comments explain WHY (safety, workarounds, design rationale); `///` doc comments explain WHAT/HOW for public APIs; every `TODO` links an issue (`// TODO(#42): ...`)

### vitest
- Explicit imports from `'vitest'`: `describe, it, expect, vi, beforeEach, afterEach` — no globals unless `globals: true` is set
- Use `defineConfig` from `'vitest/config'`; share resolvers/transformers with the Vite app when present
- Mock with `vi.mock()`, spy with `vi.spyOn()`, fake timers with `vi.useFakeTimers()`; `vi.hoisted()` for hoist-required setup
- Coverage via V8 (default, fast) or Istanbul (more accurate); configured under `test.coverage` in the vitest config
- Snapshots: `toMatchSnapshot()` for files, `toMatchInlineSnapshot()` for in-test; review snapshot diffs intentionally
- Filtering: `it.skip` / `it.only` / `it.concurrent`; tag-based filtering for groups; smart watch reruns only affected tests via the Vite module graph
- Type-level tests via `expectTypeOf` / `assertType`; runs at test time
- Coverage globs across drives: prefer `**/<file>` over absolute paths to avoid Windows path mismatches

### nodejs-best-practices
- Pick framework by context — Hono (edge/serverless), Fastify (perf), NestJS (enterprise/team), Express (legacy/ecosystem). ASK if unclear
- ESM by default for new projects; consider native TS via Node 22 `--experimental-strip-types` for scripts
- Validate at boundaries: request body/params/headers, env vars at startup, external API responses; pick Zod/Valibot/ArkType by need
- Never use sync I/O in production; offload CPU-bound work to worker threads or external services
- Centralize error handling: throw custom errors anywhere → catch at top middleware → return safe response, log full context
- Test priorities: critical paths (auth/payments) → edge cases → error handling; skip framework code

### nodejs-backend-patterns
- Layered structure: controllers (HTTP only) → services (business logic, framework-agnostic) → repositories (data access)
- Validate at API boundary with Zod (or Joi); reject early with appropriate 4xx
- Custom error classes extending `AppError` with `statusCode`; map to responses via a global error-handler middleware
- Wrap async handlers in `asyncHandler` so rejections flow to the error middleware (Express)
- Auth: JWT (short access + refresh) + bcrypt for passwords; secrets from env; rate-limit auth endpoints stricter than the general API
- Default middleware: `helmet`, `cors` (never `*` in prod), `compression`; HTTPS in production; structured logging (Pino/Winston)
- Use connection pools for databases; graceful shutdown; health-check endpoint

### branch-pr
- Every PR MUST link an approved issue: `Closes/Fixes/Resolves #N`; blank PRs without linkage are blocked by Actions
- Every PR MUST have exactly one `type:*` label (bug/feature/docs/refactor/chore/breaking-change)
- Branch names: `^(feat|fix|chore|docs|style|refactor|perf|test|build|ci|revert)\/[a-z0-9._-]+$`
- Conventional commits required: `^(build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(\(scope\))?!?: description$`
- Run `shellcheck` on modified scripts before pushing
- Never force-push to main; never skip hooks (`--no-verify`); no `Co-Authored-By` trailers
- All automated checks must pass: PR Validation (issue ref, `status:approved`, type label) + CI (shellcheck)

### issue-creation
- Blank issues disabled — MUST use a template (`bug_report.yml` or `feature_request.yml`)
- Every issue auto-gets `status:needs-review`; a maintainer MUST add `status:approved` before any PR can open
- Questions go to GitHub Discussions, NOT Issues
- Search for duplicates first via `gh issue list --search "keyword"`
- Bug reports require: description, steps, expected, actual, OS, agent, shell
- Feature requests require: problem, proposed solution, affected area
- Maintainer approval: `gh issue edit <n> --add-label "status:approved"` after review

### judgment-day
- Resolve skills FIRST (Pattern 0): read registry, match by code+task context, build a "Project Standards (auto-resolved)" block, inject into BOTH judge prompts and the fix-agent prompt
- Launch TWO judge sub-agents in PARALLEL (async delegate) — NEVER sequential, NEVER cross-contaminate (blind protocol)
- Orchestrator NEVER reviews code itself — only coordinates, synthesizes, asks user
- Judges classify warnings: `WARNING (real)` (normal user can trigger) vs `WARNING (theoretical)` (contrived) — theoreticals reported as INFO, NOT fixed, NOT re-judged
- Synthesize verdict: Confirmed (both judges), Suspect (one only), Contradiction (disagreement)
- Fix Agent is a SEPARATE delegation — never use one of the judges as the fixer; after fixes, re-launch BOTH judges fresh in parallel
- After 2 fix iterations, ASK the user whether to continue — never auto-escalate
- APPROVED requires: 0 confirmed CRITICALs + 0 confirmed real WARNINGs (theoreticals/suggestions may remain)
- NEVER push/commit/say "done" until every JD reaches APPROVED or ESCALATED

### skill-creator
- Skills live in `~/.claude/skills/<name>/SKILL.md` (or project `.claude/skills/`); structure: `SKILL.md` + optional `assets/` and `references/`
- Frontmatter MUST include `name`, `description` (with explicit "Trigger:" phrase), `license: Apache-2.0`, `metadata.author`, `metadata.version`
- The "Trigger:" phrase in `description` is what the agent matches — make it explicit and keyword-rich
- `assets/` holds templates, schemas, configs; `references/` links to LOCAL files only (NO web URLs)
- DON'T add a Keywords section (agent searches frontmatter, not body); DON'T duplicate docs (reference instead)
- Naming: `{technology}` for generic, `{project}-{component}` for project-specific, `{action}-{target}` for workflows
- Register the skill in `AGENTS.md` after creation

### go-testing
- Table-driven tests with `name`, `input`, `expected`, `wantErr` fields; iterate via `t.Run(tt.name, ...)`
- Test Bubbletea models directly via `m.Update(tea.KeyMsg{...})` for state transitions
- For full TUI flows use `teatest.NewTestModel(t, m)`, `tm.Send(...)`, `tm.WaitFinished(t, teatest.WithDuration(...))`, `tm.FinalModel(t)`
- Golden file testing for visual output: `testdata/<TestName>.golden`; gate updates behind a `-update` flag
- Use `t.TempDir()` for filesystem tests; mock os/exec via interfaces
- Common commands: `go test ./...`, `go test -cover ./...`, `go test -update ./...`, `go test -short ./...`

### frontend-design
- Commit to ONE bold aesthetic direction (brutalist, maximalist, minimalist, retro-futuristic, editorial, etc.) and execute with precision
- Pick distinctive fonts — AVOID Inter, Roboto, Arial, system fonts, and Space Grotesk; pair a display font with a refined body font
- Use CSS variables for theme cohesion; dominant color + sharp accents > timid evenly-distributed palettes
- AVOID generic AI patterns: purple gradients on white, cookie-cutter layouts, predictable component patterns
- Vary themes (light/dark), fonts, and aesthetics across generations — never converge on common defaults
- High-impact motion (one orchestrated page-load with staggered reveals via `animation-delay`) beats scattered micro-interactions
- Backgrounds with atmosphere: gradient meshes, noise, geometric patterns, layered transparencies, grain — not flat solids
- Match implementation complexity to vision: maximalist needs elaborate code with extensive animations; minimalist needs restraint and precision

### engram-memory
- Call `mem_save` IMMEDIATELY after decisions, bug fixes, conventions, discoveries, user confirmations/rejections — do NOT wait to be asked
- Format: `title` = verb + what; `type` ∈ {bugfix, decision, architecture, discovery, pattern, config, preference}; content structured as What / Why / Where / Learned
- Use `topic_key` (e.g. `architecture/auth-model`) for evolving topics so updates upsert; different topics MUST NOT overwrite each other
- Unsure key → call `mem_suggest_topic_key`; known ID → use `mem_update`
- Search order on recall: `mem_context` (recent) → `mem_search` (FTS5) → `mem_get_observation` (full untruncated)
- Search PROACTIVELY when starting potentially-prior work or when the user's first message references the project/feature
- BEFORE saying "done"/"listo": call `mem_session_summary` with Goal / Instructions / Discoveries / Accomplished / Next Steps / Relevant Files — MANDATORY
- AFTER compaction: IMMEDIATELY call `mem_session_summary` with the compacted summary, then `mem_context` — only then continue

---

## Project Conventions

| File | Path | Notes |
|------|------|-------|
| _none_ | — | No `CLAUDE.md`, `AGENTS.md`, `agents.md`, `.cursorrules`, `GEMINI.md`, or `copilot-instructions.md` in project root |

No project-level convention files exist. Sub-agents fall back to global `~/.claude/CLAUDE.md` (user-private, not part of this registry per resolver protocol).

Project-specific signals worth noting (read these directly when relevant — they are NOT skill rules):
- `lefthook.yml` — pre-commit/pre-push hooks
- `rust-toolchain.toml` — pinned Rust toolchain
- `rustfmt.toml` — Rust formatting config
- `deny.toml` — `cargo-deny` license/advisory policy
- `vitest.config.js` — JS test runner config
- `package.json` (`pnpm` package manager)
- `crates/` — Rust workspace
- `src-tauri/` — Tauri backend
- `tests/` — top-level test artifacts
