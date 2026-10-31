---
title: "PRD: Hekma — the Control Plane for Agent Workloads"
status: review
created: 2026-07-02
updated: 2026-10-30
---

# PRD: Hekma — the Control Plane for Agent Workloads

## 0. Document Purpose

This PRD turns the accepted product brief (`_bmad-output/planning-artifacts/briefs/brief-ktesio-2026-07-02/brief.md`, status: updated) into buildable requirements for the repositioned Ktesio. It is written for the downstream BMAD chain — architecture, epics/stories, implementation — and for Islam as the deciding owner. Vocabulary is anchored in §3 Glossary and used verbatim everywhere; features are grouped in §4 with globally numbered FRs; inferences are tagged inline as `[ASSUMPTION]` and indexed in §14. The brief and its addendum remain the product-level "why"; this document is the "what." It was drafted headless while Islam is away: nothing here overrides a `[FIXED]` brief constraint, and every invented detail is tagged.

> *(Amended 2026-10-30.)* The control-plane re-baseline (`sprint-change-proposal-2026-10-30-control-plane-pivot.md`, approved by Islam) is now the product-level why for §1, §2, §4.14 and every section marked amended 2026-10-30. Where it overrides a `[FIXED]` item (the Agent definition, the Breach Action default), the item carries Islam's 2026-10-30 ruling. Elsewhere the brief still governs.

## 1. Vision

*(Amended 2026-10-30 — the control-plane proposition, `sprint-change-proposal-2026-10-30-control-plane-pivot.md`.)*

Hekma is the control plane for agent workloads — one way to run, see and steer any agent (CrewAI, LangGraph, Hermes, OpenCode or a raw model call), from a laptop to a fleet, without ever being the reason prod stops. Control comes in depths: Hekma can observe a run, govern its traffic on the wire, accept its announcement, or supervise its process; every run shows which depth applies. The CLI serves individuals and emergencies; the Control API and OTLP telemetry are the daily surface for teams.

Running a personal agent today means living inside whatever runner its authors shipped; running two means learning two of everything — lifecycle, config, memory, and whatever cost story each one tells. Hekma ends the per-agent tax. It is a **runtime engine** that wraps any Agent behind one operating model: the same start/pause/resume/stop, the same configuration surface, the same way to wire memory, the same token budgets and dollar cost caps — identical whether the Agent underneath is NousResearch's Hermes Agent or something structurally nothing like it.

The engine ships two ways at once. Operators get `hekma`, a cross-platform CLI that installs, isolates, and fully controls personal agents from the terminal. Hosts — platforms and apps that run personal agents for their users — embed the same engine and inherit a resilient, consistent, predictable runtime instead of building supervision, budget enforcement, and memory plumbing in-house. One engine, two surfaces; `hekma` is the engine's first and complete consumer, which is also the standing proof that embedding works.

The bet is that consistency itself is the product. Agents become interchangeable workloads: adopt one the way you adopt a container image, and your existing controls just apply. "Runs under Hekma" should come to mean *governable, cost-bounded, and consistent to operate* — for a solo operator's laptop fleet and for a host's production runtime alike.

## 2. Target User

### 2.1 Jobs To Be Done

- **Operate any personal agent with controls I already know** — start it, pause it, stop it, inspect it — without reading its native runner docs. (Functional)
- **Never be surprised by an agent's bill** — set a token or dollar ceiling once and hear about it loudly the moment it is crossed, or have Hekma pause or stop the agent when I opted in, even when I am not watching. (Functional, emotional: relief from runaway-spend anxiety) *(Amended 2026-10-30 with the `warn` default.)*
- **Adopt a new agent in minutes, not evenings** — install, isolate, configure, and run it through one consistent surface. (Functional)
- **Give my agent the memory and skills it needs** without bespoke wiring per agent. (Functional)
- **As a framework team, run and steer production flows the same way whatever they are built on** — engineers running CrewAI, LangGraph or similar flows in production get uniform control and telemetry with little or no integration work, and no unexpected shutdowns. (Functional, emotional: no fear that a cost guard strands prod work) *(Added 2026-10-30.)*
- **As a Host, offer agent hosting without building a runtime** — embed an engine that makes heterogeneous agents resilient, consistent, and predictable for my users, while I keep my own user management and billing. (Functional, strategic)
- **As an Agent author, make my agent first-class everywhere** by writing one Adapter against a small, stable contract. (Functional) `[ASSUMPTION: author audience is candidate-tier per the brief; JTBD kept but no v1 features target authors beyond the published Adapter Contract itself.]`

### 2.2 Non-Users (v1)

- Users wanting a hosted SaaS or a web dashboard — Hekma ships a CLI, an embeddable engine, a self-hosted Control API and OTLP telemetry; dashboards live in the operator's observability stack. Org RBAC and audit evidence are planned later (themes D/F), not v-next. *(Amended 2026-10-30.)*
- Agent *builders* looking for a framework to write agent logic in — Ktesio runs agents; it does not author them.
- Non-technical end users — they are served indirectly through Hosts that embed the engine.

### 2.3 Key User Journeys

*Dev-product dial: compact narratives, named protagonists. FRs reference these by ID.*

- **UJ-1. Noor caps Hermes before it can surprise her.**
  Noor, an indie developer, pays for model API keys out of pocket. She installs `kt`, registers Hermes Agent from the built-in reference Adapter, sets a Cost Cap of $5 for the session at her provider's Rate with the Breach Action set to `pause`, and starts the Agent Instance. Hermes runs a long research loop; the Usage Ledger climbs. At the cap, the engine executes the Breach Action — the Agent Instance is paused and `kt` says so, with tokens and estimated dollars spent. Noor resumes with a raised cap after dinner. She was never surprised. **Edge case:** Hermes' Adapter reports usage in batches; the engine enforces on the estimate and reconciles when the next report lands, labeling the figures as estimates throughout.
- **UJ-2. Noor adopts a second agent and already knows how to drive it.**
  A week later Noor tries a structurally different Agent — single-shot, no gateway, no native memory. Same commands: register, configure, set Token Budget, start, interact, stop. Nothing new to learn; the Adapter absorbed the differences and declared what it cannot support (pause is best-effort). `kt` surfaced that declaration up front. This journey *is* the north-star metric (SM-1).
- **UJ-3. Lena embeds the engine under her hosting platform.**
  Lena is a platform engineer at a startup that hosts personal agents for consumers. She links the Ktesio engine library, drives it through the Embedding Interface — register/start/pause/stop Agent Instances, inject per-user config and caps, subscribe to Lifecycle State and usage events — and renders her own UI on top. Her platform keeps its own accounts and billing; Ktesio keeps every hosted agent resilient, bounded, and uniformly controllable. **Edge case:** an Agent Instance crashes mid-run; the engine applies the Restart Policy, emits the state-change event, and Lena's dashboard shows the recovery without her writing supervision code.
- **UJ-4. Omar steers a production CrewAI flow with almost no integration.** *(Added 2026-10-30.)* `[ASSUMPTION: drafted from Islam's brainstorm statements, not narrated by a real user — confirm.]`
  Omar runs his company's CrewAI flows on a cloud VM. He installs Hekma there as a Service Host, points the flows' model base URL at its Metering Proxy with a Run Key bound to the flows' labels, and adds the one-line CrewAI helper that announces each flow run. Every flow run now appears as an `announced` Run with a budget, and Hekma's OTLP metrics land in the team's existing dashboards. One night a run loops and crosses its ceiling: the default `warn` raises an Anomaly and an alert, nothing stops. The on-call engineer holds that Run by Selector, raises its ceiling, and releases it; the run finishes with its state intact. A Hekma upgrade later that week restarts the Service Host in Handoff Mode; at most one call is retried, and no flow stops. **Edge case:** a flow that calls its provider with its own key is listed as unscented under `shadow`, and the team sees it before switching that environment to `enforce`.
- **UJ-5. Noor watches every session she runs from one screen.** *(Added 2026-10-30.)* `[ASSUMPTION: drafted from Islam's brainstorm statements — confirm.]`
  Noor now runs six OpenCode and Hermes sessions at once, some on her laptop and some on a small VM, all with key custody on (FR-53) so every session's calls pass through the Metering Proxy. `hekma top` on her laptop shows all of them through the Hub, exception-first: one session, burning tokens far faster than the rest, has crossed its Token Budget under `warn`, so it carries an Anomaly and is listed. She holds it, reads its recent calls, and reroutes its held call to a cheaper model. The others never leave the "healthy" count.

## 3. Glossary

*Downstream workflows must use these terms exactly; synonyms are a discipline violation.*

