---
title: "PRD: Ktesio — Unified Personal-Agent Runtime Engine"
status: review
created: 2026-07-02
updated: 2026-09-30
---

# PRD: Ktesio — Unified Personal-Agent Runtime Engine

## 0. Document Purpose

This PRD turns the accepted product brief (`_bmad-output/planning-artifacts/briefs/brief-ktesio-2026-07-02/brief.md`, status: updated) into buildable requirements for the repositioned Ktesio. It is written for the downstream BMAD chain — architecture, epics/stories, implementation — and for Islam as the deciding owner. Vocabulary is anchored in §3 Glossary and used verbatim everywhere; features are grouped in §4 with globally numbered FRs; inferences are tagged inline as `[ASSUMPTION]` and indexed in §14. The brief and its addendum remain the product-level "why"; this document is the "what." It was drafted headless while Islam is away: nothing here overrides a `[FIXED]` brief constraint, and every invented detail is tagged.

## 1. Vision

Running a personal agent today means living inside whatever runner its authors shipped; running two means learning two of everything — lifecycle, config, memory, and whatever cost story each one tells. Ktesio ends the per-agent tax. It is a **runtime engine** that wraps any Agent behind one operating model: the same start/pause/resume/stop, the same configuration surface, the same way to wire memory, the same token budgets and dollar cost caps — identical whether the Agent underneath is NousResearch's Hermes Agent or something structurally nothing like it.

The engine ships two ways at once. Operators get `kt`, a cross-platform CLI that installs, isolates, and fully controls personal agents from the terminal. Hosts — platforms and apps that run personal agents for their users — embed the same engine and inherit a resilient, consistent, predictable runtime instead of building supervision, budget enforcement, and memory plumbing in-house. One engine, two surfaces; `kt` is the engine's first and complete consumer, which is also the standing proof that embedding works.

The bet is that consistency itself is the product. Agents become interchangeable workloads: adopt one the way you adopt a container image, and your existing controls just apply. "Runs under Ktesio" should come to mean *governable, cost-bounded, and consistent to operate* — for a solo operator's laptop fleet and for a host's production runtime alike.

## 2. Target User

### 2.1 Jobs To Be Done

- **Operate any personal agent with controls I already know** — start it, pause it, stop it, inspect it — without reading its native runner docs. (Functional)
- **Never be surprised by an agent's bill** — set a token or dollar ceiling once and trust it is enforced even when I am not watching. (Functional, emotional: relief from runaway-spend anxiety)
- **Adopt a new agent in minutes, not evenings** — install, isolate, configure, and run it through one consistent surface. (Functional)
- **Give my agent the memory and skills it needs** without bespoke wiring per agent. (Functional)
- **As a Host, offer agent hosting without building a runtime** — embed an engine that makes heterogeneous agents resilient, consistent, and predictable for my users, while I keep my own user management and billing. (Functional, strategic)
- **As an Agent author, make my agent first-class everywhere** by writing one Adapter against a small, stable contract. (Functional) `[ASSUMPTION: author audience is candidate-tier per the brief; JTBD kept but no v1 features target authors beyond the published Adapter Contract itself.]`

### 2.2 Non-Users (v1)

- Teams wanting shared policy, org budgets, RBAC, or audit — that governance layer belongs to a Host, not to Ktesio.
- Users wanting a hosted dashboard or SaaS — Ktesio ships a CLI and an embeddable engine only.
- Agent *builders* looking for a framework to write agent logic in — Ktesio runs agents; it does not author them.
- Non-technical end users — they are served indirectly through Hosts that embed the engine.

### 2.3 Key User Journeys

*Dev-product dial: compact narratives, named protagonists. FRs reference these by ID.*

- **UJ-1. Noor caps Hermes before it can surprise her.**
  Noor, an indie developer, pays for model API keys out of pocket. She installs `kt`, registers Hermes Agent from the built-in reference Adapter, sets a Cost Cap of $5 for the session at her provider's Rate, and starts the Agent Instance. Hermes runs a long research loop; the Usage Ledger climbs. At the cap, the engine executes the Breach Action — the Agent Instance is paused and `kt` says so, with tokens and estimated dollars spent. Noor resumes with a raised cap after dinner. She was never surprised. **Edge case:** Hermes' Adapter reports usage in batches; the engine enforces on the estimate and reconciles when the next report lands, labeling the figures as estimates throughout.
- **UJ-2. Noor adopts a second agent and already knows how to drive it.**
  A week later Noor tries a structurally different Agent — single-shot, no gateway, no native memory. Same commands: register, configure, set Token Budget, start, interact, stop. Nothing new to learn; the Adapter absorbed the differences and declared what it cannot support (pause is best-effort). `kt` surfaced that declaration up front. This journey *is* the north-star metric (SM-1).
- **UJ-3. Lena embeds the engine under her hosting platform.**
  Lena is a platform engineer at a startup that hosts personal agents for consumers. She links the Ktesio engine library, drives it through the Embedding Interface — register/start/pause/stop Agent Instances, inject per-user config and caps, subscribe to Lifecycle State and usage events — and renders her own UI on top. Her platform keeps its own accounts and billing; Ktesio keeps every hosted agent resilient, bounded, and uniformly controllable. **Edge case:** an Agent Instance crashes mid-run; the engine applies the Restart Policy, emits the state-change event, and Lena's dashboard shows the recovery without her writing supervision code.

## 3. Glossary

*Downstream workflows must use these terms exactly; synonyms are a discipline violation.*

