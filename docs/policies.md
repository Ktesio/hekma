---
title: "Policies and the warn default"
description: "Standing limits that apply to every Run a Selector matches, and warn as the default Breach Action. In development."
---

# Policies and the warn default

> **In development.** Apart from the `block` Breach Action, nothing on this page is in Hekma 0.11.0 or on main yet. It describes the design being built in the open, and names, commands and defaults may change before it ships. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/policies/](https://hekma.ktesio.dev/docs/policies/), where it is kept current.

Budgets in Hekma today belong to one agent at a time. A developer running a dozen coding and personal agents wants one ceiling for all of them, and a looping session to be loud, not frozen mid-task. A team running CrewAI or LangGraph flows in production wants one rule for "every nightly research flow", including tomorrow's runs, that never stops production unless someone chose that.

A **Policy** is that rule: standing limits that apply to every [Run](runs.md) its Selector matches, now and later. A Run is one unit of agent work, whether or not Hekma started it. The default **Breach Action**, what happens when a ceiling is reached, also changes from `pause` to `warn`.

## What works today

In Hekma 0.11.0, limits are per-agent [config keys](https://hekma.ktesio.dev/docs/budgets/#set-a-budget): `budget.tokens.*`, `budget.dollars.*`, `cost.rate.*`, and one `budget.breach_action` for all of an agent's caps. The Breach Action defaults to `pause`, and `warn` only writes a line to `logs/breaches.log`. A cap fires once per run, and [raising it does not re-arm it](https://hekma.ktesio.dev/docs/budgets/#resume).

On main, the [Control API](https://hekma.ktesio.dev/docs/control-api/) can set the same keys over HTTP. `budget.breach_action` also accepts `block`, the wire fence, on agents whose usage is read off the wire (see [Run Hekma as a service](https://hekma.ktesio.dev/docs/hosting/)). There are no Policies or labels yet.

## What is coming

**Policies.** A Policy holds any mix of Token Budgets, Cost Caps, Rates, a Breach Action and, once Run Keys land (planned), a **Scent-Check Mode**: how Hekma treats a call without a valid Run Key, `off`, `shadow` (served and recorded as unscented) or `enforce` (refused). It names its Runs with a **Selector**, a label expression such as `team=research,env=prod`. Selectors match only *bound* labels, set by the Operator or a Host through an agent's configuration, a [Run Key](run-keys.md), a Metering Proxy endpoint or `hekma label`. Labels a Run sets about itself never pick its Policy, so an agent cannot choose a looser one.

**One limit at a time.** Hekma resolves each limit on its own, in this order:

| Source | Rule |
|---|---|
| Explicit limits on an agent or a Run | Set by the Operator or Host. They outrank every Policy. |
| Matching Policies | The most specific Selector wins; a tie goes to the tightest value. |
| The default Policy | Fills whatever is still unset. |

Selector A is more specific than B when A's terms are a strict superset of B's; Selectors that cannot be compared tie. From loosest to tightest, Breach Actions run `warn` < `pause` < `stop` < `block`, and Scent-Check Modes run `off` < `shadow` < `enforce`.

Once Run Keys land (planned), a **Caller**, an agent or helper acting on its own Run through its Run Key, can also set a limit; it applies only while tighter than the effective one. Callers can tighten, never loosen.

**The default Policy** is a stored Policy named `default` that you can edit like any other: no ceiling, Breach Action `warn`, Scent-Check Mode `off`, and a model-keyed Rate table, so Runs that Hekma only sees on the wire still have a price.

**The warn default.** `warn` acts on nothing and signals loudly: an **Anomaly** on the Run in `hekma ps`, an event, an OTLP metric when export is configured, and a response header on calls that pass through the **Metering Proxy**, Hekma's interception point for model traffic. `pause`, `stop` and `block` stay opt-in. Nothing Hekma does on its own stops a production Run unless an Operator configured it, and a Run is never treated differently for running long.

**Upgrades keep your pause.** An agent registered before the change that relied on the implicit `pause` default keeps `pause`, written into its config, with a "kept on pause by upgrade" note in listings until you set a Breach Action yourself. A malformed `breach_action` fails closed to `pause` with a diagnostic, never to `warn`.

**`block`, the wire fence.** `block` is never a default. On main it already works per agent: on a breach, the agent's metering listener refuses further model calls with HTTP 402 and a typed, non-retryable error, and the fence lifts when the Run ends (raise the ceiling, then start a new Run). With Policies it will apply to every Run on the Metering Proxy, and raising a ceiling will lift the fence on a live Run. An agent that bypasses the listener or the proxy bypasses the fence too.

## How it will work

You will manage Policies with `hekma policy` or `/v0/policies` on the Control API; every change is an event. A change applies at once to every matched open Run: lowering a ceiling below a Run's usage breaches it now, and raising one re-arms its breach latch. Policies will have no versions at first; versioned policy-as-data comes later.

Each Run has a **Control Depth**, how far Hekma controls it: `observed` < `wire` < `announced` < `supervised`. Where a Policy's Breach Action is beyond a Run's depth, such as `pause` on a Run known only from its telemetry, the Run acts as `warn` for that ceiling and carries an Anomaly naming the depth and Policy. Policy changes report how many Runs fall back.

A sketch of the planned command; the final flags may differ.

```text
hekma policy create nightly-research \
  --selector team=research,env=prod \
  --tokens-per-run 2000000 \
  --breach-action pause
```

## Open questions

- **Scent-Check defaults per environment.** The default Policy starts at `off`. Whether a laptop and a server should start differently is still open; deciding it will not overwrite a default Policy you have edited.
- **Pinning a Policy for long Runs.** For now a Run follows the latest Policy. Whether it should keep the one in force when it started will be decided with versioned policy-as-data.

## Read next

- [Runs](runs.md): Control Depth, Run Status and labels.
- [Run Keys](run-keys.md): how a Run gets bound labels and an identity.
- [Budgets and cost](https://hekma.ktesio.dev/docs/budgets/): per-agent budgets as they work in 0.11.0.
