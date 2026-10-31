# Epic 19 Context: Wire Governance for Hosts

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. Refreshed 2026-10-30 against the control-plane pivot and the readiness gate. -->

> **Numbering note:** Epic 19 was renumbered from Epic 17 on 2026-10-19 — remote main's autonomous Fibery-backlog tranches consumed epic numbers 15/16 while this approved 2026-09-30 correct-course output sat uncommitted locally. The 2026-09-30 sprint change proposal's "Epic 17" is today's **Epic 19**; its "Story 17.x" map to 19.x.

> **Re-scoped 2026-10-30** by `sprint-change-proposal-2026-10-30-control-plane-pivot.md` and the readiness gate (`_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md`). Host Task scope is gone: Host units of work are **child Runs** (Story 21.2). Story 19.3 is removed.

## Goal

The Host enforces money at its own external choke point and wants Hekma's rules there, provably. It also wants Hekma's cooperative meter to fence (refuse further model calls on breach), to see per-call records, and to reconcile against the Host's confirmed figures. Units of work are child Runs (Epic 21). This epic realizes FR-43 (in part), FR-44 and FR-45. This is money code: every story gets the AI-55 two-pass review. Sources: `_bmad-output/planning-artifacts/epics.md` (Epic 19), the two proposals above, PRD §4.12 and §4.14.

## Stories

- Story 19.1: Governance conformance vectors — **done** (PR #334).
- Story 19.2: The wire fence — a `block` Breach Action. Buildable now on today's per-instance listener; lands before 21.3.
- ~~Story 19.3: Task scope~~ — removed; child Runs in 21.2, latch re-arm on a raised ceiling in 21.6.
- Story 19.4: Per-call records for Hosts — the Host read surface over the shared proxy's records; runs after 22.1 and 23.1.
- Story 19.5: Reconciliation ingestion — after 19.4; a Run, including a child Run.

## Requirements & Constraints

- **`block` (19.2):** never a default Breach Action; once configured it always refuses, whatever the Scent-Check Mode (PRD FR-43 amendment). On breach the pipeline flips a lock-free per-instance flag; `forward()` refuses further model calls with a typed, non-retryable response (not 429 — SDKs retry it) until the Run ends; raising a ceiling un-fences a live Run once 21.6 lands. The breach event and log record the fence like any action. The Host's choke point stays the enforcement of record; the fence is cooperative (NFR-6).
  - The spec records the refusal response as the first slice of `proxy-wire v1`: a status code common SDKs do not retry, a JSON error body with a stable type and code naming dimension and scope, never content. 23.1 documents it unchanged and carries `block` into the shared snapshot; 23.4 reuses it for `reject`.
  - The flag is rebuilt from the durable breach record on engine restart and adoption, as the latch is.
  - `block` is refused at configuration on an instance with no listener (self-reported metering only).
  - The new `BreachAction` variant is a semver-visible engine change.
  - After 21.1 the store writer, not the Supervisor, sets the flag (AD-27).
- **Per-call records (19.4):** for each proxied call — call id, run id, Control Depth, token id, disposition (`pass` or `block` until 23.4) and hold end, status class, latency, model when present, tokens, sub-second timestamp; NEVER request or response content (the sentinel no-leak matrix extends); readable through the facade and the Control API.
- **Reconciliation (19.5):** a Host submits confirmed usage for a Run (including a child Run); reconciled figures sit BESIDE estimated rows and never overwrite them; drift is reported; every surface labels which figure it shows (FR-45, AD-8 — `EstimateLabel::Reconciled` is the designed, producer-less seam).

## Technical Decisions

- **AD-7 (2026-09-30, corrected 2026-10-30):** `block` fences model calls with no lock in the request path (AD-18 holds). The Task scope was never built and is replaced by child Runs (AD-25). Until the published snapshot carries dispositions (23.1), `block` is the per-instance flag on today's listener, set only by the pipeline. No code path other than the pipeline triggers a Breach Action.
- **AD-7 (2026-10-30):** the default Breach Action is `warn` and lives in the default Policy (21.3); `block` is opt-in only.
- **AD-8 (2026-09-30):** reconciliation ingestion is the `reconciled` label's first producer; reconciled figures never overwrite estimated rows.
- **AD-26 / AD-27:** per-call records come from the shared Metering Proxy into the calls store (23.1); 19.4 reads them.
- **Existing seams** (survey 2026-10-23): `BreachAction` = pause/stop/warn (`domain/budget.rs`); the enforcement choke point `enforce_budget` → `apply_breach` with the per-Run `(dimension, scope)` latch (`supervisor/usage.rs`); the loopback `forward()` with its honest static-502 discipline (`metering/listener.rs`); `EstimateLabel::Reconciled` (`domain/cost.rs`); config validation in `config.rs`; the std-only SHA-256 module; the Control API v0 route pattern (`hekma-host/src/control/`).

## UX & Interaction Patterns

- Terminal product, CLI-first; the Control API where the story names it.
- Every refusal or diagnostic names the reason and the remediation (raise or reset the ceiling; the label shown), per the surfaced-not-silent rule (AI-18).
- No new exit-code numbers: new diagnostics join existing classes.

## Cross-Story Dependencies

- 19.1 (done) is the vectors foundation: `block` adds decision-consumer cases; ceiling-raise re-arm vectors land with 21.6 and ancestor-ceiling vectors with 21.2, each an additive `vectors_version` bump.
- 19.2 lands before 21.3, whose Policies order `block` among Breach Actions.
- 19.4 needs 22.1 (token ids) and 23.1 (the shared proxy and calls store); 19.5 follows 19.4.
- House gates on every story (AGENTS.md): CLI-first, ≥95% coverage, docs in the same change, cross-platform, graceful degradation, Fibery mirrored in the same change.
