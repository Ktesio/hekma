---
title: "Attach and wire control"
description: "Bring agents Hekma does not launch under control with little or no code, and hold, release, reroute or reject their model calls. Planned."
---

# Attach and wire control

> **Planned.** Nothing on this page is in Hekma 0.11.0 or on main yet. It describes the design being built in the open, and names, commands and defaults may change before it ships. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/attach/](https://hekma.ktesio.dev/docs/attach/), where it is kept current.

A solo developer has coding and personal agents open in several terminals; a team runs CrewAI or LangGraph flows inside its own services. Neither is started with `hekma agent start`. Hekma will bring both under control with little or no integration: point the agent's model base URL at Hekma, and its work becomes a [Run](runs.md) you list, meter and steer with the same verbs as an agent Hekma launched.

## What works today

Hekma 0.11.0 has one piece of this: a per-agent loopback proxy for [engine-observed metering](https://hekma.ktesio.dev/docs/metering/#engine-observed). Hekma opens a proxy on `127.0.0.1` when the agent starts, forwards each call to `metering.upstream_base_url` and reads the token counts from each response. The proxy lives inside the starting command, so the agent cannot run detached. [Metering agents you don't control](https://hekma.ktesio.dev/docs/design/metering-agents-you-dont-control/) explains why the meter sits on the wire.

0.11.0 cannot meter an agent it did not start or hold a call; a pause suspends the agent's process.

## What is coming

Every Run will have a **Control Depth**, a ladder in which each rung includes the one below: `observed` < `wire` < `announced` < `supervised`. This page covers the first three.

| Control Depth | How a Run gets there | What Hekma can do |
|---|---|---|
| `observed` | It sends Hekma its telemetry over OTLP | List it, estimate usage and warn on budgets; other verbs fail fast |
| `wire` | Its model calls pass through the Metering Proxy | Meter, budget, hold, release, reroute and reject its calls |
| `announced` | `wire`, and it declared itself and keeps a Lease | As `wire`, known before its first call |
| `supervised` | Hekma owns its process | Everything, including process pause and stop |

- **One shared Metering Proxy per host.** The **Metering Proxy** is where Hekma meets model traffic. It will replace today's per-agent proxy with one listener per host on a fixed port, loopback by default.
- **Endpoint bindings.** You will create bindings with `hekma binding`: a path prefix, the upstream origin it forwards to, labels bound to it and optional routes to reroute to.
- **Wire Runs from traffic.** The first call through a binding creates a `wire` Run, carrying the binding's labels with no code change.
- **Self-announce.** A flow can declare itself through a header on its first call, one call to an announce endpoint, or a helper for CrewAI and LangGraph. The helpers will be documented as supported only once they are validated against real flows. Announcing returns a [Run Key](run-keys.md).
- **Leases.** An announced Run keeps a **Lease** alive by heartbeat or by its own traffic. An expired Lease shows the Run's **Run Status** as `silent`. It never stops the Run.
- **Inbound OTLP.** Hekma will accept OTLP/HTTP+JSON metrics at `/v1/metrics`. Telemetry that names a Run creates an `observed` Run, with usage marked estimated.
- **In front of a gateway.** Hekma is not a model gateway. It will add identity headers (run id and labels, never content) so LiteLLM, Portkey or a similar gateway behind it can attribute calls.

## How it will work

Wire control acts on calls, not on processes. Each verb takes a **Selector**, a label expression that names a set of Runs.

| Verb | Effect |
|---|---|
| hold | New calls wait at the proxy with the agent's state intact. Hekma sets no time limit; only the client's own timeout ends a hold. |
| release | Held calls go through unchanged, in arrival order. |
| reroute | Calls go to another route configured on the binding. |
| reject | Calls get the typed, non-retryable response a `block` **Breach Action** returns. |

Where an agent has a wire path, a hold can stand in for a native pause. `hekma ps` will show it as qualified, because a hold freezes calls, not the process.

Nothing here stops a production run on its own. The default Breach Action will become `warn`, a Run's duration never changes how it is treated, and only a [Policy](policies.md) or a verb you run acts on a Run.

The proxy will survive restarts: an in-place upgrade hands its listening socket to the new process, and a hold belongs to its Run, so a retried call is held again.

A sketch of the planned commands; the final names and flags may differ:

```text
# Bind a path to an upstream and label everything that calls it.
hekma binding add --path /support --upstream https://api.openai.com --label team=support

# In the flow's environment: the only change the flow needs.
export OPENAI_BASE_URL=http://127.0.0.1:<proxy-port>/support

# Later: hold every support Run's calls, then let them through.
hekma hold --selector team=support
hekma release --selector team=support
```

Base-URL-only traffic is served under the Policy's **Scent-Check Mode** `off` or `shadow`; under `enforce` the client must present a Run Key or announce.

## Open questions

- **Default Scent-Check Mode per environment.** Proposed: `off` on a laptop, `shadow` under a long-running host, `enforce` by opt-in. Until decided, the default is `off`.
- **When the Control API leaves 0.x.** Bindings and the announce endpoint are Control API routes, unstable until a second Host validates the API.

## Read next

- [Runs and Control Depth](runs.md)
- [Policies](policies.md)
- [Run Keys and key custody](run-keys.md)
- [Meter token usage](https://hekma.ktesio.dev/docs/metering/), for the proxy in 0.11.0
