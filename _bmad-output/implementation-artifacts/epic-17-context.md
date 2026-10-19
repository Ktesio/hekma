# Epic 17 Context: Service Host

<!-- Compiled from planning artifacts. Edit freely. Regenerate with compile-epic-context if planning docs change. -->

> **Numbering note:** Epic 17 was renumbered from Epic 15 on 2026-10-19 — remote main's autonomous Fibery-backlog tranches consumed epic numbers 15/16 while this approved 2026-09-30 correct-course output sat uncommitted locally (`_bmad-output/implementation-artifacts/sprint-status.yaml:176-180`). The sprint change proposal and older artifacts say "Epic 15"/"Story 15.x" — those map to today's Epic 17/17.x. Beware: the proposal's own "Epic 17" (Wire Governance) is today's **Epic 19**, and its "Epic 16" is today's **Epic 18**.

## Goal

The first production Host — a hosted container platform, the PRD's UJ-3 archetype — needs supervision that outlives a single command and a local surface to drive it. Today a plain `start` supervises only for the command's lifetime and `--detach` leaves no crash detection, enforcement, or events between commands. This epic realizes FR-41: a long-running Service Host that owns an Engine continuously and serves a local, versioned Control API, plus state export so a Host with an ephemeral disk keeps its event record, plus a static Linux release and a container recipe so Hekma runs as a container supervisor. It discharges long-standing deferrals (AI-20's host mode, PRD Q6's delivery path) on evidence rather than speculation, while keeping Hekma generic — every story ships its own fixtures, no guarantee rests on private Host evidence. Batch-epic shape: full acceptance criteria are derived at the epic's spec stage (17.1). Sources: `_bmad-output/planning-artifacts/epics.md:1107-1113`, `_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md:25-61` (APPROVED by Islam 2026-09-30, Batch mode — proposal lines 620-625).

## Stories

- Story 17.1: Host-mode spec — the shim crate and the lock model (spec stage; settles D1/D2 by measurement)
- Story 17.2: `hekma serve` — the long-running host
- Story 17.3: Control API v0
- Story 17.4: Export for ephemeral hosts — the outbox and log capture modes
- Story 17.5: Static Linux release and the container recipe

