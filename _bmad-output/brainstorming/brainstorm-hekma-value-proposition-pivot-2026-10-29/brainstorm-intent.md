---
title: Hekma value proposition pivot — brainstorm intent
date: 2026-10-29
source: .memlog.md
status: final
---

# Hekma value proposition pivot — brainstorm intent

## Accepted value proposition

> Hekma is the control plane for agent workloads — one way to run, see and steer any agent (CrewAI, LangGraph, Hermes, OpenCode or a raw model call), from a laptop to a fleet, without ever being the reason prod stops

## Problem it replaces

- Hekma as a cost gatekeeper (budget block/pause) is not enough reason to adopt it.
- Businesses build on LangGraph/CrewAI and fear prod workflows pausing on cost: a crash restarts, a pause strands work.
- Individual users (OpenCode, Hermes) must stay first-class; they already love per-session usage on the command line.
- Gaps found: a blocked agent (notably Hermes) bypasses Hekma by calling the provider directly; gateways like LiteLLM show usage but cannot control one specific session.
- Hekma is OSS: the bar is "worth adopting", not "worth paying for".

## Design principles (set by the user)

1. **Signal, don't kill — make it loud.** Hekma warns (ps anomaly, warn telemetry, response header); the agent or framework corrects itself; ops alert on the telemetry.
2. **Run duration never changes how a run behaves**, unless it breaches a Hekma policy.
3. **Callers cannot override non-negotiables.** Hekma-level standard policies bind; callers may only act where Hekma config allows.
4. **One consistent interface regardless of agent tech.** Same management, observation, control and telemetry for any runtime; no new tooling when the agent tech changes. Installs anywhere (local, cloud function, VM) or connects to Hekma endpoints.
5. **Scale to millions of runs** (10k to 10M must be smooth).
6. **CLI for emergencies; API + OTel for daily work.** OTel-first, UI-optional: Hekma is the control plane and telemetry source, not another dashboard.
7. **Scent check per policy: off / shadow / enforce** (dev lenient, prod and regulated strict).

## Next horizon (chosen)

### A. One control plane
- Same verbs everywhere: `hekma ps / logs / top / pause / resume / stop / budget` behave identically for CrewAI, LangGraph, Hermes, OpenCode or a raw HTTP model call.
- Swap-the-engine portability: moving a flow between frameworks keeps its dashboards, policies, alerts and runbooks.
- `hekma top`: live TUI of sessions, state, tokens/s, burn rate, budget left, with hotkeys (pause, kill, attach, raise limit).
- Complement gateways, don't compete: tag every call with session/run identity; can sit in front of LiteLLM or Portkey.

### B. Control depth and attach
- Explicit control-depth ladder: observed (telemetry only) < wire (hold/reroute/reject calls) < registered (self-announced, heartbeats) < supervised (`hekma run` owns the process). `hekma ps` shows a **CONTROL** column per run.
- Wire hold/release/reroute: Hekma holds a run's next model call (zero code via base-URL override), suspending it mid-step with state intact; later release it, reroute to a cheaper model, or reject.
- Self-announce: a framework registers a run on start (header, OTel span or one SDK call), turning wire-discovered runs into registered ones so ps is accurate.
- Leases: attached runs hold a lease renewed by heartbeat or traffic; an expired lease shows as *silent*, not gone.

### C. Run identity and key custody ("colony scent")
- Registering (or being owned by `hekma run`) grants a unique run key that rides every request; it expires and only the original process holds the refresh key.
- Key custody with virtual provider keys: the agent never holds the real provider key; the run key is the per-run virtual key that only works through Hekma.
- Bypass detection: flag direct connections from a supervised session to known provider hosts, on the CLI and in OTLP.
- Scent check off / shadow (log unscented calls) / enforce (reject), chosen per policy.

### G-core. Operability and scale
- Shim per run (containerd-shim / tmux pattern): a tiny shim owns each process and its pipes; the daemon can crash, restart or upgrade and reattach.
- Durable run registry: runs, budgets, leases and holds journaled to disk; a restart recovers ps and spend-so-far exactly.
- Zero-downtime upgrades of the daemon under months-long runs.
- Tailnet nodes + hub: every box runs `hekma serve`; one hub (or peer federation) gives one view and control of the whole estate; nodes roll up locally, hubs merge summaries.
- Selectors on every verb (`hekma pause --selector flow=claims,region=eu`); CLI pagination, limits, filtering.
- Exception-first views: show only anomalous, held, over-budget or silent runs; healthy runs are counts.
- OTel cardinality discipline: run_id on traces/exemplars only; flow/policy labels on metrics.

## Later (planned, not next)

- **D. Policy that never breaks prod.** Policy as live, versioned, audited data (API or watched file, no redeploy); two-layer policy (callers declare per-flow policy in request headers, operators bound; callers only tighten unless headroom is delegated); budget headers on every response; typed "ask, don't work around" exhaustion response; open policy-header spec; shadow mode and canary rollout; windowed budgets; loud-by-default.
- **E. Customer safety at the wire.** Run binding (customer_id, application_id) checked on every tool call and outbound; tool-argument contracts; intake preflight against a declared input schema; customer-facing outbox (hold, sample, approve, recall window); subject-scoped runs and contamination canaries; graduated sealing (loud -> seal outbox -> seal subject -> full hold); quarantine and resume; self-heal contract header.
- **F. Proof and compliance.** Tamper-evident hash-chained run ledger; approval gates on high-impact tools with named approvers; egress/residency allow-lists and optional PII redaction; evidence pack per flow; four-eyes policy changes. Supports deployers' EU AI Act logging and human-oversight duties (not legal advice; dates in flux).

## Cross-cutting insights

- **The run key is the keystone.** One credential ends the bypass, makes ps accurate in attach mode, carries the run binding against contamination, and carries the flow policy. Building C now pre-pays D and E.
- **The wire hold resolves zero-integration vs checkpoint.** Outside the process Hekma cannot checkpoint framework state; holding the next model call pauses the run with state intact and no SDK. With key custody, wire control is also bypass-proof.
- **Loud-by-default plus graduated sealing** reconciles "no prod shutdowns" with auditor fears (wrong approvals, wrong customer messages, cross-customer contamination).
- **Solo multiplexer and fleet are one product.** `hekma top` at scale is selectors plus exception-first views; the tailnet hub bridges laptop and fleet.
- **Shim per run plus durable registry** answers three voices at once: Hekma-crash risk to owned processes, months-long runs, and "no unexpected shutdowns".

## Audiences

One core serves both: the solo dev running many OpenCode/Hermes/ACP sessions side by side (multiplexer, per-session usage and control, tailnet view of their estate), and enterprise framework teams running CrewAI/LangGraph flows in prod (attach with little or no integration, OTel into existing observability, policy that never strands work). A committed first production host anchors the enterprise path.

## Open questions

- OSS business model (adoption first; monetization out of scope here).
- Policy versioning for long runs: pin by default vs opt-in follow-latest (ledger records which).
- Run keys optional? Requiring them may break processes that fail to authenticate; off/shadow/enforce is the proposed answer, defaults per environment undecided.
- First-class CrewAI/LangGraph attach paths (base-URL override, self-announce) need validating with real flows.
- Attach-only discovery: without self-announce, ps learns of runs only from incoming traffic.
- Completeness check: the user wants the plan to miss nothing from the gaps found versus Hekma today.
