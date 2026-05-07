# Proposal: hw-encoder-mft-vendor-compat-slice1-nvenc

> **Persistence mirror**: this file is a verbatim mirror of engram observation
> (`topic_key: sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/proposal`).
> Hybrid artifact store: engram is canonical; this file is committed for diff
> review. Update both when revising — orchestrator owns sync.

## SDD chain link

- **Predecessor**: `hw-encoder-mft-rework` — PR #16 (`ee32ff4`), archived
  APPROVED_WITH_CARRY_FORWARD. That change shipped Bug 2's fix (NO_WAIT polling
  + dual-arm counters in `pump_loop`) and explicitly carried forward Bug 1
  ("vendor MFT priming/setup failure family") for a dedicated follow-up.
- **This change** is the first of two chained slices that close Bug 1:
  - **Slice 1 (this)**: `hw-encoder-mft-vendor-compat-slice1-nvenc` — fix
    Manifestation B only (NVIDIA NVENC `SetOutputType` →
    `MF_E_INVALIDMEDIATYPE`, HRESULT `0xC00D6D76`). Target host: Host B (JDNHS).
  - **Slice 2 (queued)**: `hw-encoder-mft-vendor-compat-slice2-intel-qsv` —
    fix Manifestation A (Intel QSV `ProcessOutput` → access violation,
    `0xC0000005`). Target host: Host A (Usuario\Desktop). NOT in this change.
- **Successor (gated)**: `hw-encoder-default-on-flip` — flip
  `default = ["hw-encoder"]` in `crates/sm-infra/Cargo.toml`. Gated on BOTH
  Slice 1 and Slice 2 archiving cleanly with 18/18 PASS on each respective host
  plus a 24h soak. Not in this change.

