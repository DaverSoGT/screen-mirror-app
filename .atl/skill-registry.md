# Skill Registry — screen-mirror-app

**Generated**: 2026-04-23
**Project root**: `C:\Users\JDNHS\OneDrive\Escritorio\screen-mirror-app`
**Status**: Empty project — no stack detected yet

---

## User Skills (global)

| Skill | Trigger | Path |
|-------|---------|------|
| go-testing | Writing Go tests, Bubbletea TUI testing, teatest, table-driven tests, golden files | `~/.claude/skills/go-testing/SKILL.md` |
| skill-creator | Creating new AI agent skills, documenting patterns for AI | `~/.claude/skills/skill-creator/SKILL.md` |
| branch-pr | Creating a pull request, preparing a branch for submission | `~/.claude/skills/branch-pr/SKILL.md` |
| issue-creation | Creating GitHub issues, bug reports, feature requests | `~/.claude/skills/issue-creation/SKILL.md` |
| judgment-day | Parallel adversarial review ("judgment day", "dual review", "juzgar") | `~/.claude/skills/judgment-day/SKILL.md` |
| skill-registry | Create or update the project skill registry | `~/.claude/skills/skill-registry/SKILL.md` |

### SDD Skills (Spec-Driven Development suite)

| Skill | Phase | Path |
|-------|-------|------|
| sdd-init | Initialize SDD context | `~/.claude/skills/sdd-init/SKILL.md` |
| sdd-explore | Explore/investigate an idea | `~/.claude/skills/sdd-explore/SKILL.md` |
| sdd-propose | Create change proposal | `~/.claude/skills/sdd-propose/SKILL.md` |
| sdd-spec | Write requirements/scenarios | `~/.claude/skills/sdd-spec/SKILL.md` |
| sdd-design | Technical design document | `~/.claude/skills/sdd-design/SKILL.md` |
| sdd-tasks | Break change into task checklist | `~/.claude/skills/sdd-tasks/SKILL.md` |
| sdd-apply | Implement tasks | `~/.claude/skills/sdd-apply/SKILL.md` |
| sdd-verify | Validate implementation | `~/.claude/skills/sdd-verify/SKILL.md` |
| sdd-archive | Close completed change | `~/.claude/skills/sdd-archive/SKILL.md` |
| sdd-onboard | Guided SDD walkthrough | `~/.claude/skills/sdd-onboard/SKILL.md` |

---

## Project Skills (local)

None — no `.claude/skills/`, `.gemini/skills/`, `.agent/skills/`, or `skills/` directory exists.

---

## Project Conventions

None detected. No `CLAUDE.md`, `AGENTS.md`, `agents.md`, `.cursorrules`, `GEMINI.md`, or `copilot-instructions.md` in project root.

**Global conventions** (from `~/.claude/CLAUDE.md`) apply:
- Conventional commits only; no AI attribution lines
- Never build after changes
- Ask and wait — never assume answers
- Verify before agreeing; explain disagreement with evidence
- Clean/Hexagonal/Screaming Architecture, atomic design, container-presentational pattern
- Skills auto-load: go-testing for Go tests, skill-creator for new skills

---

## Compact Rules (auto-resolved, per skill)

### go-testing
- Use table-driven tests with `name`, `input`, `want` fields
- For Bubbletea TUI: use `teatest.NewTestModel(t, model)` and `WaitFor` assertions
- Prefer golden file testing for complex output (`testdata/*.golden`)
- Coverage via `go test -cover`; target meaningful paths, not lines

### skill-creator
- Skills go in `~/.claude/skills/<name>/SKILL.md` with YAML frontmatter
- Frontmatter MUST include `name`, `description`, `license`
- `description` MUST contain the trigger phrase(s)
- Keep SKILL.md focused: When to Use, Critical Patterns, Workflow, Examples

### branch-pr
- Every PR MUST link an approved issue (no exceptions)
- Every PR MUST have exactly one `type:*` label
- Automated checks must pass before merge
- Never force-push to main

### issue-creation
- Blank issues are disabled — MUST use a template
- Every issue gets `status:needs-review` automatically
- A maintainer MUST add `status:approved` before any PR can open
- Questions → Discussions, not Issues

### judgment-day
- Launch 2 BLIND judges in parallel on same target
- Iterate up to 2 rounds; escalate after that
- Synthesize findings, apply fixes, re-judge
- Resolve skills before launching judges (inject compact rules into their prompts)

### skill-registry
- Write `.atl/skill-registry.md` in project root
- Save to engram with `topic_key: "skill-registry"`, `type: "config"`
- Dedupe by name; project-level skills win over user-level
- Include both User Skills and Project Conventions sections
