---
stepsCompleted: [1, 2, 3, 4]
status: complete
inputDocuments:
  - _bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md
  - _bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/addendum.md
  - _bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md
executionNote: "Headless-conservative run inside the parent orchestrator (Islam's standing 'proceed inline, non-blocking' direction, 2026-07-02, during a flapping platform classifier). Step menus auto-continued with [C]; Party Mode offers skipped (requires subagent orchestration, gated) — validation gap covered by bmad-check-implementation-readiness next. Step-4 validation passed: 39/39 FRs covered (FR-27/FR-32 seeded early + completed later, annotated in the coverage map), no starter template (brownfield restructure = Story 1.1), entities introduced only when first needed (SQLite 1.2, tokio 1.4, usage_events 3.1), no forward dependencies, epics standalone on predecessors. 8 epics, 37 stories. No UX design contract exists (CLI/engine product — PRD review confirmed no UX phase needed). bmad-help invocation skipped (parent orchestrator owns routing; next = readiness check → sprint planning)."
---

# ktesio - Epic Breakdown

## Overview

This document provides the complete epic and story breakdown for ktesio, decomposing the requirements from the PRD, UX Design if it exists, and Architecture requirements into implementable stories.

## Requirements Inventory

### Functional Requirements

FR-1: Register an Agent Instance from an installed Adapter with a unique Fleet name (Agent Home created; duplicate names rejected; multi-instance per Agent kind)
FR-2: Isolated Agent Home per Agent Instance (no cross-instance leakage; concurrent same-kind instances)
FR-3: Unregister/remove an Agent Instance (retain-or-delete choice; stop-first or --force; no orphans)
FR-4: Fleet visibility (name, kind, Lifecycle State, budget/cap status, Usage Ledger totals; ≤2s freshness; --json)
FR-5: Start an Agent Instance via its Adapter with effective config, Skill Set, Memory Backing, budgets applied
FR-6: Stop with graceful→forced escalation (default 30s window; no surviving processes, cross-platform)
FR-7: Pause/resume with honest per-Adapter (and per-OS) semantics: guaranteed / best-effort / unsupported, surfaced not silent
FR-8: Defined lifecycle state machine, identical across Agents; invalid transitions rejected uniformly; every transition emits an event
FR-9: Crash detection + Restart Policy (never/on-failure/always; backoff; crash-loop bound)
FR-10: State persistence across Engine restarts and reboots (registrations, budgets, ledgers, last-known states; ledger loss ≤1s per spine AD-6)
FR-11: Unified layered config model with deterministic precedence (defaults < kind < instance < invocation)
FR-12: Adapter config mapping to agent-native mechanisms; explicit `agent.*` pass-through namespace (verbatim, marked unvalidated)
FR-13: Effective-config inspection with per-value source-layer provenance
FR-14: Secrets never logged/echoed/rendered unmasked; --reveal acknowledgment for machine output
FR-15: Attach/detach one Memory Backing per Agent Instance, uniform commands, not while running
FR-16: v1 Memory Backing kinds: `filesystem` (managed dir, byte-durable) and `native` (explicit delegation)
FR-17: Memory portability boundary explicit (guarantees vs delegation; Agent Home export carries filesystem backing)
FR-18: Token Budgets per Agent Instance at per-run and cumulative scopes; live-changeable
FR-19: Metering ingestion per declared Metering Source (self-reported | engine-observed); no-metering Adapters rejected at registration
FR-20: Rate supply (input/output $/1M) and dollar derivation; no retroactive repricing; inert-with-notice when no Rate
FR-21: Cost Cap enforcement executing the Breach Action (ratified default: pause) within the latency bound; per-instance configurable pause/stop/warn
FR-22: Usage & cost visibility per instance and Fleet-wide (tokens by scope, dollars, headroom; --json; totals equal the Ledger)
FR-23: Estimate honesty — every dollar figure labeled estimated|reconciled (type-enforced per spine AD-8)
FR-24: Send text input to a running Agent Instance uniformly; unsupported interaction fails fast quoting the Capability Declaration
FR-25: Stream output and read retained logs (rotated 10MB×3; timestamped, attributed agent-out|agent-err|engine)
FR-26: Scriptable surface: --json on every read command; stable documented exit codes; schema-compatibility tested
FR-27: Published, versioned Adapter Contract with machine-readable Capability Declaration (per-OS keyed); conformance test-kit runs against any Adapter
FR-28: Hermes reference Adapter end-to-end (lifecycle incl. gateway model, config mapping, self-reported metering, memory, interaction)
FR-29: Second-agent contract validation on paper before contract freeze — opencode (Islam 2026-07-02), structural characterization first
FR-30: Adapter Contract semver; incompatible-version rejection with both versions named
FR-31: Engine library exposes every capability through the Embedding Interface (no CLI required)
FR-32: kt consumes only the public Embedding Interface (CI-enforced; the embeddability proof)
FR-33: Host event/telemetry subscription (state transitions, usage updates, breaches, crash/restart) with stable versioned payload schemas
FR-34: Engine embeds clean: no TTY, no prompts, no global-state collisions
FR-35: Provision Skills to an Agent Instance (git/local source, commit-locked, reproducible; adapter informed of Skill Set location)
FR-36: Skill Set lifecycle per instance (list, upgrade/re-lock, remove, integrity check with remediation)
FR-37: The legacy skill-manager command surface is removed at the pivot release (0.6.0), with the removal explicitly stated in that release's CHANGELOG/RELEASE_NOTES and README — superseding the original "remain functional with a notice" requirement (amended 2026-07-14, ratified by Islam — Epic 9 correct-course, Story 9-3)
FR-38: The removal is announced at the pivot release via CHANGELOG.md/RELEASE_NOTES.md/README, naming the retired commands and the version at which they were removed — a clean break at a major pivot boundary satisfies the announce-and-document intent without an in-tool notice-then-remove window (Option A; amended 2026-07-14, ratified by Islam — Epic 9 correct-course, Story 9-3; Story 9.4's notice-stub path (Option B) not pursued)
FR-39: Continuity of `kt` name and channels (crates.io, Homebrew, install scripts) through the pivot

### NonFunctional Requirements

NFR-1: Resilience — agent crashes never crash the Engine/kt; orphan cleanup/adoption after Engine crash; graceful per-instance degradation with reasons + remediations
NFR-2: Cross-platform parity (Linux/macOS/Windows); OS gaps get closest-equivalent + documented difference (per-OS capability honesty; OS-conditional code only in backends per spine AD-4/conventions)
NFR-3: Test coverage ≥95% on src/, CI-enforced (cargo tarpaulin) — non-negotiable
NFR-4: Performance budgets (validated as testable targets by architecture): read commands <1s @25-instance Fleet; supervision overhead ≤2% CPU, ≤50MB RSS steady-state per running instance
NFR-5: Observability — structured, timestamped, attributed, rotation-bounded logs; stdout=output, stderr=diagnostics
NFR-6: Security & privacy — secrets per FR-14; Agent Home isolation is process/filesystem-level, NOT a security sandbox (boundary stated in docs)
NFR-7: Documentation currency — docs/README update in the same change; Adapter Contract + Embedding Interface docs version with the code
NFR-8: Runtime & dependency policy — Rust 2021+; lean deps; new deps tokio + rusqlite architecture-justified (Islam sign-off pending, non-blocking)

### Additional Requirements

From the Architecture spine (AD-1..16, status final) — these bind story design and acceptance criteria:

- No starter template: brownfield restructure of the existing repo into a 5-crate Cargo workspace (ktesio-engine, ktesio-adapter-api, ktesio-adapters-hermes, ktesio-conformance, kt) — affects Epic 1 foundation stories (AD-2)
- Hexagonal boundary: domain core free of adapter/OS/frontend deps; all variability through ports (AD-1)
- CI additions: crate-visibility + semver-check jobs on ktesio-engine and ktesio-adapter-api (AD-2); grep-lint for the single-currency-formatter rule (AD-8)
- Two adapter kinds behind one trait: native (hermes, mock) + manifest (adapter.toml, schema owned by ktesio-adapter-api under contract semver) (AD-3)
- Per-OS ProcessBackend: Unix process groups/signals; Windows Job Objects; capability declarations keyed (capability × OS) (AD-4)
- Write-ahead spawn records + orphan adoption on engine start (AD-5)
- SQLite (rusqlite, bundled, WAL) as sole state store; usage_events append-only + rollups; one txn per usage event (AD-6)
- Single metering/enforcement pipeline; Run = starting→terminal-state span; UsageEvent minimum shape {instance id, run id, input tokens, output tokens, metering source, timestamp}; v1 engine-observed = loopback OpenAI-compatible listener (AD-7)
- Type-enforced honesty: EstimateLabel on all currency rendering (AD-8); SecretString redaction everywhere (AD-10)
- Layered TOML config with persisted EffectiveConfig provenance snapshots (AD-9)
- MemoryBacking port with filesystem + native impls (AD-11)
- Engine-owned interaction/log capture; manifest default stdin/stdout channel (AD-12)
- tokio async-first engine with blocking facade for kt; no TTY in engine (AD-13)
- One event schema, two consumers: versioned serde structs for host subscription AND kt --json (AD-14)
- Lifecycle transition table as data, exhaustively unit-tested; backoff 1s×2 cap 60s, crash-loop stop at 5 (AD-15)
- Skills machinery built fresh in engine::skills (Epic 9 removed the legacy modules this originally planned to relocate); shell-out git stays; no legacy shims (AD-16, corrected 2026-07-14, ratified by Islam)
- Engine is sole filesystem path authority (conventions)
- Distribution: existing channels continue; NEW — publish ktesio-engine + ktesio-adapter-api to crates.io for hosts/adapter authors (conventions)
- Carried verification follow-ups: pin tokio/rusqlite exact versions at adoption; Hermes primary-source verification at the start of the hermes-adapter story; opencode structural characterization before contract freeze

### UX Design Requirements

None — no UX design contract exists. This is a CLI + embeddable-library product; the PRD reviewer confirmed no UX phase is required. Terminal UX conventions (miette diagnostics, ui.rs patterns, stdout/stderr discipline) are carried as ADOPTED conventions from the existing codebase via the spine.

### FR Coverage Map

FR-1: Epic 1 — Register an Agent Instance
FR-2: Epic 1 — Isolated Agent Home
FR-3: Epic 1 — Unregister/remove
FR-4: Epic 1 — Fleet visibility
FR-5: Epic 1 — Start
FR-6: Epic 1 — Stop with escalation
FR-7: Epic 1 — Pause/resume honesty
FR-8: Epic 1 — State machine
FR-9: Epic 1 — Crash detection + Restart Policy
FR-10: Epic 1 — State persistence
FR-11: Epic 2 — Layered config model
FR-12: Epic 2 — Adapter config mapping + pass-through
FR-13: Epic 2 — Effective-config provenance
FR-14: Epic 2 — Secrets handling
FR-15: Epic 5 — Attach/detach Memory Backing
FR-16: Epic 5 — filesystem + native backings
FR-17: Epic 5 — Portability boundary
FR-18: Epic 3 — Token Budgets
FR-19: Epic 3 — Metering ingestion (both sources)
FR-20: Epic 3 — Rate & dollar derivation
FR-21: Epic 3 — Cost Cap enforcement (pause default)
FR-22: Epic 3 — Usage & cost visibility
FR-23: Epic 3 — Estimate honesty (type-enforced)
FR-24: Epic 4 — Send input uniformly
FR-25: Epic 4 — Stream output & retained logs
FR-26: Epic 4 — Scriptable surface (--json, exit codes)
FR-27: Epic 6 — Published Adapter Contract + Capability Declaration (minimal declaration seeded in Epic 1; completed and frozen in Epic 6)
FR-28: Epic 6 — Hermes reference Adapter
FR-29: Epic 6 — opencode paper conformance validation
FR-30: Epic 6 — Contract semver
FR-31: Epic 7 — Engine exposes full capability
FR-32: Epic 7 — kt on public API only (boundary + CI seeded in Epic 1; proven and hardened in Epic 7)
FR-33: Epic 7 — Event/telemetry subscription
FR-34: Epic 7 — Embeds clean (no TTY)
FR-35: Epic 8 — Provision Skills to an instance
FR-36: Epic 8 — Skill Set lifecycle
FR-37: Epic 9 — Legacy surface removed at the pivot release (amended from "deprecation notices")
FR-38: Epic 9 — Removal announced via release notes at a stated version (amended from "notice-window lifecycle")
FR-39: Epic 9 (preserved: kt name / crates.io / Homebrew / self-update kept) + Epic 8 Story 8-5 (per-channel upgrade verification in release checks)

NFR mapping (cross-cutting unless noted): NFR-1 → Epic 1 (resilience core) + all DoDs; NFR-2 → Epic 1 (per-OS backends) + every OS-touching story; NFR-3 → every story's DoD (coverage gate); NFR-4 → Epic 7 (benchmark story); NFR-5 → Epic 4; NFR-6 → Epic 2 (secrets) + docs; NFR-7 → every story's DoD + Epic 8 (migration docs); NFR-8 → Epic 1 (tokio/rusqlite adoption stories).

## Epic List

### Epic 1: Run and Control Any Agent Through One Lifecycle
An Operator can register an agent described by a manifest adapter, start/pause/resume/stop it with identical commands, watch it in the Fleet listing, and trust that crashes are detected, restarts follow policy, and nothing is lost across engine restarts or reboots. Includes the brownfield foundation: 5-crate workspace restructure, tokio + SQLite adoption, per-OS process backends, and the CI boundary/coverage gates.
**FRs covered:** FR-1..FR-10 (+ FR-27 minimal Capability Declaration seed, FR-32 boundary seed)

### Epic 2: Configure Any Agent One Way
An Operator configures every agent through one layered TOML model with deterministic precedence, inspects the effective config with per-value provenance, passes agent-native extras through an explicit namespace, and never sees a secret leak into logs, events, or output.
**FRs covered:** FR-11..FR-14

### Epic 3: Bound Tokens and Dollars (Cost Governance)
An Operator sets token budgets and rate-derived dollar caps per agent and trusts the engine to meter usage (self-reported or engine-observed), enforce the breach action (pause by default) within the latency bound, and always label estimates honestly. UJ-1's cap moment lands here.
**FRs covered:** FR-18..FR-23

### Epic 4: Talk To and Observe Any Agent Uniformly
An Operator sends input to any running agent with one command, attaches to live output, reads retained attributed logs, and scripts against stable --json schemas and exit codes.
**FRs covered:** FR-24..FR-26

### Epic 5: Wire Memory Consistently
An Operator attaches or detaches a Memory Backing (managed filesystem dir, or explicit delegation to the agent's native memory) with the same commands for every agent, with the guarantee/delegation boundary explicit and the managed backing surviving restarts byte-identically.
**FRs covered:** FR-15..FR-17

### Epic 6: First-Class Hermes and a Frozen Public Adapter Contract
An Operator runs the real NousResearch Hermes Agent end-to-end under Ktesio (UJ-1 for real), adapter authors get a published, versioned Adapter Contract with per-OS Capability Declarations and a conformance test-kit, and the contract freezes only after the opencode paper validation feeds back.
**FRs covered:** FR-27..FR-30

### Epic 7: Embed the Engine (Hosts)
A Host embeds the engine library, drives every capability without a TTY, subscribes to state/usage/breach events with stable schemas, and depends on crates.io-published ktesio-engine + ktesio-adapter-api. kt consuming only the public API is proven in CI, and the NFR-4 performance budgets are benchmarked. UJ-3 lands here.
**FRs covered:** FR-31..FR-34

### Epic 12: Durable Detach & the Production-Usable Observed Channel
An Operator can detach a start from the CLI's lifetime (the agent survives via the adoption path), and the engine-observed metering channel works against real providers: streamed completions are metered (the `include_usage` terminal frame), HTTPS upstreams dial directly (vendored rustls), and a store outage degrades the observed drain loudly with a bounded-skip instead of wedging or silently losing. Opened 2026-09-15 by Islam's ratification (recommended across the board) of ai-20-ai-47-product-calls-2026-09-15.md.
**FRs covered:** FR-19 amendment (observed-channel production usability; no new FR numbers — this epic lands ratified follow-ups)

### Epic 8: Provision Skills and Migrate Legacy Users

> **(2026-09-09 — CLOSED AS SUPERSEDED by Islam's decision, option A):** the skills-provisioning premise was overtaken by the project pivot. Epic 9 removed the legacy skills-manager surface and repositioned Ktesio as an agent runner, so skills provisioning is no longer a Ktesio capability. The stories below are RETIRED WITHOUT DEVELOPMENT (8-4 was already self-marked superseded; 8-5's install-channel concern is carried by the release process and `scripts/public` installers). Definitions retained for the record. GitHub issues #95-99 closed 2026-09-09.

An Operator provisions commit-locked Skills to a managed agent, built fresh in `ktesio-engine::skills` under the hexagonal boundary (Epic 9 removed the legacy machinery this was originally planned to relocate). Existing v0.5.0 users already received the pivot's removal notice via the 0.6.0 release notes (Epic 9); this epic's remaining migration-continuity work is confirming every install channel still yields a working `kt` after the upgrade.
**FRs covered:** FR-35..FR-39
> **Correction 2026-07-13 (see `sprint-change-proposal-2026-07-13.md` + Epic 9):** the "retire the legacy CLI now" course-correction **supersedes** Story 8-4 (deprecate-in-place) and **changes the premise of Story 8-1 / AD-16** (the skills machinery it planned to relocate-and-reuse is being deleted, so 8-2/8-3 must build agent skill-provisioning in `engine::skills` fresh). FR-37/FR-38 amendments are flagged for Islam in the proposal. Final re-scope is Story 9-3 (architect-owned) — do not treat this Epic 8 summary as current until that lands.
> **Update 2026-07-14 — RATIFIED AND APPLIED:** Islam ratified Story 9-3's re-scope proposal exactly as drafted, keeping Story 8-1 as its own story (not merged into 8-2). The re-scope is now applied below and in `ARCHITECTURE-SPINE.md` AD-16: Story 8-1 is rewritten to build `engine::skills` fresh; Story 8-4 is marked superseded; Story 8-5's acceptance criteria are corrected; FR-37/FR-38 above are amended; Story 9.4 is recorded not pursued (Option A chosen). This Epic 8 summary — and every story below it — is current as of this update.

### Epic 9: Retire the Legacy Skill-Manager CLI *(added 2026-07-13 via correct-course)*
Complete the agent-runner pivot in the shipped `kt` binary: remove the retired skill-manager command surface, its exclusively-legacy modules, and its tests; re-brand the top-level `kt` identity from "Agentic skills package manager" to the agent runner; and reconcile the stale architecture/skill docs — so `kt --help` and the shipped behavior match the already-repositioned README and `docs/`. Preserves the `kt` name, the `ktesio` crates.io package, the install channels, and `kt self-update` (FR-39).
**FRs touched:** FR-37/FR-38 (amended — removal replaces deprecate-in-place; ratified by Islam 2026-07-14), FR-39 (preserved)

**Epic dependency order:** 1 → 2 → 3 → {4, 5} → 6 → 7 → 8. Each epic delivers standalone value on top of its predecessors; none requires a later epic to function. (Epics 4 and 5 are mutually independent; Epic 8 is now a fresh build in `engine::skills` — not a relocation, per the 2026-07-14 re-scope — so the original "could swap earlier if migration pressure demands" no longer applies: Epic 9 already resolved the migration pressure via its clean removal, and Epic 8 stays in its natural last position with no rush.) **Epic 9** (correction) is independent of Epics 4–7 and should run next — before the first pivoted release — and re-scopes parts of Epic 8.

## Epic 1: Run and Control Any Agent Through One Lifecycle

An Operator can register a manifest-described agent, drive it through one lifecycle (start/pause/resume/stop), watch the Fleet, and trust crash handling and persistence — on Linux, macOS, and Windows. Includes the brownfield foundation (5-crate workspace, SQLite, tokio, CI boundary gates) introduced only as each story needs it.

### Story 1.1: Restructure into the five-crate workspace without breaking the shipping CLI

As the Ktesio maintainer,
I want the repo restructured into the spine's Cargo workspace (ktesio-engine, ktesio-adapter-api, ktesio-adapters-hermes, ktesio-conformance, kt) with the existing CLI code living in the kt crate,
So that every later story lands inside enforced boundaries while v0.5.0 behavior keeps shipping.

**Acceptance Criteria:**

**Given** the current single-crate repo
**When** the workspace restructure lands
**Then** `cargo build --release` produces a `kt` binary whose existing commands (init/search/install/publish/upgrade/list/show/doctor/uninstall/self-update) behave exactly as v0.5.0 (integration suite green)
**And** the five workspace crates exist with `kt` depending only on `ktesio-engine`'s public API and `ktesio-adapter-api` types (AD-2)
**And** CI adds crate-visibility + semver-check jobs for ktesio-engine and ktesio-adapter-api and keeps `cargo fmt --check`, clippy `-D warnings`, `cargo test --all-targets`, and `cargo tarpaulin --fail-under 95` green (NFR-3)
**And** no NEW OS-conditional code exists outside `ktesio-engine::backends`, enforced by a CI grep gate; the two pre-existing v0.5.0 self-update files (`crates/kt/src/update_check.rs`, `crates/kt/src/cli/self_update.rs`) are explicitly grandfathered until epic 8 relocates/deprecates them (12 OS-conditional attributes as of 2026-07-02) (spine conventions; AC amended per code-review decision D3, ratified by Islam 2026-07-02)

### Story 1.2: Register and remove Agent Instances with isolated Agent Homes

As an Operator,
I want to register an Agent Instance under a unique name and remove it cleanly,
So that each agent I manage has its own isolated home and the Fleet stays consistent. (FR-1, FR-2, FR-3)

**Acceptance Criteria:**

**Given** a fresh engine state (SQLite store introduced here per AD-6: rusqlite bundled, WAL; exact version pinned and recorded per the spine's verification note)
**When** I register an Agent Instance with a unique name
**Then** an Agent Home is created with instance config and an empty Usage Ledger, the instance enters Lifecycle State `registered`, and all paths are computed only by the engine (path-authority convention)
**And** registering a duplicate name fails with a diagnostic naming the conflict and a remediation hint
**Given** two Agent Instances of the same Agent kind
**When** both are registered
**Then** their Agent Homes are disjoint and independently configured
**Given** a registered (not running) Agent Instance
**When** I remove it choosing retain or delete
**Then** retain leaves the Agent Home intact on disk, delete removes it, and every other Agent Home is byte-identical afterward (FR-2 isolation)
**And** removing a `running` instance requires stop-first or an explicit `--force` acknowledgment

### Story 1.3: Bring any simple agent via a manifest adapter

As an Operator,
I want to register an agent described by a declarative `adapter.toml` supplied by path,
So that I can put my own agents under Ktesio without writing Rust. (FR-1 path registration; FR-27 seed)

**Acceptance Criteria:**

**Given** the `ktesio-adapter-api` crate defining the AgentAdapter trait, the manifest schema (types + validation owned by this crate under contract semver, AD-3), and a minimal per-OS Capability Declaration
**When** I register an Agent Instance from a directory containing a valid `adapter.toml` (exec/args/env templates per lifecycle op, capability declaration, metering-source config, interaction wiring)
**Then** registration succeeds and the effective (current-OS) Capability Declaration is visible for the instance
**And** an invalid manifest (missing mandatory section) is rejected with a diagnostic naming the section (FR-27 consequence)
**And** the `ktesio-conformance` crate ships a mock adapter + scripted fake agent used by this and all later lifecycle/governance tests
**And** an adapter whose manifest declares no viable Metering Source is rejected at registration with a clear diagnostic (FR-19 hard line)

### Story 1.4: Start and stop any registered agent identically on all three OSes

As an Operator,
I want `start` and `stop` to behave the same for every agent,
So that lifecycle knowledge transfers across agents and platforms. (FR-5, FR-6, FR-8 core)

**Acceptance Criteria:**

**Given** a registered Agent Instance (mock or manifest) and the tokio-based supervision core (AD-13; exact tokio version pinned and recorded at adoption)
**When** I start it
**Then** the adapter launches it with the Agent Home's effective config applied, state transitions `registered→starting→running` per the data-driven transition table (AD-15), and each transition emits an event with prior state, new state, cause, timestamp
**And** a failed launch lands in `failed` with the adapter's diagnostic preserved and no zombie process
**When** I stop a running instance
**Then** graceful shutdown is requested via the adapter and escalates to forced termination after the configurable window (default 30s), the escalation is recorded in the instance log, and no process of the instance survives — verified on Linux, macOS, and Windows (Unix process groups / Windows Job Objects per AD-4, NFR-2)
**And** invalid transitions (e.g. stop on `stopped`) return the same error class for every adapter (FR-8)

### Story 1.5: Pause and resume with honest, per-OS semantics

As an Operator,
I want pause/resume that tells the truth about what it can guarantee for this agent on this OS,
So that I never mistake a best-effort pause for a guaranteed one. (FR-7)

**Acceptance Criteria:**

**Given** an adapter declaring `pause: guaranteed-via-signal` on Unix
**When** I pause and resume the running instance
**Then** SIGSTOP/SIGCONT are used, states transition `running→paused→running`, and the Usage Ledger continues from where it left off
**Given** an adapter declaring `pause: best-effort` (the Windows default per AD-4)
**When** I pause
**Then** the pause proceeds cooperatively and a visible qualifier is emitted in CLI text and the event payload — never a silent success
**Given** an adapter declaring `pause: unsupported`
**When** I pause
**Then** the command fails fast quoting the Capability Declaration and attempts no fake pause

### Story 1.6: Survive agent crashes: detection, restart policy, and orphan adoption

As an Operator,
I want crashes detected and handled by policy, and no orphan processes ever left behind,
So that unattended agents are safe to run. (FR-9, NFR-1)

**Acceptance Criteria:**

**Given** a running Agent Instance whose process exits without a requested stop
**When** the engine detects the exit
**Then** the instance is marked `failed` with the exit cause, and the Restart Policy applies: `never` stays failed; `on-failure` restarts with exponential backoff (1s base, ×2, 60s cap) and a visible restart count; crash-looping stops after 5 consecutive failures with the reason stated (AD-15 defaults, per-instance configurable)
**Given** spawn write-ahead records (AD-5: instance id + PID + process start-time fingerprint persisted before exec completes)
**When** the engine itself crashes and restarts
**Then** records matching a live fingerprint are adopted back under supervision, non-matching records are marked `failed` with last-known cause, and no agent process is left unsupervised (verified by an engine-kill integration test)

### Story 1.7: See the whole Fleet and trust it across reboots

As an Operator,
I want a Fleet listing that reflects reality and state that survives engine restarts and machine reboots,
So that I can always answer "what is running and in what state." (FR-4, FR-10)

**Acceptance Criteria:**

**Given** several Agent Instances in different Lifecycle States
**When** I list the Fleet
**Then** each row shows name, Agent kind, Lifecycle State, budget/cap status, and Usage Ledger totals, reflects any transition within 2 seconds, and is available as human-readable and `--json`
**Given** a machine reboot while instances were running
**When** the engine starts and I list the Fleet
**Then** every registration, budget, and ledger total is intact and previously-running instances show `stopped`/`failed` per orphan reconciliation — never lost
**And** a crash of the engine itself loses at most 1 second of usage data (AD-6 per-event transactions)

## Epic 2: Configure Any Agent One Way

One layered TOML configuration model with provenance and safe secrets, for every agent.

### Story 2.1: Set configuration once, with deterministic layers

As an Operator,
I want config layered as engine defaults < agent-kind defaults < instance config < invocation overrides,
So that the same key always resolves predictably. (FR-11)

**Acceptance Criteria:**

**Given** the same key set at kind level and instance level
**When** the effective config resolves
**Then** the instance value wins, every time, per the documented precedence (AD-9)
**Given** a config write with an unknown key outside the pass-through namespace
**When** validation runs at write time
**Then** the write is rejected and the nearest valid key is suggested
**And** all four layers are TOML and stored/read only through engine APIs (path authority)

### Story 2.2: Map unified config to each agent's native mechanism

As an Operator,
I want documented unified keys delivered into the agent's native config (files, env, flags) by its adapter, with agent-native extras under an explicit `agent.*` pass-through,
So that I configure agents without learning per-agent formats. (FR-12)

**Acceptance Criteria:**

**Given** a documented unified key (e.g. model selection)
**When** the instance starts on the mock adapter and on a manifest adapter in tests
**Then** the key lands in the agent's native mechanism per the adapter's mapping
**Given** keys under `agent.*`
**When** the instance starts
**Then** they are delivered verbatim and rendered as unvalidated in effective-config output

### Story 2.3: Inspect the effective config with per-value provenance

As an Operator,
I want to see exactly what will apply on next start and where each value came from,
So that config debugging is never guesswork. (FR-13)

**Acceptance Criteria:**

**Given** values set across multiple layers
**When** I inspect the instance's effective config
**Then** every rendered value names its source layer (default / kind / instance / override), and a persisted EffectiveConfig snapshot with the same provenance is written to the Agent Home at start (AD-9)
**And** no secret-classified keys exist at this point in the epic; the masking requirement for effective-config output binds from the moment the secrets capability exists (FR-14)

### Story 2.4: Keep secrets out of everything

As an Operator,
I want secret values referenced indirectly and never logged, echoed, or serialized unmasked,
So that my API keys are safe by construction. (FR-14, NFR-6)

**Acceptance Criteria:**

**Given** config referencing `secret:NAME` with the env and 0600-file resolvers (AD-10)
**When** the instance starts
**Then** the resolved value reaches the adapter, lives only in a SecretString newtype whose Display/Debug redact, and is masked in effective-config output
**And** a test proves secrets never appear in engine/CLI logs, event payloads, or `--json` output
**And** machine-readable output includes a secret value only under an explicit `--reveal` acknowledgment

## Epic 3: Bound Tokens and Dollars (Cost Governance)

Budgets, rate-derived caps, honest estimates, and runner-level enforcement — the headline promise.

### Story 3.1: Meter self-reported usage into one durable ledger

As an Operator,
I want the engine to ingest the agent's own usage accounting into a per-instance Usage Ledger,
So that consumption is tracked from day one. (FR-19 self-reported half)

**Acceptance Criteria:**

**Given** the mock adapter declaring `self-reported` and emitting usage batches
**When** usage arrives (including a replayed batch)
**Then** UsageEvents land in the append-only `usage_events` table with the minimum shape {instance id, run id, input tokens, output tokens, metering source, timestamp} (AD-7), one transaction per event, without double-counting
**And** a Run is delimited exactly as the span from `starting` to the next terminal state, and per-run totals reflect it
**And** the active Metering Source is visible in Fleet listing detail

### Story 3.2: Enforce token budgets at the runner level

As an Operator,
I want per-run and cumulative Token Budgets whose breach triggers the configured Breach Action,
So that token consumption is bounded even when I'm not watching. (FR-18, FR-21 pipeline)

**Acceptance Criteria:**

**Given** a Token Budget set on a running instance
**When** metered consumption reaches the budget
**Then** the BudgetEvaluator (inside the ledger commit path, AD-7) emits a BreachDecision and the supervisor executes the Breach Action — default `pause` (ratified) — within one metering interval, and a breach event is always recorded regardless of action
**And** budgets are inspectable and changeable while `running`, applying immediately
**And** the Breach Action is per-instance configurable among pause/stop/warn

### Story 3.3: Turn a $/1M rate into an enforced dollar cap

As an Operator,
I want to supply input/output Rates and set a Cost Cap,
So that a runaway agent pauses before it surprises my bill. (FR-20, FR-21; SM-2)

**Acceptance Criteria:**

**Given** a Rate supplied for an instance
**When** usage accrues
**Then** the Usage Ledger derives dollars from metered tokens, historical dollars keep the Rate in force when consumed (no retroactive repricing), and changing the Rate re-prices future consumption only
**Given** no Rate supplied
**When** I use dollar features
**Then** they are inert and say so, while token features work fully
**Given** an integration test driving the mock agent past its Cost Cap
**When** the cap is breached
**Then** the Breach Action executes in 100% of runs with ledger-breach→lifecycle-action latency ≤ metering interval + 1s

### Story 3.4: Observe usage when the agent can't report it

As an Operator,
I want engine-observed metering for agents with no usage accounting,
So that governance never depends on the agent's cooperation. (FR-19 engine-observed half)

**Acceptance Criteria:**

**Given** an adapter declaring `engine-observed`
**When** the instance starts
**Then** the engine runs a loopback-only HTTP forward listener, the adapter injects it as the agent's OpenAI-compatible `base_url`, and usage fields parsed from responses land in the Usage Ledger within the flush bound of call completion (AD-7)
**And** the listener binds loopback only and refuses external interfaces
**And** a manifest adapter with neither source viable remains rejected at registration (regression guard on FR-19's hard line)

### Story 3.5: Read usage and cost honestly, per instance and Fleet-wide

As an Operator,
I want token/dollar visibility with headroom, where every dollar is labeled,
So that I can trust what I read. (FR-22, FR-23; SM-C3)

**Acceptance Criteria:**

**Given** instances with budgets, Rates, and accrued usage
**When** I read usage per instance or Fleet-wide
**Then** tokens by scope, derived dollars, active budgets/caps, and headroom render human-readable and `--json`, and output totals equal the Usage Ledger exactly
**And** every dollar figure carries its `estimated`|`reconciled` label, enforced at the type level by the single currency-rendering module (AD-8) with a CI grep-lint proving no other module formats currency

## Epic 4: Talk To and Observe Any Agent Uniformly

One interaction surface: send, stream, script.

### Story 4.1: Send input to any running agent with one command

As an Operator,
I want to send text input uniformly, with honest failure when an agent can't receive it,
So that interacting doesn't require per-agent knowledge. (FR-24)

**Acceptance Criteria:**

**Given** a running instance on the mock adapter and on a manifest adapter (stdin channel per AD-12)
**When** I send a text input
**Then** the adapter routes it to the agent's native input channel and the same command works on both
**Given** an adapter declaring `interaction: unsupported`
**When** I send input
**Then** the command fails fast quoting the Capability Declaration

### Story 4.2: Attach to live output and read retained logs

As an Operator,
I want to stream a running agent's output and read what it said while I was detached,
So that observation is uniform and nothing is lost. (FR-25, NFR-5)

**Acceptance Criteria:**

**Given** a running instance emitting output while no one is attached
**When** I later attach or read retained logs
**Then** output captured to the Agent Home is readable, bounded by rotation (10MB × 3 per AD-12), and every line is timestamped and attributed (`agent-out` | `agent-err` | `engine`)
**And** live attach streams both agent streams with the same attribution

### Story 4.3: Script against stable JSON and exit codes

As an Operator,
I want `--json` on every read command and documented, stable exit codes,
So that kt is automatable without the Embedding Interface. (FR-26)

**Acceptance Criteria:**

**Given** the read commands (fleet list, status, usage, effective-config, logs)
**When** invoked with `--json`
**Then** output serializes the same versioned serde structs the event stream uses (AD-14), each payload carrying `schema_version`
**And** exit codes are documented and covered by compatibility tests that fail CI on unannounced change (schema-compatibility per PRD §7)

## Epic 5: Wire Memory Consistently

One memory interface for every agent, with the guarantee/delegation boundary explicit.

### Story 5.1: Attach a managed filesystem Memory Backing

As an Operator,
I want to attach an engine-managed `filesystem` Memory Backing to an agent with one command,
So that agent memory persists under my control regardless of the agent's native story. (FR-15, FR-16 filesystem half)

**Acceptance Criteria:**

**Given** a registered, not-running Agent Instance
**When** I attach a `filesystem` Memory Backing
**Then** the engine creates the managed directory inside the Agent Home (path authority) and hands the backing descriptor to the adapter at next start (AD-11)
**And** the same attach command sequence works identically on the mock adapter and a manifest adapter
**And** attach/detach on a `running` instance is rejected (no hot-swap)
**Given** stop/start cycles and engine restarts
**When** the instance runs again
**Then** the managed directory's contents survive byte-identically

### Story 5.2: Delegate to native memory with an explicit boundary

As an Operator,
I want a `native` backing that says plainly Ktesio only guarantees Agent Home persistence,
So that I always know what is guaranteed versus delegated. (FR-16 native half, FR-17)

**Acceptance Criteria:**

**Given** an instance with the `native` Memory Backing
**When** I inspect effective config or backing status
**Then** the delegation is recorded and visible: memory semantics belong to the agent; Ktesio guarantees only Agent Home persistence
**Given** an Agent Home copied to another machine (documented portability procedure)
**When** the instance runs there
**Then** a `filesystem` backing travels with it and the agent runs with memory intact, and the guarantees-vs-delegation boundary is stated in docs and command output (NFR-7)

## Epic 6: First-Class Hermes and a Frozen Public Adapter Contract

The reference agent runs for real; the contract earns its version freeze.

### Story 6.1: Verify Hermes Agent surfaces from primary sources

As the Ktesio maintainer,
I want the Hermes Agent's real CLI, gateway process model, config, and usage surfaces verified from primary sources before adapter code is written,
So that the reference adapter is built on facts, not search excerpts. (Carried spine follow-up; de-risks FR-28)

**Acceptance Criteria:**

**Given** the brief addendum's Hermes analysis carries a search-excerpt caveat
**When** this story completes
**Then** a written verification note (in the epic's planning folder) confirms or corrects: gateway/process model, lifecycle verbs, config mechanism, usage/analytics surface, interaction channels — each cited to primary docs or the Hermes repo
**And** any contract-impacting surprise is fed back as an Adapter Contract change proposal before Story 6.2 starts

### Story 6.2: Run the real Hermes Agent under Ktesio lifecycle

As an Operator,
I want to register, start, stop, and (per its declaration) pause the real NousResearch Hermes Agent,
So that the flagship agent is governed like any other. (FR-28 lifecycle half)

**Acceptance Criteria:**

**Given** the ktesio-adapters-hermes native adapter with a per-OS Capability Declaration
**When** I register and start a Hermes instance
**Then** Hermes launches through its gateway model with unified config mapped to its native mechanism, transitions follow the standard state machine, and stop terminates the full process tree on all three OSes
**And** every Epic 1 lifecycle AC (FR-4..FR-9 consequences) passes against the Hermes adapter, with declared best-effort capabilities explicitly surfaced
**And** integration tests run sandboxed/recorded where network-bound (isolation strategy documented in the test module)

### Story 6.3: Govern and interact with Hermes end-to-end (UJ-1 for real)

As an Operator,
I want Hermes metered (self-reported), budgeted, capped, memory-wired, and interactive under the same commands,
So that UJ-1 — cap Hermes before it surprises me — works on the real agent. (FR-28 completion)

**Acceptance Criteria:**

**Given** a Hermes instance with a Rate and Cost Cap set
**When** usage accrues past the cap in a controlled test
**Then** self-reported usage lands in the Usage Ledger (batches reconciled without double-count), the Breach Action pauses the instance, and `kt` reports tokens + estimated dollars with honest labels
**And** memory attach (filesystem + native) and input/output interaction work through the standard commands
**And** the UJ-1 journey runs end-to-end as an integration test using only documented commands

### Story 6.4: Prove any adapter with the conformance test-kit

As an adapter author,
I want a conformance kit that exercises every contract section against my adapter,
So that "identical controls" is testable, not aspirational. (FR-27; SM-1 interim basis)

**Acceptance Criteria:**

**Given** the ktesio-conformance TCK
**When** run against the mock adapter
**Then** it exercises lifecycle transitions (including crash), config mapping, both Metering Sources, memory attachment, interaction, and Capability Declaration edge cases (e.g. `pause: unsupported`), reporting per-capability compliance
**And** the Hermes adapter passes all sections applicable to its declaration
**And** the TCK runs as a cargo test harness any third-party adapter crate can invoke

### Story 6.5: Validate the contract against opencode on paper

As the Ktesio maintainer,
I want opencode's real structure characterized and mapped section-by-section against the Adapter Contract,
So that the contract is proven non-Hermes-shaped before it freezes. (FR-29, Islam's ruling)

**Acceptance Criteria:**

**Given** opencode (opencode.ai) as the selected second agent
**When** this story completes
**Then** a characterization note records its actual session/TUI model, client/server split, provider config, and usage surface from primary sources, naming which structural-distance axes it covers vs Hermes and which the mock must cover
**And** a conformance mapping document maps every contract section to opencode, with contract change proposals fed back
**And** unresolved axes are explicitly listed as contract-freeze risks, not silently dropped

### Story 6.6: Freeze and publish the Adapter Contract v1

*(Acceptance criteria extended 2026-08-28 — propagating a decision already ratified by Islam on 2026-08-24 (Story 5-2, Q-1/Q-2), not a new ruling. Story 5-2 deferred the `--json` memory wire surface out of Epic 5 and into this story — "the wire-format freeze moves to Epic 6 where Hermes' real consumers force the shape" — and Story 5-1's DC-6 "ONE intentional announced key-set edit" obligation transferred with it. Until now that obligation lived only in the compiled `epic-6-context.md`, not in the criteria that bind the work: a developer working from this file alone would have frozen contract v1 with the memory wire surface still missing, and the freeze is irreversible under the semver policy this same story publishes.)*

As an adapter author,
I want a semver'd, documented Adapter Contract whose version the engine negotiates,
So that adapters built today keep working tomorrow. (FR-30, PRD §7)

**Acceptance Criteria:**

**Given** feedback from Stories 6.4 and 6.5 applied
**When** ktesio-adapter-api tags contract v1
**Then** loading an adapter with an incompatible contract version fails with both versions named and the compatibility rule quoted
**And** contract docs (trait + manifest schema + capability declaration + versioning policy) publish with the crate (NFR-7)
**And** the semver-check CI job guards the crate from unannounced breakage

**Given** the `--json` memory wire surface Story 5-2 deferred into this epic (ratified by Islam 2026-08-24, Q-1/Q-2), carrying Story 5-1's "ONE intentional announced key-set edit" obligation with it
**When** contract v1 freezes
**Then** the same change lands memory on the wire — every memory read reachable via `--json`, none left human-output-only — carrying both Memory Backing kinds and the guarantee level in the typed form reserved for exactly this freeze (`GuaranteeLevel`'s snake_case wire strings, adopted unchanged)
**And** it lands as exactly ONE announced key-set edit: Story 4-3's frozen key-set and `schema_version` assertions are re-pinned to the new shape in that same change, and the edit is announced in the release's CHANGELOG/RELEASE_NOTES and docs (PRD §7, NFR-7) — never as silent drift
**And** contract v1 is not tagged with this surface still deferred, since after the freeze the same story's semver policy makes adding it a breaking wire change

## Epic 7: Embed the Engine (Hosts)

The upward half of dual delivery, proven and published. UJ-3 lands here.

### Story 7.1: Drive every capability through the library alone

As a Host,
I want the full register→configure→cap→start→breach→pause→stop flow working with no CLI involved,
So that embedding is real, not theoretical. (FR-31)

**Acceptance Criteria:**

**Given** a test host binary linking ktesio-engine only
**When** it drives the full UJ-3 flow through the Embedding Interface
**Then** every §4.1–4.9 capability used is reachable and behaviorally identical to the kt path (assertions shared between both test suites)
**And** any capability found unreachable is closed in this story

*(2026-09-09: §4.9 skills provisioning is owned by Epic 8 post-pivot; §4.8 is discharged by the story's own host test; §4.4/§4.6 facade reachability cited from the Epic-6 e2e.)*

### Story 7.2: Subscribe to engine events with stable schemas

As a Host,
I want ordered state/usage/breach/crash events per instance with versioned payloads,
So that my platform UI reflects runtime truth without polling. (FR-33)

**Acceptance Criteria:**

**Given** a subscriber registered via the Embedding Interface
**When** lifecycle transitions, usage updates, breaches, and crash/restarts occur
**Then** events arrive in order per Agent Instance, each payload schema-validated and carrying `schema_version` (AD-14)
**And** slow subscribers cannot stall supervision (bounded channel policy documented and tested)

### Story 7.3: Embed clean — no TTY, no prompts, blocking facade

As a Host,
I want the engine to run headless inside my process,
So that embedding never fights my runtime. (FR-34)

**Acceptance Criteria:**

**Given** a no-TTY test harness
**When** the full Story 7.1 flow runs
**Then** zero interactive prompts occur and no global process state collides with the host's (AD-13)
**And** the blocking() facade covers the full async API and is what kt itself uses

### Story 7.4: Prove the boundary and publish the crates

As a Host,
I want ktesio-engine and ktesio-adapter-api on crates.io with kt proven to use only their public API,
So that I can depend on what the CLI depends on. (FR-32; distribution convention)

**Acceptance Criteria:**

**Given** the CI boundary jobs from Story 1.1
**When** the release lands
**Then** a build-level check proves kt uses only public engine API (violations fail CI) and cargo-semver-checks guards both crates
**And** ktesio-engine + ktesio-adapter-api publish to crates.io with an embedding quickstart doc (host example compiles in CI)

*(2026-09-09: publication PREPARED AND HELD by Islam's standing no-deployment instruction; the machinery is complete per docs/release-process.md and executes on his explicit go.)*

### Story 7.5: Benchmark the performance budgets

As an Operator,
I want the NFR-4 numbers measured, not asserted,
So that the budgets in the PRD are real. (NFR-4)

**Acceptance Criteria:**

**Given** a 25-instance Fleet fixture
**When** the benchmark suite runs in CI (or a designated perf job)
**Then** read commands complete <1s, supervision overhead per running instance measures ≤2% CPU and ≤50MB RSS steady-state — or the measured values replace the budgets in PRD/spine via a documented update
**And** regressions against the ratified budgets fail the perf job

## Epic 8: Provision Skills and Migrate Legacy Users

Skills become agent-scoped; v0.5.0 users get a path, not a break.

### Story 8.1: Build the skills-provisioning foundation in `ktesio-engine::skills`

*(Rewritten 2026-07-14 — ratified re-scope, Story 9-3 Part B. Originally "Relocate the install/lock machinery into the engine"; Epic 9 deleted the modules this story planned to relocate, so the premise changed from relocating existing code to building it fresh. Kept as its own story per Islam's ratification, not merged into 8-2.)*

As the Ktesio maintainer,
I want a fresh `ktesio-engine::skills` module — Skill Set types, a git shell-out helper, and per-Agent-Home `skills.json`/`skills.lock` read/write — built new inside the engine's hexagonal boundary,
So that Stories 8-2/8-3 have a tested foundation to provision and manage Skills on, without depending on code that no longer exists. (AD-16, corrected; enables FR-35..36)

**Acceptance Criteria:**

**Given** the `ktesio-engine` crate and no surviving skill-manager code anywhere in the workspace (removed by Epic 9)
**When** the `skills` module is built
**Then** `ktesio-engine::skills` defines the Skill Set types (a skill entry; its source — git ref or local path; its locked commit), a git shell-out helper (ADOPTED: shell out, no libgit2 — consistent with the deleted implementation's approach), and read/write for a per-Agent-Home `skills.json` (declared skills) + `skills.lock` (resolved commits) pair, reachable only through the engine's public API (AD-2)
**And** the design is informed by, but not copied from, the deleted `crates/kt/src/{manifest,lockfile,git,install_target}.rs` (recoverable in git history at/before commit `42896b7`) — reused for the *shape* of the problem (manifest/lockfile split, commit-locking, shell-out git), not as code, since the old implementation predates the hexagonal boundary and cannot compile inside it unchanged
**And** the module carries its own new unit-test suite proving parse/validate/lock-write/lock-read round-trips (the deleted CLI-level tests — `adoption_cli`, `install_default`, `install_fallback`, `publish` — exercised a command surface that no longer exists and cannot be resurrected as-is; treat them as a source of test-case ideas, not test code, when writing the new suite)
**And** coverage stays ≥95% on the new module (NFR-3 — no relocation discount applies, because nothing is being relocated)

**Scope guard:** this story owns only the foundation (types + git plumbing + Agent-Home-scoped file I/O). Wiring it to "add a Skill to an instance" is Story 8-2; over-time management (list/upgrade/remove/integrity) is Story 8-3. No legacy shim layer exists or is created — there is nothing left to shim (Epic 9 removed it outright).

### Story 8.2: Provision Skills to a managed agent

As an Operator,
I want to add commit-locked Skills to an Agent Instance from git or local paths,
So that my agent gets its "what it knows" reproducibly. (FR-35)

**Acceptance Criteria:**

**Given** a registered Agent Instance
**When** I add a Skill by git source or local path
**Then** it installs into the Agent Home, the Skill Set lockfile records source + exact commit, and re-provisioning from lock is byte-reproducible
**And** the adapter is informed of the Skill Set location, mapping to the agent's native skills convention where one exists

**Dependency note (2026-07-14, Story 9-3 re-scope):** depends on Story 8-1's freshly built foundation, not relocated code — the acceptance criteria above are unchanged, since they were already engine/instance-scoped and never literally depended on the legacy kt modules.

### Story 8.3: Manage each agent's Skill Set over time

As an Operator,
I want per-instance list/upgrade/remove/integrity for Skills,
So that Skill Sets stay healthy per agent. (FR-36)

**Acceptance Criteria:**

**Given** an instance with installed Skills
**When** I list, upgrade (re-lock to newer commit), or remove a Skill
**Then** the Skill Set and lockfile update consistently, scoped to that instance only
**And** the integrity check detects a hand-modified installed Skill and reports it with remediation, per instance

**Dependency note (2026-07-14, Story 9-3 re-scope):** depends on Stories 8-1/8-2's freshly built foundation, not relocated code — the acceptance criteria above are unchanged, since they were already engine/instance-scoped and never literally depended on the legacy kt modules.

### Story 8.4: Deprecate the legacy surface loudly and kindly — SUPERSEDED

> **Superseded by Epic 9 (2026-07-14, ratified re-scope, Story 9-3 Part B):** commits `42896b7`/`45d203e` removed the legacy skill-manager surface outright, with no shim layer and no in-tool deprecation-notice window. The removal is announced via the pivot release's CHANGELOG/RELEASE_NOTES/README instead (FR-37/FR-38 amended above). There is no legacy command left to attach a notice to. The story text below is kept as a record of the original plan, not as current work.

As a legacy v0.5.0 user,
I want working commands with clear notices and a published path,
So that the pivot never strands me silently. (FR-37, FR-38 — superseded; see above)

**Acceptance Criteria (superseded, not implemented):**

**Given** any legacy general-purpose skills command
**When** invoked
**Then** it works and emits exactly one deprecation notice per invocation to stderr (never stdout), naming the replacement, the migration doc, and the removal-target version
**And** pivot release notes + README state the deprecation, the ≥90-day/one-minor window, and the replacement (NFR-7)

### Story 8.5: Keep every install channel unbroken through the pivot

*(Acceptance criteria corrected 2026-07-14 — ratified re-scope, Story 9-3 Part B: the original "legacy commands still run (with notices) for the stated window" clause assumed Option B/C from the sprint-change-proposal; Epic 9 shipped Option A, a clean removal. Much of this story's substance is already satisfied as a side effect of Epic 9 — `self-update`/`install_channel.rs` were explicitly kept, and the 0.6.0 CHANGELOG/RELEASE_NOTES entries already state the removal. The remaining scope is the per-channel upgrade verification below.)*

As a legacy v0.5.0 user,
I want upgrades via crates.io, Homebrew, and the install scripts to keep working,
So that `kt` remains the same tool I installed. (FR-39)

**Acceptance Criteria:**

**Given** a v0.5.0 installation from each published channel
**When** the user upgrades to the pivot release (0.6.0)
**Then** a working `kt` results, presenting the agent-runner surface only (no legacy commands — Epic 9's clean removal, FR-37/38 as amended), with the removal clearly stated in that release's CHANGELOG/RELEASE_NOTES/README — verified per channel in release checks
**And** the `kt` name, crates.io package, Homebrew tap, and install scripts all carry over unchanged (FR-39)

## Epic 9: Retire the Legacy Skill-Manager CLI

*Added 2026-07-13 via the correct-course workflow (`sprint-change-proposal-2026-07-13.md`). Islam-approved course correction to finish the pivot in the shipped binary. Independent of Epics 4–7; run before the first pivoted release. Supersedes Epic 8 Story 8-4 and re-scopes Story 8-1 / AD-16.*

Complete the agent-runner pivot in the shipped `kt` binary so `kt --help` and behavior match the already-live repositioned README and `docs/`. Verified against `crates/kt/src/main.rs`: the skill-manager cluster (nine command handlers over `skills_sh/discovery/install_target/manifest/lockfile/git/skill`) is self-contained and unreachable from `cli/agent.rs` (imports only `error::Agent*` + `ui`) and `cli/self_update.rs` — a clean excision. `kt self-update` and its modules are kept (binary maintenance, FR-39; grandfathered in Story 1.1's AC).

### Story 9.1: Remove the legacy skill-manager command surface, modules, and tests

As the Ktesio maintainer,
I want the retired skill-manager commands and every module and test that exists only to serve them deleted from the `kt` crate,
So that the binary carries only the agent runner (plus binary self-maintenance) and no dead skill-manager code.

**Acceptance Criteria:**

**Given** the current `kt` crate
**When** the excision lands
**Then** the `Commands` enum no longer contains `Init`, `Install`, `Search`, `Publish` (+ `PublishCommands`), `Upgrade`, `List`, `Show`, `Doctor`, or `Uninstall`/`remove`, and their dispatch arms and `*_AFTER_HELP` constants are gone
**And** `cli/{init,install,search,publish,upgrade,list,show,doctor,uninstall}.rs` and the support modules `skills_sh.rs, discovery.rs, install_target.rs, manifest.rs, lockfile.rs, git.rs, skill.rs` are deleted, and `cli/mod.rs` no longer declares them
**And** `error.rs` retains the `Agent*` family and `SelfUpdateFailed` but drops the skill-only variants (`Manifest*` for skills.json, `Lockfile*`, `Git*`, `Skill*`, `InstallInvalidFormat`, …) — the `AgentManifest*` adapter.toml variants are explicitly preserved
**And** `crates/kt/tests/{adoption_cli,install_default,install_fallback,publish}.rs` are deleted and `tests/helpers/mod.rs` keeps only the agent helpers (`TestContext::new`, `run_kt_agent`, `run_kt_agent_with_env`, `KtRun`); `tests/agent_cli.rs` is unchanged (it has zero skill-manager references)
**And** `Cargo.toml` `[dependencies]` are pruned to those still used after the excision (compiler + `cargo-machete`/`cargo-udeps` verified), while every dependency still used by the agent or `self-update` path is retained
**And** `kt agent …` and `kt self-update` behave exactly as before, and all nine CI gates are green (build, fmt, clippy `-D warnings`, `test --all-targets`, tarpaulin `--fail-under 95` on `src/` proven locally per #101, crate-visibility, semver-check, currency grep-lint, MSRV 1.96.1)

### Story 9.2: Reposition the top-level `kt` identity to the agent runner

As an Operator,
I want `kt --help`, `kt --version`, and the crate metadata to present Ktesio as the agent runner,
So that the tool describes itself the way the README and `docs/` already do, with one canonical way to list/show the Fleet.

**Acceptance Criteria:**

**Given** the removals from Story 9.1
**When** identity repositioning lands
**Then** the top-level clap `about` (`main.rs`) no longer says "Agentic skills package manager" and instead describes the agent runner, and `crates/kt/Cargo.toml` `description` + `keywords` match (no "skills"/"package-manager" framing)
**And** `kt --help` lists only agent-runner-relevant top-level commands (`agent`, `self-update`) with no retired skill command present
**And** top-level `kt list` / `kt show` are removed, making `kt agent list` / `kt agent show` the single canonical surface (matching `docs/commands.md`) — see the reconciliation decision in the proposal
**And** the `main.rs` unit tests are rewritten: `test_cli_subcommands_exist` asserts the new surface (`agent`, `self-update` present; the nine retired names absent), `test_subcommand_help_includes_details_and_examples` iterates the surviving commands, and `test_cli_help_includes_license_and_repository`, `test_self_update_skips_passive_update_check`, and every `test_agent_*` test stay green
**And** new `CHANGELOG.md` / `RELEASE_NOTES.md` entries state the retired commands and the removal version (FR-38 "removal at a stated version") — historical entries untouched

### Story 9.3: Reconcile the stale architecture and skill-manager docs (architect-owned)

As a reader of Ktesio's docs,
I want the architecture document and the residual skill-manager docs to describe only the shipped product,
So that no doc contradicts the retired CLI, and the AD-16 / Epic-8 skills plan reflects reality.

**Acceptance Criteria:**

**Given** `docs/architecture.md` L136-222
**When** the reconciliation lands
**Then** the stale Modules block, the Install/Search/Publish/Upgrade Command Flow, the skill-oriented Design Choices, and the See-Also links to `manifest.md`/`lockfile.md` are re-authored to the agent-runner architecture or removed — no `skills_sh.rs`/`lockfile.rs`/`manifest.rs` or `skills.json`/`skills.lock` reference remains as current architecture
**And** `docs/lockfile.md` is removed or repurposed and `docs/manifest.md` is verified (skills.json → removed, or already `adapter.toml` → kept) with the architecture See-Also links corrected
**And** AD-16 and Epic 8 are updated to reflect the retirement: Story 8-4 marked superseded, Story 8-1's "relocate existing kt modules" premise replaced by "build agent skill-provisioning in `engine::skills`," and Stories 8-2/8-3/8-5 re-anchored (proposals recorded for Islam's ratification, not applied unilaterally to the PRD)
**And** `commands.md`, `get-started.md`, `installation.md`, `troubleshooting.md`, `README.md` are confirmed free of retired-command references

*Ownership: architect (Winston) — edits the architecture spine and re-scopes AD-16/Epic-8. The dev may execute the pure `docs/architecture.md` tail rewrite under the architect's direction.*

### Story 9.4 — NOT PURSUED (Option A chosen): Emit kind removal notices for retired commands

> **Resolved 2026-07-14 (ratified re-scope, Story 9-3 Part B):** this story was gated on Islam picking Option B (hidden removal-notice stubs) in the sprint-change-proposal's Section 6 decision. Epic 9 shipped Option A instead (a clean release-boundary break — no stubs, no in-tool notice; see the amended FR-37/FR-38 above and `main.rs`, which carries no hidden command variants for the retired names). This story is **not pursued**; kept below as a record of the alternative that was considered.

As a v0.5.0 user upgrading to the pivot,
I want a clear one-line message when I run a retired command,
So that I am told the tool became an agent runner and where to migrate, instead of a bare "unrecognized subcommand" error.

**Acceptance Criteria (not implemented — story not pursued):**

**Given** a retired command name (e.g. `kt install`)
**When** it is invoked on the pivot release
**Then** `kt` exits non-zero with exactly one stderr line naming the retirement, the replacement, the migration doc, and the removal version (hidden behavior-free clap stubs or a top-level unknown-subcommand interceptor — no skill machinery retained)
**And** the notice is covered by a test and appears on stderr only

## Tracking

Synced to GitHub 2026-07-02: Project [Ktesio #5](https://github.com/users/iMagdy/projects/5) (linked to Ktesio/ktesio) · epics = issues #55–#62 · stories = issues #63–#99 (issue titles carry the BMAD keys; bodies mirror this file and are BMAD-managed — edit here, re-run the sync script). Full key→issue map: `_bmad-output/implementation-artifacts/github-sync-map.json`. Sync tool: `_bmad-output/implementation-artifacts/github_sync.py` (idempotent).


## Epic 10: Consolidate & Harden the Embedding Surface

The post-release hardening pass: pay down the deferrals the epic-7 suites accumulated, give hosts
a first-class diagnostic surface, and turn the event bus's documented crash-window limitation into
a one-call remedy. Opened 2026-09-10 by sprint change proposal
(sprint-change-proposal-2026-09-10.md, Islam-approved).

### Story 10.1: One test-support home for the embedding suites
Consolidate the manifest builders and drain helpers scattered across the epic-7 suites into a
single parameterized test-support module in ktesio-conformance (the embedding quickstart stays
deliberately standalone). AC: no duplicate builder/drain logic outside the support module; all
suites green; boundary graph unchanged. (Issue #164.)

### Story 10.2: Host-provided diagnostic sink
Engine gains an optional diagnostic sink — the two AD-12 diagnostics (DC-10 memory-delivery
notice, enforcement breadcrumb) route to the sink when a host provides one, defaulting to stderr
otherwise. Removes both embed_clean audit allowlist entries. Additive public API, documented.

### Story 10.3: Event-bus resync helper + ratified subscriber-active budget
(a) Subscribe-with-backfill: on subscribe, backfill from the committed logs before live events —
the crash-window at-most-once limitation becomes a one-call remedy, tested with a
crash-window simulation. (b) Measure subscriber-active overhead in the perf harness and ratify a
subscriber-active budget from the measurement.


## Epic 11: Technical Debt & Process Cleanup

The post-release hardening sprint: 65 open retro action items from epics 1-7 organized into 7
thematic stories, each sized for one focused session. Opened 2026-09-10 by sprint change proposal
(sprint-change-proposal-2026-09-10-cleanup.md, Islam-approved).

### Story 11.1: Engine robustness batch
11 targeted engine fixes from retro items AI-4/7/8/9/12/13/14/15/16/41/44: error-message precision,
pause/resume ordering, poll-error surfacing, adoption budget re-evaluation, N+1 read elimination.

### Story 11.2: Config & secrets batch
Atomic config writes, env-shadowing warn, secret-to-flag warn, leading-dash accept.

### Story 11.3: Memory robustness batch
memory.dir strip gap, migration crash-atomicity, registry reverse-conflict test, adoption
stranding fix.

### Story 11.4: Docs, docs-gate & process batch
Honest-state pattern doc, review rules, away-mode drill, MSRV contributor docs, table-prefix
naming, docstring fixes, metering/label fixes, rustdoc failure.

### Story 11.5: Cross-platform & CI batch
Survival test parity, _live cross-OS robustness, 3-OS matrix timing, docs probe, coverage gate.

### Story 11.6: Supervision & operations decisions
PRODUCT DECISIONS needed from Islam: daemon/detach, HTTPS upstream, Dependabot triage, tracing
exposure documentation.

### Story 11.7: Orchestration & review process batch
test_automation pins, review rules adoption, sprint-status sync, playbook patterns.


## Epic 12: Durable Detach & the Production-Usable Observed Channel

> **(2026-09-15 — OPENED by Islam's ratification, "recommended across the board", of
> `ai-20-ai-47-product-calls-2026-09-15.md`.)** AI-20 and AI-47 — the two product calls
> epic-11's §6 gated — decided: detach via the adoption path (option b), streaming usage
> parse lands before the rustls TLS upstream, provider schemas defer behind a parse seam,
> and the stranded observed-drain defer (epic-11 retro F3) folds in as 12-4. Execution
> order: 12-1 (independent, highest user-facing value) → 12-2 → 12-3 → 12-4.

### Story 12.1: Detached start survives the CLI exit (AI-20, ratified option b)
`kt agent start --detach` persists `running` and drops the handle WITHOUT the process-group
kill — the agent survives the CLI exit and the next command adopts it through the existing
AD-5 fingerprint path. Detach v1 REFUSES engine-observed instances with an explanatory
error (their loopback listener dies with the command — the same loud strand as adoption);
enforcement windows (no crash detection / budget enforcement / event delivery between
commands) are documented honestly in `--help` + the stderr notice.

### Story 12.2: Streaming usage parse (AI-47b)
The forward listener requests `stream_options.include_usage` on forwarded stream requests
and parses the terminal SSE usage frame into the same `ParsedUsage` choke point (O(1)
line-scanner; the injected `stream_options` documented as an upstream-visible
modification); the observed_metering std-only upstream stub gains SSE cases.

### Story 12.3: HTTPS upstream via vendored rustls+ring (AI-47a)
`metering.upstream_base_url` accepts `https://` (rustls+ring, vendored — no system TLS
dependency; the bundled-SQLite C-build precedent settled the philosophy question). The
start-time `https://` refusal error is replaced by a working forward; supply-chain teeth
(audit/boundary review) updated for the new dependency tree.

### Story 12.4: Observed-drain durability (retro F3 fold)
The observed drain gets the AI-41 treatment the self-reported channel has: park + bounded
retry + the loud SKIPPED diagnostic instead of silent best-effort loss under store failure
(closes the deferred-work entry routed "→ 11-6", stranded by 11-6's partial landing).