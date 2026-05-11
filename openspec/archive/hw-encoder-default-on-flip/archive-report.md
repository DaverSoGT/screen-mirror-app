# Archive Report: hw-encoder-default-on-flip

> **Status**: SHIPPED (with soak gate deviation accepted)
> **Branch lifecycle**: `feat/hw-encoder-default-on-flip` @ `70a6dd8` → merge commit `4659016` (PR #23) → branch deleted on origin
> **Master baseline**: `efcac92` → `4659016`
> **Date**: 2026-05-11
> **Artifact store**: hybrid (engram + this file)

---

## Outcome

v0.2.0 SHIPPED on master. `hw-encoder` Cargo feature is now default on Windows. The sender app runs with GPU-accelerated H.264 MFT encoder by default on hosts with compatible HW (Intel Quick Sync, NVIDIA NVENC). Automatic SW fallback to OpenH264 active for hosts without HW. Env var kill-switch `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` preserved as Tier 1 rollback.

**User-visible effect**: first slice of Bucket A work (Bug 1 cross-vendor closure + default-on flip) that materially changes end-user behavior. Previous slices (1–6 R2) all merged but were invisible because the feature remained opt-in.

Diff: 5 files, +69 / −20.

| File | LOC delta | Purpose |
|------|-----------|---------|
| `crates/sm-infra/Cargo.toml` | +14 / −7 | Feature flip + comment refresh (cites #816 + env var) |
| `crates/sm-infra/README.md` | +18 / −8 | Default-on framing + env var as Tier 1 rollback documented |
| `CHANGELOG.md` | +40 / −1 | `## [0.2.0] - 2026-05-10` entry with Changed/Documentation/Compatibility |
| `src-tauri/Cargo.toml` | +1 / −1 | Version bump 0.1.0 → 0.2.0 (DD4: line 3, not 7) |
| `Cargo.lock` | +1 / −1 | Auto-update from version bump |

---

## Soak Gate Deviation (CRITICAL CARRY-FORWARD)

Spec R12 / Design DD5 / Tasks Phase C specified a 24h-per-host parallel soak with explicit thresholds (ERROR count == 0, SW fallback count == 0 during soak, panic count == 0, viewer reconnect ≤ 1s).

**Actual gate executed: ZERO.** The soak was not run.

### Recalibration timeline
1. **Original (per spec/design)**: 24h × 2 hosts, parallel.
2. **First recalibration**: 3h × 2 sequential — engram #825 documented this deviation (user operational constraint: cannot keep both hosts powered+connected 24h, nor sequentially for 48h total).
3. **Second recalibration**: smoke battery of 5 targeted tests (~3-4h total) — more diagnostic coverage per hour invested, but more manual orchestration steps.
4. **User stop signal**: "no veo progreso significativo en el proyecto, lo veo tal cual hace una semana." Honest assessment showed ~2143 lines of SDD planning for 15 LOC of code change, and no user-visible value would land until this PR merged.
5. **Final decision**: drop soak gate, merge PR #23, rely on existing safety nets.

### Safety nets that DID validate this merge

1. **CI 12/12 SUCCESS** on PR #23 (Check/Test/Clippy windows+macos+ubuntu; Rustfmt; MSRV; JS Tests).
2. **P2 #809 cross-vendor evidence** from Slice 6 R2: Intel QSV IDR @ idx 1 + NVENC IDR @ idx 0 proven on probes.
3. **Slice 6 R2 archive #816**: 27/27 PASS on Host A (Intel QSV) and Host B (NVENC) with the underlying MFT mechanism this default-on flip exposes.
4. **Automatic SW fallback** in `factory.rs` (production-tested via existing `init_failed_falls_back_to_software_encoder` unit test).
5. **Env var kill-switch** `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` (Tier 1 rollback, no rebuild required, already in production code).
6. **DD7 Tier 2 rollback**: `git revert <merge-sha>` + ship v0.2.1 hotfix.

### Accepted risks

- Multi-day driver state accumulation not validated (would require continuous unattended soak).
- Real-user-load reconnect-cycle stress not validated locally (only validated via Slice 6 R2 probes which exercise the underlying mechanism).
- Sender memory leak detection not validated locally — relies on user feedback post-deploy.

These risks are documented and accepted. If regressions surface, the env var rollback is the immediate mitigation; the v0.2.1 hotfix is the durable mitigation.

---

## Acceptance Criteria — Final Status

| AC | Status | Evidence |
|----|--------|----------|
| AC-1..AC-6 (Cargo.toml + comment + version + README + CHANGELOG) | VERIFIED | PR #23 diff inspection |
| AC-7..AC-9 (factory.rs, windows_mft.rs, sm-domain unchanged) | VERIFIED | `git diff master --stat` confirmed only 4 expected files (+ Cargo.lock auto) |
| AC-10 (clippy 0 warnings) | VERIFIED | CI Clippy across 3 OS |
| AC-11 (nextest workspace GREEN) | VERIFIED | CI Test across 3 OS, 626/626 PASS locally |
| AC-12 (15 unit tests run+PASS Windows CI) | VERIFIED | CI Test (windows-latest) — 15 `windows_mft::tests` migrated from "not compiled" to "compiled+run" |
| AC-13, AC-14 (macOS/Linux unchanged) | VERIFIED | CI Check + Test (no new test count, no new compilation surface) |
| AC-15..AC-19 (soak gate) | **NOT VERIFIED — DEVIATION** | Soak dropped; see above |
| AC-20 (PR body soak evidence) | DEVIATION | PR body has soak plan as scaffolding but no soak evidence; this archive documents the deviation |
| AC-21 (release notes) | VERIFIED | CHANGELOG `[0.2.0] - 2026-05-10` entry present |

**Net**: 14/21 AC VERIFIED, 7 deviated (all soak-gated). Deviations documented here per spec §9 corrigendum pattern.

---

## Follow-ups Captured

| # | Item | Source | Priority | Notes |
|---|------|--------|----------|-------|
| 1 | `hw-encoder-backend-disclosure-in-sender-diagnostics` | Proposal D3 | LOW-MED | Expose active encoder backend (HW/SW) via `sender_diagnostics`. Out-of-scope for default-on; relevant for observability post-default-on. Type-erasure (`Box<dyn VideoEncoder>`) requires trait method or tuple return. |
| 2 | `factory.rs` `\\ SAFETY:` paste artifacts at lines 149, 165, 169 | Design DD9 side-observation | XS | Cosmetic typo: `\\` instead of `//`. Out-of-scope for this slice. |
| 3 | Soak observability backfill | This archive's deviation register | MED-CONDITIONAL | If user feedback surfaces issues post-default-on, add tracing instrumentation or crash reporting. Not committed to. |
| 4 | DRAIN-spam cleanup | sdd-init v15 (pre-deferred XS) | LOW | pump_loop fires COMMAND_DRAIN ~12× post-disconnect. ~3 LOC guard. Cosmetic. |

---

## Roadmap Delta (for sdd-init v16)

- **Status change**: v0.1.0 SHIPPED → v0.2.0 SHIPPED on master.
- **Bucket A**: fully CLOSED end-to-end (Slice 6 R2 fix + default-on flip).
- **HW encoder feature**: opt-in → default-on (Windows).
- **No concrete next-direction roadmap in sdd-init**: was last visited as "v2 candidates" in v15 (`hw-encoder-default-on-flip` ready, `hw-encoder-mft-disconnect-drain-once` XS pre-deferred, `v0.2.0 release` pending). With `v0.2.0 release` now de-facto done (this PR), the next-direction slate needs explicit definition before the next SDD cycle.

Suggested candidates for v16 roadmap (not committed):
- `v0.2.0 release-tag-and-publish` (semi-done — version bumped in code; git tag + GitHub release missing).
- `hw-encoder-backend-disclosure-in-sender-diagnostics` (D3 follow-up).
- `hw-encoder-mft-disconnect-drain-once` (XS cleanup, pre-deferred).
- `factory.rs typo cleanup` (XS, follow-up #2 above).
- User-visible feature direction TBD by user (no current candidates in sdd-init).

---

## Engram Chain

explore #819 → proposal #820 → spec #822 → design #821 → tasks #823 → apply-progress #824 → soak-deviation #825 → **archive-report (this)**

---

## Commits Register

| SHA | Type | Description |
|-----|------|-------------|
| `70a6dd8` | feat (config) | `chore(infra): enable hw-encoder by default on Windows (v0.2.0)` — 4 file edits + Cargo.lock auto-update, single atomic commit per DD6 |
| `4659016` | merge | PR #23 merge commit (this slice on master) |
| TF.4 housekeeping | chore | `chore(repo): archive hw-encoder-default-on-flip SDD artifacts` (this archive landing on master) |

---

## Lessons Learned

1. **SDD planning ratio**: a 15-LOC change took 2143 lines of planning artifacts. For sub-100-LOC config/docs changes, the SDD machinery is disproportionate. Future small slices should consider a lighter cadence (explore-light → proposal-only → apply → archive) without full spec/design/tasks decomposition.
2. **Soak gates need realistic operational constraints**: the 24h × 2 parallel soak was unachievable from the start given user's hardware availability. A pre-flight conversation about operational feasibility would have caught this before spec lock.
3. **Visible value matters**: users measure progress by user-visible effects, not by infrastructure commits. Slices 1–6 R2 all landed but were invisible because the feature was opt-in. Default-on flip was the materialization event for the entire week's work.
