---
title: "Runs, Control Depth, ps and top"
description: "Every agent workload as a Run with an id, labels, a status and a Control Depth, listed with hekma ps, steered by Selector and watched with hekma top. In development."
---

# Runs, Control Depth, ps and top

> **In development.** Nothing on this page is in Hekma 0.11.0 or on main yet. It describes the design being built in the open, and names, commands and defaults may change before it ships. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/runs/](https://hekma.ktesio.dev/docs/runs/), where it is kept current.

With a dozen agents on a laptop, you want one list of those that need you. With CrewAI or LangGraph flows in production, you want the flows that broke a limit, and one command for all of them. Hekma is making the **Run** the one thing every listing and verb acts on.

## What works today

In 0.11.0 a [run](https://hekma.ktesio.dev/docs/concepts/#run) is one life of a registered agent's process. `hekma agent list` shows the [fleet](https://hekma.ktesio.dev/docs/concepts/#fleet), `hekma agent usage NAME` shows this run's tokens, and per-run [budgets](https://hekma.ktesio.dev/docs/budgets/) reset at each new run. There are no labels, no Selectors and no way to list work Hekma did not start.

## What is coming

A **Run** will be one unit of agent work with a stable run id, a Control Depth, labels, budgets and a Run Status, whether or not Hekma started its process: a supervised agent, a child Run inside a Host or framework flow, or a client Hekma found.

| Part | Stage |
|---|---|
| Runs for supervised agents, run ids, labels, parent and child Runs | In development |
| `hekma ps`, Selectors on every verb, `--dry-run`, `hekma label` | In development |
| `hekma top` | Planned |
| Runs Hekma did not start (depths `wire`, `announced`, `observed`) | Planned |

**Control Depth** says how far Hekma controls a Run, on a ladder where each rung includes the one below: `observed` (known from telemetry or an announcement only) < `wire` (its model calls pass through the **Metering Proxy**) < `announced` (wire, plus the Run declared itself and keeps a **Lease**, its claim to be alive) < `supervised` (Hekma owns the process). A Run keeps its id when its depth changes. A supervised agent whose calls skip the proxy shows as `supervised (no wire)`.

Which verbs work depends on the depth; “no” means the verb fails at once with an error naming the depth.

| Verb | `observed` | `wire` | `announced` | `supervised` |
|---|---|---|---|---|
| List, usage | estimated, from telemetry | per call | per call | per call, plus process state |
| `logs` | no | per call | per call | process output, per call |
| Budgets with `warn` | from telemetry | yes | yes | yes |
| `pause` / `resume` | no | holds its calls | holds its calls | pauses the process |
| `stop` | no | rejects its calls | rejects its calls, ends the Lease | stops the process |
| `send`, `attach` | no | no | no | as the adapter declares |
| hold, release, reroute, reject | no | yes | yes | yes |

**Run Status** is `active`, `held`, `silent` (no recent heartbeat or traffic), `stale` (its machine is unreachable) or `ended`. An **Anomaly** is a condition Hekma makes loud, such as a budget breach or a Run gone `silent`. It never acts on a Run, and no Run is stopped for running long: only a verb you run or a [Policy](policies.md) you configure acts on one.

## How it will work

**Labels** are `key=value` pairs that record their source. Bound labels come from you or your Host: an agent's `labels` config key, a [Run Key](run-keys.md), a Metering Proxy endpoint, or `hekma label`. Caller labels come from the agent or framework itself (a **Caller**). A Caller cannot set a bound key, and only bound labels select Policies, so an agent cannot label its way into looser limits.

**Parent Runs.** Runs are flat unless one names a parent. A child's usage rolls up to every ancestor, and their ceilings apply to it. Stopping or pausing the Run that owns a supervised process also stops or pauses the child Runs inside it; otherwise a verb on a parent reaches children only through a Selector. A crash relaunch continues the same Run, so per-run budgets will no longer reset on a crash.

**`hekma ps`** is exception-first: Runs with an Anomaly or a `held`, `silent` or `stale` status are listed, healthy ones are counted, and `--all` lists everything. The CONTROL column shows the depth, `--json` each label's source, and `--limit` with a cursor pages.

A **Selector** (`key=value,…`) names a set of Runs on any listing or verb. When it matches more than one Run, the verb shows the set and asks you to confirm (`--yes` skips this); `--dry-run` only shows it.

A sketch of the planned commands; the final flags and output may differ:

```text
$ hekma ps --selector team=support
RUN                             CONTROL     STATUS  ANOMALIES      LABELS
run_01JBX7Q2M4V8K3N6P9R2T5W8YZ  supervised  held    budget-breach  team=support,flow=triage
run_01JBX7R5C8D1F4G7H0J3K6M9NP  wire        silent  silent         team=support,flow=replies
212 healthy Runs not shown (--all lists them)

$ hekma label --selector flow=replies env=prod --dry-run
```

**`hekma top`** will show each Run's status, depth, tokens per second, burn rate and budget left, live. Hotkeys will pause, stop, hold, raise a limit and attach, confirming as the matching command does. It can watch a remote `hekma serve`.

Every filter, Selector and page parameter will have a [Control API](https://hekma.ktesio.dev/docs/control-api/) equivalent, so teams can work from scripts and their own observability stack. The goal is that listing stays fast from thousands to millions of Runs.

## Open questions

- The Control API stays at version 0 until a second Host validates it; whether another milestone can make it stable is undecided, so expect the Run routes to change.

## Read next

- [Policies](policies.md): standing limits for every Run a Selector matches.
- [Run Keys](run-keys.md): giving each Run an identity on the wire.
- [Attach and wire control](attach.md): bringing in Runs Hekma did not start.