PQ envelope (decided in `sdd/hw-encoder-mft-vendor-compat-rework/pq-decisions`,
engram #148): PQ-1 = C (Phase 0 on Host B only), PQ-2 = B (two slices),
PQ-3 = defer T-NEW-3, PQ-4 = include enumeration fallback if scope allows,
PQ-5 = Slice 2 (NV12 stride / `MF_MT_DEFAULT_STRIDE`).

---

## Intent

Fix Manifestation B of Bug 1: `setup_mft` constructs an output `IMFMediaType`
that NVIDIA NVENC's H.264 hardware MFT rejects with `MF_E_INVALIDMEDIATYPE`
(`0xC00D6D76`) at `SetOutputType`. On Host B this single rejection cascades
into 11 of 18 smoke-test failures because every test that exercises the
encoding path fails to initialise — only the lifecycle-only tests (which never
reach `setup_mft` or which pass solely on the stop-deadline contract) survive.
Slice 1 changes the output type construction in
`crates/sm-infra/src/encode/windows_mft.rs::setup_mft` (lines 531–580) so that
the resulting type is accepted by NVENC, while leaving the predecessor's
priming, drain, NO_WAIT polling, and dual-arm counter invariants intact and
keeping the public `VideoEncoder` / `EncoderConfig` API frozen. Success means
all 18 smoke tests PASS on Host B with `--features hw-encoder`, all 7 quality
gates remain GREEN, and Manifestation A's behaviour on Host A is unchanged
(Slice 2 owns it). Manifestation B is in the initialisation path, the failure
is fully reproducible, and the fix surface is a small set of attribute writes
in one function — ideal scope for a single tightly-scoped slice.

---

## Scope

### In scope

| Area | Change |
|------|--------|
| `setup_mft` output type construction (lines 531–580) | Modify the attribute set written to the output `IMFMediaType` so NVENC accepts `SetOutputType`. Exact attribute change is a **design-time decision** anchored on Phase 0 transcripts (see Approach below). |
| `apply_pending_codec_settings` (lines 725–749) — only if H-B1/H-B2 confirmed | If profile or bitrate is removed from `SetOutputType`, set it instead via `ICodecAPI::SetValue` once after the MFT is started but before the first `ProcessInput`. Mechanism already exists; we only add new keys. |
| `init_mft_sync` (lines 294–344) and `enumerate_and_activate` (lines 347–395) — **conditional on PQ-4 budget** | Add an enumeration-fallback loop: if `setup_mft(pactivates[i])` fails, try `pactivates[i+1]`. Self-heals dual-GPU machines (Intel + NVIDIA). ~20 LOC; included only if the chained-PR slice budget allows. |
| Phase 0 instrumentation prep commit | Add temporary attribute-walk trace in `setup_mft` (gated by feature flag — see Decision D-1) so the user can capture transcripts on Host B. Removed before the feature commits land. |
| `crates/sm-infra/tests/windows_mft_encode.rs` | The 18 existing `#[ignore]` smoke tests — same set, same names. Target on Host B: 18/18 PASS with `--features hw-encoder`. |
| 7 quality gates | All must remain GREEN: `cargo check --workspace`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --check --all`, `cargo nextest run --workspace`, `cargo deny check`, `cargo check --no-default-features`, `cargo check --features hw-encoder`. |

### Out of scope

| Area | Reason |
|------|--------|
| Manifestation A (Intel QSV `ProcessOutput` AV, `0xC0000005`) | Slice 2 (`hw-encoder-mft-vendor-compat-slice2-intel-qsv`). Different fix surface, different host, requires its own Phase 0. |
| `MF_MT_DEFAULT_STRIDE` on input type | PQ-5 = Slice 2. Hypothesis H-A1 belongs to the Intel QSV diagnosis. |
| `crates/sm-infra/src/encode/bgra_to_nv12.rs` (NV12 layout, stride padding, `Nv12::new`) | Untouched. Always-compiled file; any change here would risk the SW path and the unit-test invariants. Slice 2 if needed. |
| Public domain API: `VideoEncoder`, `EncoderConfig`, `EncodedPacket`, `EncoderError` | **FROZEN** per `sdd-init/screen-mirror-app`. Hexagonal invariant `no_platform_deps.rs` still applies. |
| `default = ["hw-encoder"]` flip in `Cargo.toml` | `hw-encoder-default-on-flip` (gated on both slices + soak). |
| New tests (T-NEW-3 = `mft_handles_have_output_before_need_input`) | PQ-3 = defer. Existing 18 tests provide the regression surface. |
| Feature flag taxonomy | No new flag. `hw-encoder` stays as the only platform-encoder gate. |
| Host A diagnostics, runs, or hypotheses | Slice 2 owns Host A end-to-end, including its own Phase 0 prep commit. |

---

## Approach (high level)

The exploration (#147) established that NVENC's `MF_E_INVALIDMEDIATYPE` could
plausibly be triggered by any of three shapes (H-B1 `MF_MT_MPEG2_PROFILE`
rejection, H-B2 `MF_MT_AVG_BITRATE` rejection, H-B3 strict
`GetOutputAvailableType`-based negotiation), with H-B1 holding the highest
prior. We do **not** guess in design — we let the Phase 0 transcripts pick.
This proposal commits to the *chain*, not the exact line.

### Chain

```
Phase 0 prep commit (instrumentation)
        │ user runs Host B smoke under hw-encoder feature
        │ user captures transcripts (per-attribute SetOutputType outcomes)
        ▼
sdd-spec  ─┐
           ├─ both consume Phase 0 transcripts as load-bearing input
sdd-design ┘   (design picks the exact fix shape: H-B1 / H-B2 / H-B3)
        ▼
sdd-tasks (RED/GREEN per capability under Strict TDD)
        ▼
sdd-apply (chained PRs honouring auto-chain + 400-line budget)
        ▼
sdd-verify (7 gates GREEN + Host B 18/18 smoke evidence)
        ▼
sdd-archive (carries forward Slice 2 explicitly)
```

### Phase 0 — already scoped, NOT_STARTED

The Phase 0 *plan* is fixed by the exploration; only the *runs* remain. The
plan: instrument `setup_mft` with a binary-search attribute walk so each
candidate attribute (PROFILE, AVG_BITRATE, INTERLACE_MODE, PAR, FRAME_RATE,
FRAME_SIZE) is added incrementally and `SetOutputType` is retried, recording
which prefix passes and which subsequent attribute first triggers
`MF_E_INVALIDMEDIATYPE`. The trace also records the result of a single
`GetOutputAvailableType(0, 0)` probe (success → enables H-B3 fallback;
`E_NOTIMPL` → H-B3 not viable, design must remove offending attributes).

The instrumentation is **not** the production fix. It is a one-commit prep
step (D-1 below) that the user runs on Host B and whose transcripts feed
sdd-design. The instrumentation commit is reverted (or the trace is removed)
before the production fix commit lands so the diff that ships is minimal.

### Expected fix shapes (design will pick exactly one)

These are the candidates ranked by exploration priors. Design selects the
shape that the Phase 0 transcripts justify; verify ratifies it on Host B.

1. **H-B1 — Remove `MF_MT_MPEG2_PROFILE` from output type, set profile via
   `ICodecAPI` later.** Highest prior. NVENC's H.264 MFT may refuse profile in
   the output type, expecting the caller to drive profile through
   `CODECAPI_AVEncH264VLevel` / `CODECAPI_AVEncH264VProfile` post-start. We
   already have `apply_pending_codec_settings` running before each first
   `ProcessInput` — we extend it with a one-shot profile-setter executed once
   per session.

2. **H-B2 — Remove `MF_MT_AVG_BITRATE` from output type, drive bitrate
   exclusively via `ICodecAPI` (`CODECAPI_AVEncCommonMeanBitRate`).** That key
   is *already* the rate-change path (`apply_pending_codec_settings`,
   line 740); the only addition is a one-shot initial-bitrate setter so we do
   not depend on the output-type attribute.

3. **H-B3 — `GetOutputAvailableType(0, 0)`-guided construction.** Clone the
   first available type, overlay only `MF_MT_FRAME_SIZE` and `MF_MT_FRAME_RATE`
   on top, then call `SetOutputType`. Vendor-agnostic; eliminates attribute
   guessing entirely. Viable only if NVENC's MFT does not return `E_NOTIMPL`
   from `GetOutputAvailableType` — Phase 0 confirms this.

The three are *not* mutually exclusive — H-B1 + H-B2 stacked is plausible.
Design MAY adopt the union if Phase 0 evidence demands it.

### PQ-4 conditional — enumeration fallback

If the chained-PR slice budget allows after the type fix lands (which depends
on whether H-B3 expands the diff), add a small loop in `init_mft_sync` so
`setup_mft` failure on `pactivates[i]` falls through to `pactivates[i+1]`
before erroring. ~20 LOC, no new public API, no test change required (the
existing 18 smoke tests cover the activated MFT regardless of which index it
came from). If budget is tight, this drops out of Slice 1 and migrates to its
own follow-up change. Design records this decision once Phase 0 transcripts
fix the diff size of the primary fix.

---

## Decisions

These are the design-relevant decisions committed by this proposal. Design
treats them as load-bearing constraints; Slice 2 may revisit them only with
explicit re-proposal.

| ID | Decision | Rationale | Out-of-scope alternative |
|----|----------|-----------|-----------|
| **D-1** | Phase 0 instrumentation lives in a **separate prep commit**, gated by `cfg(debug_assertions)` AND the `hw-encoder` feature. Reverted (or its trace block deleted) before the production-fix commit lands. | Keeps the production diff minimal. `cfg(debug_assertions)` ensures release builds never carry the attribute-walk overhead. The `hw-encoder` gate keeps the no-default-features build clean (gate 6). | A long-lived `cfg(feature = "encoder-trace")` flag — rejected: adds permanent feature taxonomy for a one-off diagnostic. |
| **D-2** | The fix shape preference order is **H-B1 → H-B2 → H-B1+H-B2 → H-B3**. Design picks the *narrowest* shape Phase 0 transcripts justify. | Smallest diff that fixes the rejection wins. H-B3 is a wider rewrite; only used if H-B1/H-B2 cannot be supported by the transcripts. | "Always go H-B3" — rejected: larger blast radius, depends on `GetOutputAvailableType` not being `E_NOTIMPL`. |
| **D-3** | If H-B1 or H-B2 lands, the corresponding attribute is set via `ICodecAPI::SetValue` **after** `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING`/`START_OF_STREAM` (already issued in `setup_mft` lines 627–634) and **before** the first `ProcessInput` — i.e. inside the `pump_loop` NeedInput arm via the existing `apply_pending_codec_settings` path (line 908), extended with a `session_init_pending` one-shot. | That is the attested-safe ordering for ICodecAPI calls on hardware MFTs (predecessor design §7 DD10/DD11) and reuses an audited code path. Avoids opening a second ICodecAPI call site. | Setting in `setup_mft` before streaming begins — rejected: contradicts predecessor's DD10/DD11 ordering rules. |
| **D-4** | Delivery uses **chained PRs** under the cached `auto-chain` strategy; each chained slice respects the 400-line budget. Phase 0 prep is its own commit (often its own PR). | Cached delivery strategy is `auto-chain`. The exact line count is a Phase 0 / design output; we honour the orchestrator's budget guard. | Single PR with `size:exception` — rejected: `auto-chain` was the chosen strategy; exception requires a new orchestrator gate. |
| **D-5** | `default = []` in `crates/sm-infra/Cargo.toml` is **unchanged**. The HW path stays opt-in via `--features hw-encoder` for this slice. | Default-on flip is a separate, gated change (`hw-encoder-default-on-flip`); flipping it here would conflate the fix with a policy change and break the no-default-features quality gate's value as a regression sentinel. | Flip default-on as a "free win" once Slice 1 PASSes — rejected: gated on Slice 2 + soak. |
| **D-6** | Feature flag taxonomy is **unchanged**: `hw-encoder` remains the single gate. No new flag is introduced for the fix or the Phase 0 trace. | Adding flags is a one-way door; the fix is not vendor-conditional in code (we converge attribute construction to a shape that works on both vendors). Phase 0 trace is gated by `cfg(debug_assertions)` per D-1, not by a feature. | New `nvenc-compat` flag — rejected: encoders are negotiated at runtime; gating in Cargo features rebuilds the whole crate per host. |
| **D-7** | Profile / bitrate **timing** when set via `ICodecAPI`: post-`Start`, pre-first-`ProcessInput`. A new `session_init_pending: AtomicBool` (or equivalent) on `MftEncoderShared` is set true at thread spawn and consumed once inside `apply_pending_codec_settings`. | Predecessor's `keyframe_pending` and `pending_bitrate` use the same pattern; we extend, not invent. AcqRel ordering matches DD10/DD11. | Setting in `setup_mft` synchronously — rejected: violates DD10/DD11 ordering. Setting eagerly in `pump_loop` start — rejected: risks setting before MFT is fully primed. |
| **D-8** | PQ-4 enumeration-fallback loop is **conditional** on Slice 1 line budget. If it does not fit, it is deferred to a dedicated follow-up change `hw-encoder-mft-enumeration-fallback`, not Slice 2. Slice 2 owns Manifestation A only. | Keeps slices single-responsibility. The fallback is a robustness improvement, not a vendor-specific fix; bundling it with Intel QSV diagnosis would muddy that slice's verify gate. | Always include in Slice 1 — rejected: line-budget risk if H-B3 is the chosen shape. Defer to Slice 2 — rejected: orthogonal concerns. |
| **D-9** | Test surface is **unchanged**: the 18 existing `#[ignore]` smoke tests in `crates/sm-infra/tests/windows_mft_encode.rs`. T-NEW-3 stays deferred. New unit tests are permitted (and expected) to cover any pure-Rust helper extracted while shaping the fix (e.g. an attribute-set builder), but no new smoke tests are added. | Existing tests already cover all 11 NVENC failures. Adding T-NEW-3 risks scope creep and (per exploration) requires a real vendor MFT to exercise. PQ-3 = defer. | Add T-NEW-3 in this slice — rejected: PQ-3 = defer. Add new smoke tests speculatively — rejected: scope. |

---

## Out-of-scope deferrals (carry-forward)

These items are explicitly **not** addressed by Slice 1 and remain owned by
named successor changes:

- **Slice 2** (`hw-encoder-mft-vendor-compat-slice2-intel-qsv`) — Manifestation
  A (Intel QSV `0xC0000005` AV in `ProcessOutput`), Phase 0 on Host A,
  `MF_MT_DEFAULT_STRIDE`, `MF_MT_VIDEO_NOMINAL_RANGE`, `Nv12` stride padding
  (H-A1, H-A2), and any SEH wrapper if the AV proves to be a genuine driver
  bug.
- **`hw-encoder-default-on-flip`** — `default = ["hw-encoder"]` flip; gated on
  Slice 1 + Slice 2 archived clean + 18/18 on both vendors + 24h soak.
- **PQ-4 fallback as a standalone change** (`hw-encoder-mft-enumeration-fallback`)
  if D-8's budget condition forces it out.
- **T-NEW-3** (`mft_handles_have_output_before_need_input`) — long-tail
  regression coverage; deferred per PQ-3.
- **Predecessor follow-ups** still open and untouched here: `insta` dev-dep
  cleanup in `sm-domain`, Windows runner clippy CI job
  (`ci-windows-clippy`), domain-level `max_fps = Some(0)` rejection unit
  test (R5.4 partial). All inherited from `screen-capture-windows` /
  `hw-encoder-mft-rework`; not part of Slice 1.

---

## Risks

Predecessor-style risk register. Items #1–#3 are exploration #147 risks
narrowed to Slice 1; items #4–#7 are Slice-1-specific.

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| 1 | Phase 0 transcripts inconclusive (e.g. NVENC rejects multiple attribute combinations non-deterministically). | Low | High — design blocked. | Phase 0 plan already includes both binary-search and a `GetOutputAvailableType` probe. If both produce noise, fall back to H-B3 (clone-and-overlay) which is vendor-agnostic. |
| 2 | `GetOutputAvailableType(0, 0)` returns `E_NOTIMPL` on NVENC. | Medium | Medium — eliminates H-B3 path. | Probe captured in Phase 0 transcript. If `E_NOTIMPL`, design selects H-B1/H-B2 path. Codified as D-2's preference order. |
| 3 | Production fix on Host B unintentionally regresses Host A behaviour. | Low | High — Slice 2 starts with worse baseline. | Slice 1 changes only attributes that are known accepted by Intel QSV (Intel's failure is in `ProcessOutput`, not `SetOutputType` — its setup PASSes today). Verify gate explicitly does not require Host A re-runs but does require that `cargo nextest run --workspace` on the developer's host (no HW gate active) stays green. Slice 2 verify will detect any regression. |
| 4 | `cfg(debug_assertions)` Phase 0 trace bloats the debug build's log noise to the point that the user cannot capture clean transcripts. | Low | Medium — re-run cycle. | Trace is scoped to `setup_mft` only and emits at `tracing::trace!` level — user controls via `RUST_LOG=sm_infra::encode=trace`. Predecessor uses the same pattern. |
| 5 | `ICodecAPI::SetValue(CODECAPI_AVEncH264VProfile)` rejected on Host B (driver returns HRESULT). | Medium | Medium — H-B1 path partially blocked. | Predecessor's bitrate-rejection policy (line 742, `tracing::warn!` only, never crash) extends to profile rejection: log warn, continue. Smoke tests ratify the resulting stream (Annex-B start codes, IDR emission); if the encoder still produces a valid H.264 stream without an explicit profile, profile rejection is benign. If smoke tests fail because of profile, design escalates to H-B2 or H-B3. |
| 6 | Chained PR slice budget breached (estimated diff > 400 lines once the chosen fix shape is known). | Medium | Low (delivery) / Medium (review velocity) | `auto-chain` strategy already cached; orchestrator's review-workload guard splits the work into sub-slices automatically. Phase 0 prep + production fix + (conditional) PQ-4 fallback are natural commit boundaries. |
| 7 | Public API drift (someone "helpfully" extends `EncoderConfig` to expose profile/bitrate-init knobs while shaping the fix). | Low | High — breaks frozen-API invariant. | Hexagonal invariant test `no_platform_deps.rs` plus `sdd-verify`'s explicit re-check of `crates/sm-domain/src/encode.rs` against the predecessor's archived shape. Design rejects any `EncoderConfig` change. |

---

## Quality gates and acceptance criteria

### 7 quality gates — all must be GREEN before archive

1. `cargo check --workspace`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo fmt --check --all` (includes `examples/*.rs` per project convention)
4. `cargo nextest run --workspace`
5. `cargo deny check`
6. `cargo check --no-default-features` (no-default-features build stays buildable)
7. `cargo check --features hw-encoder` (gated build stays buildable)

### Smoke evidence (Slice 1 specific)

- **Host B (JDNHS, NVIDIA NVENC), `--features hw-encoder`**: 18/18
  `#[ignore]` smoke tests in `crates/sm-infra/tests/windows_mft_encode.rs`
  PASS. Transcript captured and persisted as a verify-report attachment.
  Specifically, the 11 currently-failing NVENC tests (`SetOutputType` HRESULT
  `0xC00D6D76`) MUST move to PASS without any test-side workaround. The 7
  currently-passing tests MUST remain PASS.
- **Developer host (no HW gate)**: `cargo nextest run --workspace` (no
  `--features hw-encoder`) MUST remain green. The HW smoke tests stay
  `#[ignore]` and are not exercised in the default test run.
- **Host A (Usuario\Desktop, Intel QSV)**: NOT a Slice 1 verify gate.
  Manifestation A is owned by Slice 2. Verify will note Host A's pre-existing
  AV signature as "carry-forward, owned by Slice 2", same shape as the
  predecessor's APPROVED_WITH_CARRY_FORWARD outcome.

### Acceptance contract

Slice 1 archives APPROVED only when (a) all 7 gates are GREEN, (b) Host B
smoke transcript shows 18/18 PASS with `--features hw-encoder`, and (c) the
archive-report explicitly lists Slice 2 + PQ-4 follow-up + default-on flip as
named carry-forward items.

---

## Result Contract

- **status**: done
- **executive_summary**: Slice 1 fixes only Manifestation B of Bug 1 (NVENC
  `SetOutputType` → `MF_E_INVALIDMEDIATYPE`) by changing the attribute set on
  the output `IMFMediaType` in `setup_mft` (lines 531–580). The exact shape
  (H-B1 remove `MF_MT_MPEG2_PROFILE`, H-B2 remove `MF_MT_AVG_BITRATE`, or H-B3
  `GetOutputAvailableType`-guided) is a design-time decision anchored on Phase
  0 transcripts the user captures on Host B before design lands. Public
  `VideoEncoder` / `EncoderConfig` API stays frozen, `default = []` stays
  unchanged, no new feature flags, 18 existing smoke tests are the regression
  surface, and PQ-4 enumeration-fallback is conditional on the chained-PR
  budget. Verify gate: 7 quality gates GREEN + Host B 18/18 PASS smoke
  evidence; Manifestation A remains owned by Slice 2 as named carry-forward.
- **artifacts**:
  - engram `topic_key: sdd/hw-encoder-mft-vendor-compat-slice1-nvenc/proposal`
  - openspec `openspec/changes/hw-encoder-mft-vendor-compat-slice1-nvenc/proposal.md`
- **next_recommended**: `sdd-spec` and `sdd-design` (orchestrator launches both
  in parallel after this returns; both consume Phase 0 transcripts as
  load-bearing input — design picks the exact fix shape).
- **risks**: Phase 0 transcripts inconclusive; `GetOutputAvailableType` →
  `E_NOTIMPL`; ICodecAPI profile rejection on Host B; chained-PR budget
  breach forcing PQ-4 deferral; debug-build trace noise on Host B; accidental
  Host A regression Slice 2 inherits; public API drift while shaping the
  fix.
- **skill_resolution**: injected
- **phase_0_status**: **NOT_STARTED**.
  - **Instrumentation prep commit (proposed name)**:
    `feat(infra): add hw-encoder setup_mft attribute-walk trace (debug-only)`
    — adds a `cfg(debug_assertions)`-and-`feature = "hw-encoder"`-gated trace
    block in `crates/sm-infra/src/encode/windows_mft.rs::setup_mft` that
    (a) calls `mft.GetOutputAvailableType(0, 0)` and logs success/HRESULT,
    (b) re-attempts `SetOutputType` with attribute prefixes
    `[MAJOR + SUBTYPE]`, `+ FRAME_SIZE`, `+ FRAME_RATE`, `+ PAR`,
    `+ INTERLACE_MODE`, `+ AVG_BITRATE`, `+ MPEG2_PROFILE`, logging the
    HRESULT for each, before performing the original (currently-failing)
    `SetOutputType` call. To be reverted (or its trace block removed) before
    the production-fix commit lands per Decision D-1.
  - **Host B run command (user executes before design)**:
    ```
    $env:RUST_LOG = "sm_infra::encode=trace"
    cargo nextest run --workspace --features hw-encoder --run-ignored=ignored-only --no-fail-fast 2>&1 | Tee-Object -FilePath phase0-host-b-nvenc.log
    ```
    Persist `phase0-host-b-nvenc.log` and attach (or paste sanitised excerpt)
    when launching `sdd-design`. Design treats the per-attribute-prefix
    HRESULT table as load-bearing input.
