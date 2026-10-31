---
stepsCompleted: [1, 2, 3, 4]
status: complete
inputDocuments:
  - _bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md
  - _bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/addendum.md
  - _bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md
  - _bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md
  - _bmad-output/planning-artifacts/sprint-change-proposal-2026-10-30-control-plane-pivot.md
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
FR-7: Pause/resume with honest per-Adapter (and per-OS) semantics: guaranteed / best-effort / unsupported, surfaced not silent (amended 2026-10-30: a wire hold may stand in for native pause when the Run has a wire path, surfaced as qualified)
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
FR-19: Metering ingestion per declared Metering Source (self-reported | engine-observed); no-metering Adapters rejected at registration (amended 2026-10-30: wire and announced Runs metered at the Metering Proxy; observed Runs from their own telemetry)
FR-20: Rate supply (input/output $/1M) and dollar derivation; no retroactive repricing; inert-with-notice when no Rate
FR-21: Cost Cap enforcement executing the Breach Action within the latency bound; warn/pause/stop/block (amended 2026-10-30: default `warn` with loud signals; instances relying on the implicit default keep `pause` on upgrade)
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
FR-40: ACP backend support — any ACP v1 agent via the builtin `acp` kind; tiered usage acquisition, never fabricated (added 2026-09-19; landed in the PRD 2026-09-30)
FR-41: Service host mode — a long-running host owning supervision continuously, driven by a versioned Control API (added 2026-09-30; amended 2026-10-30: Handoff Mode, TLS beyond loopback, the CLI auto-starts a local Service Host)
FR-42: Service-grade lifecycle — readiness and liveness probes, exit-code classes with a stable-window reset, cooperative pause, drain (Adapter Contract v1.1.0; added 2026-09-30)
FR-43: Governance at the wire and per unit of work — child-Run budgets (Task absorbed 2026-10-30), the `block` fence, content-free per-call records with run id, Control Depth and token id (added 2026-09-30)
FR-44: Portable governance semantics — versioned Governance Vectors verified against the evaluator (added 2026-09-30)
FR-45: Reconciliation with confirmed actuals — the `reconciled` label's first producer (added 2026-09-30)
FR-46: OTLP telemetry export — opt-in, operator-configured collector; labeled metrics only, never content (added 2026-10-23)
FR-47: Agents-in-CI recipe — budget-capped, wall-clock-bounded, exit-code-honest GitHub Actions lanes with usage in $GITHUB_STEP_SUMMARY (added 2026-10-23)
FR-48: Adapter gallery — docs-curated agents validated by hekma-conformance in CI, with per-capability honesty tables (added 2026-10-23)
FR-49: Runs as first-class entities — run id, Control Depth (observed < wire < announced < supervised), labels with source, Run Status, optional parent; verb × depth table (added 2026-10-30)
FR-50: Listing and verbs at scale — `hekma ps` with CONTROL column, exception-first view, keyset paging, Selectors on every verb with set resolution, `hekma budget`, Control API parity (added 2026-10-30)
FR-51: Discovered Runs — `wire` via the Metering Proxy base URL, `observed` via inbound OTLP; endpoint label bindings; unattributed traffic; identity headers to gateways (added 2026-10-30)
FR-52: Self-announce and Leases — three announce channels, one Run per identity, Leases, `silent`, `ended` (added 2026-10-30)
FR-53: Run identity and key custody — the Run Key is the provider API key the agent holds; custody by default; Scent-Check Modes off/shadow/enforce; Callers only tighten (added 2026-10-30)
FR-54: Bypass detection — Anomaly on direct provider connections, honest per OS, signals only (added 2026-10-30)
FR-55: Process continuity across Service Host restarts — Shims, Handoff Mode, holds belong to the Run, crash recovery (added 2026-10-30)
FR-56: Wire hold, release, reroute and reject — Run-level holds with a held-call cap (added 2026-10-30)
FR-57: `hekma top` — live terminal view with hotkeys over the Control API (added 2026-10-30)
FR-58: Nodes and Hub — one view and control of every machine (added 2026-10-30)
FR-59: Telemetry at Run scale — no run id on metrics, allow-listed labels, exemplars (added 2026-10-30)
FR-60: Policies — standing limits by Selector; most specific wins per limit; default Policy; bound labels only (added 2026-10-30)

### NonFunctional Requirements

NFR-1: Resilience — agent crashes never crash the Engine/kt; orphan cleanup/adoption after Engine crash; graceful per-instance degradation with reasons + remediations
NFR-2: Cross-platform parity (Linux/macOS/Windows); OS gaps get closest-equivalent + documented difference (per-OS capability honesty; OS-conditional code only in backends per spine AD-4/conventions)
NFR-3: Test coverage ≥95% on src/, CI-enforced (cargo tarpaulin) — non-negotiable
NFR-4: Performance budgets (validated as testable targets by architecture): read commands <1s @25-instance Fleet; supervision overhead ≤2% CPU, ≤50MB RSS steady-state per running instance
NFR-5: Observability — structured, timestamped, attributed, rotation-bounded logs; stdout=output, stderr=diagnostics
NFR-6: Security & privacy — secrets per FR-14; Agent Home isolation is process/filesystem-level, NOT a security sandbox (boundary stated in docs)
NFR-7: Documentation currency — docs/README update in the same change; Adapter Contract + Embedding Interface docs version with the code
NFR-8: Runtime & dependency policy — Rust 2021+; lean deps; new deps tokio + rusqlite architecture-justified (Islam sign-off pending, non-blocking)
NFR-4 (amended 2026-10-30): Run-scale budgets — first page of `hekma ps` < 1s at 1M Runs; Metering Proxy adds ≤ 10ms p99; measured at 10k / 100k / 1M Runs; 10M Runs per Hub design target
NFR-9: No unrequested stops — Hekma stops, pauses or blocks a Run only on a verb, an opted-in Breach Action or `enforce`, a declared probe failure, or Service Host shutdown with Handoff Mode off (added 2026-10-30)

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

From the spine's 2026-10-30 amendments (control-plane pivot) — these bind Epics 21–25:

- One data-path owner per state directory: the Service Host runs the Metering Proxy, the store writer and the snapshot publisher; the CLI auto-starts a local loopback Service Host and routes every write through the Control API (AD-20)
- Credential resolver with an authority matrix: operator/Host bearer, announce-scoped, Run Key, refresh, proxy-access, Node (AD-20); TLS required beyond loopback (AD-20, AD-26)
- The Run Key is the provider API key the agent holds; opaque 256-bit tokens hashed at rest, token ids in records, never in a URL; key custody by default for supervised Runs whose key Hekma holds (AD-22)
- One Shim per supervised process (`hekma shim`), shim `ProcessBackend`, socket access control, protocol versions, Handoff Mode releases never kills (AD-23)
- Run registry, labels with source, child Runs, dispositions with precedence, run identities, Policy resolution as one core function, one snapshot publisher (AD-25)
- One shared Metering Proxy replacing per-instance listeners: identification order, bindings with upstreams, unattributed Run, per-call records and `open_calls`, wire contract `proxy-wire v1` (AD-26)
- Store writer is the only state.db writer and runs the AD-7 pipeline in batches; outbox into state.db; rotating calls store; keyset reads; ended-Run compaction; new port trait (AD-27)
- Inbound OTLP/HTTP+JSON receiver for `observed` Runs (AD-28); `otlp-metrics v2` (AD-21)
- AD-18 reopen trigger (4) fired: archived per-instance plan Steps 0–4 activate