- **Agent** — a third-party agent program that Ktesio runs: a personal agent (e.g. Hermes Agent, OpenClaw) or a coding agent (e.g. opencode, GitHub Copilot CLI). Not authored by Ktesio. `[FIXED — clarified by Islam 2026-09-07: coding agents are in scope by definition, not only by FR-29's choice of opencode as the second agent.]`
- **Adapter** — the integration component that makes one Agent runnable by the Engine by satisfying the Adapter Contract. One Adapter per Agent kind.
- **Adapter Contract** — the versioned, documented specification an Adapter implements: lifecycle operations, config mapping, Metering Source declaration, memory attachment, interaction channel, and a Capability Declaration.
- **Capability Declaration** — the Adapter's machine-readable statement of which controls it supports as *guaranteed*, *best-effort*, or *unsupported* (e.g. pause: best-effort).
- **Engine** — the embeddable Ktesio core: registry, supervisor, config, metering, budget enforcement, memory wiring, interaction routing. Delivered as a Rust library. `[ASSUMPTION: library-first; service/IPC delivery is out of v1 — see §13 Q6.]`
- **Embedding Interface** — the Engine's public surface a Host (and `kt` itself) drives: commands in (lifecycle, config, budgets), state/usage/events out.
- **Host** — a platform or app that embeds the Engine to run Agents for its own users.
- **Operator** — the human driving `kt` directly.
- **Agent Instance** — one registered, installed occurrence of an Agent under Engine management, with its own Agent Home, config, budgets, and Lifecycle State.
- **Agent Home** — the per-Agent-Instance isolated directory holding its config, state, logs, Skill Set, and working data.
- **Lifecycle State** — one of: `registered`, `starting`, `running`, `paused`, `stopping`, `stopped`, `failed`. `[ASSUMPTION: exact state set — architecture may refine.]`
- **Restart Policy** — per-Agent-Instance rule for automatic restart on failure: `never`, `on-failure`, `always`. Default `never`. `[ASSUMPTION]`
- **Memory Backing** — a storage destination for an Agent's memory, attached through the Engine's memory interface.
- **Token Budget** — a ceiling on tokens an Agent Instance may consume within a scope (per run and cumulative). `[ASSUMPTION: scopes]`
- **Rate** — an Operator/Host-supplied price per 1M tokens (input and output rates supported). `[ASSUMPTION: split rates]`
- **Cost Cap** — a dollar ceiling derived from Rate × metered tokens, enforced by the Engine.
- **Breach Action** — what the Engine does when a Token Budget or Cost Cap is reached: `pause` (default), `stop`, or `warn`. `[FIXED — pause default ratified by Islam 2026-07-02.]`
- **Metering Source** — where usage numbers come from for an Agent Instance, declared by its Adapter: `self-reported` (Agent/Adapter reports) or `engine-observed` (Engine intercepts model traffic). Figures are estimates unless reconciled.
- **Usage Ledger** — the Engine's per-Agent-Instance record of tokens and derived dollars over time.
- **Skill** — a reusable instruction/capability directory provisioned *to a specific Agent Instance* (agent-scoped; the general-purpose skills manager is deprecated).
- **Skill Set** — the locked collection of Skills provisioned into one Agent Home.
- **Fleet** — all Agent Instances the Engine manages in a given installation.
- **`kt`** — the Ktesio CLI; the Engine's complete reference frontend.
- **Service Host** — a long-running process that owns an Engine continuously and serves a local Control API to the Host that runs it (FR-41).
- **Control API** — the Service Host's versioned local HTTP/JSON surface: lifecycle, configuration, usage, and events by stream or cursor.
- **Task** — a Host-defined unit of work inside a Run (e.g. one request the Host sent the Agent). Budgets and latches can be scoped to it (FR-43).
- **Drain** — stop new work, wait for in-flight work up to a bounded window, then stop (FR-42).
- **Readiness / Liveness Probe** — Adapter-declared checks that an Agent can serve, and is still alive, beyond its process existing (FR-42).
- **Governance Vectors** — the versioned, portable test cases that state the Engine's budget and cost decision rules (FR-44).

## 4. Features

*Actor note: every capability written as "The Operator can…" is equally available to a Host through the Embedding Interface (FR-31); FRs name the Operator for readability, not exclusivity. Numeric bounds inside consequences that carry an `[ASSUMPTION]` tag are placeholders — architecture must validate or replace each one before any story cites it as an acceptance criterion.*

### 4.1 Agent Registration & Fleet

**Description:** The Operator (or a Host via the Embedding Interface) registers an Agent by naming its Adapter, creating an Agent Instance with an isolated Agent Home. The Fleet is inspectable at any time: every Agent Instance, its Lifecycle State, budgets, and usage at a glance. Realizes UJ-1, UJ-3. Registration covers Adapters shipped with Ktesio (Hermes reference) and local Adapters supplied by path; a remote Adapter registry/distribution channel is deliberately deferred. `[ASSUMPTION: no adapter registry in v1.]`

#### FR-1: Register an Agent Instance
The Operator can register an Agent Instance from an installed Adapter, giving it a unique name in the Fleet.
**Consequences (testable):**
- Registering creates an Agent Home containing instance config and an empty Usage Ledger; the Agent Instance enters Lifecycle State `registered`.
- Registering two Agent Instances with the same name fails with a clear diagnostic and a remediation hint.
- The same Agent kind can be registered as multiple, independently configured Agent Instances. `[ASSUMPTION: multi-instance is in scope v1.]`

#### FR-2: Isolated Agent Home
Each Agent Instance owns an Agent Home; nothing about one Agent Instance's config, state, logs, or Skill Set leaks into another's.
**Consequences (testable):**
- Removing one Agent Instance leaves every other Agent Home byte-identical.
- Two Agent Instances of the same Agent kind hold disjoint Agent Homes and can run concurrently with different config. `[ASSUMPTION: concurrent same-kind instances supported.]`

#### FR-3: Unregister / remove
The Operator can remove an Agent Instance, with an explicit choice about whether the Agent Home is retained or deleted.
**Consequences (testable):**
- Removal of a `running` Agent Instance requires stop-first or an explicit `--force` acknowledgment. `[ASSUMPTION: force semantics.]`
- After removal with deletion, no orphan processes and no orphan Agent Home remain.

#### FR-4: Fleet visibility
The Operator can list the Fleet and see, per Agent Instance: name, Agent kind, Lifecycle State, Token Budget and Cost Cap status, and current Usage Ledger totals.
**Consequences (testable):**
- The listing reflects a Lifecycle State change within 2 seconds of the transition. `[ASSUMPTION: freshness bound placeholder.]`
- Output is available both human-readable and machine-readable (`--json`).

### 4.2 Unified Lifecycle

**Description:** One lifecycle vocabulary for every Agent: start, pause, resume, stop, with a defined state machine, crash detection, Restart Policy, and state persistence across Engine restarts. Where an Agent cannot honor a transition natively, its Adapter's Capability Declaration says so and the Engine surfaces it rather than pretending. Realizes UJ-1, UJ-2, UJ-3.