- **Agent** — a third-party agent workload that Hekma governs: an agent program (personal or coding agent, e.g. Hermes Agent, opencode), an agent flow inside a framework process (e.g. CrewAI, LangGraph), or a direct model-calling client. Not authored by Hekma. `[FIXED — coding agents clarified by Islam 2026-09-07; widened to framework flows and model clients 2026-10-30.]`
- **Adapter** — the integration component that makes one Agent runnable by the Engine by satisfying the Adapter Contract. One Adapter per Agent kind.
- **Adapter Contract** — the versioned, documented specification an Adapter implements: lifecycle operations, config mapping, Metering Source declaration, memory attachment, interaction channel, and a Capability Declaration.
- **Capability Declaration** — the Adapter's machine-readable statement of which controls it supports as *guaranteed*, *best-effort*, or *unsupported* (e.g. pause: best-effort).
- **Engine** — the embeddable Ktesio core: registry, supervisor, config, metering, budget enforcement, memory wiring, interaction routing. Delivered as a Rust library. `[ASSUMPTION: library-first; service/IPC delivery is out of v1 — see §13 Q6.]`
- **Embedding Interface** — the Engine's public surface a Host (and `kt` itself) drives: commands in (lifecycle, config, budgets), state/usage/events out.
- **Host** — a platform or app that embeds the Engine to run Agents for its own users.
- **Operator** — the human who runs and configures Hekma directly: through `kt`, configuration, or the Control API (FR-41) with a bearer; never a Caller. *(Amended 2026-10-30.)*
- **Agent Instance** — one registered, installed occurrence of an Agent under Engine management, with its own Agent Home, config, budgets, and Lifecycle State.
- **Run** — one unit of agent work with a stable run id, a Control Depth, labels, budgets and a Run Status. Runs are flat and grouped by labels; a Run may optionally name a parent Run (FR-49). A supervised Agent Instance's process span is one Run, and a relaunch by the Restart Policy (FR-9, FR-42) stays within it; one process may host many Runs. A Run keeps its run id when its Control Depth changes. Token Budgets, Cost Caps, Breach Actions and the Usage Ledger apply per Run as well as per Agent Instance. *(Added 2026-10-30; supersedes AD-7's process-span definition.)*
- **Control Depth** — how far Hekma controls a Run, a strict ladder in which each rung includes the one below: `observed` (Hekma knows the Run from its telemetry or its announcement, not from its model traffic), `wire` (the Run's model traffic passes through the Metering Proxy, which can meter, hold, reroute or reject its calls; Hekma knows the Run only from that traffic), `announced` (wire, and the Run declared itself and keeps a Lease; it holds a Run Key unless it announced only through an OTel resource attribute, FR-52), `supervised` (announced, and Hekma owns the process). A supervised Run whose traffic does not pass through the Metering Proxy shows as `supervised (no wire)` (FR-49). Independent of Lifecycle State, which only supervised Runs carry; not to be confused with the `engine-observed` Metering Source.
- **Run Status** — `active`, `held`, `silent`, `stale` (its Node is unreachable) or `ended`, shown with any Anomaly flags. A Run ends when its process span ends (supervised), when it announces its end, or when it has been `silent` longer than the silent window; ending is bookkeeping and never acts on a process. `[ASSUMPTION: exact set and silent window — architecture may refine.]`
- **Anomaly** — a condition Hekma makes loud; an Anomaly never acts on a Run by itself (any action comes from a Breach Action, a verb or Scent-Check `enforce`): a Token Budget or Cost Cap breach under any Breach Action (FR-21), an unscented call, a detected bypass, a Run gone `silent`, a supervised Run that `failed` and was not relaunched (FR-49), a Breach Action its Control Depth cannot execute (FR-49, FR-60). Shown in `hekma ps`, emitted as an event and counted in OTLP.
- **Run Key** — a per-Run credential that the client presents in its provider API-key slot; the Metering Proxy identifies the Run from it and swaps in the real provider key (key custody), so it is also the client's virtual provider key. Issued when a Run announces through the Metering Proxy or the Control API (FR-52; never through telemetry) or starts supervised. Valid for the Run's life and revocable at any time; it does not expire on a clock, because clients never refresh an API key mid-run. Rotation is optional, through a refresh credential that goes only to the announcing process or, for a supervised Run, to the process the Engine launched (a relaunch gets a fresh one). *(Amended 2026-10-30: the Run Key is the virtual provider key, ruled by Islam.)*
- **Proxy-access credential** — an Operator-issued credential per Metering Proxy endpoint binding that lets a base-URL-only client reach the proxy beyond loopback; it rides in the API-key slot and is swapped for the binding's custodied key, and it is not a Scent-Check identity. *(Added 2026-10-30.)*
- **Caller** — any client acting on a Run through its Run Key, an announce-scoped credential or a Run-scoped request (an agent, a framework or a helper), as opposed to the Operator or Host acting through configuration, the CLI or a Control API bearer. A Caller can only tighten limits, never loosen them.
- **Policy** — Operator- or Host-configured limits that apply to the Runs a Selector matches: Token Budgets, Cost Caps, Breach Actions and the Scent-Check Mode (FR-60). Limits the Operator or Host sets explicitly on an Agent Instance or a Run outrank every Policy; Policies fill only what is unset. A limit a Caller sets applies only while it is tighter than the effective Operator, Host or Policy limit. Versioned, live policy-as-data is later work (theme D).
- **Lease** — the claim to be alive of a Run that announced itself (at depth `observed` or `announced`) or runs `supervised`, renewed by heartbeat or traffic; an expired Lease marks the Run `silent`, not gone. A `wire` Run has no Lease and goes `silent` after an inactivity window. `[ASSUMPTION: Lease length and inactivity window defaults set in the FR-52 story spec.]`
- **Selector** — a label expression (`key=value,…`) that names a set of Runs for any listing, verb or Policy.
- **Scent-Check Mode** — per Policy, how Hekma treats a call without a valid Run Key (an endpoint-path label binding identifies a Run for listing and Policies but is not a Scent-Check identity): `off`, `shadow` (served and recorded as unscented) or `enforce` (refused). It governs identity only and never changes how a Breach Action behaves. A call with no identity is governed by the Policy matching the labels bound to the endpoint it arrives on, else the default Policy.
- **Metering Proxy** — the Engine's interception point for model traffic (the `engine-observed` path, FR-19); a Service Host can expose it as an endpoint that clients it did not start point their base URL at (FR-51).
- **Shim** — the small process owner, one per supervised process, that keeps that process and every Run it hosts alive across Service Host restarts (FR-55); the mechanism is architecture's to choose.
- **Handoff Mode** — the Service Host shutdown mode in which supervised Runs stay up and a restarted Service Host reattaches (FR-55).
- **Node / Hub** — a Service Host on one machine (Node), and the Service Host that merges several Nodes into one view and routes verbs to them (Hub).
- **Agent Home** — the per-Agent-Instance isolated directory holding its config, state, logs, Skill Set, and working data.
- **Lifecycle State** — one of: `registered`, `starting`, `running`, `paused`, `stopping`, `stopped`, `failed`. `[ASSUMPTION: exact state set — architecture may refine.]`
- **Restart Policy** — per-Agent-Instance rule for automatic restart on failure: `never`, `on-failure`, `always`. Default `never`. `[ASSUMPTION]`
- **Memory Backing** — a storage destination for an Agent's memory, attached through the Engine's memory interface.
- **Token Budget** — a ceiling on tokens an Agent Instance or a Run may consume within a scope (per run and cumulative). `[ASSUMPTION: scopes]`
- **Rate** — an Operator/Host-supplied price per 1M tokens (input and output rates supported). `[ASSUMPTION: split rates]`
- **Cost Cap** — a dollar ceiling derived from Rate × metered tokens, enforced by the Engine.
- **Breach Action** — what the Engine does when a Token Budget or Cost Cap is reached: `warn` (default), `pause`, `stop`, or `block`. `[FIXED — warn default ratified by Islam 2026-10-30, superseding the 2026-07-02 pause default.]`
- **Metering Source** — where usage numbers come from for an Agent Instance, declared by its Adapter: `self-reported` (Agent/Adapter reports) or `engine-observed` (Engine intercepts model traffic through its Metering Proxy, i.e. Control Depth `wire` or deeper — unrelated to the Control Depth `observed`). A Run with no Adapter is metered `engine-observed` through the Metering Proxy (depth `wire` or deeper), or, at depth `observed`, only from its own telemetry. Figures are estimates unless reconciled.
- **Usage Ledger** — the Engine's per-Agent-Instance and per-Run record of tokens and derived dollars over time.
- **Skill** — a reusable instruction/capability directory provisioned *to a specific Agent Instance* (agent-scoped; the general-purpose skills manager is deprecated).
- **Skill Set** — the locked collection of Skills provisioned into one Agent Home.
- **Fleet** — all Agent Instances and Runs the Engine manages in a given installation.
- **`kt`** — the Ktesio CLI; the Engine's complete reference frontend.
- **Service Host** — a long-running process that owns an Engine continuously and serves the Control API (FR-41); a Node when part of a Hub (FR-58).
- **Control API** — the Service Host's versioned HTTP/JSON surface for Operators and Hosts: lifecycle, configuration, usage, Runs, Policies, and events by stream or cursor. Local by default; reachable beyond loopback only when configured, with authentication and transport encryption.
- **Task** — *superseded 2026-10-30 by Run*: a Host-defined unit of work is a Run (FR-49) with Host-chosen labels; there is no separate Task scope (FR-43 amendment).
- **Drain** — stop new work, wait for in-flight work up to a bounded window, then stop (FR-42).
- **Readiness / Liveness Probe** — Adapter-declared checks that an Agent can serve, and is still alive, beyond its process existing (FR-42).
- **Governance Vectors** — the versioned, portable test cases that state the Engine's budget and cost decision rules (FR-44).
- **OTLP / Telemetry Export** — the opt-in path by which the Service Host pushes engine metrics (never content) to an operator-configured OTLP collector endpoint (FR-46).
- **Adapter Gallery** — the docs page of curated Agents that run under Hekma, each validated by the conformance kit in CI with a per-capability honesty table (FR-48).

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

> *(Amended 2026-10-30.)* Runs may also be created by self-announce (FR-52) or wire discovery (FR-51), without registering an Agent Instance.

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

> *(Amended 2026-10-30.)* For a supervised Run whose traffic passes through the Metering Proxy, the Operator can ask for a wire hold (FR-56) instead of native pause; it is surfaced as a qualified pause, never as native. A `pause` Breach Action on such a Run whose native pause is `unsupported` uses the wire hold, qualified. Without a wire path, the rules above stand unchanged.

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

**Description:** The headline guardrails. Token Budgets bound consumption; a supplied Rate turns tokens into dollars and a Cost Cap bounds those. Enforcement happens at the runner level — by default the Engine signals loudly when a ceiling is hit; opted in, it acts on the Run's lifecycle or its calls — which proxy-layer budget tools do not do. *(Amended 2026-10-30.)* Figures are labeled estimates unless reconciled. Realizes UJ-1, UJ-3.

#### FR-18: Token Budgets
The Operator can set a Token Budget per Agent Instance at two scopes: per-run and cumulative. `[ASSUMPTION: scopes; per-window (daily/monthly) deferred.]`
**Consequences (testable):**
- Consumption reaching the budget triggers the configured Breach Action within one metering interval.
- Budgets are inspectable and changeable while `running`; changes apply immediately.

#### FR-19: Metering ingestion
The Engine ingests usage per the Adapter's declared Metering Source: `self-reported` (Adapter forwards the Agent's own usage accounting) or `engine-observed` (the Adapter routes the Agent's model traffic through the Engine's Metering Proxy).
**Consequences (testable):**
- The active Metering Source for each Agent Instance is visible in Fleet listing detail.
- With `self-reported`, delayed batches reconcile the Usage Ledger without double-counting; with `engine-observed`, usage lands in the Usage Ledger within the flush bound of the call completing.
- An Adapter with no viable Metering Source is rejected at registration with a clear diagnostic (metering is mandatory for governance honesty). `[ASSUMPTION: metering mandatory — a no-metering escape hatch would hollow out the core promise; flagged for Islam.]`