### UX Design Requirements

None — no UX design contract exists. This is a CLI + embeddable-library product; the PRD reviewer confirmed no UX phase is required. Terminal UX conventions (miette diagnostics, ui.rs patterns, stdout/stderr discipline) are carried as ADOPTED conventions from the existing codebase via the spine.

### FR Coverage Map

FR-1: Epic 1 — Register an Agent Instance
FR-2: Epic 1 — Isolated Agent Home
FR-3: Epic 1 — Unregister/remove
FR-4: Epic 1 — Fleet visibility
FR-5: Epic 1 — Start
FR-6: Epic 1 — Stop with escalation
FR-7: Epic 1 — Pause/resume honesty; amendment (wire hold stands in for native pause) → Epic 23 (23.4)
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
FR-19: Epic 3 — Metering ingestion (both sources); amendment (wire, announced and observed metering) → Epic 22 (22.2) + Epic 23 (23.1, 23.3)
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
FR-40: Epic 14 — ACP backend support
FR-41: Epic 17 — Service host mode
FR-42: Epic 18 — Service-grade lifecycle
FR-43: Epic 19 (19.2, 19.4) + Epic 21 (21.2 child Runs) + Epic 23 (23.4) — Governance at the wire and per unit of work
FR-44: Epic 19 (19.1) — Portable governance semantics
FR-45: Epic 19 (19.5) — Reconciliation with confirmed actuals
FR-46: Epic 20 (20.1) — OTLP telemetry export
FR-47: Epic 20 (20.2) — Agents-in-CI recipe; amendment (the recipe sets `stop` explicitly) → Epic 21 (21.3)
FR-48: Epic 20 (20.3) — Adapter gallery
FR-49: Epic 21 (21.1, 21.2, 21.9) — Runs as first-class entities
FR-50: Epic 21 (21.5, 21.6) — Listing and verbs at scale
FR-51: Epic 23 (23.1 incl. `hekma binding`, 23.3) + Epic 21 (21.8) + Epic 22 (22.5) — Discovered Runs
FR-52: Epic 23 (23.2) — Self-announce and Leases
FR-53: Epic 22 (22.1, 22.2, 22.3) — Run identity and key custody
FR-54: Epic 22 (22.4) — Bypass detection
FR-55: Epic 24 (24.1, 24.2, 24.3) — Process continuity
FR-56: Epic 23 (23.4) — Wire hold, release, reroute, reject
FR-57: Epic 21 (21.7) — `hekma top`
FR-58: Epic 25 — Nodes and Hub
FR-59: Epic 24 (24.5) — Telemetry at Run scale
FR-60: Epic 21 (21.3, 21.9) — Policies
FR-21 (2026-10-30 amendment): Epic 21 (21.3) — `warn` default and the upgrade write
FR-41 (2026-10-30 amendment): Epic 21 (21.4) CLI auto-start + Epic 24 (24.2) Handoff Mode

NFR-9 → Epic 24 (24.2, 24.3) + every Epic 21–24 DoD; NFR-4 Run-scale amendment → Epic 21 (21.1) + Epic 24 (24.4).

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

### Epic 21: One Control Plane *(opened 2026-10-30 via correct-course)*
Runs become first-class: one store writer, Run registry, Policies and the `warn` default; the CLI routes through one local Service Host; `hekma ps`, Selectors, `hekma budget` and `hekma top` act on any Run the same way. **FRs covered:** FR-49, FR-50, FR-57, FR-60, FR-21 and FR-41 amendments, part of FR-43 and FR-51.

### Epic 22: Run Identity and Key Custody *(opened 2026-10-30 via correct-course)*
Run Keys that double as the agent's provider API key, key custody by default, Scent-Check Modes, bypass detection and an authenticated proxy beyond loopback. **FRs covered:** FR-53, FR-54, part of FR-51.

### Epic 23: Attach and Wire Control *(opened 2026-10-30 via correct-course)*
One shared Metering Proxy, self-announce and Leases, inbound OTLP for `observed` Runs, and wire hold / release / reroute / reject. **FRs covered:** FR-51, FR-52, FR-56, part of FR-43.

### Epic 24: Operability at Scale *(opened 2026-10-30 via correct-course)*
One Shim per supervised process, Handoff Mode and in-place upgrades, crash recovery, the scale substrate and benchmark, and `otlp-metrics v2`. **FRs covered:** FR-55, FR-59, NFR-4 amendment, NFR-9.

### Epic 25: Nodes and Hub *(opened 2026-10-30 via correct-course; placeholder)*
One view and control across machines. Detailed when Epic 24 closes and AD-24 is drafted. **FRs covered:** FR-58.

### Epic 20: Adoptability *(opened 2026-10-23 via correct-course)*
Opt-in OTLP metrics export into existing observability stacks, a budget-capped agents-in-CI recipe dogfooded in this repository's CI, and a conformance-validated adapter gallery with honesty tables. **FRs covered:** FR-46, FR-47, FR-48.

### Epic 19: Wire Governance for Hosts *(opened 2026-09-30 via correct-course)*
A Host shares the Engine's decision rules as portable vectors, fences further model calls when a
ceiling is reached, reads per-call facts, and feeds back confirmed actuals. *(Re-scoped 2026-10-30:
Task scope moved to Epic 21 as child Runs.)* **FRs covered:** FR-43, FR-44, FR-45.

### Epic 18: Service-Grade Lifecycle *(opened 2026-09-30 via correct-course)*
Readiness and liveness probes, exit-code classes with a stable-window reset, cooperative pause
commands and a drain verb — an additive Adapter Contract minor (v1.1.0) under which a gateway-style
service agent is a plain manifest adapter. **FRs covered:** FR-42 (+ FR-9 amendment).

### Epic 17: Service Host *(opened 2026-09-30 via correct-course)*
A long-running host that owns supervision continuously and serves a local, versioned Control API;
state export for ephemeral disks; static Linux releases and a container recipe. **FRs covered:**
FR-41.

### Epic 14: ACP Backend Support *(opened 2026-09-19 via correct-course)*
### Epic 13: Engine Correctness & the Supervisor Module Boundary *(opened 2026-09-19)*
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

