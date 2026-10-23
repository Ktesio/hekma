# Epic 19 Context: Wire Governance for Hosts

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. -->

> **Numbering note:** Epic 19 was renumbered from Epic 17 on 2026-10-19 — remote main's autonomous Fibery-backlog tranches consumed epic numbers 15/16 while this approved 2026-09-30 correct-course output sat uncommitted locally (`_bmad-output/implementation-artifacts/sprint-status.yaml:176-180`). The sprint change proposal's "Epic 17" is today's **Epic 19**; its "Story 17.x" map to 19.x.

## Goal

The Host enforces money at its own external choke point and wants Hekma's rules there, provably. It also wants Hekma's cooperative meter to fence (refuse further model calls on breach), to attribute usage to its own units of work (Tasks), and to reconcile against the Host's confirmed figures. This epic realizes FR-43, FR-44 and FR-45. This is money code: every story gets the AI-55 two-pass review. Sources: `_bmad-output/planning-artifacts/epics.md:1201-1245`, `_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md` (APPROVED by Islam 2026-09-30, Batch mode).

## Stories

- Story 19.1: Governance conformance vectors — FOUNDATION, **runs first overall** (an external Host consumer waits on it; proposal §5, sprint-status epic-19 note)
- Story 19.2: The wire fence — a `block` Breach Action (D4, AD-7 amendment)
- Story 19.3: Task scope — Host units of work (AD-7 amendment)
- Story 19.4: Per-call records for Hosts
- Story 19.5: Reconciliation ingestion — the `reconciled` label's first producer (FR-45, AD-8)

## Requirements & Constraints

From FR-43..FR-45 (`_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md` §4.12) and D4/D5 (proposal §4.1):

- Governance semantics become a **portable, versioned artifact**: decision vectors (ledger state and ceilings in, decisions out) covering the `≥` threshold, per-scope latches and re-arm, token ceilings before dollar ceilings, integer micro-dollar arithmetic, round-half-up, saturation, and no retroactive repricing — generated from and verified against the engine's own evaluators, shipped as versioned files a non-Rust Host pins by version and checksum (D5, FR-44).
- The fence is a **Breach Action** (`block`): on breach the pipeline flips a lock-free per-instance flag; `forward()` refuses further model calls with a typed, non-retryable response (not 429 — SDKs retry it), until the ceiling is raised or reset. The breach event and log record the fence like any action. The Host's own choke point stays the enforcement of record; the fence is cooperative (NFR-6 — Hekma is not a sandbox) (D4).
- **Tasks** are Host-defined units of work inside a Run: begin/end through the facade and the Control API; ledger rows and events carry the Task; per-Task budgets and latches sit beside per-Run and cumulative ones; raising a ceiling re-arms its latch within the Run; usage outside any Task records as untasked (FR-43, AD-7 amendment).
- **Per-call records**: for each observed completion — status class, latency, model when present, tokens, Task, sub-second timestamp; NEVER request or response content (the sentinel no-leak matrix extends); readable through the facade and the Control API.
- **Reconciliation**: a Host submits confirmed usage for a Run or Task; reconciled figures sit BESIDE estimated rows and never overwrite them; drift is reported; every surface labels which figure it shows (FR-45, AD-8 — `EstimateLabel::Reconciled` is the designed, currently producer-less seam).

## Technical Decisions

- **AD-7 (2026-09-30 amendment):** a `block` Breach Action fences further model calls (lock-free flag checked in `forward()`, no lock taken in the request path — AD-18's premise holds); a Task is a budget scope beside per-Run and cumulative; raising a ceiling re-arms its latch within the Run; still true: no code path other than the pipeline triggers a Breach Action.
- **AD-8 (2026-09-30):** reconciliation ingestion is the `reconciled` label's first producer; reconciled figures never overwrite estimated rows.
- **Money-code review rule:** every story in this epic gets the AI-55 two-pass review (epics.md epic preamble).
- **Existing seams the epic builds on** (survey 2026-10-23): `BreachAction` = pause/stop/warn (`domain/budget.rs:86`); the enforcement choke point `enforce_budget` → `apply_breach` with the per-Run `(dimension, scope)` latch (`supervisor/usage.rs:961/1089`); the loopback `forward()` with its honest static-502 discipline (`metering/listener.rs:521-626`); `EstimateLabel::Reconciled` forward seam (`domain/cost.rs:365-378`); store schema v10 with the v9 nullable-`run_id` additive-column precedent; the 17-4 std-only SHA-256 module; the Control API v0 route pattern (`hekma-host/src/control/`).
- **Execution order:** 19.1 FIRST overall (before Epic 18), per the ratified handoff; then 19.2→19.5 after Epic 18's stack (this session's plan; the only hard order is 19.1 before the rest of 19, since the vectors are the foundation the Host guard pins).

## UX & Interaction Patterns

- Terminal product; new verbs surface on `kt agent` (CLI-first) and the Control API where the story names them.
- Every refusal/diagnostic names the reason and the remediation (raise or reset the ceiling; end the Task; the label shown), per the surfaced-not-silent rule (AI-18).
- No new exit-code numbers (DC-4 pattern): new diagnostics join existing classes.

## Cross-Story Dependencies

- 19.1 gates 19.2..19.5 (the shared `governance::decide` core + the vectors discipline extend with each story: `block` adds a decision-consumer + vectors cases; Task scope adds a scope + the ceiling-raise re-arm vectors; reconciliation adds the label producer).
- hekma-host's drain.rs module doc reserves the AD-15 drain composite for Epic 18 — unrelated to this epic's verbs, but the docs must keep the two "drain" senses distinct when 19.3's Task verbs land beside them.
- House gates on every story (AGENTS.md): CLI-first, ≥95% coverage, docs in the same change, cross-platform, graceful degradation, Fibery mirrored in the same change.