> *(Amended 2026-10-30.)* Metering at registration remains mandatory for supervised Agent Instances. `wire` and `announced` Runs, and supervised Runs whose traffic passes through the Metering Proxy, are metered there; `supervised (no wire)` Runs keep their Adapter's reports; `observed` Runs carry only the usage their own telemetry reports, labeled estimated.

#### FR-20: Rate & cost derivation
The Operator can supply a Rate (input and output $/1M tokens, per Agent Instance or per model key) and the Engine derives dollar figures in the Usage Ledger from metered tokens.
**Consequences (testable):**
- Changing the Rate re-prices future consumption only; the Usage Ledger keeps historical dollars at the Rate in force when consumed. `[ASSUMPTION: no retro-repricing.]`
- With no Rate supplied, dollar features are inert and say so; token features work fully.

#### FR-21: Cost Cap enforcement
The Operator can set a Cost Cap per Agent Instance (per-run and cumulative scopes, same as FR-18); on breach, the Engine executes the Breach Action — a loud signal under `warn`, otherwise an action on the Run's lifecycle or its calls. *(Amended 2026-10-30.)*
**Consequences (testable):**
- In an integration test driving a mock Agent past its Cost Cap, the Breach Action executes in 100% of runs, and the latency from ledger-breach to the signal or action is ≤ the metering interval + 1s. `[ASSUMPTION: latency placeholder.]`
- The Breach Action is configurable per Agent Instance among `warn` / `pause` / `stop` / `block`, with `warn` the ratified default; a breach always emits a loud signal (event, `ps` Anomaly, an OTLP counter when export is configured (FR-46), and a response header where the call passes through the Metering Proxy) whatever the action. *(Amended 2026-10-30; the default was `pause` until then.)*
- On upgrade, every Agent Instance registered before the change that relied on the implicit default keeps `pause`, written into its config explicitly; a Breach Action set explicitly at any configuration layer is untouched, and only new registrations get `warn`. Each instance kept on `pause` this way carries a "kept on pause by upgrade" note in the Fleet listing (FR-4) and on each of its Runs wherever `hekma ps` lists them (human-readable and `--json`), until a Breach Action is set on it explicitly, and one event is emitted at upgrade. *(Ratified by Islam 2026-10-30.)*
- The default changes for programmatic registrations only in a semver-breaking Embedding Interface release, or at a Control API minor version under §7 item 4, each with a release note; every registration response and registration event names the effective Breach Action, so a Host learns of the change before it applies.

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

#### FR-41: Service Host mode
A Host can run the Engine as a long-running Service Host that owns supervision, metering listeners
and enforcement continuously, and drive it through a versioned Control API (loopback by default) instead of
linking the library.
**Consequences (testable):**
- Supervision, crash detection, budget enforcement and event delivery run continuously while the
  Service Host runs. On SIGTERM the Service Host drains and stops every Agent Instance within a bounded window, and
  it never SIGKILLs an agent on its own exit before that window ends — unless the Service Host runs in
  Handoff Mode (FR-55), in which case Runs stay up and a restarted Service Host reattaches.
  *(Amended 2026-10-30.)*
- Exactly one Service Host owns a state directory at a time; a second owner is refused with a clear
  diagnostic. When none runs, the first CLI command that needs a write or the Metering Proxy starts
  a local, loopback-only Service Host in the background and every CLI command routes through it;
  read-only commands work without one. *(Amended 2026-10-30, ruled by Islam.)*
- The Control API binds only where configured, requires a bearer resolved through the secret
  mechanism, requires transport encryption beyond loopback — a non-loopback bind without both is refused at start with a clear diagnostic, and it is how `hekma top` (FR-57) and a Hub (FR-58) reach a remote Service Host *(amended 2026-10-30)* — and returns no secrets and no raw agent output unless explicitly enabled.
- Events are available live (stream) and by cursor-based pull with one global order across event
  families, so a Host whose disk is ephemeral can export what it needs before the Service Host exits.
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
the Engine's Metering Proxy.
**Consequences (testable):**
- A Host can open and close a child Run (FR-49) on a running Agent Instance. Each call is
  attributed by the run identity it carries (Run Key or identity header), never by time window; a
  call with no child's identity stays on the parent Run and is never split across open children.
  A child's usage rolls up to its parent, and the child's budgets and latches apply
  alongside the parent's and the cumulative ones. *(Amended 2026-10-30: Task became a child Run.)*
- A `block` Breach Action refuses further model calls through the Metering Proxy with a typed, non-retryable
  response until the ceiling is raised or reset. The breach is recorded like any other.
- Raising a ceiling re-arms its latch within the same Run.
- Per-call records are available to the Host — status class, latency, model when present, tokens,
  run id, Control Depth, Run Key identity once FR-53 lands, sub-second time — and never contain
  request or response content. *(Amended 2026-10-30.)*
- The docs state that the fence is as cooperative as the meter: an Agent that bypasses the injected
  endpoint bypasses both (NFR-6).

> *(Amended 2026-10-30.)* Task is absorbed into Run: a Host unit of work is a child Run (FR-49) the
> Host opens through the Control API or self-announce. `block` is never a default Breach Action; once
> configured it always refuses, whatever the Scent-Check Mode. Key custody (FR-53) and bypass
> detection (FR-54) narrow the cooperative-fence caveat for supervised Runs.

#### FR-44: Portable governance semantics
The Engine's budget and cost decision rules are published as versioned Governance Vectors that a
Host implementation in another language can run.
**Consequences (testable):**
- The vectors cover thresholds, latches and re-arm, evaluation order, integer arithmetic, rounding,
  saturation and repricing. CI verifies each vector against the Engine's own evaluator.
- Vectors are versioned and checksummed; changing a rule changes the vectors and the version.

#### FR-45: Reconciliation with confirmed actuals
A Host can submit provider- or Host-confirmed usage for a Run (including a child Run), and the Engine records it as
`reconciled` alongside its own `estimated` figures.
**Consequences (testable):**
- Reconciled figures never overwrite estimated rows. Both stay readable, and the drift between them
  is reported.
- Surfaces label each figure `estimated` or `reconciled` (AD-8), and totals state which label they
  carry.

### 4.13 Adoptability *(added 2026-10-23 — v1.x; sprint-change-proposal-2026-10-23-adoptability.md)*