(Story definitions: `_bmad-output/planning-artifacts/epics.md:1115-1158`; historical alias in the proposal's §4.4, lines 341-384; tracker block `_bmad-output/implementation-artifacts/sprint-status.yaml:180-186`.)

## Requirements & Constraints

From FR-41 (`_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md:373-387`):

- Supervision, crash detection, budget enforcement, and event delivery run **continuously** while the host runs — no supervision gaps between commands.
- On SIGTERM the host drains, then stops every Agent Instance within a bounded window, and never SIGKILLs an agent on its own exit before that window ends.
- Exactly one Service Host owns a state directory at a time; a second owner is refused with a clear diagnostic.
- The Control API binds only where explicitly configured, requires a bearer token resolved through the engine's secret-resolution mechanism, and returns no secrets and no raw agent output unless explicitly enabled.
- Events are available live (stream) and by cursor-based pull with one global order across event families, so a Host whose disk is ephemeral can export everything it needs before the host exits.
- The Control API carries its own version and is marked unstable (`0.x`) until a second Host validates it (the same discipline under which the Adapter Contract froze only after a second agent validated it).
- Non-goals that bind the whole epic: this is a surface for the Host that runs the engine — not a hosted control plane, not team governance, not remote telemetry (PRD §8/§6; proposal lines 79-82, 169).
- Scope-leep guard: the fence/API stays as cooperative as the meter — Hekma is not a sandbox (NFR-6); the host crate's HTTP dependency passes an NFR-8 dependency review.
- Naming ruling: the Host is described generically ("a hosted container platform"); no Host identifier, hostname, or infrastructure detail enters the repository (`sprint-change-proposal-...:17-21`).

## Technical Decisions

- **AD-20 — the Service Host is a shim over the public engine API** (`_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md:178-196`): a new host crate depends only on `hekma-engine`'s public API (one AD-2 boundary-gate entry); the `hekma` CLI stays synchronous; the engine library stays transport-agnostic. The host may own process signals; the engine library still may not (the embed-clean audit is unchanged). Rejected alternatives: a sync server inside the CLI crate (NFR-8 sign-off, poor SSE) and a listener owned by the engine (violates transport-agnostic).
- **D2 — the lock model re-opens on AD-18 trigger (1)** (`ARCHITECTURE-SPINE.md:220-221`; proposal lines 159-163): the serve shape is **measured** against NFR-4 by the story 7-5 method; then either the coarse two-mutex model is re-ratified with that evidence or the archived per-instance plan is activated. The decision is 17.1's, on measurement — never assumed.
- **D3 — Control API rules** (proposal lines 165-169): explicit bind; bearer via `SecretResolver`; no secret reveal or raw agent output unless enabled; events over SSE and by cursor pull.
- **Ratified 2026-09-30 amendments this epic implements** (`ARCHITECTURE-SPINE.md:198-223`): AD-14 — one global export sequence across the transition/breach/usage event families, pulled by cursor, with the per-family `resync_events` unchanged; AD-12 — log capture modes `full` (default, today's behavior), `metadata`, `off`, and `agent.log` rotation bounded like `output.log`.
- **Versioning discipline:** every new public surface is versioned; engine-surface changes are additive — any new `EngineError` arm or struct field is an announced minor under the semver gate (proposal lines 109-113).
- **17.2 specifics:** a fixed `metering.listen` port option (which also closes the adoption strand) and a child-environment allowlist — agents inherit only what is declared, not the host's whole environment (`epics.md:1125-1134`).
- **17.4 specifics:** the outbox is acknowledged by the Host; the state directory can be placed on a Host-chosen volume; capture mode `off` lets a Host keep possibly-user-content agent output off disk entirely (`epics.md:1144-1150`).

## UX & Interaction Patterns

No GUI — terminal product. Relevant operator-facing constraints:

- Operator-visible notices from the host go through the diagnostic sink, not silent drops (`epics.md:1133`).
- `hekma serve` is a foreground host owning one Engine for its whole life (`epics.md:1125-1127`).
- Story 17.5 ships a docs page, "running Hekma as a container supervisor," covering: the host as the container command or as an init-supervised service; how drain meets the container's stop grace; state-directory placement; and the cooperative-meter caveat (NFR-6) (`epics.md:1152-1158`).
- Static musl `x86_64` release artifacts, checksummed, join the release matrix; `aarch64` is deferred (`epics.md:1153`).

## Cross-Story Dependencies

- **17.1 gates everything in this epic:** it settles the host crate's name, the HTTP stack (NFR-8 review), and the AD-18 lock-model outcome, and delivers the AD-20 spine text plus a bounded-options proposal for Islam — following AI-58 draft-then-ratify (nothing applied to the spine before ratification) (`epics.md:1115-1123`).
- **17.3 depends on the sibling epics for two verb groups:** the Control API adds `drain` (arrives with Epic 18, the Adapter Contract v1.1.0 lifecycle) and the Task verbs (arrive with Epic 19, wire governance) once those epics land (`epics.md:1135-1137`).
- **Execution order** (proposal lines 136-140): Epic 19's Story 19.1 (governance vectors) runs first overall — an external Host consumer waits on it; then 17.1; then 17.2→17.3→17.4→17.5, which may run in parallel with Epic 18's stories after 17.1.
- **Cross-epic amendments land elsewhere but are consumed here:** Epic 18's contract minor states the older-engine rejection rule on the release surface (AI-55); Epic 19's Task scope and fence are money code and carry the AI-55 two-pass review.
- **House gates on every story** (`AGENTS.md`): CLI-first, ≥95% coverage (tarpaulin), docs updated in the same change, cross-platform, graceful degradation with surfaced (never silent) partial failures.
