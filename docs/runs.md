---
title: "Runs, Control Depth, ps and top"
description: "Every agent workload as a Run with an id, labels, a status and a Control Depth: supervised Runs, child Runs, hekma label, hekma ps, Selectors on the lifecycle verbs and hekma budget are on main; hekma top is in development."
---

# Runs, Control Depth, ps and top

> **Partly on main.** Runs for supervised agents, child Runs, labels from configuration, `hekma run`, `hekma label`, `hekma ps`, Selectors on the lifecycle verbs and `hekma budget` are on main and ship in the next release. `hekma top` and Runs Hekma did not start are still being built, and their names, commands and defaults may change before they ship. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

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

Keys start with a lowercase letter and use `a-z`, `0-9`, `_`, `.` and `-` (at most 63 characters); values are 1 to 128 letters, digits or `_ . : / @ + -`, and never a `secret:` reference. Labels merge per key across configuration layers, like any key. Setting a label with `hekma agent config set` while the agent runs updates its Run at once; a label in another layer applies at the next start. Use the `labels` key for labels every future Run of an agent should carry.

<a id="bound-labels"></a>**Bound labels on a Run (`hekma label`).** Bind a label to one Run, to an agent's open Run, or to every open Run a Selector matches:

```bash
hekma label set my-agent team=core
hekma label set run_01JZ6Q9G3S8V2X7K4M1N5P0R2T env=prod tier=gold --json
hekma label set --selector team=core,env=prod tier=gold --dry-run
hekma label remove --selector team=core tier --yes
```

A label set this way is bound to the Run and records its origin, `label-verb`. Per key it outranks the agent's `labels` configuration (origin `config`) and any label the Run set about itself, so a later `labels.*` write, a crash relaunch or a restart of Hekma never overwrites it. Labels belong to the Run: a relaunch keeps them, and a new `start` opens a new Run without them. `hekma label remove` takes back a label set this way; if the configuration binds the same key, its value returns in the same change. A label that comes from configuration is changed with `hekma agent config set` (or overridden on the Run with `hekma label set`), and a label the Run set about itself cannot be removed here. Removing an absent key changes nothing, and a Run that has ended is refused. Each change is one `labels-changed` Run event naming the bound labels before and after.

`--selector key=value[,key=value…]` matches every open Run carrying all those labels, bound or caller. When it matches more than one Run, the set is listed and you are asked to confirm; `--yes` skips the question, and without a terminal the command is refused unless `--yes` is given, so a script never hangs. `--dry-run` lists each Run's planned change and writes nothing. Each Run reports its own result (`labeled`, `unchanged`, `would label`, or why it was refused), a refusal does not stop the others, and the exit code is the first failure's: `3` no such Run, `4` refused by the Run's state or labels, `2` an invalid request. A Selector that matches no Run is a success with a note; one that matches more than 1000 Runs is refused. `--json` prints the one results document every set-taking verb shares, `{schema_version, verb, dry_run, matched, results}` with `verb` `label`. The same verb is on the [Control API](control-api.md#bound-labels) as `PATCH /v0/runs/{id}/labels`, and over a selected set as one `PATCH /v0/runs/labels`.

A Caller cannot set a key that is already bound: a caller label on a bound key is refused (`label-bound`).

**Child Runs.** A Host can open a child Run under an agent's Run for one unit of its own work, with its own ceilings:

```bash
hekma run open --parent my-agent --label task=review --token-budget 20000
hekma run show run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
hekma run close run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
```