#### FR-46: OTLP telemetry export
The Operator (or Host) can configure the Service Host to export engine metrics over OTLP to a
collector endpoint they choose. Export is opt-in and disabled by default; payloads carry counters
and gauges derived from the Usage Ledger and supervision state — never request, response, or agent
content, never secrets.
**Consequences (testable):**
- With no collector endpoint configured, no export path exists: no dial, no payload, nothing leaves
  the machine (§6 default preserved) — enforced by test.
- Every exported dollar figure carries its estimate/reconciled label (FR-23, FR-45) as an
  attribute; no code path exports an unlabeled dollar amount.
- The exposition is versioned: metric names, types and attributes are a documented, semver'd
  surface; breaking changes follow the §7 deprecation path.
- Payloads contain no secrets and no request/response content — the FR-14/FR-43 sentinel no-leak
  matrix is extended to the export path.
- A failed or undeliverable export degrades visibly (diagnostic and counter) and never delays
  supervision or enforcement (NFR-1).

#### FR-47: Agents-in-CI recipe
The docs ship a recipe for running a budget-capped agent as a GitHub Actions step: the recipe
starts the agent under Hekma with a Token Budget and Cost Cap, drives a review-style task, writes
usage into `$GITHUB_STEP_SUMMARY`, and the job's outcome is honest about what ended it.
**Consequences (testable):**
- Spend is capped per run: the recipe sets the Breach Action to `stop` explicitly (the default is
  `warn` since 2026-10-30), and a breach executes it within the FR-21 latency bound, and the job ends in a state that names the breach — no path masks a breach as success.
- Every lane is wall-clock bounded (`timeout-minutes`); a lane that ends on a bound reports which
  one — budget or clock — not a generic failure.
- Usage lands in `$GITHUB_STEP_SUMMARY` rendered from `--json` output: tokens and derived dollars,
  labeled estimated (FR-22/FR-23).
- Exit codes are the stable documented surface (FR-26): success, breach, and agent failure are
  distinguishable by a workflow consumer.
- The recipe is exercised as a dogfood lane in this repository's own CI — a real Ktesio bot run,
  budget-capped — not merely documented.

#### FR-48: Adapter gallery
The docs ship an adapter gallery: a curated page of Agents that run under Hekma, each entry backed
by a manifest validated against the conformance kit in CI, with a per-capability honesty table.
**Consequences (testable):**
- The gallery opens with ten seed entries — Copilot CLI, Hermes, OpenCode, Claude Code, Codex,
  Gemini CLI, goose, Crush, Aider, Cursor CLI — each a validated manifest or an honestly named gap.
- The honesty table uses the Capability Declaration vocabulary (guaranteed / best-effort /
  unsupported) with an explicit absence token where a capability or its metering is untested —
  never a fabricated pass or an implied zero (AI-18).
- Every gallery manifest is validated in CI by `hekma-conformance`; a manifest that fails, or an
  entry claiming more than its validation proves, fails the build.
- Where no metering tier yields usage, the entry shows the honest not-available marker naming the
  tier (the FR-40 precedent), never a blank.
- Public gallery pages carry no planning-provenance wording (epic/story/proposal references) —
  enforced by the `check_docs` gate.
- Additions respect SM-C1: an entry ships only when its controls behave uniformly; breadth is not
  the metric.

### 4.14 Control Plane *(added 2026-10-30 — next horizon; sprint-change-proposal-2026-10-30-control-plane-pivot.md)*

**Description:** Hekma governs Runs, not only the processes it starts. Any agent workload — a supervised Agent Instance, a framework flow that announced itself, a client that only points its model base URL at the Metering Proxy, or one that only sends Hekma its telemetry — becomes a Run with a Control Depth, labels and budgets, listed and steered through the same verbs, Control API and OTLP telemetry. Teams work through the Control API and their own observability stack (OTel-first, UI-optional); the CLI serves individuals and emergencies. Realizes UJ-4, UJ-5. Principles that bind every FR here: signal, don't kill (NFR-9); a Run's duration never changes how it is treated; a Caller can never loosen what a Policy sets; and every surface scales to millions of Runs (NFR-4).

#### FR-49: Runs as first-class entities
The Engine records every Run with a stable run id, a Control Depth, labels, budgets and a Run Status, whether or not Hekma started its process.
**Consequences (testable):**
- Starting a supervised Agent Instance creates a Run at depth `supervised`; one process may host many Runs. A child Run opened inside a supervised process (FR-43) is at most `announced`: its verbs act on its own calls, never on the shared process; where that process has no wire path, a child-level `pause`, `stop` or `block` and the matching Breach Actions fail fast or are refused, naming the Metering Source.
- A supervised Agent Instance whose model traffic does not pass through the Metering Proxy (Metering Source `self-reported`, e.g. Hermes, FR-28) shows as `supervised (no wire)`: its usage comes from its Adapter's reports, every wire-only cell below fails fast naming the Metering Source, and `block` set directly on it is refused at configuration (one inherited from a Policy follows FR-60's fallback). Turning on key custody (FR-53) routes its traffic through the Metering Proxy and makes it fully `supervised`. *(Ruled by Islam 2026-10-30.)*
- Each verb behaves per Control Depth as the table below states; a cell marked *fails fast* returns an error naming the depth, and a *qualified* cell succeeds with a visible qualifier (the FR-7 honesty rule). Every cell is covered by a conformance test.

  | Verb | `observed` | `wire` | `announced` | `supervised` |
  |---|---|---|---|---|
  | List, usage | from its telemetry, estimated | per-call records | per-call records | per-call records and process state |
  | `logs` | fails fast | per-call records | per-call records | process output and per-call records |
  | Budgets, `warn` | from its telemetry | honored | honored | honored |
  | `pause` / `resume` | fails fast | qualified: holds / releases the Run's calls | qualified: holds / releases the Run's calls | Adapter pause (FR-7); a wire hold only as the FR-7 amendment allows |
  | `stop` | fails fast | qualified: rejects further calls, the process keeps running | qualified: rejects further calls and ends the Lease | process stop (FR-6) |
  | `send` (FR-24), `attach` (FR-25, FR-57) | fails fast | fails fast | fails fast | per the Adapter's Capability Declaration; a child Run fails fast |
  | hold / release / reroute / reject | fails fast | honored | honored | honored |
  | Breach Action `pause` / `stop` / `block` | set directly: refused at configuration; from a Policy: FR-60 fallback | as the verbs above (`block` as reject), latched until the ceiling is raised or reset | as `wire` | as the verbs above, latched |

- A supervised Run's Run Status follows its Lifecycle State: `starting`, `running` and `stopping` show `active`; `paused` shows `held`; `stopped` shows `ended`; `failed` shows `ended` with an Anomaly unless the Restart Policy relaunches it. A relaunch continues the same Run, so per-run budgets do not reset; only an Operator or Host `start` opens a new Run.
- Ended Runs are kept for a configurable retention window, then compacted into rollups that keep usage totals and labels. `[ASSUMPTION: retention default.]`

- A Run's Control Depth can change during its life (e.g. discovered on the wire, then announced under the same identity, or key custody turned on); it keeps its run id, and each change is an event. Verbs and Breach Actions then follow the cell for the new depth: a configured Breach Action the new depth cannot execute raises an Anomaly and acts as `warn` until the Run regains a depth that supports it, and a latched Breach Action stays latched across the change.
- Labels are `key=value` pairs, each recording its source: bound (by the Operator or Host — through Agent Instance configuration, which supervised Runs inherit, a Run Key, a Metering Proxy endpoint, or an Operator-only label verb and Control API call) or Caller (set at announcement or in identity headers). A Caller cannot set a key that is already bound. Only bound labels select Policies (FR-60); any label groups, filters and dimensions telemetry, and `--selector` on listings and verbs matches both. `hekma ps --json` shows each label's source.
- A flow moved to a different runtime keeps its bound labels (the same Run Key, endpoint or instance configuration), and with them its Policies, dashboards and alerts.
- Runs are flat by default. A Run may name a parent Run on the same Node: its usage counts on itself and rolls up to every ancestor, every ancestor's ceilings apply, and a breach executes the Breach Action of the level whose ceiling broke over the Runs beneath it. A verb on a child never acts on its parent; a verb on a parent reaches its children only through an explicit Selector or flag — except that `stop` or `pause` on the Run that owns a supervised process stops or pauses the child Runs inside that process with it, each recording the cause (`parent-stopped`, `parent-paused`). A parent that ends does not end its children, except those inside its own supervised process; other children keep rolling up to it, and its ceilings keep binding them until its last child ends. Naming a parent that does not exist, is on another Node, or would form a cycle is refused with a typed error naming the cause. Nesting is capped at 8 levels. `[ASSUMPTION: same-Node parents only; 8-level cap.]`
- No Run is stopped, expired or deprioritized for running long; only a Policy or a verb acts on a Run.