Synced to GitHub 2026-07-02: Project [Ktesio #5](https://github.com/orgs/Ktesio/projects/3) (linked to Ktesio/ktesio) · epics = issues #55–#62 · stories = issues #63–#99 (issue titles carry the BMAD keys; bodies mirror this file and are BMAD-managed — edit here, re-run the sync script). Full key→issue map: `_bmad-output/implementation-artifacts/github-sync-map.json`. Sync tool: `_bmad-output/implementation-artifacts/github_sync.py` (idempotent).


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

## Epic 13: Engine Correctness & the Supervisor Module Boundary

> **(2026-09-19 — opened to close the two HIGH retro defects and the twice-carried
> split study.** Story 13.1 closes epic-5 retro A1 (the `memory.dir` strip gap on the
> invocation-override path — the one real correctness finding of the epic-5 retro);
> story 13.2 closes epic-6 retro B1 (the legacy start fallback re-reads `adapter.toml`
> without renegotiating, bypassing the 6-6 load gate); story 13.3 is the supervisor
> module-boundary study carried since epic-11 (F-agg2) and named in AD-18's carried
> debt — its spec-stage deliverable is a bounded-options boundary proposal;
> implementation of the chosen boundary is ratification-gated per AI-58. All paths
> post-rename (`crates/hekma-engine`).)

### Story 13.1: Close the memory.dir strip gap on the invocation-override path (epic-5 retro A1)
The operator-spoofed `memory.dir` strip (`start_inner`'s 2b-memory-spoof block) covers the
local effective-config path only; when invocation overrides exist (engine-observed
instance), the re-fold can resurrect a hand-set `memory.dir` into `apply_config_mapping`,
delivering through a declared mapping (hermes → `HERMES_HOME`) with no DC-10 notice.
Fix: make delivery refusal uniform across both config paths (strip or refuse identically),
surface the degradation honestly, pin the strip with tests at BOTH layers (none exists
today), and correct the stale docs claim.
**Resolution (2026-09-19):** the DELIVERY defect was already closed by story 11-3
(`bee7d48`) — the override-branch strip exists (`supervisor.rs:1191-1205`, comment
"(2b-memory-spoof, the override branch — story 11-3, A1)") and its engine-observed pin
(`tests/observed_metering.rs:826`) mutation-verifies; the retro's "no test pins any
layer" was half-stale. 13-1's remaining truth: the LOCAL-path strip (5-1's E4) shipped
with no pin — landed here (`tests/memory.rs::a_hand_set_memory_dir_never_reaches_a_plain_start_surfaces`,
mutation-verified, positive-controlled). Tracker + retro item reconciled closed.

### Story 13.2: Renegotiate the contract on the legacy start fallback (epic-6 retro B1)
The launch-less/legacy snapshot fallback re-parses `adapter.toml` at start time without
calling `negotiate_contract_version`, so a manifest edited to an incompatible
`contract_version` after registration starts via the fallback, bypassing the 6-6 load
gate. Fix: negotiate on the fallback re-parse (mapping to the existing
fallback-incompatible error surface) + a bypass test (registered compatible, edited
incompatible, start via fallback → refused).
**Resolution (2026-09-19):** already closed by `7b1a70c2` (PR #172, 2026-09-07, retro
item #2) — the fallback re-parse negotiates (`adapter/mod.rs:338-352`), surfaced as
`LaunchResolveError::ContractIncompatible`, with the bypass tests
(`resolve_start_launch_renegotiates_a_drifted_contract_major` / `..._still_accepts_same_major`)
and the docs correction all landed there; verified at HEAD, no code change in epic-13.
Tracker + retro item reconciled closed.

### Story 13.3: Supervisor module boundary — the split study (epic-11 F-agg2, AD-18 carried debt)
`crates/hekma-engine/src/domain/supervisor.rs` is ~9.1k lines (4.8k production) after
growing +36% in epic-11 and +~1k in epic-12. The spec-stage deliverable is a COMPLETE
bounded-options proposal for a deliberate module boundary (AI-58: draft-then-ratify) —
options concretely stated with trade-offs, a recommendation, and a migration shape that
keeps every step compile-gated and independently revertible. Implementation follows only
after Islam ratifies a boundary.

## Epic 14: ACP Backend Support

> **(2026-09-19 — opened via correct-course from Islam's directive; Sprint Change Proposal:
> `sprint-change-proposal-2026-09-19-acp.md`, APPROVED with decisions D1–D6.)** Any agent
> speaking the Agent Client Protocol (ACP, Zed's JSON-RPC-over-stdio standard, v1) becomes a
> Hekma backend through a new builtin `acp` kind — minimal integration friction, no per-agent
> adapter authoring. Serves SM-1 (cross-agent operability) and SM-5 (adapter effort ~zero for
> ACP-capable agents); adds FR-40 to the PRD. The frozen adapter contract v1 is NOT engaged
> (builtin kinds don't negotiate — epic-6 retro B3 precedent). All paths post-rename
> (`crates/hekma-*`). Batch-epic shape (10–13 precedent).

### Story 14.1: ACP transport core — the builtin `acp` kind (D1, D3, D6)
Spawn the agent under existing supervision; run the ACP `initialize` handshake (pin the ACP version RANGE at the spec stage (docs claim v1; Hermes tracks 0.8.1–<1.0);
negotiate the version at `initialize`, refusing unsupported ones with a surfaced, traffic-free error — the
6-6 shape transposed); advertise MINIMAL client capabilities (baseline only per D3 — no
`fs.*`, no `terminal`, no `elicitation`; un-advertised capability requests get typed,
surfaced refusals); `session/new` at start; `send` = a `session/prompt` turn; the
`session/update` stream routes into the existing per-instance output log + events; stop =
`session/cancel` then the engine's normal termination ladder; pause/resume via the existing
process semantics (SIGSTOP parity unchanged). JSON-RPC framing is OS-uniform stdio (AD-4:
no per-OS cfg outside `backends/`); the JSON-RPC dependency question settles under NFR-8's
lean policy at the spec stage.

### Story 14.2: Sessions across lifetimes (D4)
Persist the last ACP session id alongside the spawn record (additive schema extension);
on re-adoption, resume via `session/load` when the agent advertises `loadSession`, else
`session/new` with an honest stderr note. Detach (epic-12) composes: the process survives
the CLI exit; the session resumes on the next command's reattach.

### Story 14.3: ACP usage acquisition — tiered, honest (D2, AMENDED: metering is critical)
The `acp` kind acquires REAL token usage tiered — `—` is the surfaced LAST RESORT, never the stance:
(T1) parse the de-facto usage channels on the ACP stream — usage-bearing `session/update` chunks
(Claude-adapter shapes carry Anthropic usage fields) and usage-update notifications (Gemini/ADK
`usageMetadata` shapes), tolerant of absence and `_meta` extension shapes, tracking the draft
"End-Turn Token Usage" RFD for standardization; (T2) the engine-observed loopback channel (3-4)
where the agent honors a provider base-URL override; (T3) the optional stderr KTESIO_USAGE
sentinel (cooperative agents — 14.5's mode). When NO tier yields usage, the Fleet/config surfaces
show the honest `—` (METERING_SEED_CELL pattern) with a surfaced notice naming the gap and the
tiers attempted — never fabricated counts, never silent absence (AD-8, AI-18). Config mapping
(env/args) and Memory Backing delivery unchanged; `--help`/notice honesty throughout.

### Story 14.6: Cached tokens across the metering pipeline (D8, ACP-independent — FOUNDATION)
The gap predates ACP and bites every kind: `metering/parse.rs` explicitly ignores
`prompt_tokens_details` (OpenAI's `cached_tokens` home) and `total_tokens`; the `Rate` cost
model carries only input/output prices; the ledger has no cached column. This story:
ParsedUsage parses the three provider cached shapes (OpenAI `prompt_tokens_details.cached_tokens`;
Anthropic `cache_read_input_tokens` + `cache_creation_input_tokens`; Gemini
`cachedContentTokenCount`) — absence of a known field parses as known-zero, unknown fields stay
uninterpreted (honest lower bound); the ledger gains an additive nullable `cached_tokens` column
(next migration after v6, additive-nullable precedent); `Rate` gains an OPTIONAL cached rate —
unset prices cached tokens AT THE INPUT RATE (documented conservative default: overstates cost,
never understates), set prices at the cached rate; token budgets count cached tokens; fleet and
config surfaces distinguish the cached contribution. Runs FIRST after 14.1 — the ACP acquisition
story (14.3) lands on this foundation.

**Execution order (amended 2026-09-19 — metering is critical):** 14-1 (transport core) →
**14-6 (cached-token pipeline foundation)** → 14-3 (ACP usage acquisition) → 14-2 (sessions
across lifetimes) → 14-5 (hermes deprecation path) → 14-4 (verification & docs).

### Story 14.5: Hermes-kind deprecation path (D7 — Islam's amendment, Hermes speaks ACP)
The bespoke `hermes` builtin (epic-6) is a special case of `acp`: Hermes Agent ships a first-class
`hermes-acp` stdio entry point (listed on agentclientprotocol.com). Land the retirement path, not a hard removal:
(a) the `acp` kind gains an OPTIONAL self-reported metering mode reading the KTESIO_USAGE sentinel from the shared
stderr agent.log (hermes cooperates; D2's default stays unmetered), and the HERMES_HOME memory-delivery proof is
re-established under the `acp` kind; (b) docs announce the deprecation — new registrations steer to `--kind acp`,
the `hermes` kind keeps working, actual removal at a later MAJOR per the CLI-surface policy (no stranded instances,
no silent metering loss); (c) real-hermes-over-acp verification joins 14.4's smoke matrix.

### Story 14.4: Verification & docs
A fake ACP agent binary in `hekma-conformance` (scriptable JSON-RPC over stdio) drives the
test matrix: handshake, prompt turn on the wire, update capture, cancel/stop, load/no-load
adoption (14.2), un-advertised-capability refusal (D3), protocol-major refusal (D6).
Real-agent smoke against `gemini --acp` when present (skip otherwise, surfaced). Docs:
commands.md (the `acp` kind), supported-agents page, architecture AD-19 (spine; seed text
in the proposal), adapter-contract scope sentence (builtins don't negotiate — B3), metering
honesty note. Release-surface announcement per AI-55 (new kind, additive schema, no break).

## Epic 17: Service Host

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> The first production Host — a hosted container platform, the UJ-3 archetype — needs supervision
> that outlives a command and a local surface to drive it. AI-20 deferred exactly this "as a deliberate
> future epic when embedder demand … materializes"; PRD §13 Q6 asked for its delivery path. Realizes
> FR-41. Batch-epic shape (the 10–14 precedent): full ACs are derived at the spec stage.

### Story 17.1: Host-mode spec — the shim crate and the lock model (D1, D2; spec stage, AI-58)
Settle the two decisions this proposal leaves to measurement:
1. **The host crate.** Its name, its HTTP stack under an NFR-8 review, and its AD-2 boundary entry.
   The CLI stays synchronous and the engine transport-agnostic.
2. **The lock model.** AD-18's reopen trigger (1) fires here. Measure the serve shape against NFR-4 by
   the 7-5 method, then either re-ratify the coarse two-mutex model with that evidence or activate the
   archived per-instance plan.

Deliverables: the AD-20 spine text, and a bounded-options proposal for Islam.

### Story 17.2: `hekma serve` — the long-running host
- A foreground host that owns one Engine for its whole life, with a state-directory lock so exactly
  one owner exists.
- A fixed `metering.listen` port option, which also closes the adoption strand.
- A child environment allowlist: agents inherit only what is declared, not the host's whole
  environment.
- SIGTERM drains then stops every instance within a bounded window, and the host never SIGKILLs an
  agent on its own exit before that window ends.
- Operator-visible notices through the diagnostic sink.

### Story 17.3: Control API v0 (D3)
- An HTTP/JSON projection of the public facade — lifecycle, config and budgets, fleet and status,
  usage, events — plus `drain` and the Task verbs once Epics 16 and 17 land.
- Explicit bind; a bearer resolved through `SecretResolver`; no secret reveal and no raw agent output
  unless explicitly enabled.
- Events over SSE, and pull by cursor.
- Versioned and marked unstable (`0.x`) until a second Host validates it.
- A conformance-style suite drives the whole API against the fake agents.

### Story 17.4: Export for ephemeral hosts — the outbox and log capture modes
- A global sequence across the transition, breach and usage families, pulled by cursor and
  acknowledged by the Host (AD-14 amendment); the per-family `resync_events` stays.
- The state directory can be placed on a volume the Host chooses.
- Log capture modes `full` (today's behaviour, the default), `metadata` and `off`, with `agent.log`
  rotation bounded like `output.log` (AD-12 amendment). A Host whose agent output may carry user
  content can keep it off disk.

### Story 17.5: Static Linux release and the container recipe
- Static musl `x86_64` release artifacts, checksummed, in the release matrix; `aarch64` is deferred.
- A docs page, "running Hekma as a container supervisor", covering:
  - the host as the container command or as an init-supervised service;
  - how drain meets the container's stop grace;
  - state-directory placement;
  - the cooperative-meter caveat (NFR-6).

## Epic 18: Service-Grade Lifecycle

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> A service agent is ready later than its process exists, can hang while alive, asks its supervisor to
> restart it with an exit code, and must drain before a stop. None of that is expressible today.
> Realizes FR-42 as an additive Adapter Contract minor (v1.1.0). Per the contract's own rule, an
> older engine still rejects a newer same-major manifest that uses a new section — this is stated on
> the release surface (AI-55).

### Story 18.1: Readiness probes
- Declared probes: an HTTP GET with an expected status, a file's age, or a log-line match — each with a
  timeout.
- `starting → running` waits for the probe; a timeout lands the instance in `failed` with the probe
  named (AD-15 amendment).
- Honest per-OS semantics where a probe kind needs them.

### Story 18.2: Liveness and the unresponsive failure
- Declared liveness: a heartbeat file's maximum age, or an HTTP check.
- A failure moves `running → failed` with cause `unresponsive`, and the Restart Policy applies.
- Liveness never probes a paused instance.

### Story 18.3: Exit-code classes and the stable-window reset
- Declared restart-requested codes (e.g. 75) relaunch without charging the crash loop.
- Declared clean codes are not crashes.
- The consecutive-failure counter resets after a stable running window.
- FR-9's `always` is recorded as subsumed.

### Story 18.4: Cooperative pause commands and the drain verb
- A declared pause/resume command pair (e.g. a sentinel-writing CLI verb) drives pause and resume.
  It is still surfaced as best-effort when it only stops new work.
- `drain` = stop new work (the pause command, if declared) → wait for idle, bounded → stop. The
  outcome records completed versus timed out, and no new lifecycle state is added.

### Story 18.5: A service-mode gateway as a manifest adapter
- A reference manifest for a gateway-style service agent: an API server, readiness on its models
  endpoint, heartbeat liveness, exit-75 restart, and a cooperative pause.
- A conformance-kit fake that behaves like one, with TCK sections for readiness, liveness, exit
  classes and drain.
- The `hermes`-kind deprecation (14-5) gains its service-mode successor here: removal waits for this
  story (AD-19 amendment).

## Epic 19: Wire Governance for Hosts

> **(2026-09-30 — opened via correct-course: `sprint-change-proposal-2026-09-30-hosted-platform-readiness.md`.)**
> The Host enforces money at its own external choke point and wants Hekma's rules there, provably. It
> also wants Hekma's cooperative meter to fence, to attribute usage to its own units of work, and to
> reconcile against the Host's figures. Realizes FR-43, FR-44 and FR-45. This is money code: every
> story gets the AI-55 two-pass review.

### Story 19.1: Governance conformance vectors — FOUNDATION, runs first (D5, FR-44)
- Versioned, checksummed JSON vectors — ledger state and ceilings in, decisions out — covering:
  - the `≥` threshold;
  - per-scope latches and re-arm;
  - token ceilings before dollar ceilings;
  - integer micro-dollar arithmetic;
  - round-half-up;
  - saturation;
  - no retroactive repricing.
- They are generated from, and verified against, the evaluator in CI, and published so a non-Rust Host
  can pin them.
- Small, and first: the Host's guard waits on them.

### Story 19.2: The wire fence — a `block` Breach Action (D4, AD-7 amendment)
- *(Amended 2026-10-30.)* `block` is never a default Breach Action; once configured it always
  refuses, whatever the Scent-Check Mode (PRD FR-43 amendment). Built on today's per-instance
  listener so the waiting Host is not blocked on Epic 23; Story 23.1 carries it into the shared
  proxy's disposition snapshot so the fence never lapses, and 23.4 reuses its response for `reject`
  (AD-7, AD-26).
- On breach, the pipeline flips a lock-free per-instance flag.
- `forward()` refuses further model calls with a typed, non-retryable response (not 429), until the
  Run ends (operator stop/start); raising a ceiling un-fences a live Run once 21.6 lands.
- The breach event and log record the fence like any action.
- Docs state its cooperative scope (NFR-6).
- *(Readiness 2026-10-30.)* The spec records the refusal response as the first slice of `proxy-wire v1`: a
  status code common SDKs do not retry, a JSON error body with a stable type and code naming
  dimension and scope, never content (AD-7).
- The flag is rebuilt from the durable breach record on engine restart and adoption, as the latch
  is; `block` is refused at configuration on an instance with no listener (self-reported metering).
- The new `BreachAction` variant is a semver-visible engine change: bump per the armed gate. After
  21.1 the store writer sets the flag (AD-27).

### ~~Story 19.3: Task scope — Host units of work~~ *(removed 2026-10-30)*
Absorbed into Story 21.2 as child Runs: one Run concept, no Task/Run split (PRD FR-43 amendment,
spine AD-25). Latch re-arm on a raised ceiling lands with `hekma budget` in Story 21.6.

### Story 19.4: Per-call records for Hosts
- *(Amended 2026-10-30.)* Runs after Stories 22.1 and 23.1: the shared Metering Proxy emits the
  records into the calls store (AD-26, AD-27); this story is their Host read surface.
- For each proxied call: call id, run id, Control Depth, token id, disposition (`pass` or `block`
  until 23.4 lands) and hold end, status class, latency, model when present, tokens, sub-second
  timestamp.
- Never request or response content — the sentinel no-leak matrix is extended.
- Readable through the facade and the Control API.

### Story 19.5: Reconciliation ingestion — the `reconciled` label's first producer (FR-45, AD-8)
- A Host submits confirmed usage for a Run (including a child Run).
- Reconciled figures sit beside estimated rows and never overwrite them.
- Drift is reported, and every surface labels which figure it shows.

## Epic 20: Adoptability

> **(2026-10-23 — opened via correct-course: `sprint-change-proposal-2026-10-23-adoptability.md`.)**
> The brainstorm trigger: Hekma runs agents well, but a newcomer's first hour lacks the artifacts
> adoption runs on — governance metrics do not feed an existing Grafana/Datadog stack, there is no
> recipe for running a governed bot in CI, and no validated index of which agents run under Hekma
> today. Islam approved the epic 2026-10-23 (standard OTLP; gallery in docs; the ten-agent seed
> list; D1 = the hand-rolled OTLP/HTTP+JSON exporter per AD-21; D2 = the PRD §6 amendment; D3 =
> in-repo CI recipe). Realizes FR-46, FR-47 and FR-48. Purely additive: this epic does not disturb
> Epics 18 or 19 — no story touches the lifecycle declarations, the fence, Task scope, the vectors,
> or reconciliation beyond reading their outputs. Batch-epic shape (the 10–14 precedent): full ACs
> are derived at the spec stage. 20.1 is a public-exposition + privacy-surface story: AI-55
> two-pass review applies.

### Story 20.1: OTLP telemetry export (FR-46, AD-21)
- Opt-in, off by default: a configured collector endpoint turns export on and nothing else does —
  no configuration, no egress, asserted by test (the §6 amendment's default).
- Hand-rolled OTLP/HTTP+JSON exporter in `hekma-host` (`src/otlp/`), zero new runtime deps (NFR-8);
  reads the public event bus read-only; derives metrics from the AD-14 payloads; never blocks
  supervision (bounded POSTs, surfaced drops, `hekma.otlp.dropped`).
- Semconv-first naming (`gen_ai.client.token_usage`, `gen_ai.client.operation_duration` once 19.4
  exists) with `hekma.*` only where no semconv exists; every currency metric carries
  `estimate_state: estimated|reconciled` (AD-8 extension).
- Config on one surface (`--otlp-endpoint`, `--otlp-headers` with `secret:NAME`, bounded flush
  interval); versioned exposition documented in `docs/telemetry.md` (`otlp-metrics v1`).
- No content, no secrets: the FR-14/FR-43 no-leak matrix extends to the export path.
- Grafana/Datadog ingest proven in CI against a collector fixture, not asserted from spec
  compliance.

### Story 20.2: Agents in CI — the GitHub Actions recipe (FR-47)
- A docs recipe page: a budget-capped agent lane (Token Budget + Cost Cap, Breach Action
  configured) driving a review-style task under `hekma`, on the static musl artifact (x86_64
  runners pinned).
- Wall-clock bounded (`timeout-minutes`); a lane that ends on a bound says which one — budget or
  clock — in the job output.
- Usage into `$GITHUB_STEP_SUMMARY`, rendered from `--json`: tokens and derived dollars, labeled
  estimated (FR-22/FR-23).
- Exit-code honesty (FR-26): success, breach and agent failure are distinguishable; no path masks
  a breach as success.
- Secret plumbing is first-class in the recipe (auth-gated handshakes refuse at `-32000` when
  logged out — the recipe must show `secret:` wiring, not footnote it).
- Dogfood proof: a Ktesio bot lane runs in this repository's own CI under the recipe — the lane is
  the recipe's test.
- The composite marketplace Action is deferred (trigger: a third-party user asks).

### Story 20.3: The adapter gallery (FR-48)
- Ten seed entries — Copilot CLI, Hermes, OpenCode, Claude Code, Codex, Gemini CLI, goose, Crush,
  Aider, Cursor CLI — each a validated manifest or an honestly named gap, under `docs/adapters/`.
- The honesty table per entry, in the Capability Declaration vocabulary (guaranteed / best-effort /
  unsupported) with explicit absence tokens — never a fabricated pass or implied zero (AI-18);
  tables carry upstream stability status and the re-validation duty (the docs/agents.md
  discipline, generalized).
- A CI validation lane runs `hekma-conformance` over every gallery manifest against bundled
  fake/shim execs (the `hermes_shim` precedent) — CI never launches real agents; a failing
  manifest, or an entry claiming more than its validation proves, fails the build.
- Metering honesty: where no tier yields usage, the entry shows the not-available marker naming
  the tier (the FR-40 precedent), never a blank.
- Public pages carry no planning-provenance wording — enforced by the `check_docs` gate; the
  authoring guide cites `docs/adapter-contract.md` (the conformance crate has no README).
- Additions respect SM-C1: an entry ships only when its controls behave uniformly.

## Epic 21: One Control Plane

> **(2026-10-30 — opened via correct-course: `sprint-change-proposal-2026-10-30-control-plane-pivot.md`.)**
> Hekma becomes the control plane for agent workloads: every agent workload is a Run that the same
> verbs, Control API and telemetry can see and steer. This epic lays the foundations the rest of the
> horizon stands on — one writer, the Run registry, Policies, one data-path owner — and the
> operator surfaces on top. Realizes FR-49, FR-50, FR-57, FR-60 and the FR-21 / FR-41 amendments
> (UJ-4, UJ-5). Batch-epic shape: full ACs are derived at the spec stage. 21.1 and 21.3 are money
> code (AI-55 two-pass review).
>
> **Build order across Epics 19 and 21–25** *(21.9 inserted by the readiness gate, 2026-10-30)*: 21.1 → 21.2 → 21.9 → 21.3 → 21.4 → 21.5 → 21.6 → 22.1 → 23.1 →
> 19.4 → 22.2 → 22.3 → 23.2 → 23.3 → 23.4 → 21.7 → 21.8 → 22.5 → 24.1 → 22.4 → 24.2 → 24.3 → 24.4 →
> 24.5 → 25. Story 19.2 may run at any time but lands before 21.3 (Policies order `block`); 19.5
> follows 19.4. Decisions behind these stories: `implementation-readiness-2026-10-30.md`.

### Story 21.1: The store writer and Run-scale storage (AD-27, NFR-4)
- One store writer task with its own connection becomes the only state.db writer: every write is a
  message acked after commit; the Registry connection becomes read-only.
- The writer runs the AD-7 pipeline in bounded batches (≤ 250 ms or 512 events): usage commit, budget
  evaluation and latches in the same transaction, outbox append, process Breach Actions sent to the
  Supervisor over a channel — never under `Mutex<Supervisor>`.
- The outbox moves from `outbox.jsonl` into state.db (same transaction as the family commit),
  keeping the export contract.
- New port trait for the new reads and writes (`StateStore` stays frozen), shaped for a later
  external database; keyset pagination helpers; usage rollups per instance.
- Dedup keys gain a launch generation, fixing the adoption sequence-reset undercount.
- A `--serve-shape`-style benchmark harness lands here so every later story measures against NFR-4.
- *(Readiness 2026-10-30.)* Transition and breach records move into `transition_events` and `breach_events`,
  committed with the outbox row; `instance.log` and `breaches.log` become best-effort mirrors;
  `resync_events` reads SQLite; the upgrade imports the existing logs once (D1, AD-27).
- The writer takes no engine mutex; acked writes flush at once (the 250 ms / 512 bound is for
  unacked streams); the ack wait is bounded at 1 s, then a typed retryable error; the caller applies
  returned process actions before releasing its locks (D2, AD-27).
- `run_latches` keyed by the run id string (no foreign key until 21.2), seeded on upgrade from
  `breaches.log` for each live instance's current run.
- Until 21.4 the single-writer rule holds per engine process; cross-process writes keep
  `BEGIN IMMEDIATE` and the busy timeout.
- The spec states 21.1's own NFR-4 pass criterion and the harness's seed shape.

### Story 21.2: Runs as first-class entities (FR-49, FR-43, AD-25)
- Run registry beside `Supervisor`; `runs`, `run_labels` (bound | caller source), `run_identities`
  tables; ULID run ids (`run_` prefix), legacy ids still valid.
- Every supervised start projects a `supervised` Run; a Restart Policy relaunch keeps the Run; Run
  Status and Anomalies are stored, projected from Lifecycle State per the AD-15 amendment.
- Child Runs replace Task (19.3): open/close through the facade and the Control API; usage rows carry
  the hosting instance; ancestor rollups; cascade on `stop`/`pause` of the process-owning Run.
- Depth changes keep the run id and emit an event; new event families for Runs.
- The verb × Control Depth table is a conformance-tested fixture, filled in as later stories enable
  each cell.
- *(Readiness 2026-10-30.)* The instance `labels` config key (a map merged per key across the FR-11 layers;
  no invocation override) gives supervised Runs their bound labels (D3, AD-25).
- Builds the snapshot publisher (AD-25) with the Run index; 21.3 adds resolved Policies to it.
- Child Runs here are registry, rollup and cascade bookkeeping, tested with synthetic
  run-attributed usage; a Host-opened child is `announced` with no Lease until 23.2; before 23.4 a
  `parent-paused` cascade records Run Status `held` with its cause. Attributed calls to a child
  land in 23.1.
- The spec writes the Run Status transition table for supervised and child Runs and the set/clear
  rule for each Anomaly; the upgrade backfills the live run as a `supervised` Run with its legacy
  id and historical ids as `ended` Runs; a release note says per-run budgets no longer reset on a
  crash relaunch.
- Ancestor ceilings extend the Governance Vectors additively with a `vectors_version` bump.

### Story 21.3: Policies, the default Policy and the `warn` default (FR-60, FR-21, AD-7, AD-25)
- `policies` table and the pure Policy-resolution function in the domain core: explicit limits
  first, most specific Selector per limit, ties to the tightest, then the default Policy; bound
  labels only; Caller limits apply only while tighter.
- The stored, editable `default` Policy carries the default Breach Action (`warn`), the
  Scent-Check Mode seeded `off` until 22.3 decides Q9, and the model-keyed Rate table.
- The upgrade migration writes `pause` for instances relying on the implicit default, with the store
  marker, the "kept on pause by upgrade" note and one `breach_default_upgraded` event; a malformed
  `breach_action` fails closed to `pause`.
- A Policy Breach Action a Run's depth cannot execute falls back to `warn` plus an Anomaly; create /
  change reports the fallback count.
- `hekma policy` and `/v0/policies` CRUD; every change is an event.
- *(Readiness 2026-10-30.)* Policies follow the latest (PRD Q11 interim): a change re-resolves every matched
  open Run in the writer's next batch; lowering below usage breaches at once, raising re-arms.
  Resolved Policies join the published snapshot.
- The upgrade write is crash-safe: a pending list in state.db first, idempotent `config.toml`
  writes under their locks, then markers, event and list cleared in one transaction; an unwritable
  config keeps `pause` through its marker (AD-7).
- The spec records the Rate table's contents and model matching; a Policy Cost Cap on an unpriced
  model raises an Anomaly instead of staying silently inert.
- Updates the agents-in-CI recipe and the dogfood lane to set `stop` explicitly and drops the
  "`pause` (the default)" wording (FR-47 amendment, NFR-7).

### Story 21.4: One data-path owner — the CLI routes through a local Service Host (FR-41, AD-20)
- When no Service Host runs, the first CLI command needing a write or the proxy auto-starts a
  loopback-only `hekma serve` in the background; every CLI write routes through the Control API;
  read-only commands open state.db read-only.
- The credential resolver with the authority matrix replaces the single bearer compare (bearer
  class first; the other classes land with their stories).
- A non-loopback Control API bind requires TLS and is refused at start without it.
- Existing CLI behaviour and exit codes stay the same from the user's side; agents now outlive the
  terminal.
- *(Readiness 2026-10-30.)* The spec lists the Control API routes added for each CLI write the frozen
  projection lacks (send, memory, restart policy).

### Story 21.5: `hekma ps` and Run listing (FR-50)
- `hekma ps`: run id or name, CONTROL column, Run Status, Anomalies, labels with source, usage and
  headroom; human-readable and `--json`.
- Exception-first default view (Anomaly, `held`, `silent`, `stale`, plus `ended` Runs with an
  Anomaly inside the retention window); `--all`.
- Keyset paging with `--limit` / `--cursor`; `/v0/runs` with the same `selector`, `limit`, `cursor`;
  SSE frames gain `id: <outbox seq>` and a server-side `selector`.
- Reads come from rollups and indexes only; first page < 1 s at the benchmark sizes available.

### Story 21.6: Selectors on every verb and `hekma budget` (FR-50)
- `--selector`, `--dry-run`, `--yes` on every lifecycle, hold and budget verb; the CLI shows the
  matched set and confirms; the Control API returns the set with a token and acts on exactly that
  set, reporting success or failure per Run.
- `hekma budget` reads, sets and raises Token Budgets and Cost Caps on matched Runs; raising
  re-arms the latch (the 19.3 re-arm, landed here), extending the Governance Vectors additively
  with a `vectors_version` bump. Raising a ceiling also clears the matching `block` fence reasons
  on the live Run (19.2's deferred live un-fence).
- Only Operator or Host credentials raise a ceiling.

### Story 21.7: `hekma top` (FR-57)
- Live terminal view over the Control API: Run Status, Control Depth, tokens/s, burn rate, budget
  left; refresh ≤ 2 s; exception-first and Selector-filterable at scale.
- Hotkeys pause, stop, hold, raise a limit and attach, each confirming as the CLI verb does.
- Points at a local or remote Service Host (and later a Hub).
- *(Readiness 2026-10-30.)* Attach needs raw output over the Control API, which FR-41 allows only when
  explicitly enabled: record that switch and its authority in AD-20 before this story starts.

### Story 21.8: Gateway complement — identity headers and recipes (FR-51, PRD §8)
- Operator-switchable outbound identity headers (run id and labels, never content) on forwarded
  calls.
- Docs recipes for Hekma in front of LiteLLM and Portkey; the "not a model gateway" line stated.

### Story 21.9: Bound labels — `hekma label` (FR-49, FR-60, AD-20, AD-25)
- *(Added by the readiness gate, 2026-10-30, ruled by Islam as D3.)* Runs after 21.2 and before
  21.3, so Policies have bound labels to select.
- `hekma label` sets and removes bound labels on Runs, with `--selector`, `--dry-run` and `--yes`
  like every Run verb; `/v0/runs/{id}/labels` mirrors it; Operator bearer only.
- A Caller can never set a bound key; each change is an event and re-resolves the Run's Policies.

## Epic 22: Run Identity and Key Custody

> **(2026-10-30 — opened via correct-course.)** One credential ends the bypass, makes listing
> accurate in attach mode and carries the run binding. The Run Key is the provider API key the agent
> holds (Islam's ruling). Realizes FR-53 and FR-54 and part of FR-51. Security-relevant: key custody
> needs the security review the PRD names before it is documented as supported.

### Story 22.1: Run Keys — minting, hashing, refresh and revocation (FR-53, AD-22)
- `hk_run_` / `hk_ref_` / `hk_ann_` tokens: 256 bits from the OS CSPRNG, shown once, stored as
  SHA-256 hashes (the engine's std-only implementation) with 12-hex token ids.
- Issued at supervised start and on announce; valid for the Run's life; revocation applies on the
  next call via the snapshot; optional rotation through the refresh credential bound to the
  announcing or launched process.
- Tokens only through `Env` mappings, never in a URL, argv or file; the no-leak matrix extends to
  tokens.
- Snapshot carries the hash → Run index; the authority matrix gains the Run Key, refresh and
  announce-scoped classes.

### Story 22.2: The Run Key in the API-key slot — key custody by default (FR-53, AD-22)
- Supervised Runs whose provider key Hekma holds get the Run Key in their API-key variable and the
  proxy base URL; the proxy swaps in the real key, bound to its upstream origin over `https`.
- Custody forces the child env allowlist, strips provider variables, refuses `File` / `Flag` targets.
- Self-reported adapters without a Hekma-held key show `supervised (no wire)`; engine-observed
  adapters without one are refused at start with a remediation.
- Host / Origin guard on the proxy; the threat model and stated limits in the docs.

### Story 22.3: Scent-Check Modes (FR-53)
- `off` / `shadow` / `enforce` per Policy; unscented calls are an Anomaly and an OTLP counter;
  `enforce` refuses with a typed response.
- Decides PRD §13 Q9 (defaults per environment, redefined now that every CLI write routes through a
  Service Host) in the spec and records it in the default Policy, never overwriting a default
  Policy the Operator has edited (seeded `off` by 21.3).
- A Caller can never lower the mode.

### Story 22.4: Bypass detection in the Shim (FR-54, AD-23)
- Runs after 24.1: each Shim compares its process tree's TCP connections with the resolved
  provider-host list, per OS in `backends/`.
- A bypass is an Anomaly naming the Run and host, plus an OTLP counter by host; `supervised (no
  wire)` Runs get an informational note instead.
- An OS without an implementation reports "unavailable", never clean.

### Story 22.5: An authenticated proxy beyond loopback (FR-51, AD-20, AD-26)
- TLS required for any non-loopback proxy bind; proxy-access credentials per endpoint binding
  (issued through `hekma binding`, shown once, hashed, swapped for the binding's custodied key, not
  a Scent-Check identity).
- Port-squatting limit and the socket-activation recommendation in the docs.

## Epic 23: Attach and Wire Control

> **(2026-10-30 — opened via correct-course.)** Framework flows and plain clients become Runs with
> little or no integration, and Hekma can hold, reroute or reject a Run's calls without touching its
> process. Realizes FR-51, FR-52, FR-56 and part of FR-43.

### Story 23.1: The shared Metering Proxy (FR-51, AD-26)
- One listener per data-path owner on a fixed port replaces the per-instance `ObservedListener`;
  supervised instances get its address through `metering.base_url`.
- Identification order (Run Key, then binding), identity headers as hints, bindings with upstream
  origins, the labelled-binding Run and the single unattributed Run, the typed no-route error.
- Per-call records and `open_calls` through a bounded channel to the store writer (never dropped
  silently); usage attributed at request time, dedup on call id.
- Reads only the published snapshot: no engine mutex, no SQLite on the hot path; Run creation on a
  first call awaits the writer with a bound.
- `proxy-wire v1` documented in `docs/` (PRD §7 item 6).
- *(Readiness 2026-10-30.)* `hekma binding` and `/v0/bindings` create endpoint bindings (path prefix, upstream
  origin, bound labels, optional custodied key), their reroute routes and per-binding announce and
  proxy-access credentials; Operator bearer only; each change is an event (D3, AD-20).
- Carries 19.2's `block` into the snapshot with the same response, so the fence never lapses.
- Creates the calls store (AD-27) and its first producer.
- Calls carrying a child's identity (Run Key or a hint beneath the parent's identity) are
  attributed to the child Run; calls without one stay on the parent.

### Story 23.2: Self-announce, run identities and Leases (FR-52)
- Announce header (returns Run Key and refresh credential after publish), Run-scoped announce
  endpoint under the announce-scoped credential, and the identity → Run mapping that keeps one Run
  per identity across channels.
- Leases and the `wire` inactivity window; the sweeper marks `silent` and `ended`; silence never
  stops a Run.
- CrewAI and LangGraph helpers, documented as supported only once validated against real flows in
  CI.

### Story 23.3: Inbound OTLP and `observed` Runs (AD-28)
- OTLP/HTTP+JSON receiver at `/v1/metrics` under the announce-scoped credential;
  `hekma.run.external_id` and `hekma.label.*` attributes create or update `observed` Runs.
- `gen_ai.client.token_usage` enters the ledger as `telemetry-reported`, always `estimated`.
- Protobuf / gRPC stay deferred; the docs show a Collector translating.

### Story 23.4: Wire hold, release, reroute and reject (FR-56, FR-43, FR-7)
- Run-level dispositions with precedence (reject / breach `block` > hold > reroute > pass); each verb
  clears only its own reason.
- Holds queue a Run's new calls in arrival order with a per-Run cap (typed hold-full response plus
  an Anomaly); hold end causes recorded; held count and oldest age reach `hekma ps`.
- Reroute to routes configured through `hekma binding` (a custodied key never leaves its origin);
  reject reuses 19.2's typed non-retryable response.
- The FR-7 amendment: a wire hold may stand in for native pause, surfaced as qualified.

## Epic 24: Operability at Scale

> **(2026-10-30 — opened via correct-course.)** Hekma is never the reason prod stops: agents survive
> Service Host restarts, upgrades and crashes, and every surface holds at a million Runs. Realizes
> FR-55, FR-59, NFR-9 and the NFR-4 amendment.

### Story 24.1: One Shim per supervised process (AD-23)
- `hekma shim` (internal, hidden) owning the process group / job, pipes, raw-log redirects and the
  `output.log` tailer; detached from the Service Host's session.
- A shim `ProcessBackend`: spawn, stop, pause, poll as RPCs; exit pushed; adopt reconnects.
- Socket access control (0700 dir, peer uid check, Windows DACL, per-Shim connection secret);
  protocol version in the first message.
- Spawn record committed before the child launches; adoption owner/lease columns renamed the
  ownership claim.
- Closes the recorded drift: Windows adopted-process stops, Unix vs Windows survival asymmetry,
  adopted handles without stdin.

### Story 24.2: Handoff Mode and in-place upgrades (FR-55, FR-41, AD-20, AD-23, AD-26)
- SIGTERM in Handoff Mode releases Shims without stopping them; the next Service Host reattaches.
- In-place upgrade sequence: successor launched, predecessor flushes its writer, listening socket
  and `host.lock` handed over, state.db migrated, Shims reattached, then the predecessor exits.
- Holds belong to the Run: a held call may see one retryable error and is re-held.
- Service-manager checks (systemd `KillMode`, Windows breakaway) with an honest "unverified" state.
- Flips Handoff Mode to the default when the story's acceptance passes (PRD FR-55 gate).

### Story 24.3: Crash recovery and registry completeness (FR-55, NFR-1, NFR-9)
- `kill -9` tests: holds, Leases and Run records survive within the flush bound; open calls close
  as `crash` on the next start; in-flight calls fail retryably.
- With Handoff Mode off, orphans are cleaned up and their Runs ended with the cause; with it on, they
  are adopted (NFR-1, NFR-9).
- SM-7's suite: no Run stopped, paused or blocked by anything NFR-9 does not list.

### Story 24.4: Scale substrate and the Run-scale benchmark (NFR-4, AD-18, AD-25, AD-27)
- AD-18 archived plan Steps 0–4.
- Snapshot publication costs O(changed entries) and holds only non-ended Runs.
- Ended-Run compaction into rollups (labels and totals kept); calls-store rotation and retention.
- Benchmark at 10k / 100k / 1M Runs: first page of `hekma ps` < 1 s, proxy overhead ≤ 10 ms p99;
  results recorded against NFR-4.

### Story 24.5: `otlp-metrics v2` (FR-59, AD-21)
- `hekma.run_id` leaves metric attributes; `hekma.control_depth`, `hekma.policy` and allow-listed
  label keys (empty by default) with a cardinality cap and overflow fold; instance series bounded.
- Exemplars carry per-Run detail; `https://` collectors supported.
- A versioned exposition change under AI-55's two-pass review; traces stay gated by the PRD §6
  amendment.

## Epic 25: Nodes and Hub

> **(2026-10-30 — opened via correct-course; placeholder.)** One view and control of every machine
> an Operator runs Hekma on (FR-58). No story is written until spine AD-24 is drafted, after Epic 24
> closes; it must settle the rollup endpoint and cursor, Hub merge, Hub-routed verbs, per-Node auth,
> partition behaviour and the PRD §6 amendment for data crossing machines.
