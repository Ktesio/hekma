# Epic 11 Context: Technical Debt & Process Cleanup

## 1. Epic Goal & Origin

Post-release hardening sprint: **65 open retro action items** accumulated across epics 1–7,
organized into **7 thematic stories** (11-1..11-7), each sized for one focused session.
Most items are diagnostic polish, test-gate additions, or documentation fixes — individually
small, collectively a maintainability tax. The epic-6 retro proved unowned deferrals have a
zero completion rate across three+ epics, which motivated this epic.

**Origin:** opened 2026-09-10 by sprint change proposal
`_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-10-cleanup.md`
(**Islam-approved**, batch mode per Islam's standing autonomous instruction).
The change proposal contains the per-item fix tables reproduced below and is the
authoritative source for the ratified execution order.

**Scope (from proposal §4, Impact Analysis):** no PRD conflicts — every item hardens or
polishes an already-shipped capability. No structural architecture changes — engine fixes are
local (atomic writes, error ordering, poll-error surfacing); the sink from 10-2 already
established the diagnostic routing pattern. Boundary gate unchanged (all fixes within existing
crates). No dependencies added.

## 2. Stories & Ratified Execution Order

From the change proposal §5 (Direct Adjustment):

**11-1 → 11-3 → 11-2 → 11-5 → 11-4 → 11-7 → 11-6**

(engine → memory → config → CI/docs-gate → docs → process → decisions-last)

## 3. Per-Story Detail

### Story 11-1: Engine robustness batch — MED

Engine correctness/polish from epics 1–3. Proposal header says "12 items"; the table
enumerates 11 (epics.md also states 11): AI-4, AI-7, AI-8, AI-9, AI-12, AI-13, AI-14,
AI-15, AI-16, AI-41, AI-44.

| Item | Fix |
|---|---|
| AI-4 | RegistryError::Io names the offending relative path, not the env-var placeholder |
| AI-7 | resume() on an Unsupported-pause instance: fail-fast → honest diagnostic with remediation (don't strand) |
| AI-8 | guaranteed pause without in-memory handle: record `pause-unavailable` cause, don't silently return Ok |
| AI-9 | pause(): persist state=paused BEFORE signaling SIGSTOP (match stop()'s persist-first order) |
| AI-12 | poll_once: persistent poll errors surface as crash-detection input, not silent `None` |
| AI-13 | adopted process exit: record the exit-code-unavailable fact in the crash cause |
| AI-14 | fingerprint-read failure at spawn: don't write start_time=0 sentinel |
| AI-15 | `show --json`: single-instance lookup instead of O(N) fleet scan |
| AI-16 | `fleet_entry_for`: batch-read spawn records while holding the lock (kill the N+1) |
| AI-41 | ledger INSERT failure: retry/diagnose, don't silently drop the usage event + advance cursor |
| AI-44 | adopt_orphans: re-evaluate budgets after adoption (the crash-gap) |

**Owner:** first engine-touching story.
**Acceptance:** each fix ships with a test; battery green.

### Story 11-2: Config & secrets batch — MED

6 items: AI-24, AI-27, AI-28, AI-33, AI-39, AI-26.

| Item | Fix |
|---|---|
| AI-24 | set_config: temp-file + rename (atomic write) |
| AI-27 | env target shadowing: warn when a mapped env target overwrites a base-launch env var |
| AI-28 | config-file render + set_config: same atomic write pattern |
| AI-33/39 | secret:NAME → flag target: warn at config-set time (steer to env/file); runtime enforcement via the existing audit |
| AI-26 | `config set` leading-dash values: accept via `--` separator (clap standard) |

**Owner:** first config/IO-touching story.

### Story 11-3: Memory robustness batch — MED

4 items: epic-5-retro-item-1, epic-5-retro-item-3, epic-5-retro-item-4, AI-46.

| Item | Fix |
|---|---|
| epic-5-retro-item-1 | memory.dir strip gap: strip in the invocation-override branch too + test (A1) |
| epic-5-retro-item-3 | migration crash-atomicity: BEGIN IMMEDIATE or idempotent DDL + comment fix (B3) |
| epic-5-retro-item-4 | registry reverse-conflict test + deferred-work direction fix (A5) |
| AI-46 | engine-observed adoption: clear the stale base_url on adoption (the stranded-listener fix) |

**Owner:** first engine/memory-touching story.
**Overlap:** AI-46 also appears in 11-6 — implement it ONCE here in 11-3; 11-6 references it.

### Story 11-4: Docs, docs-gate & process batch — LOW

9 items: AI-18, AI-19, AI-21, AI-22, AI-25, AI-30, AI-43, AI-45, AI-34.

| Item | Fix |
|---|---|
| AI-18 | Document the honest-state/surfaced-not-silent pattern in AGENTS.md |
| AI-19 | Adopt the review rule: deferred-as-unreachable must be proven across every surface |
| AI-21 | Codify the away-mode/interruption recovery drill |
| AI-22 | Contributor docs: MSRV-pin vs CI-stable split + mise/RUSTUP_TOOLCHAIN note |
| AI-25 | `config get` table-prefix: name the child leaves that exist under it |
| AI-30 | config_json docstring: fix accessor names |
| AI-43 | Compact human `list`: surface the Metering Source (AC-C literal wording fix) |
| AI-45 | Fleet list dollar cell: same truncation-label fix as the Budget cell |
| AI-34 | Fix rustdoc --document-private-items failure on kt/src/main.rs |

**Owner:** tech-writer / docs-touching story.
**Overlap:** AI-19 also appears in 11-7 — adopt the rule once (here or in 11-7, whichever
runs first per the ratified order — 11-4 precedes 11-7), and reference it from the other story.

### Story 11-5: Cross-platform & CI batch — MED

6 items: AI-29, AI-35, AI-38, AI-37, AI-54, epic-7-retro-item-2, AI-71.

| Item | Fix |
|---|---|
| AI-29 | Unix-only survival tests: Windows equivalents or documented per-OS honesty |
| AI-35/38 | _live test cross-OS robustness (fake_agent .exe, readiness handshake) |
| AI-37 | Run the 3-OS matrix earlier (per-story or nightly on feature branches) |
| AI-54 | Codify full cross-OS + coverage CI on every feature-branch commit |
| epic-7-retro-item-2 | Live-docs probe: scheduled check that docs.ktesio.dev serves the newest page |
| AI-71 | Restore the workspace coverage gate to green (94.94% → ≥95%) |

**Owner:** first CI/infra-touching story.

### Story 11-6: Supervision & operations decisions — NEEDS ISLAM

5 items: AI-20, AI-46, AI-47, AI-48, AI-36. Per proposal §6: this story requires Islam's
product decisions **before its implementation items can be commissioned** — which is why it
runs LAST in the ratified order.

| Item | Decision needed |
|---|---|
| AI-20 | PRODUCT DECISION: daemon/detach for durable cross-CLI supervision — build it or document the per-invocation model as intentional |
| AI-46 | (overlaps 11-3) engine-observed adoption stranding: clear stale base_url on adoption |
| AI-47 | HTTPS upstream support for engine-observed metering (v1 is HTTP-only) |
| AI-48 | Latent tracing exposure: confirmed safe (no subscriber in tree); document as a dependency-audit checkpoint |
| AI-36 | Dependabot vulnerabilities on main: review/triage |

**Owner:** Islam (product decisions) + dev (implementation where commissioned).

**Decided vs. still needs a call:**
- **Already decided / no new call needed:** AI-46 — the fix is fully specified and ratified
  (clear stale base_url on adoption); it is implemented in Story 11-3, so 11-6 only tracks its
  closure. AI-48 — safety is confirmed (no subscriber in tree); the remaining work is
  documentation-only (record it as a dependency-audit checkpoint).
- **Needs Islam's decision:** AI-20 — build daemon/detach vs. document the per-invocation
  model as intentional. AI-47 — commission HTTPS upstream support or defer (v1 stays
  HTTP-only). AI-36 — outcomes of the Dependabot review/triage on main (which vulnerabilities
  to fix now vs. accept).

### Story 11-7: Orchestration & review process batch — LOW

7 items: AI-17, AI-19, AI-55, AI-56, AI-57, AI-58, epic-6-retro-item-9.

| Item | Fix |
|---|---|
| AI-17 | test_automation.py: tighten the assertIn pins + lazy-install placement |
| AI-19 | Adopt the review rule: deferred-as-unreachable proven across every surface |
| AI-55 | Extend the two-pass review default to public release-surface changes |
| AI-56 | Sync sprint-status 8-1 entry to the ratified rewrite |
| AI-57 | Codify the resumable-subagent recovery flow as a standing playbook pattern |
| AI-58 | Document the draft-then-ratify pattern for architecture-decision corrections |
| epic-6-retro-item-9 | Bind or retire: the carry-forward items that survived #168's ratification |

**Owner:** orchestrator / process.
**Overlap:** AI-19 duplicated with 11-4 (see 11-4 note).

## 4. Already Resolved Before the Epic — Do NOT Redo

From the change proposal §2 ("Already Resolved — mark done"):

- **AI-3** (semver cache key): `b590dc8` / PR #174 — version-keyed cache + verify-or-reinstall.
- **AI-6** (contract_version strict parse): `dc29e86` / PR #157 — `STRICT_SEMVER_REQUIREMENT` const.
- **AI-64** (mutation pass mandate): applied in 6-6; standing rule; formally closed on #168.
- **#168** (bind-or-retire AI-63/65/66/69): closed with 4 ratified dispositions.
- **epic-6-retro-item-8** (tracking hygiene): lands in the cleanup epic's own chore commits
  (not a story item; do not re-scope it into a story).

## 5. Cross-Cutting Constraints (Durable Engineering Gates)

These apply to every story in this epic:

- **CLI-first** — every feature is reachable via the `kt` CLI; output to stdout, diagnostics
  to stderr; all commands support `--help`/`--version`.
- **Test coverage MUST stay ≥ 95%** — enforced in CI via
  `cargo tarpaulin --workspace --fail-under 95`. New code ships with tests. (AI-71 in 11-5
  exists precisely because the gate had drifted to 94.94%.)
- **Documentation currency** — update `docs/` and `README.md` in the same change as the code
  they describe; stale docs are treated as a bug. `_bmad-output/` and `_bmad/` are tracked in
  git — keep them current in the same change that makes them stale.
- **Cross-platform** — Linux, macOS, and Windows; use path-agnostic std APIs.
- **Graceful degradation** — partial failures report a clear reason and a remediation, and do
  not abort the whole operation. (This gate is the thematic backbone of many Epic 11 fixes:
  honest diagnostics over silent Ok/None.)

## 6. Handoff Gates (run before handing off ANY story's code changes)

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace --all-targets`
- `python3 scripts/check_docs.py`

Proposal §7 additionally requires the full battery green after every story, including
`test_automation` and tarpaulin.

## 7. Success Criteria (proposal §7)

- All 65 items marked done or formally retired with rationale.
- Battery green after every story (fmt/clippy/tests/tarpaulin/check_docs/test_automation).
- Zero new open action items created by the cleanup itself.
- Per-epic PR: one PR covering all of Epic 11, per the standing flow.

## 8. Delivery Notes for the Developer Agent

- Implementation handoff is **Scope: Moderate** — each story is directly implementable by the
  Developer agent with bmad-build and 3-lens review per story (proposal §6).
- Do not start 11-6 implementation before Islam's AI-20/AI-47/AI-36 decisions land; 11-6 is
  intentionally last in the ratified order.
- AI-46: implement once in 11-3; reference from 11-6. AI-19: adopt once (11-4 precedes 11-7);
  reference from 11-7.
- Sprint tracking keys on the exact item IDs quoted above (AI-N, epic-N-retro-item-N, #NNN);
  sprint-status.yaml tracks them — use IDs verbatim in status updates and commit messages.