#### FR-5: Start
The Operator can start a `registered`/`stopped` Agent Instance; the Engine launches it through its Adapter with the Agent Home's effective config, Skill Set, Memory Backing, and budgets applied.
**Consequences (testable):**
- A successful start transitions `starting → running` and is visible in Fleet listing and events.
- A failed start lands in `failed` with the Adapter's diagnostic preserved and surfaced; no zombie process remains.

#### FR-6: Stop
The Operator can stop a `running`/`paused` Agent Instance; the Engine requests graceful shutdown via the Adapter and escalates to forced termination after a timeout.
**Consequences (testable):**
- Graceful window elapses → forced termination occurs and is recorded as such in the Agent Instance's log. Default window 30s, configurable. `[ASSUMPTION: default.]`
- After stop, no process belonging to the Agent Instance survives (verified cross-platform).

#### FR-7: Pause / resume with honest semantics
The Operator can pause and resume a `running` Agent Instance. Pause semantics follow the Adapter's Capability Declaration: guaranteed, best-effort, or unsupported — surfaced before and during use.
**Consequences (testable):**
- Pausing under a `best-effort` declaration emits a visible qualifier (CLI text and event payload), not a silent success.
- Pause on an `unsupported` declaration fails fast with the declaration quoted; it does not attempt a fake pause.
- Resume restores the pre-pause Lifecycle State trajectory (`paused → running`) and the Usage Ledger continues from where it left off.

#### FR-8: Defined state machine
All Lifecycle State transitions are defined, enumerable, and identical across Agents; invalid transitions are rejected uniformly.
**Consequences (testable):**
- `stop` on a `stopped` Agent Instance and `resume` on a `running` one return the same error class for every Adapter.
- Every transition is emitted as an event with prior state, new state, cause, and timestamp.

#### FR-9: Crash detection & Restart Policy
The Engine detects an Agent Instance exiting outside a requested stop, marks it `failed` with cause, and applies the Restart Policy.
**Consequences (testable):**
- With `on-failure`, a crashed Agent Instance restarts with backoff and the restart count is visible; with `never`, it stays `failed` with the exit diagnostic. Backoff parameters are configurable with sane defaults. `[ASSUMPTION: backoff defaults.]`
- Crash-looping is bounded: after N consecutive failures the Engine stops retrying and says why. `[ASSUMPTION: N configurable, default 5.]`

> *(Amended 2026-09-30.)* `always` is subsumed. Every exit the Engine did not request already
> restarts under `on-failure`. What services actually need is exit-code classes (restart-requested,
> clean) and a stable-window reset of the crash-loop counter — FR-42.

#### FR-10: State persistence across Engine restarts
The Fleet's registrations, budgets, Usage Ledgers, and last-known Lifecycle States survive Engine/`kt` restarts and host machine reboots.
**Consequences (testable):**
- After a machine reboot, Fleet listing shows every Agent Instance with accurate persisted state (running instances shown as `stopped`/`failed` per detection, not lost).
- Usage Ledger totals are durable — a crash of the Engine itself loses at most the last flush interval of usage data. `[ASSUMPTION: flush interval bound, default ≤5s of data.]`

### 4.3 Unified Configuration

**Description:** One configuration model for every Agent Instance, with defined precedence; Adapters translate the unified model into each Agent's native config. The Operator learns one surface; agent-specific knobs still reachable through a namespaced pass-through rather than new file formats. Realizes UJ-2, UJ-3.

#### FR-11: Unified config model with precedence
The Operator can set configuration at defaults < Agent-kind level < Agent Instance level < invocation override, with deterministic precedence. `[ASSUMPTION: precedence chain.]`
**Consequences (testable):**
- The same key set at instance level overrides the same key at kind level in the effective config, every time.
- Config is validated at write time; unknown keys outside the pass-through namespace are rejected with the nearest valid key suggested.

#### FR-12: Adapter config mapping
Each Adapter maps unified config keys to the Agent's native mechanism (files, env vars, flags) at start time; agent-native extras live under an explicit pass-through namespace.
**Consequences (testable):**
- A documented unified key (e.g. model selection) lands in the Agent's native config for both the reference Adapter and a mock second Adapter in tests.
- Pass-through keys are delivered verbatim and marked as un-validated in effective-config output.

#### FR-13: Effective-config inspection
The Operator can view the resolved, effective configuration of an Agent Instance — what will actually apply on next start — including each value's source layer.
**Consequences (testable):**
- Every rendered value names its source (default / kind / instance / override).
- Secrets render masked (see FR-14).

#### FR-14: Secrets handling
Secret-classified config values (API keys, tokens) are stored and delivered to Agents without ever being logged, echoed, or rendered unmasked.
**Consequences (testable):**
- Secrets never appear in Engine/CLI logs, event payloads, or effective-config output (masked rendering only) — enforced by test.
- Secret values are excluded from any machine-readable output unless an explicit `--reveal` acknowledgment is used. `[ASSUMPTION: reveal flag; storage mechanism (keychain/env/file permissions) is an architecture decision — see addendum.]`

### 4.4 Memory Wiring

**Description:** A consistent interface to attach a Memory Backing to an Agent Instance, whatever the Agent's native memory story. v1 keeps the guaranteed surface small and honest: a filesystem Memory Backing under the Agent Home plus delegate-to-native, with richer backings deferred. Realizes UJ-1. `[ASSUMPTION: v1 backing set — §13 Q4.]`

#### FR-15: Attach / detach Memory Backing
The Operator can attach one Memory Backing to an Agent Instance and detach it, through the same commands regardless of Agent.
**Consequences (testable):**
- Attach before start → the Adapter receives the Memory Backing descriptor at launch; detach requires the Agent Instance not `running`. `[ASSUMPTION: no hot-swap in v1.]`
- The same attach command sequence works identically on the reference Adapter and the mock second Adapter.

