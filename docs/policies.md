---
title: "Policies and the warn default"
description: "Standing limits that apply to every Run a Selector matches, and warn as the default Breach Action."
---

# Policies and the warn default

> **On main, unreleased.** Policies and the `warn` default are on main and ship in the next release. Scent-Check Modes are on main too. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/policies/](https://hekma.ktesio.dev/docs/policies/), where it is kept current.

A developer running a dozen coding and personal agents wants one ceiling for all of them, and a looping session to be loud, not frozen mid-task. A team running CrewAI or LangGraph flows in production wants one rule for "every nightly research flow", including tomorrow's runs, that never stops production unless someone chose that.

A **Policy** is that rule: standing limits that apply to every [Run](runs.md) its Selector matches, now and later. The default **Breach Action**, what happens when a ceiling is reached, is `warn`.

## A Policy

A Policy holds any mix of Token Budgets (per run and cumulative), Cost Caps (per run and cumulative, estimated from Rates), a flat Rate and a Breach Action. It names its Runs with a **Selector**, a label expression such as `team=research,env=prod`, and every term must match.

Selectors match only *bound* labels: labels the Operator set through an agent's `labels` configuration or with [`hekma label`](runs.md#bound-labels). Labels a Run sets about itself never pick its Policy, so an agent cannot choose a looser one.

```text
hekma policy create nightly-research \
  --selector team=research,env=prod \
  --tokens-per-run 2000000 \
  --dollars-per-run 25 \
  --breach-action pause
```

`hekma policy list`, `show`, `change` and `delete` manage them; each takes `--json`, and every write takes `--dry-run`, which reports what it would match and writes nothing. `/v0/policies` on the [Control API](control-api.md) does the same over HTTP. Every change is a `policy` event.

## One limit at a time

Hekma resolves each limit on its own, in this order:

| Source | Rule |
|---|---|
| A malformed `breach_action` anywhere | Fails closed to `pause` with a diagnostic — before every other source, a Run's own action included. Only the action fails closed: the ceilings, the Rate and the model an agent set explicitly still apply. |
| Explicit limits on a Run, then on an agent | A limit set on the Run itself with [`hekma budget set`](runs.md#run-limits) comes first, then the agent's configuration (any layer). Both outrank every Policy. |
| Matching Policies | The most specific Selector wins; a tie goes to the tightest value. |
| The default Policy | Fills whatever is still unset. |

Selector A is more specific than B when A's terms are a strict superset of B's; Selectors that cannot be compared tie. From loosest to tightest, Breach Actions run `warn` < `pause` < `stop` < `block`. A tie between two ceilings goes to the lower one.

`hekma budget show <run-id>` and `hekma run show <run-id> --json` name where
each of its Run's limits came from: the Run itself, the agent's
configuration, a Policy by name, or the default Policy.

**Which one to use.** A limit set with `hekma budget set` belongs to one Run:
it ends with the Run, and the agent's next `start` opens a new Run without it
— the tool for "give this Run more room" or "rein this one in now". Limits
every future Run of an agent should carry go in its `budget.*`
configuration; standing rules for a whole class of Runs, now and later, are
Policies.

## The default Policy

`default` is a stored Policy you change like any other, with `hekma policy change default`. It starts with no ceiling, Breach Action `warn` and Scent-Check Mode `off`. It cannot be deleted or given a Selector.

It also carries a Rate table keyed by model, used for Runs with no Rate of their own. The table starts empty: Hekma never invents prices. Add an entry for the exact model name an agent's `model` key sets:

```text
hekma policy change default --rate-for gpt-4.1 --rate-input 2.00 --rate-output 8.00
hekma policy change default --clear rate:gpt-4.1
```

A Rate comes from the agent's own `cost.rate.*` keys first, then a matching Policy's flat Rate, then this table. A flat Rate belongs to a named Policy — setting one on `default` is refused, because only the table prices `default`'s Runs. A Cost Cap set by a Policy on a Run whose model has no Rate cannot be enforced; the Run carries the `unpriced-model` Anomaly until a Rate resolves, so the gap is never silent.

## The warn default

`warn` acts on nothing and signals loudly: the breach is recorded, raises the `breach` Anomaly on the Run, and is published as an event (and an OTLP metric when export is configured). `pause`, `stop` and `block` stay opt-in. Nothing Hekma does on its own stops a Run unless an Operator configured it.

**Upgrades keep your pause.** An agent registered before this change that relied on the implicit `pause` default keeps `pause`: the first open of the upgraded state directory writes `budget.breach_action = "pause"` into the agent's configuration, records one `breach_default_upgraded` event, and shows a "kept on pause by upgrade" note until you set a Breach Action yourself. An agent whose configuration cannot be written still pauses. A malformed `breach_action` fails closed to `pause` with a diagnostic, never to `warn`.

## Changes apply at once

A Policy change applies to every matched open Run immediately — the same rule `hekma budget set` and an agent's `budget.*` configuration write apply. Lowering a ceiling to or below a Run's usage breaches it now; raising one above the usage re-arms its breach latch, so the next crossing fires again, and lifts the `block` fence for that ceiling on a live Run. Clearing a ceiling or deleting the Policy re-arms too — a latch never outlives the ceiling that fired it. Changing a Policy's Selector re-resolves the Runs both the old and the new Selector match, so the Runs only the old one governed loosen as well. Deleting a Policy re-resolves the Runs it governed under what remains.

An unknown `--clear` field is refused (exit 2 on the CLI, `400` on the API): a typo would otherwise report success while clearing nothing.

## Control Depth and fallback

Each Run has a **Control Depth**, how far Hekma controls it. Where a Policy's Breach Action is beyond a Run's depth, such as `block` on an agent whose calls skip the [metering proxy](metering-proxy.md), or `pause` on a child Run inside an agent's process, the Run acts as `warn` for that ceiling and carries the `depth-fallback` Anomaly. The action takes effect if the Run's depth later allows it. Creating or changing a Policy reports how many matched Runs fall back.

## `block`, the wire fence

`block` is never a default. On a breach, the [metering proxy](metering-proxy.md) refuses further model calls of the Run, and of every Run beneath it, with HTTP 402 and a typed, non-retryable error. It works at every depth with a wire path, not only for supervised agents. The fence lifts when the Run ends, or when the ceiling is raised above the Run's usage — by a Policy change, `hekma budget set` or an agent configuration write — reason by reason. An agent that bypasses the proxy bypasses the fence too; for an agent in its own Shim, [bypass detection](run-keys.md#bypass-detection-preview) notices such a connection.

## Not yet

- **Scent-Check Modes.** Every Policy may carry a [Scent-Check Mode](run-keys.md#scent-check-modes): `off`, `shadow` or `enforce` (`--scent-check` on `hekma policy create` and `change`; `--clear scent-check` removes it). A tie between matching Policies takes the strictest mode. A Run's own Caller may only tighten its Run's mode, never lower it, and the default stays `off` everywhere.
- **Pinning a Policy for long Runs.** A Run follows the latest Policy. Versioned policy-as-data comes later.

## Read next

- [Runs](runs.md): Control Depth, Run Status and labels.
- [Run Keys](run-keys.md): how a Run will get bound labels and an identity.
- [Budgets and cost](https://hekma.ktesio.dev/docs/budgets/): per-agent budgets and Rates.