#### FR-50: Listing and verbs at scale
The Operator can list Runs with `hekma ps` and act on many at once through Selectors, from the CLI and the Control API alike.
**Consequences (testable):**
- `hekma ps` shows each Run's name or id, CONTROL column (its Control Depth), Run Status, Anomalies, labels, usage and headroom, human-readable and `--json`.
- The default view is exception-first: Runs with an Anomaly or a `held`, `silent` or `stale` status are listed, healthy Runs are counted, and `ended` Runs are left out unless they carry an Anomaly (e.g. a supervised Run that `failed` without relaunch), which stay listed until the ended-Run retention window compacts them (FR-49); `--all` lists every Run.
- Every lifecycle, hold and budget verb accepts `--selector`. A verb that matches more than one Run first resolves the matched set: the CLI shows it and asks for confirmation (or `--yes`), and the Control API returns it with a token; acting with the token acts on exactly that set, and the result reports success or failure per Run. `--dry-run` resolves the set without acting.
- Listings page with `--limit` and a cursor; no read path loads the whole Run set.
- `hekma budget` reads, sets and raises Token Budgets and Cost Caps on the Runs a Selector matches now; standing rules for later Runs are Policies (FR-60). Raising a ceiling re-arms its latch. Only the Operator or Host can raise a ceiling; a Caller cannot.
- Every listing filter, Selector and page parameter has a Control API equivalent with the same semantics.

#### FR-51: Discovered Runs
A client Hekma did not start becomes a Run without code changes: at depth `wire` when its model traffic passes through the Metering Proxy, at depth `observed` when it only sends Hekma its telemetry.
**Consequences (testable):**
- Pointing the client's model base URL at the Metering Proxy is the only integration needed for a `wire` Run to be metered, listed and governed under Scent-Check Mode `off` or `shadow`; under `enforce` the client must also present a Run Key (FR-53) or announce (FR-52).
- The Operator can bind labels to a Metering Proxy endpoint path or a Run Key (FR-53), so base-URL-only clients carry labels with no code change; their traffic forms one Run per bound identity until a per-run identity (a per-call identity header or an announcement, FR-52) splits it. An endpoint-path binding identifies the Run for listing and Policies but is not a Scent-Check identity.
- Hekma accepts inbound OTLP from clients; telemetry carrying a run identity creates or updates an `observed` Run.
- A discovered Run appears after its first call or telemetry batch, under the identity it presented. Under Scent-Check `off` or `shadow`, traffic with no identity is listed as unattributed rather than dropped; under `enforce` it is refused and counted as an Anomaly.
- A Metering Proxy endpoint reachable beyond loopback requires authentication (Run Key, or a proxy-access credential for base-URL-only clients); unauthenticated non-loopback serving is refused at start.
- Identity headers (run id and labels, never content) are added to forwarded calls so a gateway in front of the provider (e.g. LiteLLM, Portkey) can attribute them; the Operator can turn them off.

#### FR-52: Self-announce and leases
A framework or client can announce a Run on start, so Hekma knows the Run before its first call and keeps it accurate in `hekma ps`.
**Consequences (testable):**
- Announcing works through any of three channels, each stating what it returns:
  - an announce header on the Run's first call through the Metering Proxy returns the Run Key and refresh credential in response headers;
  - one call to a Run-scoped announce endpoint of the Control API, authorized by an announce-scoped credential (Caller authority: it can create a Run and set its Caller labels, never raise a ceiling, change a Policy or act on another Run, and it is never the Operator's or Host's bearer), returns the Run Key and refresh credential;
  - an OTel resource attribute creates or updates the Run's identity and Lease but issues no Run Key.
- A Run that announces itself and routes its model traffic through the Metering Proxy is at depth `announced`; one that announces without doing so stays at depth `observed`, keeping its identity and Lease.
- A Run discovered on the wire or through telemetry and later announced under the same identity stays one Run (FR-49).
- A Run that announced itself renews its Lease by heartbeat or traffic; an expired Lease shows the Run as `silent`, and silence alone never stops it. A Run that announces its end, or stays `silent` past the silent window, is `ended`.
- CrewAI and LangGraph helpers are documented as supported only after they are validated against real flows in CI. `[ASSUMPTION: the two frameworks named are the first supported pair.]`

#### FR-53: Run identity and key custody
Hekma can issue a Run Key to each Run, hold the real provider keys itself, and check calls for a valid identity under a per-Policy Scent-Check Mode.
**Consequences (testable):**
- Announcing a Run through the Metering Proxy or the Control API (FR-52), or starting it as a supervised Agent Instance, issues a Run Key valid for the Run's life and revocable; only the announcing process (for a supervised Run, the process the Engine launched, including a relaunch under the Restart Policy) holds the refresh credential that rotates it, and a refresh from any other process fails.
- The Run Key is the provider API key the agent holds: Hekma places it in the agent's API-key slot, and the Metering Proxy identifies the Run from it and swaps in the real provider key. Key custody is the default for a supervised Run whose provider key is configured through Hekma; the real provider key never appears in the agent's environment, arguments or Agent Home. An agent that brings its own provider key, which Hekma does not hold, cannot be attributed on the wire: a `self-reported` adapter shows as `supervised (no wire)` (FR-49), and an `engine-observed` adapter has no viable Metering Source and is refused at start with a remediation (FR-19). Run Keys never travel in a URL.
- Scent-Check Mode is set per Policy: `off` checks nothing, `shadow` serves a call with no valid identity and records it as an unscented Anomaly, `enforce` refuses it with a typed response. The mode never changes how Breach Actions behave.
- A Caller cannot lower the Scent-Check Mode, raise a ceiling, or loosen any other limit a Policy sets; it may only tighten them, and its labels never select a Policy (FR-60).
- Run Keys and refresh credentials are secrets under FR-14.
- The docs state the threat model: key custody keeps the provider key away from the agent process and its files; it does not contain a compromised agent, and a process that reaches a provider with other credentials is caught only by FR-54, best-effort.

**Gates:** default Scent-Check Mode per environment is decided in the story spec `[ASSUMPTION: off for a laptop CLI, shadow under the Service Host, enforce opt-in — §13 Q9]`. `[NOTE FOR PM: key custody is a security-relevant promise — it needs a security review before it is documented as supported.]`

#### FR-54: Bypass detection
Hekma flags a supervised Run that reaches a known model provider directly instead of through the Metering Proxy.
**Consequences (testable):**
- A detected bypass shows as an Anomaly in `hekma ps` naming the Run and the provider host, and as an OTLP counter by provider host and allow-listed labels (the Run via exemplar, FR-59).
- For a `supervised (no wire)` Run, direct provider calls are expected: `hekma ps` carries an informational note naming the provider host (not an Anomaly) until key custody is turned on for it.
- Detection signals; on its own it never stops or pauses the Run.
- Where an OS gives no reliable signal, the CLI and docs say detection is unavailable on that OS; it never reports a Run as clean without having checked.

#### FR-55: Process continuity across Service Host restarts
In Handoff Mode, Runs survive the Service Host crashing, restarting or upgrading.
**Consequences (testable):**
- In Handoff Mode, a supervised Run's process survives the Service Host's exit (a Shim per supervised process is the expected mechanism; architecture decides). With Handoff Mode off, SIGTERM drains and stops (FR-41), and after a crash the restarted Engine cleans orphaned processes up (NFR-1), ending their Runs with the cause recorded.
- In Handoff Mode, SIGTERM leaves Runs running; a restarted Service Host reattaches to every surviving process and shows the same Runs, Leases and holds it had before, with spend so far within FR-10's flush bound.
- During a Handoff Mode restart, holds stay in place because a hold belongs to its Run, not to a connection: a held or in-flight call's connection may end with one retryable error that is recorded, and the retried call is held again (or forwarded if the hold was released). Each Run sees at most one such error per restart. The Metering Proxy refuses no connection during an in-place upgrade or under a socket-activated service manager (systemd, launchd); under other service managers, connections are refused until the new Service Host binds, and clients see that as retryable. *(Amended 2026-10-30, ruled by Islam.)* In every case `wire` Runs keep their identity, and announced Runs keep their identity and Lease.
- After a `kill -9` of the Service Host, the record of every hold survives; every held or in-flight call through the Metering Proxy ends with a retryable error to its client, recorded on its Run as ended by the crash once the Service Host restarts.
- The docs state the service-manager settings Handoff Mode needs (e.g. not killing the whole process group), and the Service Host warns at start when it detects a setting that would take Runs down with it; where it cannot tell, it says Handoff Mode is unverified there and how to check, and it never reports a handoff as successful for a Run whose process did not survive.
- Run records, budgets, Leases and holds survive a crash of the Service Host within FR-10's flush bound, at every Control Depth, whatever the mode.

**Gate:** Handoff Mode is opt-in until this FR ships and the default after it; drain-and-stop (FR-41) stays available by configuration. *(Ratified by Islam 2026-10-30.)*