#### FR-16: v1 Memory Backing set
The Engine ships two Memory Backing kinds: `filesystem` (a managed directory inside the Agent Home) and `native` (explicit delegation to the Agent's own memory with Ktesio guaranteeing only persistence of the Agent Home).
**Consequences (testable):**
- A `filesystem` Memory Backing survives stop/start and Engine restarts byte-identically.
- Choosing `native` records in effective config that memory guarantees are delegated — visible to the Operator.

#### FR-17: Memory portability boundary
What Ktesio guarantees (persistence, isolation, portability of the managed directory) versus what it delegates (semantics of the Agent's own memory) is explicit in docs and in command output.
**Consequences (testable):**
- Exporting an Agent Home (FR-2 isolation) carries the `filesystem` Memory Backing with it and the Agent runs elsewhere with memory intact. `[ASSUMPTION: export as a v1 capability — may defer; flagged in §10.]`

### 4.5 Token & Cost Governance

**Description:** The headline guardrails. Token Budgets bound consumption; a supplied Rate turns tokens into dollars and a Cost Cap bounds those. Enforcement happens at the runner level — the Engine acts on the Agent Instance's lifecycle when a ceiling is hit — which is precisely what proxy-layer budget tools do not do. Figures are labeled estimates unless reconciled. Realizes UJ-1, UJ-3.

#### FR-18: Token Budgets
The Operator can set a Token Budget per Agent Instance at two scopes: per-run and cumulative. `[ASSUMPTION: scopes; per-window (daily/monthly) deferred.]`
**Consequences (testable):**
- Consumption reaching the budget triggers the configured Breach Action within one metering interval.
- Budgets are inspectable and changeable while `running`; changes apply immediately.

#### FR-19: Metering ingestion
The Engine ingests usage per the Adapter's declared Metering Source: `self-reported` (Adapter forwards the Agent's own usage accounting) or `engine-observed` (the Adapter routes the Agent's model traffic through an Engine-provided interception point).
**Consequences (testable):**
- The active Metering Source for each Agent Instance is visible in Fleet listing detail.
- With `self-reported`, delayed batches reconcile the Usage Ledger without double-counting; with `engine-observed`, usage lands in the Usage Ledger within the flush bound of the call completing.
- An Adapter with no viable Metering Source is rejected at registration with a clear diagnostic (metering is mandatory for governance honesty). `[ASSUMPTION: metering mandatory — a no-metering escape hatch would hollow out the core promise; flagged for Islam.]`

#### FR-20: Rate & cost derivation
The Operator can supply a Rate (input and output $/1M tokens, per Agent Instance or per model key) and the Engine derives dollar figures in the Usage Ledger from metered tokens.
**Consequences (testable):**
- Changing the Rate re-prices future consumption only; the Usage Ledger keeps historical dollars at the Rate in force when consumed. `[ASSUMPTION: no retro-repricing.]`
- With no Rate supplied, dollar features are inert and say so; token features work fully.

#### FR-21: Cost Cap enforcement
The Operator can set a Cost Cap per Agent Instance (per-run and cumulative scopes, same as FR-18); on breach, the Engine executes the Breach Action on the Agent Instance's lifecycle.
**Consequences (testable):**
- In an integration test driving a mock Agent past its Cost Cap, the Breach Action executes in 100% of runs, and the enforcement latency from ledger-breach to lifecycle action is ≤ the metering interval + 1s. `[ASSUMPTION: latency placeholder.]`
- The Breach Action is configurable per Agent Instance among `pause` / `stop` / `warn`, with `pause` the ratified shipped default; breaches are always recorded as events regardless of action.

#### FR-22: Usage & cost visibility
The Operator can read, per Agent Instance and Fleet-wide: tokens consumed (by scope), derived dollars, active budgets/caps, and headroom — human-readable and `--json`.
**Consequences (testable):**
- Visibility output totals equal the Usage Ledger exactly; every dollar figure carries its estimate/reconciled label.

#### FR-23: Estimate honesty
Every dollar figure the Engine renders is labeled `estimated` unless the Metering Source provides provider-confirmed actuals; the boundary is stated in docs and command output.
**Consequences (testable):**
- No code path renders an unlabeled dollar amount (enforced by a rendering-layer test).

### 4.6 Unified Interaction

**Description:** One way to talk to and observe any running Agent: send input, stream output and logs, and script against machine-readable state — the same commands regardless of what the Agent is. Realizes UJ-1, UJ-2.

#### FR-24: Send input
The Operator can send a text input to a `running` Agent Instance through one command; the Adapter routes it to the Agent's native input channel.
**Consequences (testable):**
- The same send command works on the reference Adapter and the mock second Adapter; an Agent with no input channel declares `interaction: unsupported` in its Capability Declaration and the command fails fast quoting it.

#### FR-25: Stream output & logs
The Operator can attach to a running Agent Instance's output stream and read its retained logs after the fact, uniformly.
**Consequences (testable):**
- Output emitted while detached is retained in the Agent Home (bounded retention, configurable) and readable later. `[ASSUMPTION: retention bound default 10MB per instance.]`
- Log lines are timestamped and attributed (agent stdout / agent stderr / engine).

#### FR-26: Scriptable surface
Every read command offers `--json`; exit codes are stable and documented, making `kt` automatable without the Embedding Interface.
**Consequences (testable):**
- JSON schemas for listing/status/usage are documented and covered by compatibility tests (see §7 versioning).

### 4.7 Adapter Contract & Reference Adapter

**Description:** The downward interface that makes everything above agent-agnostic. The Adapter Contract specifies lifecycle operations, config mapping, Metering Source, memory attachment, interaction channel, and the Capability Declaration. The Hermes Agent Adapter is the shipped, end-to-end reference; a second, structurally different Agent validates the contract on paper before v1 freezes it. Realizes UJ-2 and the north-star.

#### FR-27: Published Adapter Contract with Capability Declaration
The Adapter Contract is a versioned, documented specification; every Adapter carries a machine-readable Capability Declaration consumed by the Engine and surfaced to the Operator.
**Consequences (testable):**
- The Engine refuses to load an Adapter whose declaration omits a mandatory section, with a diagnostic naming it.
- A contract conformance test-kit runs against any Adapter and reports per-capability compliance. `[ASSUMPTION: conformance kit in v1 — it is how "identical controls" stays testable.]`

#### FR-28: Hermes reference Adapter
Ktesio ships an Adapter integrating NousResearch Hermes Agent end-to-end: lifecycle (including its gateway process model), config mapping, self-reported Metering Source via Hermes usage accounting, memory attachment, and interaction.
**Consequences (testable):**
- UJ-1 executes end-to-end against real Hermes Agent in CI-adjacent integration tests (recorded/sandboxed where network-bound). `[ASSUMPTION: test isolation strategy is architecture's call.]`
- Every FR-4/5/6/7/8 consequence passes with the Hermes Adapter; declared best-effort capabilities are explicitly listed in its Capability Declaration.

#### FR-29: Second-agent contract validation
Before the Adapter Contract is frozen for v1, it is validated against a named second Agent that is structurally unlike Hermes Agent (single-shot, no gateway, no native memory) — as a written conformance mapping, not necessarily shipped code.
**Consequences (testable):**
- A validation document exists mapping every contract section to the second Agent with any contract changes fed back before freeze; the chosen Agent is **opencode** (opencode.ai), selected by Islam 2026-07-02 — its structural profile vs Hermes Agent (session model, process shape, metering surface) is characterized as the first step of the conformance mapping.

#### FR-30: Contract versioning
The Adapter Contract carries a semantic version; the Engine states which contract versions it accepts and rejects mismatches informatively.
**Consequences (testable):**
- Loading an Adapter with an incompatible contract version fails with both versions named and the compatibility rule quoted (policy in §7).

### 4.8 Embeddable Engine

**Description:** The upward half of dual delivery. The Engine is a Rust library whose Embedding Interface exposes everything `kt` can do; `kt` itself is built exclusively on that interface. Hosts get lifecycle command, config/budget injection, and a subscription surface for state, usage, and breach events. Realizes UJ-3.

#### FR-31: Engine library exposes full capability
Every capability in §4.1–4.7 and §4.9 is reachable through the Embedding Interface without invoking the CLI.
**Consequences (testable):**
- An integration test drives register→configure→cap→start→breach→pause→stop purely through the library, no CLI involved.

#### FR-32: `kt` is built on the public Embedding Interface
The CLI consumes only the Engine's public surface — no private backdoors — making `kt` the standing embeddability proof.
**Consequences (testable):**
- A build-level check (visibility boundaries / crate structure) enforces that `kt` uses only public Engine API; violations fail CI.

#### FR-33: Event & telemetry subscription
A Host can subscribe to Engine events: Lifecycle State transitions, usage-ledger updates, budget/cap breaches, and crash/restart occurrences, each with stable payload schemas.
**Consequences (testable):**
- Every FR-8/FR-9/FR-21 event arrives to a subscriber in order for a given Agent Instance, with schema-validated payloads.

#### FR-34: Embeds clean
The Engine library runs without a TTY, without prompting, and without global process state that would collide with a Host's own runtime.
**Consequences (testable):**
- The Engine runs inside a headless test harness (no TTY) with zero interactive prompts across the full FR-31 flow; all human-interaction affordances live in `kt`, not the Engine.

### 4.9 Agent-Scoped Skills Provisioning

**Description:** The confirmed sub-feature carrying the legacy forward. Skills are provisioned *to a managed Agent Instance* — its "what it knows" — reusing the proven v0.5.0 install/lock machinery (git-sourced, commit-locked, reproducible). The standalone, general-purpose skills manager is deprecated (§8, §4.10). Realizes UJ-1 (Skill Set as part of instance setup).

#### FR-35: Provision Skills to an Agent Instance
The Operator can add a Skill (git source or local path) to an Agent Instance's Skill Set; the Engine installs it into the Agent Home, locked to an exact commit.
**Consequences (testable):**
- The Skill Set lockfile in the Agent Home records source and commit for every Skill; re-provisioning from lock is byte-reproducible.
- The Adapter is informed of the Skill Set location so the Agent can consume it; where the Agent has a native skills convention, the Adapter maps to it. `[ASSUMPTION: mapping is the Adapter's job.]`

#### FR-36: Skill Set lifecycle
Skills can be listed, upgraded (re-lock to newer commit), and removed per Agent Instance, with the same status/doctor-style integrity checks the legacy manager had.
**Consequences (testable):**
- An integrity check detects a hand-modified installed Skill and reports it with remediation, per Agent Instance.

#### FR-37: Legacy command removal
The general-purpose skills commands (project-level `skills.json` workflow) are removed outright at v0.6.0 — no transitional notice-emitting surface and no shim layer. The replacement is `kt agent …`; the removal and a migration pointer are published at the release boundary (FR-38). `[FIXED — amended in epics.md and ratified by Islam 2026-07-14 (Epic 9, story 9-3 Part B); AD-16 Correction in ARCHITECTURE-SPINE.md; shipped in 39f0a57 / PR #114. Supersedes the notice-window policy this FR previously specified.]`
**Consequences (testable):**
- From v0.6.0 no retired legacy skills command name resolves at the top level of `kt`: the names are absent, not deprecated — asserted in the CLI surface test rather than by counting notices.

### 4.10 Migration & Deprecation (Legacy Skills Manager)

**Description:** Ktesio v0.5.0 has real users on crates.io/Homebrew for whom `kt` is a skills package manager. The pivot must not strand them silently. Destination is confirmed (agent-scoped sub-feature) and the rollout mechanics are now settled as a clean release-boundary break rather than a notice window. `[FIXED — amended in epics.md and ratified by Islam 2026-07-14 (Epic 9, story 9-3 Part B); AD-16 Correction in ARCHITECTURE-SPINE.md; shipped in 39f0a57 / PR #114. The legacy general-purpose surface is removed outright at v0.6.0, announced at that same release, with no deprecation-notice window and no shim layer — superseding the announce → notice → remove proposal FR-37/FR-38 previously carried. Scope note: this settles §4.10 only; the forward-looking deprecation policy for the three §7 public contracts remains open (§13 Q7 / AI-68) and is untouched by this amendment.]`

#### FR-38: Removal announcement at the release boundary
The legacy general-purpose surface is announced and removed in the same release: the v0.6.0 changelog and release notes name the retired commands, the removal version, and the `kt agent …` replacement, and carry a migration pointer. There is no notice period and no interim release in which the legacy commands still run. `[FIXED — ratified by Islam 2026-07-14 (Epic 9, story 9-3 Part B); AD-16 Correction in ARCHITECTURE-SPINE.md; shipped in 39f0a57 / PR #114.]`
**Consequences (testable):**
- The v0.6.0 changelog and release-notes entries state the retired command names, the 0.6.0 removal version, the replacement surface, and a migration pointer; historical pre-0.6.0 entries stay byte-unchanged.

#### FR-39: Continuity of identity & channels
The `kt` binary name, crates.io package, Homebrew tap, and install scripts carry over through the pivot; a legacy user upgrading gets a working, repositioned tool whose retired commands are documented at the release boundary — not a silent breakage. `[FIXED — ratified by Islam 2026-07-14 (Epic 9, story 9-3 Part B); AD-16 Correction in ARCHITECTURE-SPINE.md; shipped in 39f0a57 / PR #114. Continuity of identity and channels stands as written; only the notice-window half of this FR is struck.]`
**Consequences (testable):**
- Upgrading from v0.5.0 via each published channel yields a working `kt` whose surface is the agent runner plus binary self-maintenance; the retired legacy command names are absent, and their removal is stated in the v0.6.0 release notes the upgrade points at.

### 4.11 ACP Backend *(added 2026-09-19; landed in this document 2026-09-30)*

#### FR-40: ACP backend support
Any agent that speaks the Agent Client Protocol (ACP) v1 can be registered, started and supervised
as a Hekma backend via the builtin `acp` kind. Core controls — start, stop, pause, resume, send,
stop escalation, config, memory backing, fleet surfaces and adoption — behave as they do for other
kinds.
**Consequences (testable):**
- Usage is acquired in tiers — engine-observed, then the stderr sentinel — and ACP context usage is
  surfaced but never billed. When no tier yields usage, the surfaces show an honest not-available
  marker naming the tiers attempted. Nothing is fabricated.
- Client capabilities beyond the ACP baseline are not advertised; requests for them are refused with
  a surfaced diagnostic.
- A detached start is refused (the protocol's pipes die with the spawner); the session resumes at
  the next start's handshake when the agent supports it.

### 4.12 Hosted-Platform Readiness *(added 2026-09-30 — v1.x; sprint-change-proposal-2026-09-30-hosted-platform-readiness.md)*

#### FR-41: Service host mode
A Host can run the Engine as a long-running Service Host that owns supervision, metering listeners
and enforcement continuously, and drive it through a local, versioned Control API instead of
linking the library.
**Consequences (testable):**
- Supervision, crash detection, budget enforcement and event delivery run continuously while the
  host runs. On SIGTERM the host drains and stops every Agent Instance within a bounded window, and
  it never SIGKILLs an agent on its own exit before that window ends.
- Exactly one Service Host owns a state directory at a time; a second owner is refused with a clear
  diagnostic.
- The Control API binds only where configured, requires a bearer resolved through the secret
  mechanism, and returns no secrets and no raw agent output unless explicitly enabled.
- Events are available live (stream) and by cursor-based pull with one global order across event
  families, so a Host whose disk is ephemeral can export what it needs before the host exits.
- The Control API carries its own version and is marked unstable until a second Host validates it.

#### FR-42: Service-grade lifecycle
An Adapter can declare how its Agent proves it is ready, how it proves it is alive, which exit codes
are not failures, and how it pauses cooperatively; the Operator or Host can drain an Agent Instance
before stopping it.
**Consequences (testable):**
- `starting → running` happens only when the declared readiness probe passes within its timeout;
  otherwise the instance lands in `failed` with the probe's reason.
- A declared liveness failure (e.g. a stale heartbeat) moves a `running` instance to `failed` with
  cause `unresponsive`, and the Restart Policy applies.
- A declared restart-requested exit relaunches without counting toward the crash loop, and a
  declared clean exit is not a crash. The crash-loop counter resets after a stable window.
- A declared pause command drives pause and resume, surfaced as best-effort when it only stops new
  work.
- Drain stops new work, waits for in-flight work up to a bounded window, then stops, recording
  whether it completed or timed out.
- These declarations are an additive Adapter Contract minor (v1.1.0), so a gateway-style service
  agent is expressible as a manifest adapter.

#### FR-43: Governance at the wire and per unit of work
A Host can scope budgets to its own units of work and have a breach refuse further model calls at
the Engine's metering proxy.
**Consequences (testable):**
- A Host can begin and end a Task on a running Agent Instance. Usage in between is attributed to the
  Task, and per-Task budgets and latches apply alongside the per-Run and cumulative ones.
- A `block` Breach Action refuses further model calls through the proxy with a typed, non-retryable
  response until the ceiling is raised or reset. The breach is recorded like any other.
- Raising a ceiling re-arms its latch within the same Run.
- Per-call records are available to the Host — status class, latency, model when present, tokens,
  Task, sub-second time — and never contain request or response content.
- The docs state that the fence is as cooperative as the meter: an Agent that bypasses the injected
  endpoint bypasses both (NFR-6).

#### FR-44: Portable governance semantics
The Engine's budget and cost decision rules are published as versioned Governance Vectors that a
Host implementation in another language can run.
**Consequences (testable):**
- The vectors cover thresholds, latches and re-arm, evaluation order, integer arithmetic, rounding,
  saturation and repricing. CI verifies each vector against the Engine's own evaluator.
- Vectors are versioned and checksummed; changing a rule changes the vectors and the version.

#### FR-45: Reconciliation with confirmed actuals
A Host can submit provider- or Host-confirmed usage for a Run or Task, and the Engine records it as
`reconciled` alongside its own `estimated` figures.
**Consequences (testable):**
- Reconciled figures never overwrite estimated rows. Both stay readable, and the drift between them
  is reported.
- Surfaces label each figure `estimated` or `reconciled` (AD-8), and totals state which label they
  carry.

## 5. Cross-Cutting NFRs

- **NFR-1 Resilience.** An Agent crashing never crashes the Engine or `kt`; the Engine cleans up or adopts orphaned Agent processes after its own crash on next start. Partial failures across the Fleet degrade gracefully with per-instance reasons and remediations (durable gate).
- **NFR-2 Cross-platform parity.** Every FR behaves equivalently on Linux, macOS, and Windows. Where an OS lacks a primitive (e.g. POSIX signal semantics on Windows), the Engine provides the closest equivalent and the difference is documented, not silent. Process-control strategy per OS is an architecture deliverable (addendum).
- **NFR-3 Test coverage ≥ 95%** on `src/`, enforced in CI (`cargo tarpaulin --fail-under 95`) — carried gate, non-negotiable.
- **NFR-4 Performance budgets.** Read commands (Fleet listing, status, usage) complete in <1s on a 25-instance Fleet; Engine supervision overhead per running Agent Instance ≤ 2% CPU steady-state and ≤ 50MB RSS. `[ASSUMPTION: all three numbers are placeholders for architecture to validate.]`
- **NFR-5 Observability.** Engine and per-Agent-Instance logs are structured, timestamped, attributed, and rotation-bounded; stdout is for command output, stderr for diagnostics (carried constitution rule).
- **NFR-6 Security & privacy.** Secrets per FR-14; Agent Home isolation per FR-2 is process/filesystem-level, **not** a security sandbox — the boundary is stated in docs. `[NOTE FOR PM: if sandboxing ever becomes a promise, it is a v2+ initiative with real security review; v1 must not imply it.]`
- **NFR-7 Documentation currency.** Docs and README update in the same change as behavior (carried gate); the Adapter Contract and Embedding Interface docs version with the code.
- **NFR-8 Runtime & dependency policy.** Rust 2021+ single binary for `kt`; Engine as a Rust library crate; dependency additions follow the existing lean policy (clap/miette/indicatif/serde baseline). `[ASSUMPTION: no new heavyweight runtime deps without architecture sign-off.]`

## 6. Constraints & Guardrails

- **Cost.** The product's own promise applies to itself: governance features must not depend on paid third-party services; cost enforcement runs entirely locally. Dollar figures are estimates unless reconciled (FR-23) — the Engine must never present estimate as actual.
- **Safety.** Breach Actions act on lifecycle (pause/stop) — the Engine never silently drops an Agent's in-flight work without recording that it did (FR-6/FR-21 events). Crash-loop bounds (FR-9) prevent restart storms.
- **Privacy.** Usage Ledgers, config, memory, and logs stay local in the Agent Home / Engine state dir; no telemetry leaves the machine. `[ASSUMPTION: zero remote telemetry in v1 — flag any exception to Islam explicitly.]`

## 7. Public Surface, Versioning & Deprecation

Three public contracts, versioned independently:

1. **The Adapter Contract** — semver; Engine declares accepted range (FR-30). Breaking changes require a major bump and a migration note for Adapter authors.
2. **The Embedding Interface** — the Engine crate's public API, semver per Rust conventions; `kt`-only needs never justify breaking Hosts (FR-32 keeps them on the same surface, which is the point).
3. **The `kt` CLI surface** — command names, flags, exit codes, and `--json` schemas are a compatibility surface once v1 ships; breaking changes follow the same deprecation path as FR-38 (announce → notice → remove).

Deprecation policy (all three surfaces): announced in release notes, minimum one minor-version notice window, removal only at a major. `[ASSUMPTION: policy mechanics pending Islam.]`

## 8. Non-Goals (Explicit)

Carried from the brief (confirmed direction, list awaiting Islam's re-confirmation — §13 Q5):

- **Not an agent framework or authoring toolkit.** Ktesio runs Agents; it does not help build agent logic.
- **Not an LLM/model server.** Model hosting is out; Ktesio governs Agents, not models.
- **Not team/multi-user governance** — shared policies, org budgets, RBAC, audit, dashboards — including when embedded: the Host owns users, billing, policy.
- **No hosted control plane / web UI / SaaS from Ktesio itself.** Hosts may build those atop the Engine; that is the point of embedding, not a Ktesio deliverable.
- **No provider-side billing enforcement.** Caps act on the Agent Instance lifecycle from rate-derived estimates; Ktesio does not replace provider billing limits.
- **No deep per-agent feature parity.** The unified surface is the product; Agent-native extras stay native (reachable via config pass-through, FR-12).
- **No general-purpose skills package management** (deprecated per §4.10).
- **No security sandboxing claim in v1** (NFR-6).

## 9. MVP Scope

### 9.1 In Scope (v1)

- Features §4.1–§4.9 as specified, with the Hermes reference Adapter (FR-28) and the second-agent contract validation (FR-29, paper).
- Memory Backings: `filesystem` + `native` only (FR-16).
- Token Budgets + Rate-derived Cost Caps with configurable Breach Action (FR-18–FR-23).
- Engine library + `kt` on the public Embedding Interface (FR-31–FR-34).
- Agent-scoped Skills provisioning + the legacy surface's clean removal at v0.6.0 (FR-35–FR-39).
- All §5 NFRs including 3-OS parity and the ≥95% coverage gate.

### 9.2 Out of Scope for MVP

- **Second Adapter as shipped code** — v1 ships one implemented Adapter (Hermes) plus the contract-level validation of a second (opencode). `[FIXED — ruled by Islam 2026-07-02: option (a); SM-1 is fully measurable at v1.x when the opencode Adapter ships, interim measurement via the conformance kit's mock Adapter.]`
- Adapter registry / remote Adapter distribution — deferred; local + bundled Adapters only.
- Richer Memory Backings (vector stores, mem0/Letta-style tiers) — deferred to v1.x+ behind the same interface (FR-15).
- Per-window budgets (daily/monthly) — deferred (FR-18 note).
- Service/IPC delivery of the Engine (embed = Rust library only in v1) — §13 Q6.
- Agent Home export/import as a polished feature — `[NOTE FOR PM: FR-17 assumes basic export; demote to v1.x if it drags.]`
- skills.sh public search of the legacy manager — dropped in the refocus. `[ASSUMPTION — §13 Q7.]`
- Hosted/web anything, team governance, sandboxing — non-goals (§8).

### 9.3 v1.x additions

FR-40 (shipped in Epic 14) and FR-41..FR-45 (Epics 17–19, added 2026-09-30; renumbered from 15–17 on 2026-10-19 — main's autonomous Fibery-backlog tranches had consumed epic numbers 15/16 while the correct-course output sat uncommitted) are v1.x additions, not the v1 MVP.

## 10. Why Now

The 2025–2026 agent landscape fragmented into single-stack frameworks, single-agent harnesses, parallel launchers, in-code governance SDKs, and proxy-level budget tools — while personal agents (Hermes Agent and peers) became genuinely adoptable by individuals. The operator-facing runner that unifies lifecycle *and* dollar governance across heterogeneous agents is the identified gap (brief addendum §A/§B, research-grounded hypothesis). Platforms hosting personal agents are emerging now and are rebuilding runtime plumbing each — the embeddable-engine window is open before someone else standardizes it.

## 11. Success Metrics

**Primary**
- **SM-1 (north-star, ratified): Cross-agent operability** — 100% of core controls (lifecycle, config, memory, token limit, cost cap, interaction) behave identically across ≥2 structurally different Agents. Validates FR-5–FR-30. *Measurement basis ruled by Islam 2026-07-02: at v1, Hermes + the conformance kit's mock Adapter; fully measured at v1.x when the opencode Adapter ships.* `[FIXED]`
- **SM-2: Cost-cap efficacy** — 100% of runaway scenarios in the enforcement test suite end in the configured Breach Action within the latency bound. Validates FR-18–FR-23.

**Secondary**
- **SM-3: Time-to-operate** — a competent Operator goes from install to a governed, running Hermes Agent (UJ-1) in ≤15 minutes using only Ktesio docs. Validates FR-1–FR-5, FR-20–FR-21, FR-28. `[ASSUMPTION: placeholder target.]`
- **SM-4: Embeddability proof** — binary: `kt` consumes only the public Embedding Interface, enforced in CI. Validates FR-31–FR-32.
- **SM-5: Adapter effort** — a developer produces a working Adapter for a new Agent within ≤1 person-day against contract docs + conformance kit. Validates FR-27, FR-30. `[ASSUMPTION: placeholder target.]`

**Counter-metrics (do not optimize)**
- **SM-C1: Adapter catalog breadth.** More Adapters is *not* v1 success; uniformity of controls is. Counterbalances SM-1/SM-5 — resist shipping shallow Adapters to inflate the count.
- **SM-C2: Per-agent feature depth.** Surfacing every native capability of every Agent is explicitly not the goal (§8); counterbalances SM-3 pressure to "just expose the native thing."
- **SM-C3: Estimate precision theater.** Do not chase cost-estimate precision into presenting estimates as actuals or blocking UX on reconciliation; honesty labels (FR-23) outrank precision. Counterbalances SM-2.

## 12. Feature-Sequencing Note for Epics

Suggested epic seams (not a plan, a shape): Engine core + state machine + persistence (§4.1–4.2) → config + secrets (§4.3) → metering/budgets/caps (§4.5) → interaction (§4.6) → Adapter Contract + Hermes (§4.7) → Embedding Interface hardening (§4.8, `kt` is on it from day one via FR-32) → memory (§4.4) → skills provisioning + migration (§4.9–4.10). `[ASSUMPTION: sequencing is architecture/PM's to overrule.]`

## 13. Open Questions

1. ~~Second Agent~~ **RESOLVED 2026-07-02:** opencode (opencode.ai), Islam's selection. Structural characterization vs Hermes happens in the conformance mapping (FR-29).
2. **Licensing/positioning (blocks Host go-to-market, not the build):** PolyForm Noncommercial today; embedding makes the commercial question concrete. Owner: Islam.
3. ~~Breach Action default~~ **RESOLVED 2026-07-02:** `pause` ratified as the shipped default (per-instance configurable stands).
4. **v1 Memory Backing set:** `filesystem` + `native` proposed (FR-16); confirm or extend. Owner: Islam.
5. **Non-goals re-confirmation** (§8 list, post-embedding-expansion). Owner: Islam.
6. **Engine delivery surface beyond Rust lib** (service/IPC for non-Rust Hosts): v1 = lib only `[ASSUMPTION]`; decide the v1.x path. Owner: Islam + architecture. **Answered 2026-09-30 (ratified with sprint-change-proposal-2026-09-30-hosted-platform-readiness.md):** a Service Host with a local HTTP/JSON Control API (FR-41, AD-20),
  unstable `0.x` until a second Host validates it. The library remains the primary embedding surface.
7. **Deprecation window mechanics & skills.sh search fate** (§4.10, §9.2). Owner: Islam.
8. **Metering mandatory at registration** (FR-19 rejects Adapters with no Metering Source): confirm this hard line. Owner: Islam. **Answered 2026-09-30 (ratified with the same proposal):** metering stays mandatory at registration for manifest adapters
  (FR-19). The `acp` kind's surfaced gap (FR-40) is the one sanctioned exception. A Host that meters
  at its own choke point may feed Hekma through reconciliation (FR-45), which satisfies FR-19's
  intent without faking a source.

## 14. Assumptions Index

Every inline `[ASSUMPTION]`, indexed:

1. §2.1 — Agent-author audience: JTBD acknowledged, no v1 author-facing features beyond the published contract.
2. §3 Engine — library-first delivery; service/IPC out of v1 (Q6).
3. §3 Lifecycle State — exact state set (`registered`…`failed`) subject to architecture refinement.
4. §3 Restart Policy — `never`/`on-failure`/`always` with default `never`.
5. §3 Token Budget / FR-18 — scopes are per-run + cumulative; per-window deferred.
6. §3 Rate / FR-20 — split input/output rates; no retroactive repricing.
7. ~~§3 Breach Action / FR-21 — `pause` default~~ RESOLVED 2026-07-02: ratified.
8. §4.1 — no remote Adapter registry in v1; FR-1 multi-instance in scope; FR-2 concurrent same-kind instances; FR-3 `--force` semantics.
9. §4.2 — FR-4 freshness bound (2s); FR-6 graceful window default (30s); FR-9 backoff defaults + crash-loop bound (5); FR-10 usage flush bound (≤5s).
10. §4.3 — FR-11 precedence chain; FR-14 `--reveal` flag; secret storage mechanism deferred to architecture.
11. §4.4 — v1 backing set (Q4); FR-15 no hot-swap; FR-17 Agent Home export in v1 (may demote).
12. §4.5 — FR-19 metering mandatory (Q8); FR-21 enforcement-latency placeholder.
13. §4.6 — FR-25 retention default (10MB); FR-26 JSON schema compatibility testing.
14. §4.7 — FR-27 conformance kit in v1; FR-28 integration-test isolation strategy deferred.
15. §4.9 — FR-35 native-skills mapping is the Adapter's job; ~~FR-37 notice mechanics~~ RESOLVED 2026-07-14: moot — the legacy surface was removed outright at v0.6.0, so there are no notice mechanics to specify.
16. §4.10 — ~~FR-38 window (≥90 days / one minor cycle)~~ RESOLVED 2026-07-14: no notice window — announce and removal both land at v0.6.0; skills.sh search dropped (Q7).
17. §5 — NFR-4 all performance numbers placeholders; NFR-8 dependency policy.
18. §6 — zero remote telemetry in v1.
19. §7 — deprecation policy mechanics.
20. ~~§9.2/§11 — SM-1 interim measurement basis~~ RESOLVED 2026-07-02: option (a) ruled; opencode named as Adapter #2 / second Agent.
21. §12 — epic sequencing is advisory.
