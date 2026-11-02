---
title: "Runs, Control Depth, ps and top"
description: "Every agent workload as a Run with an id, labels, a status and a Control Depth: supervised Runs and child Runs are on main; hekma ps, Selectors and hekma top are in development."
---

# Runs, Control Depth, ps and top

> **Partly on main.** Runs for supervised agents, child Runs, labels from configuration and `hekma run` are on main and ship in the next release. `hekma ps`, Selectors, `hekma label`, `hekma top` and Runs Hekma did not start are still being built, and their names, commands and defaults may change before they ship. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

> This page mirrors [hekma.ktesio.dev/docs/runs/](https://hekma.ktesio.dev/docs/runs/), where it is kept current.

With a dozen agents on a laptop, you want one list of those that need you. With CrewAI or LangGraph flows in production, you want the flows that broke a limit, and one command for all of them. Hekma is making the **Run** the one thing every listing and verb acts on.

## What works on main

**Every supervised start is a Run.** `hekma agent start` creates a Run with a run id such as `run_01JZ6Q9G3S8V2X7K4M1N5P0R2T` (`run_` and a ULID, so ids sort by creation time), depth `supervised`, and the Run Status `active`. An agent whose model calls do not pass through Hekma's metering listener (a `self-reported` adapter) shows as `supervised (no wire)`. Ids recorded by older versions (`run-…`) stay valid.

**A crash relaunch keeps the Run.** When the Restart Policy relaunches a crashed agent, the run id stays the same, so per-run Token Budgets and Cost Caps keep counting from before the crash, a breach already recorded for the Run does not fire again, and a per-run `block` fence stays up. A `pause` or `stop` the Run had already fired on a breach is applied again to the relaunched agent, so an agent that crashes while paused by a breach comes back paused. Only `start` (by you or a Host) opens a new Run. Before this release a relaunch started a fresh Run with a fresh per-run budget.

**Run Status follows the agent.** A Run is `active` while its agent is starting, running or stopping, `held` while it is paused and `ended` once it stops. A crash marks it with the `failed` Anomaly; the Anomaly clears when the relaunch is running again, and if the Restart Policy gives up (or the relaunch never runs because the command that planned it exited) the Run ends with it. A Token Budget or Cost Cap breach marks its Run with the `breach` Anomaly, whatever the Breach Action.

**Labels from configuration.** Set `labels.<key>` on an agent and its Runs carry the label as a *bound* label:

```bash
hekma agent config set my-agent labels.team payments
```

Keys start with a lowercase letter and use `a-z`, `0-9`, `_`, `.` and `-` (at most 63 characters); values are 1 to 128 letters, digits or `_ . : / @ + -`, and never a `secret:` reference. Labels merge per key across configuration layers, like any key. Setting a label with `hekma agent config set` while the agent runs updates its Run at once; a label in another layer applies at the next start.

**Child Runs.** A Host can open a child Run under an agent's Run for one unit of its own work, with its own ceilings:

```bash
hekma run open --parent my-agent --label task=review --token-budget 20000
hekma run show run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
hekma run close run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
```

`--parent` takes a run id or an agent name (meaning that agent's open Run). A child of a supervised agent is depth `announced` and runs inside that agent's process (opened while the agent is paused, it starts `held`). Its usage counts on itself and on every ancestor, and every ancestor's ceilings apply to it: a child's own ceiling breaching records a breach on the child, and its parent's breaching acts on the agent as configured. A child's own ceilings take the `warn` Breach Action; `pause`, `stop` and `block` set directly on a child are refused until Hekma can hold or reject one Run's calls. Labels given at `open` are *caller* labels; `--external-id` makes a retried open return the same Run. Pausing or stopping the agent holds or ends its child Runs too, recording `parent-paused`, `parent-resumed`, `parent-stopped` or `parent-ended`. Runs nest at most 8 levels; a parent that does not exist or has ended is refused with an error naming the cause. `--json` prints the Run as a document; the same operations are on the [Control API](control-api.md) at `/v0/runs`.

Until model calls carry a Run's identity, usage reaches child Runs only from Hosts embedding the engine; the shared Metering Proxy will attribute calls to them.

## What is coming

A **Run** is one unit of agent work with a stable run id, a Control Depth, labels, budgets and a Run Status, whether or not Hekma started its process: a supervised agent, a child Run inside a Host or framework flow, or a client Hekma found.

| Part | Stage |
|---|---|
| Runs for supervised agents, run ids, labels from configuration, parent and child Runs | On main |
| `hekma ps`, Selectors on every verb, `--dry-run`, `hekma label` | In development |
| `hekma top` | Planned |
| Runs Hekma did not start (depths `wire`, `announced`, `observed`) | Planned |

**Control Depth** says how far Hekma controls a Run, on a ladder where each rung includes the one below: `observed` (known from telemetry or an announcement only) < `wire` (its model calls pass through the **Metering Proxy**) < `announced` (wire, plus the Run declared itself and keeps a **Lease**, its claim to be alive) < `supervised` (Hekma owns the process). A Run keeps its id when its depth changes. A supervised agent whose calls skip the proxy shows as `supervised (no wire)`.

Which verbs work depends on the depth; “no” means the verb fails at once with an error naming the depth. This table is pinned by a conformance fixture, cell by cell, as each one ships.

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

**Labels** are `key=value` pairs that record their source. Bound labels come from you or your Host: an agent's `labels` config key (on main), a [Run Key](run-keys.md), a Metering Proxy endpoint, or `hekma label`. Caller labels come from the agent or framework itself (a **Caller**). A Caller cannot set a bound key, and only bound labels select Policies, so an agent cannot label its way into looser limits.

**Parent Runs.** Runs are flat unless one names a parent (on main, as above). A verb on a parent will reach its children only through a Selector, except that stopping or pausing the Run that owns a supervised process also stops or pauses the child Runs inside it.

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