`--parent` takes a run id or an agent name (meaning that agent's open Run). A child of a supervised agent is depth `announced` and runs inside that agent's process (opened while the agent is paused, it starts `held`). Its usage counts on itself and on every ancestor, and every ancestor's ceilings apply to it: a child's own ceiling breaching records a breach on the child, and its parent's breaching acts on the agent as configured. A child's own ceilings take the `warn` Breach Action; `pause`, `stop` and `block` set directly on a child are refused until Hekma can hold or reject one Run's calls. Labels given at `open` are *caller* labels; `--external-id` makes a retried open return the same Run. Pausing or stopping the agent holds or ends its child Runs too, recording `parent-paused`, `parent-resumed`, `parent-stopped` or `parent-ended`. Runs nest at most 8 levels; a parent that does not exist or has ended is refused with an error naming the cause. `--json` prints the Run as a document; the same operations are on the [Control API](control-api.md) at `/v0/runs`.

Until model calls carry a Run's identity, usage reaches child Runs only from Hosts embedding the engine; the shared Metering Proxy will attribute calls to them.

**`hekma ps` — the Runs that need you, newest first.** One exception-first listing over every Run in the state directory ([the command reference](commands.md#hekma-ps---selector-sel--all---limit-n---cursor-c---json)):

```bash
hekma ps
hekma ps --selector team=core,env=prod
hekma ps --all --limit 100
```

A Run is listed when it carries an **Anomaly** (an ended Run only within seven days of its end) or a `held`, `silent` or `stale` status; healthy open Runs are counted in the footer, not listed, and `--all` lists every Run inside that window. The window is a read-side bound of seven days (the retention default) until compaction lands; older ended Runs stay in the store and are reachable by id, they just age out of listings. Rows are ordered by run id, newest first — ULID ids encode creation time, so the order is the creation order — and the footer's next-page command (the same view, Selector and `--limit` plus the `--cursor` value, so the next page continues the same listing) pages without duplicates; a page that is exactly full and last prints no next page. A Run that changes status between two pages neither moves nor repeats, a Run deleted between pages is skipped, and one that starts matching below the cursor is not chased. Legacy `run-…` ids sort below every `run_…` id and therefore land last.

Each row carries the CONTROL depth (`supervised (no wire)` for an agent whose model calls skip the metering listener), the Run Status, the Anomalies with each latched Breach Action (`breach[block]` — a fenced agent never reads as merely healthy), the labels (a caller label is suffixed), the Run's subtree usage, and its headroom against the **resolved** limits — naming the Policy that set them, with the cumulative token headroom on process-owning rows and no dollar figure where no Rate prices one. The RUN column shows the agent's name on process-owning rows and the run id on child Runs. `--json` carries every figure whole: each row's labels with source and origin, its latches, its resolved limits with each limit's source, the effective Breach Action after the depth fallback, and the `kept_on_pause` marker where the breach-default upgrade holds an agent on `pause`.

`--selector` matches bound and caller labels alike in either view, and a listing pages past the 1000-Run cap the label verb carries (a listing never loads the whole Run set). `--limit` defaults to 50 and serves at most 1000 (`0` means the maximum; above the cap it clamps with a note). A read-only read: `hekma ps` opens the state database read-only, never starts a host, creates nothing, and exits `0` with a note when nothing needs attention (the footer's counts still print). Exit `2` is an invalid selector, cursor or limit; `1` a store failure.

<a id="verbs-at-scale"></a>**Verbs at scale: Selectors on the lifecycle verbs.** `hekma agent stop`, `pause`, `resume` and `drain`, and `hekma run close`, take an agent, a run id, or `--selector`:

```bash
hekma agent pause run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
hekma agent stop --selector team=core --dry-run
hekma agent stop --selector team=core --yes --json
hekma run close --selector task=review --yes
```

On the Run that owns an agent's supervised process, the verb is the agent's own: the same stop ladder, the same drain bound, the same notes (pausing a run id pauses its agent, and the Run goes `held`). Any other Run answers from the verb table below: before Hekma can hold or reject one Run's calls, `stop`, `pause`, `resume` and `drain` on a child Run are refused, naming its depth, and a child held because its parent is paused is refused naming the parent. A Run is never skipped silently: every Run the Selector matched gets its own result (`stopped`, `paused`, `would-stop`, or `refused` with the reason), a refusal never stops the others, and the exit code is the first failure's class with the count in its message. The set is listed and confirmed as for `hekma label`: only when it holds more than one Run, only on a terminal (a pipe is refused unless `--yes` is given, and stdin is never read). `--dry-run` plans each Run from its status and depth, writes nothing and starts no Service Host. `--json` prints the shared results document. `start` and `send` take no Selector: a Selector matches open Runs, so there is nothing to start, and `send` carries text to one agent's input.

A Selector act runs on exactly the set you saw: the CLI asks the Service Host to select the Runs (`POST /v0/runs/select`), which answers with a **set token**, and acts with one call that carries it. A Run that ended between the selection and the act is that Run's `run-ended` refusal; the rest act. Slow verbs run their windows one after another, so a set's worst case is the sum of its windows; the CLI waits at most 10 seconds plus each Run's bound (capped at 10 minutes) and then says it stopped waiting while the acts may still be committing.

<a id="run-limits"></a>**Limits on a Run (`hekma budget`).** `hekma budget show` reads a Run's limits: each ceiling with where it comes from (`run`, `instance`, `policy:<name>`, `default`), its usage, the headroom, the Breach Action and any latched breach. `hekma budget set` writes limits on the Run itself:

```bash
hekma budget show my-agent
hekma budget set my-agent --tokens-per-run 500000
hekma budget set run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --dollars-per-run 3.00 --breach-action pause
hekma budget set --selector team=core --clear tokens-per-run --yes
```

A limit set on a Run outranks the agent's `budget.*` configuration and every Policy for that limit. It **belongs to the Run**, like a label set with `hekma label`: a crash relaunch keeps it, it ends with the Run, and a new `start` opens a new Run without it, so every result says it is Run-scoped (`run_scoped` and a note in `--json`). For limits every future Run should carry, set the agent's `budget.*` configuration or a [Policy](policies.md). Cumulative ceilings go on an agent's own Run only (they count the agent's lifetime usage); a child Run takes per-run ceilings. A raised ceiling above the usage re-arms its breach at once and lifts a matching `block` fence; one lowered to or below the usage breaches at once and applies the Breach Action, with no wait for the next model call. `--clear` hands a limit back to the next layer, which may raise or lower it. A limit a Caller sets is kept apart and applies only while it is tighter than the one the Operator, the Host or a Policy sets. Each change is one `limits-changed` Run event naming the Run's limits before and after. Over a Selector, `set` confirms like every set-taking verb; `show` with a Selector, or with no target, lists Runs as `hekma ps --all` does and pages with `--limit` and `--cursor`.

## What is coming

A **Run** is one unit of agent work with a stable run id, a Control Depth, labels, budgets and a Run Status, whether or not Hekma started its process: a supervised agent, a child Run inside a Host or framework flow, or a client Hekma found.

| Part | Stage |
|---|---|
| Runs for supervised agents, run ids, labels from configuration, parent and child Runs, `hekma label` with `--selector`, `--dry-run` and `--yes` | On main |
| `hekma ps`, Selectors on listings | On main |
| Selectors, `--dry-run` and `--yes` on `stop`, `pause`, `resume`, `drain` and `run close`; `hekma budget` | On main |
| Hold, release, reroute and reject on a set | In development |
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

`hekma run show --json` also carries the Run's resolved [Policy](policies.md)
limits — each with its source (the Run, the agent's configuration, a Policy
by name, or the default Policy) — plus the `depth-fallback` and
`unpriced-model` Anomalies a Policy can raise.

**Run Status** is `active`, `held`, `silent` (no recent heartbeat or traffic), `stale` (its machine is unreachable) or `ended`. An **Anomaly** is a condition Hekma makes loud, such as a budget breach or a Run gone `silent`. It never acts on a Run, and no Run is stopped for running long: only a verb you run or a [Policy](policies.md) you configure acts on one.

## How it will work

**Labels** are `key=value` pairs that record their source. Bound labels come from you or your Host: an agent's `labels` config key and `hekma label` (both on main), a [Run Key](run-keys.md), or a Metering Proxy endpoint. Caller labels come from the agent or framework itself (a **Caller**). A Caller cannot set a bound key, and only bound labels select Policies, so an agent cannot label its way into looser limits.

**Parent Runs.** Runs are flat unless one names a parent (on main, as above). A verb on a parent will reach its children only through a Selector, except that stopping or pausing the Run that owns a supervised process also stops or pauses the child Runs inside it.

A **Selector** (`key=value,…`) names a set of Runs on any listing or verb, as it already does for `hekma label`, `hekma ps`, `hekma budget` and the lifecycle verbs. When it matches more than one Run on a verb, the verb shows the set and asks you to confirm (`--yes` skips this); `--dry-run` only shows it. Listing Selectors page past the 1000-Run cap the verbs keep.

**`hekma top`** will show each Run's status, depth, tokens per second, burn rate and budget left, live. Hotkeys will pause, stop, hold, raise a limit and attach, confirming as the matching command does. It can watch a remote `hekma serve`; the `/v0/events` stream's Selector filter and resume (see the [Control API](control-api.md)) are the seam it builds on.

Every filter, Selector and page parameter of `hekma ps` has a [Control API](https://hekma.ktesio.dev/docs/control-api/) equivalent — `GET /v0/runs?selector=&view=&limit=&cursor=` — so teams can work from scripts and their own observability stack. The goal is that listing stays fast from thousands to millions of Runs.

## Open questions

- The Control API stays at version 0 until a second Host validates it; whether another milestone can make it stable is undecided, so expect the Run routes to change.

## Read next

- [Policies](policies.md): standing limits for every Run a Selector matches.
- [Run Keys](run-keys.md): giving each Run an identity on the wire.
- [Attach and wire control](attach.md): bringing in Runs Hekma did not start.
