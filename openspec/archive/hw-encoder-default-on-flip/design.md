# Design: hw-encoder-default-on-flip

> **Change**: hw-encoder-default-on-flip
> **Project**: screen-mirror-app
> **Date**: 2026-05-10
> **Branch baseline**: master @ `efcac92`
> **Artifact store**: hybrid (engram `sdd/hw-encoder-default-on-flip/design` + this file)
> **Strict TDD**: ACTIVE (`cargo nextest run --workspace`)
> **Predecessor**: sdd-init v15 (engram #186)
> **Inputs**: Proposal #820, Exploration #819
> **Scope**: S (small) — config + docs only, ~15 LOC across 4 files

---

## 0. Design summary

This is a configuration + documentation change, not an architectural one. The
runtime architecture (factory decision tree, automatic SW fallback, env-var
kill-switch, MFT encoder, ForceKeyFrame mechanism) is already in place and
empirically validated cross-vendor in Slice 6 R2 (#816). The "design" surface
here is therefore restricted to:

1. **Information design** — exact wording in `Cargo.toml` comments, `README.md`,
   and `CHANGELOG.md` such that a future reader understands the new default,
   the kill-switch, and the rollback path without spelunking through git
   history.
2. **Release-engineering design** — version-bump policy, slice boundary,
   rollback runbook, soak runbook, CI assertion plan.
3. **Forward anchors** — explicitly recording deferred work so it cannot drop
   off the roadmap.

The design choices below (DD1..DD9) lock these in.

---

## 1. Design decisions

### DD1 — `crates/sm-infra/Cargo.toml` comment rewrite (replace, do not append)

**What**: Replace the 6-line comment block at `crates/sm-infra/Cargo.toml:8-13`
and the 5-line feature doc-comment at `crates/sm-infra/Cargo.toml:18-22`. Do
NOT append, do NOT strike-through. Proposed verbatim text (≈10 lines total,
within the ~5-lines-per-block budget):

```toml
[features]
# Default: hw-encoder is ON for Windows hosts. Bug 1 (mid-stream IDR) is
# closed via the vendor-uniform CODECAPI_AVEncVideoForceKeyFrame mechanism
# (PR #22 / Slice 6 R2, ForceKeyFrame VT_UI4 BEFORE ProcessInput). On hosts
# without a compatible MFT encoder, `build_video_encoder` automatically
# falls back to the OpenH264 software path. Set the runtime env-var
# `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to force the SW path without a
# rebuild.
default = ["hw-encoder"]
# Reserved for V2 Media Foundation MFT hardware decoder adapter.
# Empty in V1 — no code is gated behind it yet.
hw-decoder = []
# Gates the WindowsMftH264Encoder MFT adapter and its `windows` crate
# dependency. Default-on as of v0.2.0; cross-vendor validated on Intel QSV
# (Host A) and NVIDIA NVENC (Host B), 27/27 PASS each.
hw-encoder = []
```

**Why**: The current text (verbatim from `crates/sm-infra/Cargo.toml:8-13`)
states "hw-encoder is OFF... currently deadlocks on real GPU hosts" and
references discoveries #582/#591/#592 — every word of this is now obsolete.
Slice 6 R2 archive #816 closed Bug 1 vendor-uniformly with 27/27 PASS on
both hosts; #809 P2 proved the ForceKeyFrame timing/variant; pump_loop was
not redesigned, it was simplified (net −250 LOC). Leaving stale comments in
the canonical feature declaration would mis-direct any future contributor.
Proposal D4 locks "replace, don't append"; git blame preserves the prior
text.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Append "UPDATE: now default-on" below the old block | Unprofessional, doubles the comment surface, confuses readers |
| Strike-through with HTML or `~~` | TOML does not render markdown; comment becomes visual noise |
| Delete the comment entirely | Loses the explanation of the env-var kill-switch and the fallback contract; future contributors would have to read `factory.rs` to learn this |
| Move the explanation to README only | Cargo.toml is the canonical declaration site; explanation belongs next to the flag |

**Trade-offs**: Verbose-ish (≈8 lines) compared to the theoretical minimum
(0 lines), but information density is high: it names the bug, the mechanism,
the PR, the fallback behaviour, and the kill-switch — every claim a future
reader will need.

---

### DD2 — `crates/sm-infra/README.md` "Hardware encoder smoke tests" rewrite

**What**: Rewrite the section at `crates/sm-infra/README.md:113-147`. Preserve
the `#[ignore]`-gated smoke-test invocation but **drop `--features hw-encoder`**
from it (the feature is now default-on, so the flag is redundant; keeping it
would imply the feature is still opt-in). Add a short paragraph stating the
default-on status. Keep the existing env-var kill-switch section unchanged in
substance — only adjust the surrounding prose so it reads as the documented
runtime rollback path rather than as a debugging curiosity.

Proposed verbatim wording for the header paragraph and command block
(replacing `README.md:113-131`; the env-var section at lines 133-147 stays
mechanically identical, prose only refreshed):

```md
### Hardware encoder smoke tests (`crates/sm-infra/tests/windows_mft_encode.rs`)

The `WindowsMftH264Encoder` uses Windows Media Foundation Transform (MFT) with
`MFTEnumEx(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)` and is the
**default encoder on Windows hosts** as of v0.2.0. Hardware-only integration
tests are annotated `#[ignore]` and live in
`crates/sm-infra/tests/windows_mft_encode.rs`; they run on a Windows host with
a dedicated GPU.

**Preconditions:**
- Windows 10 1709 (Fall Creators Update) or Windows 11
- A GPU with a hardware H.264 encoder (Intel Quick Sync, NVIDIA NVENC, AMD AMF,
  or compatible)
- Up-to-date GPU driver

**Run hardware encoder tests manually:**

```sh
cargo nextest run -p sm-infra --run-ignored only --tests windows_mft_encode
```

**Force software encoder (runtime kill-switch):**

Set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to bypass `MFTEnumEx` and use
the OpenH264 software encoder without rebuilding. This is the documented
rollback path if a host exhibits HW-encoder regressions in the field:

```sh
$env:SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER = "1"
cargo nextest run -p sm-infra
```

**Factory fallback behaviour:** if no hardware MFT encoder is found
(`InitFailed` returned by `WindowsMftH264Encoder::new`), `build_video_encoder`
automatically falls back to `WindowsOpenH264Encoder`. A one-time
`tracing::info!` log records the reason.
```

**Why**: Two reasons. First, `--features hw-encoder` is now redundant on a
default-on feature and including it in docs would teach the wrong mental
model. Second, the kill-switch must be re-framed from "useful for debugging"
to "the documented runtime rollback path" because the soak runbook (DD5) and
rollback runbook (DD7) both rely on it as the in-the-field escape hatch.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Keep `--features hw-encoder` in the smoke-test command for backwards-compat | The flag is harmless but pedagogically misleading post-flip |
| Add a "previous default was SW" historical note | Belongs in CHANGELOG, not README |
| Delete the smoke-test section entirely | Slice 6 R2 explicitly preserved these probes as regression evidence (#186 v15 testing infra note); removing them would break the contract |

**Trade-offs**: Slightly more prose around the env-var (now framed as the
rollback mechanism, not a debug knob), but this is the right framing now
that it is being relied on operationally.

---

### DD3 — `CHANGELOG.md [0.2.0]` entry: Keep-a-Changelog with `### Changed` + `### Documentation`

**What**: Convert `CHANGELOG.md` `[Unreleased]` (currently empty, line 13) into
a dated `[0.2.0]` section directly above `[0.1.0]`. Use exactly two
sub-headings per proposal D5: `### Changed` (the default-on flip + version
bump) and `### Documentation` (the Cargo.toml comment + README refresh). No
`### Added` (nothing new), no `### Fixed` (not a bug-fix slice), no
`### Removed`, no `### Security`. Date: `2026-05-10` (or `- Unreleased`
pending merge — to be finalised at apply-phase).

Proposed verbatim block:

```md
## [0.2.0] - 2026-05-10

### Changed
- **HW encoder default**: `sm-infra` now enables the `hw-encoder` feature by
  default on Windows. `WindowsMftH264Encoder` (Media Foundation Transform,
  hardware H.264) is selected automatically when a compatible MFT is found;
  hosts without compatible hardware fall back transparently to the OpenH264
  software encoder via the existing `InitFailed → OpenH264` path
  (`crates/sm-infra/src/encode/factory.rs`). Backed by PR #22 / Slice 6 R2
  (mid-stream IDR via vendor-uniform `CODECAPI_AVEncVideoForceKeyFrame`,
  validated 27/27 PASS on Intel Quick Sync and NVIDIA NVENC).
- **Shipped binary version**: `screen-mirror` (`src-tauri/Cargo.toml`) bumped
  from `0.1.0` to `0.2.0` to surface the behavioural shift to library
  consumers and downstream packagers.

### Documentation
- `crates/sm-infra/Cargo.toml` — comment block + `hw-encoder` feature
  doc-comment rewritten to describe the new default, the fallback path, and
  the `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` runtime kill-switch.
- `crates/sm-infra/README.md` — "Hardware encoder smoke tests" section
  refreshed for default-on; smoke-test invocation no longer carries
  `--features hw-encoder`; env-var documented as the operational rollback.

### Compatibility
- Windows hosts with a compatible MFT encoder: encoding is now GPU-accelerated
  by default. No code changes required by consumers.
- Windows hosts without compatible hardware: identical UX to v0.1.0 via
  automatic SW fallback (one-time `tracing::info!` log records the reason).
- macOS / Linux: no impact. The `hw-encoder` feature and the `windows` crate
  are platform-gated; non-Windows targets compile identically to v0.1.0.
- Rollback: set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` in the environment
  before launching the binary to force the SW path without rebuilding.
```

**Why**: D5 locks Keep-a-Changelog 1.1 structure (matches `CHANGELOG.md:5`
policy). A third `### Compatibility` block (informally permitted under Keep a
Changelog as a free-form section) is added because the proposal explicitly
calls out compatibility framing (SW fallback preserves UX); without it, a
user reading only the changelog would not learn the rollback mechanism.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Keep `[Unreleased]` and move to `[0.2.0]` only at release-tag time | Slices version-bump out of this PR (rejected by DD6) |
| Use `### Added` for the default-on flip | Inaccurate — nothing new was added, behaviour was changed |
| Single `### Changed` block with everything inline | Sacrifices scanability; future readers grep for the rollback mechanism |
| Add `BREAKING CHANGE` footer / `!` marker on commit | Per proposal §6 + AC-14: this is NOT a breaking change (SW fallback preserves contract) |

**Trade-offs**: Three sub-headings is more text than the strict minimum but
buys clarity at zero cost — the section is read once per release.

---

### DD4 — Version bump: single atomic flip in `src-tauri/Cargo.toml:7` to `0.2.0`

**What**: In the same PR as the `default = ["hw-encoder"]` flip, bump
`src-tauri/Cargo.toml:3` (the package version line — note: `src-tauri/Cargo.toml`
shows `version = "0.1.0"` on line 3 in the current source, NOT line 7; the
prompt's `src-tauri/Cargo.toml:7` reference is line 7 of the workspace package
table or a stale reference — to be confirmed at apply-phase against the
current file) from `0.1.0` to `0.2.0`. `sm-infra` itself does not carry a
`version` field (inherits via workspace edition/rust-version only); the
shipped binary `screen-mirror` is the single versioned package in the
workspace, so this single edit moves the project to v0.2.0.

> **Apply-phase verification note**: The version field in `src-tauri/Cargo.toml`
> is at **line 3** (`version = "0.1.0"`), not line 7, per current source. The
> apply phase MUST verify the line number before editing and use the
> file-content match (`version = "0.1.0"` immediately under
> `name = "screen-mirror"`).

**Why**: Proposal D1 locks v0.2.0 (minor) per `CHANGELOG.md:9-10` policy
("Patch bumps are bug-fix only") and per cargo / RFC 1105 / cargo-semver-checks
convention (adding a default feature is a minor bump). The shipped binary's
public version is the user-visible signal of the behavioural shift; packagers,
distribution channels, and library consumers all key off this value.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| v0.1.1 (patch) | Violates `CHANGELOG.md:10-11` policy explicitly |
| v1.0.0 (major) | Premature — repo is still 0.x pre-release per `CHANGELOG.md:8` |
| Defer version bump to a separate `v0.2.0-release-tag` slice | Creates a window where master has the flip but not the version (asymmetric; misleading to consumers); see DD6 |
| Also bump a non-existent `sm-infra` version field | `sm-infra/Cargo.toml` has no `version` line — no edit needed |

**Trade-offs**: Single atomic commit makes git bisect trivial for any
regression and signals the behavioural shift in the same PR that causes it.

---

### DD5 — Soak runbook: 24h continuous per host, parallel, log-grep gate

**What**: Per proposal D2 (24h per host in parallel, pre-merge, zero-error
+ zero-unexpected-SW-fallback). Operational specification:

1. **Hosts**: Host A (Intel Quick Sync) AND Host B (NVIDIA NVENC), running in
   parallel calendar time. Both hosts have known-good HW.
2. **Build**: each host pulls `master` tip with the PR commits cherry-picked
   or merged into a local branch.
   ```powershell
   git fetch origin
   git checkout {pr-branch}
   cargo build --release -p screen-mirror
   ```
3. **Launch with telemetry**: set `RUST_LOG` to capture encoder decisions and
   pipe to a file (PowerShell on Windows):
   ```powershell
   $env:RUST_LOG = "sm_infra::encode=info,sm_infra=warn"
   $logfile = "soak-host-$($env:COMPUTERNAME)-$(Get-Date -Format yyyyMMdd-HHmmss).log"
   cargo run --release -p screen-mirror 2>&1 | Tee-Object -FilePath $logfile
   ```
4. **Duration**: 24 hours continuous mirroring session. Viewer reconnect must
   be exercised at soak start (within first 5 minutes) AND at soak end
   (within last 5 minutes) to verify IDR-on-reconnect still works.
5. **Pass gates** (both must be true per host):
   - `Select-String -Pattern "ERROR" -Path $logfile | Measure-Object | Select -Expand Count` → **0**
   - `Select-String -Pattern "falling back to software encoder" -Path $logfile | Measure-Object | Select -Expand Count` → **0**
     (host has HW; any SW fallback indicates an HW init regression)
   - Zero panics (`Select-String -Pattern "panicked at"` → **0**)
   - Zero encoder crashes (no `EncoderError::EncodeFailed` in tail of log)
   - Viewer reconnect: subjective verification — video resumes within
     ≤1 second on both reconnect attempts (start + end of soak).
6. **Evidence persistence**: each host produces:
   - The raw log file (kept locally, SHA-256 recorded in PR body)
   - A 1-line summary (host, GPU, duration, grep counts) in PR body
   - `mem_save` with `topic_key: sdd/hw-encoder-default-on-flip/soak-report`,
     `type: discovery`, capturing the per-host summary

**Why**: Proposal D2 mandates this gate; v15 sdd-init reaffirms it; #816
Slice 6 R2 corrigendum explicitly warns against single-host overclaims. The
log-grep gates are the only telemetry available (no crash reporting in
v0.1.0); they are precise and reproducible. Running both hosts in parallel
keeps wall-clock cost at 24h, not 48h.

**Alternatives rejected**: see proposal D2 — 4h smokes (too short), 24h
single-host (overclaim risk), post-merge soak (risks broken master),
community soak (no telemetry).

**Trade-offs**: 24h wall-clock blocks merge but is the cheapest meaningful
gate without telemetry infrastructure.

---

### DD6 — Slice boundary: version bump lives in THIS PR, atomic with the flip

**What**: The PR `chore(infra): enable hw-encoder by default on Windows
(v0.2.0)` includes:
- `crates/sm-infra/Cargo.toml` — `default = ["hw-encoder"]` + comment rewrite
- `crates/sm-infra/README.md` — section rewrite
- `CHANGELOG.md` — `[0.2.0]` entry
- `src-tauri/Cargo.toml` — version bump `0.1.0` → `0.2.0`

All four edits are a single squash-merged commit. **No follow-up
`v0.2.0-release-tag` slice.**

**Why**: An atomic commit makes `git bisect` produce a clean signal if any
regression is found (one commit = one cause). Splitting the version bump
into a follow-up creates a window where `master` advertises behaviour-shift
behaviour but still says `0.1.0` — confusing to consumers and packagers, and
asymmetric with the CHANGELOG entry which already names `[0.2.0]`.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Two-PR split: flip in PR A, version+tag in PR B | Adds delivery churn; v15 records delivery strategy as `auto-chain`, S-scope, single PR |
| Tag the release post-merge without bumping `Cargo.toml` | Cargo packages bind version to file content; out-of-band tags drift from `Cargo.toml`. |
| Land version bump first, then flip in a separate PR | Inverts cause-and-effect; the version bump exists *because* of the flip |

**Trade-offs**: A larger atomic diff is harder to revert partially, but
partial revert is not a desired path — the proper rollback is the env-var
(no rebuild) or a clean revert of the whole PR (DD7).

---

### DD7 — Rollback runbook: env-var (in-field) + git revert (master hotfix)

**What**: Two tiers of rollback, each appropriate for a different failure
mode:

**Tier 1 — In-field rollback (no rebuild, no merge revert)**:
Set the runtime env-var before launching the binary:
```powershell
$env:SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER = "1"
screen-mirror.exe   # or `cargo run --release -p screen-mirror`
```
Use when: a single host hits an HW regression and we want it on SW
immediately. No rebuild, no merge churn. Effect is per-launch; persist via
user/system env or wrapper script. Smoke-tested by existing
`env_var_override_selects_software_encoder` unit test in
`crates/sm-infra/src/encode/factory.rs:148` (line per Grep, see DD9).

**Tier 2 — master-tip rollback (PR-wide regression discovered post-merge)**:
1. `git revert {merge-commit-sha}` on `master` → produces a 1-line revert PR
   that re-flips `default = []`.
2. Ship `v0.2.1` (patch) with the revert + a CHANGELOG `### Fixed` entry
   referencing the regression. Per `CHANGELOG.md:10-11` patch bumps are
   bug-fix only — a regression revert qualifies.
3. Optionally tag a `v0.1.1` hot-fix branch off `0.1.0` if `0.2.0` is already
   downstream; not currently expected because v0.2.0 is the *first* default-on
   release.

**Why**: The two tiers cover the actual operational shapes:
- A single user with a flaky GPU does NOT warrant a master revert; the env-var
  is the correct answer and the SDD chain has documented it as such since
  DD2/DD7.
- A systemic regression (e.g., the soak passed but a third-vendor host found
  in the wild hits a hang) warrants a clean revert + patch release.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Tier-1 only (no master revert path) | Leaves no recourse for systemic regressions |
| Tier-2 only (revert as the only escape hatch) | Forces a rebuild + redeploy on every host for what may be a single-host driver issue |
| Add a Tauri config UI for encoder selection | Out of scope per D3 / DD8; env-var covers the operational need |

**Trade-offs**: Two tiers mean two paths to document, but each is short and
each maps onto a distinct failure shape.

---

### DD8 — Follow-up anchor: `hw-encoder-backend-disclosure-in-sender-diagnostics`

**What**: Register, in this design, the deferred follow-up change so it
cannot drop off the roadmap:

- **Change name**: `hw-encoder-backend-disclosure-in-sender-diagnostics`
- **Scope**: XS (~3–5 LOC + 1 trait method, or a tuple return, or an atomic
  side-channel — to be decided in that change's own proposal)
- **Trigger**: post-v0.2.0
- **Origin**: Proposal D3 — the Tauri UI cannot currently surface whether
  HW or SW is in use, because `build_video_encoder` returns
  `Box<dyn VideoEncoder>` and erases the backend identity. Three
  implementation routes exist (D3 enumerates them), each XS but each
  touching the domain port — that is why D3 keeps it out of this S-scope
  slice.
- **Carried-forward acceptance signal**: this change is captured in
  proposal §3 D3, in proposal §10 (open questions resolved), and now in
  this design DD8. The next sdd-init revision (v16 post-v0.2.0 archive) MUST
  list it under "v2 / Next Direction Candidates" so `/sdd-new` discovers it
  organically.

**Why**: Without an explicit anchor, "we'll do it later" follow-ups vanish
between SDD chains. The proposal already names it; the design echoes it; the
archive (post-soak) will record it in the sdd-init refresh.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Implement it inside this slice | Breaks the S-scope budget (domain port changes touch every `VideoEncoder` impl) |
| Drop it entirely | There is real value in UI disclosure; just not urgent vs. soak gate |
| Anchor only in the archive report | Archive happens post-merge; the anchor must survive the apply/verify gates |

**Trade-offs**: One paragraph of design + one CHANGELOG-future register entry
in exchange for not losing the work item.

---

### DD9 — Strict-TDD adaptation: RED/GREEN for a config-only change

**What**: Strict TDD is ACTIVE per sdd-init v15 / proposal §5. For a
config-only change there is no new executable behaviour to test-first.
Adaptation:

- **RED equivalent**: current state — CI `check` and `test` defaults do NOT
  compile `windows_mft.rs` on Windows (per exploration #819 §2.1 and §3.1).
  The 15 unit tests in `windows_mft.rs::tests` are currently invisible to
  the default CI matrix.
- **GREEN signal (the implicit "test")**: after the flip, the same 15 tests
  migrate from "not compiled" to "compiled + run" on the Windows CI job. CI
  going green with those 15 tests now in the run set IS the green-bar
  evidence. The 8 HW-required integration tests stay `#[ignore]`-gated.
- **REFACTOR**: N/A — the diff is mechanical.
- **No new tests added.** A new test would be tautological:
  - For the env-var kill-switch: existing
    `env_var_override_selects_software_encoder` in
    `crates/sm-infra/src/encode/factory.rs:148` already proves the contract
    (verified via Grep at design-time). The flip does not change factory
    decision logic, so this test remains the canonical assertion.
  - For the default-on path: testing that `Cargo.toml` says
    `default = ["hw-encoder"]` would be a tautology over the source file.
- **The 24h soak (DD5) IS the integration test** for the runtime path.

**Why**: Strict TDD requires RED before GREEN; for a config flip, the RED is
already present in the form of "windows_mft.rs not compiled on Windows CI".
The GREEN bar is "Windows CI is green with the file compiled". Adding tests
for the sake of the TDD ritual would be cargo-culting; the existing
factory test set covers the testable behaviour, and the soak covers the
empirical behaviour.

**Alternatives rejected**:

| Alternative | Reason rejected |
|-------------|-----------------|
| Add a new unit test that asserts `cfg!(feature = "hw-encoder")` is true | Tautological; just re-asserts the Cargo.toml line we are changing |
| Add an integration test that uses real MFT | Would have to be `#[ignore]` (HW-gated); duplicates the existing 8 `#[ignore]` tests |
| Add a smoke test that the env-var kill-switch works | Already covered at `factory.rs:148` — adding another would duplicate |

**Trade-offs**: Net-zero new tests but the test count visible to Windows CI
grows by +15 unit + 8 (compiled-but-ignored) integration tests — that growth
IS the GREEN signal. Documented explicitly so a reviewer does not flag
"missing tests" against the strict-TDD policy.

---

## 2. Test approach (consolidated)

This slice adds **zero new unit or integration tests**.

| Test population | Before flip | After flip | Change |
|-----------------|-------------|-----------|--------|
| `windows_mft.rs::tests` (15 unit tests) | not compiled on Win CI defaults | compiled + run on Win CI | +15 visible |
| `windows_mft_encode.rs` (8 HW integration tests, `#[ignore]`) | not compiled on Win CI defaults | compiled + skipped on Win CI | 0 behavioural change |
| `factory.rs::tests` (3 tests incl. `env_var_override_selects_software_encoder`) | runs on Win CI | runs on Win CI | unchanged |
| `clippy --all-features` | exercises hw-encoder path | exercises hw-encoder path | unchanged |
| 24h soak (DD5) | N/A | NEW operational gate | adds pre-merge gate |

The migration of the 15 `windows_mft.rs::tests` from "not compiled" to
"compiled + run" on the Windows CI job IS the empirical GREEN signal that
the flip is wired correctly (see DD9). The env-var kill-switch remains
covered by `env_var_override_selects_software_encoder` (factory.rs:148).
The HW behaviour is covered by the 24h soak per DD5.

---

## 3. CI assertion plan (mechanical verification of spec R9–R14)

The spec (parallel sibling artifact, topic `sdd/hw-encoder-default-on-flip/spec`)
will formalise R1–Rn acceptance criteria mirroring proposal §8 (AC-1..AC-15).
For the subset that must be mechanically verifiable on CI, this design
locks the assertion method:

| Spec requirement (preview) | Mechanical assertion |
|-----------------------------|----------------------|
| `default = ["hw-encoder"]` is present | `grep -c '^default = \["hw-encoder"\]$' crates/sm-infra/Cargo.toml` == 1 (verifiable at PR-review time, not a CI step) |
| Stale comment text removed | `grep -c 'currently deadlocks\|OPT-IN ONLY\|Bucket A bugs' crates/sm-infra/Cargo.toml` == 0 |
| README no longer suggests `--features hw-encoder` for normal builds | `grep -n -- '--features hw-encoder' crates/sm-infra/README.md` returns 0 matches in the smoke-test invocation block |
| CHANGELOG entry exists for `[0.2.0]` | `grep -c '^## \[0\.2\.0\]' CHANGELOG.md` == 1 |
| `src-tauri/Cargo.toml` version == `0.2.0` | `grep -c '^version = "0.2.0"$' src-tauri/Cargo.toml` == 1 |
| `clippy --all-features` is green | Already enforced by `.github/workflows/ci.yml` |
| `nextest run --workspace` is green | Already enforced; +15 tests migrate into the run set |
| `cargo fmt --check --all` is green | Already enforced |
| No edits outside the four MVC files | `git diff --name-only origin/master...HEAD` ⊆ {Cargo.toml, README.md, CHANGELOG.md, src-tauri/Cargo.toml} (PR-review assertion) |
| No Co-Authored-By in commit | Pre-commit / review assertion (project rule) |

No new CI workflow steps are added; the existing `.github/workflows/ci.yml`
matrix already covers `check`, `test`, `clippy`, `fmt`, `msrv`, `js-test`,
and `security`. Verification of grep-based assertions happens at PR review
time, not as new CI steps (adding steps for a 15-LOC config diff would be
disproportionate).

---

## 4. Deletion register

**Empty.** No code is deleted in this slice. The Cargo.toml comment rewrite
(DD1) is a content replacement, not a deletion; the README rewrite (DD2)
likewise reframes existing prose. The git diff is purely additive +
replacement, no `git rm`.

The Slice 6 R2 corrigendum already deleted obsolete mechanisms (Mechanism G,
CleanPoint INPUT-write; net −250 LOC); nothing further to remove here.

---

## 5. Architectural concerns explicitly out of scope

These are deliberately not addressed in this slice and have explicit
follow-up anchors or pre-defer rationale:

| Concern | Status | Anchor |
|---------|--------|--------|
| `Box<dyn VideoEncoder>` type-erasure → backend identity disclosure | Deferred | DD8 → `hw-encoder-backend-disclosure-in-sender-diagnostics` |
| Tauri UI config panel for encoder selection | Out of scope | Env-var is the runtime knob (DD7) |
| DRAIN-spam cleanup (~3 LOC pump_loop guard) | Pre-deferred XS | sdd-init v15 #186 candidate list |
| Rename `hw-encoder` feature / unify with `hw-decoder` | Out of scope | `hw-decoder` is reserved for v2 MFT decoder; no current consumer |
| `windows-version` runtime gating for old Win10 builds | Out of scope | Existing `is_supported()` smoke-test guard covers it |
| Adding new HW vendors (AMD AMF, virtualised GPUs) | Out of scope | Automatic SW fallback covers unsupported hosts |
| Replacing manual log-grep soak with telemetry | Out of scope | No telemetry infra in v0.1.x |
| Workspace-level `Cargo.toml` features override | Not needed | Exploration #819 §1.1 confirms no override exists |

---

## 6. Risks revisited (cross-reference proposal §7 → design mitigations)

| Risk (from proposal) | Severity | Design mitigation |
|----------------------|----------|-------------------|
| Driver variance on unknown GPU vendors | MEDIUM | Existing SW fallback at `factory.rs:101-104` — no design change; explicitly framed in DD2 README rewrite |
| CI compilation regression | LOW | Already covered by clippy `--all-features` (exploration §3.1); +15 unit tests are pure-logic, no GPU required |
| Performance regression | LOW | DD5 soak detects via `RUST_LOG=sm_infra::encode=info` log analysis; DD7 in-field rollback via env-var |
| Session-start latency | LOW | <1s on both hosts per Slice 6 R2 evidence (#816); accepted |
| No soak telemetry | MEDIUM | **DD5** defines telemetry surrogates: `Select-String` log-grep gates + viewer-reconnect smoke at start + end |
| Stale `Cargo.toml` + README | LOW | **DD1 + DD2** rewrite both; verbatim text proposed; assertion plan in §3 |
| Version bump confusion | LOW | **DD4** locks v0.2.0; **DD3** documents in CHANGELOG; **DD6** keeps atomicity |
| UI disclosure deferred | LOW | **DD8** anchor; observable via `RUST_LOG`; soak does not depend on UI |
| **NEW**: someone reverts only the version bump | LOW | **DD6** atomicity — single commit, single revert; partial revert is not a documented path |
| **NEW**: a host enables HW but driver triggers crash mid-session | LOW | **DD7 Tier 1** env-var (no rebuild); pre-existing PR #22 IDR mechanism + automatic SW fallback unaffected |
| **NEW**: someone misreads CHANGELOG as breaking | LOW | **DD3** explicit `### Compatibility` block + AC-14 `no BREAKING CHANGE footer` |

---

## 7. SDD chain anchors

```
Predecessor : Slice 6 R2 #816 (PR #22, 966e5ee, 2026-05-10)
Explore      : #819   sdd/hw-encoder-default-on-flip/explore
Proposal     : #820   sdd/hw-encoder-default-on-flip/proposal
Design       : THIS   sdd/hw-encoder-default-on-flip/design
Spec         : pending (parallel sibling)
Tasks        : pending
Apply        : pending
Verify       : pending
Archive      : pending
Follow-up    : hw-encoder-backend-disclosure-in-sender-diagnostics (XS, post-v0.2.0)
```

---

## 8. Status

**DESIGN LOCKED.** Ready for sdd-tasks (after sibling spec is also locked).
9 design decisions captured (DD1–DD9). All proposal D1–D5 decisions echoed
and made operationally precise. No source code edited; verbatim wording
proposed for the four MVC files, to be applied in the apply phase.