#### FR-56: Wire hold, release and reroute
The Operator can hold a Run's calls at the Metering Proxy, pausing the Run with its in-process state intact, and later release, reroute or reject them.
**Consequences (testable):**
- While a Run is held, every new call it makes waits at the Metering Proxy, up to a per-Run held-call cap beyond which new calls get a typed retryable hold-full response and an Anomaly; calls already forwarded complete, and the hold is qualified accordingly. `hekma ps` shows the Run as `held` with the held-call count and the oldest held call's age.
- Release forwards every held call unchanged, in arrival order; reroute forwards them to another configured model; reject returns the typed non-retryable response of the `block` Breach Action (FR-43).
- When a held call ends without a release — the client's own timeout, the connection dropping or the process exiting — Hekma records which and shows it on the Run; it never reports the Run as cleanly paused.
- Hold, release, reroute and reject accept Selectors (FR-50) and appear in per-call records.

#### FR-57: `hekma top`
The Operator can watch Runs live in the terminal and act on them.
**Consequences (testable):**
- The view shows each Run's Run Status, Control Depth, tokens per second, burn rate and budget left, refreshing within 2 seconds. `[ASSUMPTION: refresh bound placeholder.]`
- Hotkeys pause, stop, hold, raise a limit and attach (open a supervised Run's live output), each confirming exactly as the matching CLI verb does.
- At scale the view is exception-first and filterable by Selector (FR-50); it reads through the Control API, so it can point at a remote Service Host or Hub.

#### FR-58: Nodes and hub
An Operator running Hekma on several machines can see and control every Run from one place.
**Consequences (testable):**
- Every machine runs a Service Host as a Node; a Hub merges Node summaries into one listing and routes each verb to the Node that owns the Run.
- Nodes roll up locally and the Hub never needs per-call records to list Runs.
- Each Node authenticates to the Hub separately. An unreachable Node's Runs show as `stale` and keep running; each Node keeps enforcing its own Policies while partitioned; a verb routed to an unreachable Node fails with an error naming that Node and is never reported as applied, and a Selector verb spanning Nodes reports per Node. Each partitioned Node also keeps any holds in place, and the Hub lists a stale Node's Runs with their last-known Run Status, Anomaly flags, and the age of that Node's last summary.

**Gates:** topology (single Hub or peer federation) is decided when the epic is detailed `[ASSUMPTION: Hub first]`; ships only with a dated §6 privacy amendment covering data that crosses machines.

#### FR-59: Telemetry at Run scale
Hekma's exported telemetry stays usable at millions of Runs.
**Consequences (testable):**
- Exported metrics carry Control Depth, the name of the Policy that supplied the limit a metric reports on (the default Policy's name when none did; metrics not tied to one limit carry no Policy attribute), and only the label keys on an Operator allow-list (empty by default) as attributes, never a run id. An allow-listed key whose distinct values exceed a configured cap folds further values into one overflow value and raises a diagnostic. Per-Run detail rides exemplars and traces. `[ASSUMPTION: cap placeholder.]`
- The change ships as the next version of FR-46's metrics exposition, under the §7 deprecation path.

**Gate:** trace and span export ships only with a dated §6 privacy amendment.

#### FR-60: Policies
The Operator or Host can set standing limits for every Run a Selector matches, now and later.
**Consequences (testable):**
- Policies are created, listed, changed and deleted through the CLI and the Control API; every change is an event.
- A limit set explicitly on an Agent Instance (any FR-11 layer, including the FR-21 upgrade write) or on a Run through `hekma budget` outranks every Policy; matching Policies, then the default Policy, fill only what is unset. A limit a Caller sets applies only while it is tighter than the effective Operator, Host or Policy limit. An upgraded instance kept on `pause` still pauses on breach when a default Policy says `warn`.
- A Policy applies to current and future Runs its Selector matches. When several match one Run, each limit is resolved on its own: the most specific Selector wins, where Selector A is more specific than B when A's terms are a strict superset of B's; Selectors that cannot be compared tie, and a tie resolves to the tightest value (Breach Actions `warn` < `pause` < `stop` < `block`; Scent-Check Modes `off` < `shadow` < `enforce`). *(Most-specific rule ruled by Islam 2026-10-30.)* `[ASSUMPTION: specificity definition, per-limit resolution and tie order.]`
- The default Policy sets no ceiling, Breach Action `warn`, and the environment's default Scent-Check Mode (§13 Q9; seeded `off` until Q9 is decided); the Operator can change it.
- A Policy change takes effect for every matched current Run at once: a ceiling lowered below a Run's usage breaches immediately, and a raised ceiling re-arms the Run's breach latch (§13 Q11 interim).
- When a matched Policy's Breach Action is not available at a Run's current Control Depth, that Run executes `warn` for that ceiling and carries an Anomaly naming the depth and the Policy; the action takes effect if the Run's depth later allows it. Creating or changing a Policy reports how many matched Runs fall back this way.
- Policy Selectors match only labels the Operator or Host bound (through configuration, a Run Key, a Metering Proxy endpoint, or the Operator-only label verb and its Control API call, FR-49), never labels a Caller set, so a Caller cannot choose its way into a looser Policy.
- Versioned policy-as-data, canary rollout and audit are later work (theme D).

## 5. Cross-Cutting NFRs

- **NFR-1 Resilience.** An Agent crashing never crashes the Engine or `kt`; after its own crash, on next start the Engine adopts orphaned Agent processes, reattaching to them when Handoff Mode is on (FR-55), and cleans them up only when Handoff Mode is off (NFR-9). Partial failures across the Fleet degrade gracefully with per-instance reasons and remediations (durable gate).
- **NFR-2 Cross-platform parity.** Every FR behaves equivalently on Linux, macOS, and Windows. Where an OS lacks a primitive (e.g. POSIX signal semantics on Windows), the Engine provides the closest equivalent and the difference is documented, not silent. Process-control strategy per OS is an architecture deliverable (addendum).
- **NFR-3 Test coverage ≥ 95%** on `src/`, enforced in CI (`cargo tarpaulin --fail-under 95`) — carried gate, non-negotiable.
- **NFR-4 Performance budgets.** Read commands (Fleet listing, status, usage) complete in <1s on a 25-instance Fleet; Engine supervision overhead per running Agent Instance ≤ 2% CPU steady-state and ≤ 50MB RSS. `[ASSUMPTION: all three numbers are placeholders for architecture to validate.]` *(Amended 2026-10-30.)* Run-scale budgets replace the 25-instance placeholder after the Epic 24 benchmark: read paths are paginated and indexed, and targets are measured at 10k, 100k and 1M Runs, against a design target of 10M Runs per Hub. Until then the placeholders are: the first page of `hekma ps` (and its Control API equivalent) in <1s at 1M Runs, and the Metering Proxy adding ≤10ms p99 to a model call. `[ASSUMPTION: 10M design target and both placeholder numbers.]`
- **NFR-9 No unrequested stops.** *(Added 2026-10-30.)* Hekma stops, pauses or blocks a Run only on an explicit verb; a non-`warn` Breach Action or Scent-Check `enforce` configured at any layer (including the FR-21 upgrade write and FR-60 Policies); a failed start, or an Adapter-declared Readiness or Liveness Probe failure, handled under the Restart Policy (FR-5, FR-42); a Service Host shutdown while Handoff Mode is off (FR-41); or, with Handoff Mode off, the Engine's cleanup of orphaned processes after its own crash (NFR-1). A Run's duration and Anomalies never stop a Run.
- **NFR-5 Observability.** Engine and per-Agent-Instance logs are structured, timestamped, attributed, and rotation-bounded; stdout is for command output, stderr for diagnostics (carried constitution rule).
- **NFR-6 Security & privacy.** Secrets per FR-14; Agent Home isolation per FR-2 is process/filesystem-level, **not** a security sandbox — the boundary is stated in docs. `[NOTE FOR PM: if sandboxing ever becomes a promise, it is a v2+ initiative with real security review; v1 must not imply it.]`
- **NFR-7 Documentation currency.** Docs and README update in the same change as behavior (carried gate); the Adapter Contract and Embedding Interface docs version with the code.
- **NFR-8 Runtime & dependency policy.** Rust 2021+ single binary for `kt`; Engine as a Rust library crate; dependency additions follow the existing lean policy (clap/miette/indicatif/serde baseline). `[ASSUMPTION: no new heavyweight runtime deps without architecture sign-off.]`

## 6. Constraints & Guardrails

- **Cost.** The product's own promise applies to itself: governance features must not depend on paid third-party services; cost enforcement runs entirely locally. Dollar figures are estimates unless reconciled (FR-23) — the Engine must never present estimate as actual.
- **Safety.** Breach Actions act on lifecycle (pause/stop) or the wire (block); the default `warn` acts on nothing and signals loudly (FR-21) — the Engine never silently drops an Agent's in-flight work without recording that it did (FR-6/FR-21 events). Crash-loop bounds (FR-9) prevent restart storms.
- **Privacy.** Usage Ledgers, config, memory, and logs stay local in the Agent Home / Engine state dir; no telemetry leaves the machine. `[ASSUMPTION: zero remote telemetry in v1 — flag any exception to Islam explicitly.]`

  > *(Amended 2026-10-23.)* The default stands: nothing leaves the machine unless the Operator or
  > Host configures it to. One sanctioned exception — FR-46's OTLP metrics export: disabled unless
  > a collector endpoint is explicitly configured; metrics only (counters and gauges derived from
  > the Usage Ledger and supervision state); never request or response content, never secrets
  > (FR-14). Ratified with `sprint-change-proposal-2026-10-23-adoptability.md`. §14 #18 resolves
  > as: *zero remote telemetry by default; one opt-in, metrics-only, operator-endpointed
  > exception.*

  > *(Noted 2026-10-30.)* Two planned capabilities will each need their own dated amendment here
  > before they ship: trace and span export (FR-59) and Hub federation, where
  > data crosses machines (FR-58). Nothing changes until then. FR-51's identity headers send run
  > ids and labels (never content) to the provider or gateway a call already goes to, and the
  > Operator can turn them off.

## 7. Public Surface, Versioning & Deprecation

Six public contracts, versioned independently *(4–6 added 2026-10-30)*:

1. **The Adapter Contract** — semver; Engine declares accepted range (FR-30). Breaking changes require a major bump and a migration note for Adapter authors.
2. **The Embedding Interface** — the Engine crate's public API, semver per Rust conventions; `kt`-only needs never justify breaking Hosts (FR-32 keeps them on the same surface, which is the point).
3. **The `kt` CLI surface** — command names, flags, exit codes, and `--json` schemas are a compatibility surface once v1 ships; breaking changes follow the same deprecation path as FR-38 (announce → notice → remove).

4. **The Control API** (FR-41) — `0.x` and unstable until a second Host validates it, then `1.0` under this policy; until then breaking changes ship only at a minor version with release notes.
5. **The OTLP metrics exposition** (FR-46, FR-59) — metric names, types and attributes, semver'd.
6. **The Metering Proxy wire contract** (FR-51..FR-56) — header names, response headers that return credentials, the breach header, typed reject and retryable error bodies, and the stripped-header set; versioned and additive-only until frozen. *(Added 2026-10-30.)*

Deprecation policy (all surfaces): announced in release notes, minimum one minor-version notice window, removal only at a major. `[ASSUMPTION: policy mechanics pending Islam.]`

## 8. Non-Goals (Explicit)

Re-confirmed 2026-10-30 with the control-plane proposition (closes §13 Q5):

- **Not an agent framework or authoring toolkit.** Hekma runs and governs Agents; it does not help build agent logic.
- **Not an LLM/model server.** Model hosting is out; Hekma governs Agents, not models.
- **Not a hosted SaaS or web UI.** Hekma is a self-hosted control plane, OTel-first and UI-optional; dashboards live in the operator's observability stack, and Hosts may build their own atop the Engine.
- **Not a model gateway.** Hekma's Metering Proxy meters, identifies, holds and reroutes a Run's calls; multi-provider routing, caching, fallback chains and org-wide key vaults stay with gateways (e.g. LiteLLM, Portkey), which Hekma can sit in front of (FR-51).
- **Org governance (RBAC, audit evidence, four-eyes policy changes) is later, not v-next** (themes D/F).
- **Customer-safety controls at the wire** (run binding to a customer, tool-argument contracts, a held customer outbox, contamination checks) **are later, not v-next** (theme E).
- **No provider-side billing enforcement.** Caps act on Runs from rate-derived estimates; Hekma does not replace provider billing limits.
- **No deep per-agent feature parity.** The unified surface is the product; Agent-native extras stay native (reachable via config pass-through, FR-12).
- **No general-purpose skills package management** (deprecated per §4.10).
- **No security sandboxing claim** (NFR-6). Key custody (FR-53) and bypass detection (FR-54) are narrower promises with a stated threat model; neither makes Hekma a sandbox.

## 9. MVP Scope

### 9.1 In Scope (v1)

- Features §4.1–§4.9 as specified, with the Hermes reference Adapter (FR-28) and the second-agent contract validation (FR-29, paper).
- Memory Backings: `filesystem` + `native` only (FR-16).
- Token Budgets + Rate-derived Cost Caps with configurable Breach Action (FR-18–FR-23).
- Engine library + `kt` on the public Embedding Interface (FR-31–FR-34).
- Agent-scoped Skills provisioning + the legacy surface's clean removal at v0.6.0 (FR-35–FR-39).
- NFR-1..NFR-8 including 3-OS parity and the ≥95% coverage gate (NFR-4's Run-scale amendment and NFR-9 belong to the next horizon, §9.3).

### 9.2 Out of Scope for MVP

- **Second Adapter as shipped code** — v1 ships one implemented Adapter (Hermes) plus the contract-level validation of a second (opencode). `[FIXED — ruled by Islam 2026-07-02: option (a); SM-1 is fully measurable at v1.x when the opencode Adapter ships, interim measurement via the conformance kit's mock Adapter.]`
- Adapter registry / remote Adapter distribution — deferred; local + bundled Adapters only.
- Richer Memory Backings (vector stores, mem0/Letta-style tiers) — deferred to v1.x+ behind the same interface (FR-15).
- Per-window budgets (daily/monthly) — deferred (FR-18 note).
- Service/IPC delivery of the Engine (embed = Rust library only in v1) — §13 Q6.
- Agent Home export/import as a polished feature — `[NOTE FOR PM: FR-17 assumes basic export; demote to v1.x if it drags.]`
- skills.sh public search of the legacy manager — dropped in the refocus. `[ASSUMPTION — §13 Q7.]`
- Hosted SaaS or web UI and a sandboxing claim — non-goals; org governance — later (§8).

### 9.3 v1.x additions

FR-40 (shipped in Epic 14) and FR-41..FR-45 (Epics 17–19, added 2026-09-30; renumbered from 15–17 on 2026-10-19 — main's autonomous Fibery-backlog tranches had consumed epic numbers 15/16 while the correct-course output sat uncommitted) are v1.x additions, not the v1 MVP. FR-46..FR-48 (Epic 20, added 2026-10-23 via correct-course — the Adoptability proposal) are likewise v1.x additions, not the v1 MVP. FR-49..FR-60, NFR-9 and NFR-4's Run-scale amendment (Epics 21–25, added 2026-10-30 via correct-course — the control-plane proposition) are the next horizon after v1.x.

## 10. Why Now

The 2025–2026 agent landscape fragmented into single-stack frameworks, single-agent harnesses, parallel launchers, in-code governance SDKs, and proxy-level budget tools — while personal agents (Hermes Agent and peers) became genuinely adoptable by individuals. The operator-facing runner that unifies lifecycle *and* dollar governance across heterogeneous agents is the identified gap (brief addendum §A/§B, research-grounded hypothesis). Platforms hosting personal agents are emerging now and are rebuilding runtime plumbing each — the embeddable-engine window is open before someone else standardizes it.

*(Added 2026-10-30.)* Businesses now run agent workloads in production on frameworks (CrewAI, LangGraph), and their fear is a cost guard stranding prod work, not a missing one. Gateways show usage per key but cannot steer one specific run; observability tools see runs but cannot act on them. A self-hosted control plane that runs, sees and steers any agent workload the same way, loudly but without stopping prod, is the gap the control-plane proposition targets.

## 11. Success Metrics

**Primary**
- **SM-1 (north-star, ratified): Cross-agent operability** — 100% of core controls (lifecycle, config, memory, token limit, cost cap, interaction) behave identically across ≥2 structurally different Agents. Validates FR-5–FR-30. *Measurement basis ruled by Islam 2026-07-02: at v1, Hermes + the conformance kit's mock Adapter; fully measured at v1.x when the opencode Adapter ships.* `[FIXED]`
- **SM-2: Cost-cap efficacy** — 100% of runaway scenarios in the enforcement test suite trigger the configured Breach Action (a loud signal under `warn`, the action otherwise) within the latency bound. Validates FR-18–FR-23.

**Secondary**
- **SM-3: Time-to-operate** — a competent Operator goes from install to a governed, running Hermes Agent (UJ-1) in ≤15 minutes using only Ktesio docs. Validates FR-1–FR-5, FR-20–FR-21, FR-28. `[ASSUMPTION: placeholder target.]`
- **SM-4: Embeddability proof** — binary: `kt` consumes only the public Embedding Interface, enforced in CI. Validates FR-31–FR-32.
- **SM-5: Adapter effort** — a developer produces a working Adapter for a new Agent within ≤1 person-day against contract docs + conformance kit. Validates FR-27, FR-30. `[ASSUMPTION: placeholder target.]`

**Control-plane horizon** *(added 2026-10-30; provisional until §13 Q10 is decided)*
- **SM-6: Time to attach** — an engineer turns an existing framework flow into a governed `wire` Run, with budgets and OTLP metrics, in ≤15 minutes using only Hekma docs. Validates FR-49–FR-52. `[ASSUMPTION: placeholder target.]`
- **SM-7: No Hekma-caused stops** — in the Service Host restart, upgrade and crash test suite, 0 Runs at any Control Depth are stopped, paused or blocked by anything NFR-9 does not list, 0 Metering Proxy connections are refused during an in-place upgrade or a socket-activated restart, and every call a restart or crash fails is a recorded retryable error (measured client-side for `wire` and `announced` Runs). Validates FR-55, NFR-9.
- **SM-8: Run-scale reads** — `hekma ps` and its Control API equivalent return their first page in <1s at 1M Runs (NFR-4). Validates FR-50, NFR-4.

**Counter-metrics (do not optimize)**
- **SM-C1: Adapter catalog breadth.** More Adapters is *not* v1 success; uniformity of controls is. Counterbalances SM-1/SM-5 — resist shipping shallow Adapters to inflate the count.
- **SM-C2: Per-agent feature depth.** Surfacing every native capability of every Agent is explicitly not the goal (§8); counterbalances SM-3 pressure to "just expose the native thing."
- **SM-C4: Control Depth vanity.** The share of Runs at `supervised` is not a goal; low-friction attach at `wire` is worth more than forcing processes under Hekma. Counterbalances SM-6.
- **SM-C3: Estimate precision theater.** Do not chase cost-estimate precision into presenting estimates as actuals or blocking UX on reconciliation; honesty labels (FR-23) outrank precision. Counterbalances SM-2.

## 12. Feature-Sequencing Note for Epics

Suggested epic seams (not a plan, a shape): Engine core + state machine + persistence (§4.1–4.2) → config + secrets (§4.3) → metering/budgets/caps (§4.5) → interaction (§4.6) → Adapter Contract + Hermes (§4.7) → Embedding Interface hardening (§4.8, `kt` is on it from day one via FR-32) → memory (§4.4) → skills provisioning + migration (§4.9–4.10). `[ASSUMPTION: sequencing is architecture/PM's to overrule.]`

*(Added 2026-10-30.)* Control-plane order: Runs as entities (FR-49) → listing and Selectors (FR-50) → Run Keys (FR-53) → per-call records (FR-43) → the default Policy (FR-60) → key custody and Scent-Check Modes (FR-53) → discovery, self-announce and Leases (FR-51, FR-52) → budgets over Selectors, full Policies and `hekma top` (FR-50, FR-60, FR-57) → wire hold (FR-56) → bypass detection and the authenticated endpoint (FR-54, FR-51) → process continuity, scale and telemetry at scale (FR-55, NFR-4, FR-59) → Nodes and Hub (FR-58).

## 13. Open Questions

1. ~~Second Agent~~ **RESOLVED 2026-07-02:** opencode (opencode.ai), Islam's selection. Structural characterization vs Hermes happens in the conformance mapping (FR-29).
2. **Licensing/positioning (blocks Host go-to-market, not the build):** PolyForm Noncommercial today; embedding makes the commercial question concrete. Owner: Islam.
3. ~~Breach Action default~~ **RESOLVED 2026-07-02:** `pause` ratified as the shipped default (per-instance configurable stands). **Superseded 2026-10-30:** `warn` with loud signals is the default; `pause`, `stop` and `block` are opt-in. Agent Instances registered before the change that relied on the implicit default keep `pause`, written into their config; explicitly set Breach Actions are untouched (FR-21).
4. **v1 Memory Backing set:** `filesystem` + `native` proposed (FR-16); confirm or extend. Owner: Islam.
5. ~~Non-goals re-confirmation~~ **RESOLVED 2026-10-30:** §8 rewritten with the control-plane proposition.
6. **Engine delivery surface beyond Rust lib** (service/IPC for non-Rust Hosts): v1 = lib only `[ASSUMPTION]`; decide the v1.x path. Owner: Islam + architecture. **Answered 2026-09-30 (ratified with sprint-change-proposal-2026-09-30-hosted-platform-readiness.md):** a Service Host with a local HTTP/JSON Control API (FR-41, AD-20),
  unstable `0.x` until a second Host validates it. The library remains the primary embedding surface.
7. **Deprecation window mechanics & skills.sh search fate** (§4.10, §9.2). Owner: Islam.
8. **Metering mandatory at registration** (FR-19 rejects Adapters with no Metering Source): confirm this hard line. Owner: Islam. **Answered 2026-09-30 (ratified with the same proposal):** metering stays mandatory at registration for manifest adapters
  (FR-19). The `acp` kind's surfaced gap (FR-40) is the one sanctioned exception. A Host that meters
  at its own choke point may feed Hekma through reconciliation (FR-45), which satisfies FR-19's
  intent without faking a source.
9. **Scent-Check Mode defaults per environment** (FR-53): proposed off for a laptop CLI, shadow under the Service Host, enforce opt-in. Decide in FR-53's story spec. Owner: Islam. **Interim 2026-10-30 (ruled by Islam):** the default Policy is seeded `off` until FR-53's story decides; that story never overwrites a default Policy the Operator has edited. Because every CLI write routes through a local Service Host (FR-41), "environment" must be redefined when Q9 is decided.
10. **Success metrics for the control-plane horizon:** SM-6..SM-8 and SM-C4 are provisional; confirm or replace their targets before Epic 21 closes. Owner: Islam.
11. **Policy versioning for long Runs:** pin the Policy at Run start by default, or follow the latest — decide with the later policy theme (D). Owner: Islam. **Interim 2026-10-30 (ruled by Islam):** follow the latest. Policies have no versions; a change re-resolves every matched current Run at once — lowering a ceiling below a Run's usage breaches immediately, and raising one re-arms the Run's breach latch.
12. **Control API stability exit:** it stays `0.x` until a second Host validates it (FR-41), an event Hekma does not control, while §1 makes it the daily surface for teams. Decide whether a time- or usage-based exit also counts. Owner: Islam.

## 14. Assumptions Index

Every inline `[ASSUMPTION]`, indexed:

1. §2.1 — Agent-author audience: JTBD acknowledged, no v1 author-facing features beyond the published contract.
2. §3 Engine — library-first delivery; service/IPC out of v1 (Q6).
3. §3 Lifecycle State — exact state set (`registered`…`failed`) subject to architecture refinement.
4. §3 Restart Policy — `never`/`on-failure`/`always` with default `never`.
5. §3 Token Budget / FR-18 — scopes are per-run + cumulative; per-window deferred.
6. §3 Rate / FR-20 — split input/output rates; no retroactive repricing.
7. ~~§3 Breach Action / FR-21 — `pause` default~~ RESOLVED 2026-07-02: ratified; superseded 2026-10-30 by the `warn` default.
8. §4.1 — no remote Adapter registry in v1; FR-1 multi-instance in scope; FR-2 concurrent same-kind instances; FR-3 `--force` semantics.
9. §4.2 — FR-4 freshness bound (2s); FR-6 graceful window default (30s); FR-9 backoff defaults + crash-loop bound (5); FR-10 usage flush bound (≤5s).
10. §4.3 — FR-11 precedence chain; FR-14 `--reveal` flag; secret storage mechanism deferred to architecture.
11. §4.4 — v1 backing set (Q4); FR-15 no hot-swap; FR-17 Agent Home export in v1 (may demote).
12. §4.5 — FR-19 metering mandatory (Q8; amended 2026-10-30 for Runs below `supervised`); FR-21 enforcement-latency placeholder.
13. §4.6 — FR-25 retention default (10MB); FR-26 JSON schema compatibility testing.
14. §4.7 — FR-27 conformance kit in v1; FR-28 integration-test isolation strategy deferred.
15. §4.9 — FR-35 native-skills mapping is the Adapter's job; ~~FR-37 notice mechanics~~ RESOLVED 2026-07-14: moot — the legacy surface was removed outright at v0.6.0, so there are no notice mechanics to specify.
16. §4.10 — ~~FR-38 window (≥90 days / one minor cycle)~~ RESOLVED 2026-07-14: no notice window — announce and removal both land at v0.6.0; skills.sh search dropped (Q7).
17. §5 — NFR-4 all performance numbers placeholders; NFR-8 dependency policy.
18. §6 — ~~zero remote telemetry in v1~~ RESOLVED 2026-10-23: zero by default; FR-46's opt-in, metrics-only, operator-endpointed OTLP export is the one sanctioned exception (the §6 amendment, ratified with `sprint-change-proposal-2026-10-23-adoptability.md`).
19. §7 — deprecation policy mechanics.
20. ~~§9.2/§11 — SM-1 interim measurement basis~~ RESOLVED 2026-07-02: option (a) ruled; opencode named as Adapter #2 / second Agent.
21. §12 — epic sequencing is advisory.
22. §4.14 — FR-49 same-Node parents, 8-level nesting cap, ended-Run retention default; FR-52 first supported frameworks (CrewAI, LangGraph); FR-53 Scent-Check Mode defaults (Q9); FR-57 refresh bound (2s); FR-58 Hub-first topology; FR-59 empty default metric-label allow-list and cardinality cap; FR-60 specificity definition, per-limit resolution and tie order.
23. §3 — Run Status set and silent window; Run Key lifetime and refresh window; Lease length and `wire` inactivity window.
24. §2.3 — UJ-4 and UJ-5 drafted from Islam's brainstorm statements, not narrated sessions.
25. §5/§11 — NFR-4 10M-Runs design target, 1M-Run first-page and Metering Proxy overhead placeholders; SM-6 attach target (15 min).
