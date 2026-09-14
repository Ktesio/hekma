---
title: Command Reference
description: Every kt agent command, its arguments and flags, and the unified config keys for budgets, rates, and metering.
---

# Command Reference

The agent runner lives under `kt agent`. Every command supports `--help`, and `kt --version` prints the package version.

Output discipline: command results go to stdout, while notices and diagnostics go to stderr, so `--json` output on stdout is always machine-parseable.

## `kt agent register <name> (--kind <kind> | --manifest <path>)`

Register a new Agent Instance under a Fleet-unique name.

```bash
kt agent register demo --kind mock
kt agent register my-agent --manifest ./my-agent
```

Arguments and options:

- `<name>` — Fleet-unique instance name matching `^[a-z0-9][a-z0-9_-]*$`.
- `--kind <kind>` — a native builtin adapter by kind (e.g. `mock`, `hermes`). Mutually exclusive with `--manifest`.
- `--manifest <path>` — a manifest adapter loaded from a directory (or an `adapter.toml` file). Mutually exclusive with `--kind`.

Exactly one of `--kind` or `--manifest` is required. Registration validates the adapter's per-OS Capability Declaration and Metering Source **before** any state is written — an adapter with no capabilities or no viable metering source is rejected and nothing is created. On success it prints the engine-computed Agent Home path and the effective (current-OS) Capability Declaration.

The native `mock` kind is a fixture with no launch command; it registers and configures but cannot be started. Use a manifest adapter to run a real process, or the native `hermes` builtin to launch the real Hermes gateway (`hermes gateway run --external-supervisor`) under the engine's supervision — with filesystem Memory Backing attached, the gateway receives `HERMES_HOME` pointing at the instance's managed memory dir.

**Without filesystem Memory Backing the gateway receives no `HERMES_HOME` at all and falls back to the agent's own default home.** A fleet of multiple unbacked hermes instances therefore all resolve the **same** unmanaged default home (documented fallback, not an error); attach Memory Backing (`kt agent memory attach <name> --kind filesystem`, from a terminal state) to give each instance its own isolated home.

## `kt agent list [--json]`

List every Agent Instance in the Fleet.

```bash
kt agent list
kt agent list --json
```

The human table shows name, kind, state, restart count, the token budget (ceilings + remaining + Breach Action, or `—` when un-budgeted), the real usage totals (cumulative tokens plus — when a Rate is configured — the derived dollar cost), and the Agent Home, followed by a Fleet-wide totals footer.

Both money-bearing columns are headed `Budget (tok, est. $)` and `Usage (tok, est. $)`: the `est. $` estimate qualifier lives in the HEADER because those cells are narrow and truncate — a truncated cell can never strip the label off a real dollar figure, so the cells render their dollar figures bare.

The active Metering Source is deliberately **not** a human-`list` column — the compact table is the ratified 80-column design, and a Metering column there overflows the default width and truncates cells. Read the Metering Source from `kt agent show` (the detail row), `kt agent list --json` (the `metering_source` field), or `kt agent usage`.

`--json` emits a versioned document (`schema_version`, `instances`, `totals`); dollar figures appear only when a Rate is configured (integer micro-dollars in JSON, labeled estimates). Token totals always equal the Usage Ledger exactly — see `kt agent usage` below.

## `kt agent show <name> [--json]`

Show one instance's effective per-OS Capability Declaration plus its runtime status.

```bash
kt agent show demo
kt agent show demo --json
```

The runtime status includes the Lifecycle State, Restart Policy, restart count, the token budget and dollar Cost Cap, real usage token totals (cumulative and current-run), the derived dollar cost when a Rate exists, the active Metering Source, and — for a failed instance — the failed cause. `--json` emits the same `FleetEntry` shape a `list` row uses, wrapped with the Fleet `schema_version`.

## `kt agent usage [<name>] [--json]`

Read Usage Ledger totals for one instance, or for the whole Fleet.

```bash
kt agent usage
kt agent usage my-agent
kt agent usage my-agent --json
kt agent usage --json
```

- `<name>` — optional; with a name, reports that instance's usage. Omitted, reports the Fleet-wide totals.
- `--json` — emit a single versioned document (not newline-delimited; usage is a snapshot, not a stream).

The named form reports tokens by both scopes (cumulative and current-run), the derived dollar cost per scope, and the active Metering Source. The Fleet-wide form reports summed tokens plus the summed derived dollar cost across the instances that have a Rate — flagged as a lower bound, naming how many instances are unpriced, when some metered instance has no Rate.

Token totals equal the Usage Ledger exactly, and are the same numbers `list`/`show` report — this command is a focused surface over the same data, never a second, independently-summed figure. Dollar figures appear only when a Rate is configured and are always labeled estimates; with no Rate the dollar view is honestly inert (`—`), never a fabricated `$0.00`. An instance that has never started reports all-zero totals — zeros mean "no usage recorded", not "usage was lost".

Both `--json` forms carry the same Fleet `schema_version` as `list`/`show`, since they serialize the same fleet domain types:

```json
{
  "schema_version": 2,
  "instance": "my-agent",
  "usage": {
    "cumulative_input_tokens": 1200,
    "cumulative_output_tokens": 3400,
    "current_run_input_tokens": 120,
    "current_run_output_tokens": 340,
    "cumulative_dollars": 54600,
    "current_run_dollars": 5460,
    "estimate_label": "estimated"
  }
}
```

The Fleet-wide form (no name) is the same document family with `totals` in place of `instance`/`usage`:

```json
{
  "schema_version": 2,
  "totals": {
    "total_input_tokens": 1200,
    "total_output_tokens": 3400,
    "total_dollars": 54600,
    "dollars_partial": true,
    "unpriced_count": 1,
    "estimate_label": "estimated"
  }
}
```

`totals` is byte-identical to the `totals` object `kt agent list --json` carries — one aggregate, two surfaces. `dollars_partial` is `true` when some metered instance has no Rate, making `total_dollars` a lower bound; `unpriced_count` then names how many instances were left out.

Dollars are **integer micro-dollars** (1,000,000 = $1.00) plus an `estimate_label` of `estimated` or `reconciled` — never a preformatted `$` string, so a caller formats its own currency.

Dollar fields are **omitted entirely** when no Rate is configured (`cumulative_dollars`, `current_run_dollars`, `estimate_label` on the named form; `total_dollars` and `estimate_label` on the Fleet form) — an honest absence rather than a fabricated `$0.00`. A parser must treat them as optional.

With no instances registered, the Fleet form still emits a valid document (all-zero totals) and prints a short registration hint to stderr, so an empty Fleet is never mistaken for instances that consumed nothing.

## `kt agent start <name>`

Start a registered Agent Instance.

```bash
kt agent start my-agent
```

On success the instance transitions to `running` and the new state prints to stdout. A launch failure lands the instance in `failed` with a diagnostic on stderr. Before the process spawns, the start seam can emit one-line engine diagnostics on stderr (or the host sink): a report of every launch environment variable the config mapping **overwrote** (the config value wins), and a warn-only report of config keys whose `secret:` cleartext was delivered into a **flag** target (visible on the process argv) — neither ever rejects the start.

A standalone `kt agent start` supervises the process only for that command's lifetime and stops it when the command exits (a note is printed to stderr). Durable supervision across separate CLI invocations is future work.

## `kt agent stop <name> [--timeout <secs>]`

Stop a running Agent Instance: graceful shutdown, then a forced kill after the window.

```bash
kt agent stop my-agent
kt agent stop my-agent --timeout 10
```

- `--timeout <secs>` — the graceful-shutdown window before a forced kill (default 30). No child process survives. `--timeout 0` skips the graceful window entirely: SIGTERM is sent and a forced kill escalates immediately.

## `kt agent pause <name>` / `kt agent resume <name>`

Pause a running instance, or resume a paused one, with honest per-OS semantics.

```bash
kt agent pause my-agent
kt agent resume my-agent
```

A **guaranteed** pause suspends the process (SIGSTOP on Unix); a **best-effort** pause proceeds cooperatively and prints a visible qualifier note; an **unsupported** pause fails fast, quoting the Capability Declaration. The posture is per-OS, read from the adapter's declaration. `resume` shares the dispatch: a **guaranteed** resume wakes the suspended process (SIGCONT), a **best-effort** resume records its qualifier, and a resume of an instance that is `paused` while the CURRENT declaration reads `unsupported` (declaration/OS drift) fails fast with a dedicated diagnostic that names the paused state and the escape hatch — `stop` works without pause support, so `kt agent stop <name> && kt agent start <name>` recovers (exit code 5, the capability-unsupported class). A guaranteed pause OR RESUME of an instance this engine session holds no process handle for (e.g. started by a prior engine whose process is gone) still transitions, but the recorded cause is the honest best-effort qualifier naming the missing handle — nothing was signalled at all, so the cause never reads as a plain "paused"/"resumed" command that a real suspension would earn; a budget-driven override in that no-handle case is wrapped in the same qualifier rather than replacing it.

## `kt agent send <name> <text>`

Send text input to a running Agent Instance's native input channel (v1: the spawned child's OS stdin pipe).

```bash
kt agent send my-agent "hello there"
```

A trailing newline is appended to `<text>` if it does not already end with one. Unlike `pause`/`resume`, `send` is not a lifecycle transition: the instance's state is unchanged, and only a confirmation prints to stdout.

The same three-way honesty as `pause`, with one difference: a **guaranteed** and a **best-effort** interaction level both deliver the input identically (there is no OS-conditional difference in writing to a pipe — best-effort is purely an adapter-author signal), while an **unsupported** interaction level fails fast, quoting the Capability Declaration.

`send` requires the instance to be genuinely `running`, and it inherits the same single-lifetime caveat as `start` (above): a standalone `kt agent start` supervises a process only for that command's lifetime, so a process adopted after an engine restart has no recoverable input channel in the new session — `send` on such an instance fails honestly (naming the cause) rather than silently dropping the input.

Stdin is piped only for adapters that declare interaction support (`guaranteed` or `best-effort`); an adapter that doesn't declare interaction sees stdin exactly as before this command existed (`/dev/null`-equivalent), so it never blocks waiting on input that will never arrive.

If an agent stops draining its input (a stuck/deadlocked process), `send`'s write is bounded — it fails with a distinct diagnostic naming the timeout rather than hanging, and the instance's interaction channel stays unavailable for the rest of that session until it is stopped and started again.

## `kt agent logs <name> [--follow] [--json]`

Read an Agent Instance's retained output, optionally following live output.

```bash
kt agent logs my-agent
kt agent logs my-agent --follow
kt agent logs my-agent --json
kt agent logs my-agent --follow --json
```

Every currently-retained line is printed to stdout as `<at> [<stream>] <text>`, in the order it was captured (append order — never re-sorted by timestamp, since same-second lines are common). `<stream>` is one of `agent-out`, `agent-err`, or `engine`: the spawned process's stdout and stderr are captured separately (so you can tell them apart), and a best-effort `engine` line is added at each lifecycle transition (start, stop, pause, resume, crash, restart), mirroring the same facts the structured transition log already records.

Log capture is **unconditional and capability-independent** — unlike `send`, it does not depend on the adapter's declared `interaction` support. Reading an instance's output always works, even for an adapter that declares `interaction: unsupported`; only writing to a process (`send`) is gated on that capability.

The captured output is bounded: each generation caps at 10MB, with the current generation plus its 2 most recent rotated predecessors retained (10MB × 3 total, fixed and non-configurable). `kt agent logs` never errors due to rotation — a read that spans a rotation boundary returns whatever is currently retained, not a claim of the instance's entire lifetime history.

`--follow` (`-f`) prints the retained lines first, then keeps polling for new output and printing it as it arrives — exiting cleanly with a note once the instance stops, pauses, crashes to `failed`, or otherwise leaves `running` (never hanging). This works identically whether or not the current `kt` process is the one that originally started the instance: reading only needs the instance's log file, not a live process handle, so `kt agent logs --follow` also works against an instance recovered by crash adoption in a different `kt agent start` session.

`--json` emits **newline-delimited JSON (NDJSON)**: one complete, self-contained log-line object per stdout line, each carrying its own `schema_version`. This is deliberately not a single wrapping document — `--follow` is an unbounded stream that a wrapper could never close — so the shape is identical for the one-shot and `--follow` forms, and a reader can process each line as it arrives:

```json
{"schema_version": 1, "instance": "my-agent", "stream": "agent-out", "at": "2026-07-20T12:00:00Z", "text": "hello"}
```

Lines are emitted in on-disk append order (never re-sorted by `at`, whose whole-second resolution makes ties common). An empty log emits nothing at all — zero lines, not `[]`. Under `--json`, stdout is pure NDJSON: the rotation notice and the follow-exit note go to stderr like every other diagnostic.

## `kt agent remove <name> [--delete | --retain] [--force]`

Remove an Agent Instance from the Fleet.

```bash
kt agent remove demo
kt agent remove demo --delete
kt agent remove my-agent --force
```

- `--retain` — keep the Agent Home directory on disk (the default).
- `--delete` — delete the Agent Home directory as well. This includes the `filesystem` Memory Backing contents under `<home>/memory`: `--delete` removes **everything** in the Agent Home, not just registration metadata. (Detaching, by contrast, never deletes contents.)
- `--force` — remove even if the instance is running. Required for a running instance.

`--delete` and `--retain` are mutually exclusive; when neither is given, the safe default is to retain.

## `kt agent memory attach <name> --kind <kind>`

Attach a Memory Backing to an Agent Instance. Two kinds exist, and each names its guarantee up front (NFR-7):

- **`filesystem`** — an engine-managed directory inside the instance's Agent Home whose contents persist under your control and survive stop/start cycles and engine restarts byte-identically.
- **`native`** — an explicit delegation marker: memory semantics belong to the agent's own native mechanism; Ktesio guarantees only that the Agent Home itself persists. Attaching it creates no directory, and — because a `native` **backing** delivers nothing — the engine performs no config delivery at start for it, `HERMES_HOME`-style override included; the agent's own mechanism locates its home. (This is about the backing kind, not the adapter: a `hermes` instance attached a `filesystem` backing DOES receive `HERMES_HOME` — see the attach section below.)

```bash
kt agent memory attach demo --kind filesystem
kt agent memory attach demo --kind native
kt agent memory attach demo --kind filesystem --json
```

Arguments:

- `<name>` — the Agent Instance to attach the backing to.
- `--kind <kind>` — the backing kind: `filesystem` or `native`. The full vocabulary grows without a breaking change.
- `--json` — emit the attachment as a machine-readable JSON document (below) instead of the human confirmation.

The human confirmation names the kind and prints one boundary sentence stating exactly what is guaranteed versus delegated, then the managed directory path alone on the final stdout line (scripts can read the last line); diagnostics go to stderr. For `native`, that path is the computed location only — nothing was created there.

### `memory attach --json`

`--json` writes a single versioned document to stdout and nothing else there (diagnostics stay on stderr). The document carries the backing kind and guarantee level in their typed snake_case wire strings (frozen verbatim at the Adapter Contract v1 freeze), the engine-computed managed directory, and the delivery fact — whether the injected path will actually reach the agent. `declared` reads `true` when the attached backing is `filesystem` AND the adapter's declared config mapping targets the reserved `memory.dir` key (the builtin `hermes` does — its mapping delivers `memory.dir` as `HERMES_HOME` — so a hermes instance with a filesystem backing reads `"declared": true`). It reads `false` when the mapping targets nothing, and also when the attached backing is `native` — for a `native` backing the `false` means "no delivery is offered", not "the adapter declined": nothing is delivered at start, so there is no delivery to declare.

```json
{
  "schema_version": 1,
  "instance": "demo",
  "kind": "filesystem",
  "guarantee": "managed_dir_byte_durable",
  "dir": "/home/you/.local/share/kt/agents/demo/memory",
  "declared": true
}
```

A `native` attach reads `"kind": "native"`, `"guarantee": "home_persistence_only"`, and `"declared": false` (no delivery is offered for a `native` backing — see above; this says nothing about the adapter, and a `hermes` instance with a `filesystem` backing reads `true`). The `schema_version` is the memory document family's own (currently `1`); it is a compatibility surface — any key change is announced, never silent.

For `filesystem`, the engine creates and owns the managed directory (it prints the exact path), never touches its contents — they are yours — and hands the path to the adapter at every start through the reserved `memory.dir` config key. Whether the agent actually receives it depends on the adapter declaring a config mapping for that key; if it declares none, Ktesio says so on stderr at start and the directory guarantee holds regardless. For `native`, nothing is injected at start — the agent's memory mechanism is entirely its own.

`memory.dir` is an engine-reserved delivery key, never operator configuration — do not set it yourself. Any hand-set value is stripped from the operator layers at resolve time: the engine removes it when resolving what applies at start, so it can be delivered only by the engine itself (when a `filesystem` backing is attached) and never lands in the persisted start snapshot as applied configuration.

Attach and detach require the instance to be in a terminal state (`registered`, `stopped`, or `failed`) — a Memory Backing cannot be hot-swapped under a live agent, and there is no `--force` escape. Re-attaching the same kind is an idempotent success; attaching a different kind over an existing one is rejected until you detach.

### Portability: moving an Agent Home to another machine

Both backing kinds travel with the Agent Home — the attachment lives in the state database inside the state dir, and `filesystem` contents are plain files under `<home>/memory`. To move an instance to another machine:

1. **Stop first.** Attach/detach aside, copy from a quiescent state: bring the instance to a terminal state (`registered`, `stopped`, or `failed`).
2. **Copy the whole state dir**, preserving the relative layout (`state.db`, `secrets.toml` if present, and everything under `agents/`). Do not reorganize or rename anything inside it.
3. **Open at the same relative location on the target machine** (or set `KTESIO_STATE_DIR` to the copied root).

The `filesystem` memory tree arrives byte-identical and the instance runs with memory intact; the delegation recorded for a `native` backing travels too. One caveat: a state database written by a NEWER Ktesio version refuses to open on an older one with a clear schema-version error — upgrade before copying forward.

## `kt agent memory detach <name>`

Detach an Agent Instance's Memory Backing.

```bash
kt agent memory detach demo
kt agent memory detach demo --json
```

Arguments:

- `<name>` — the Agent Instance to detach the backing from.
- `--json` — emit the detachment as a machine-readable JSON document instead of the human confirmation.

Detach is metadata-only: the attachment is removed, but the managed directory **and its contents remain on disk** — your data is never silently deleted, and re-attaching later re-adopts the existing contents. The same terminal-state requirement applies as `attach`.

With `--json`, stdout carries the versioned confirmation document and nothing else. It is intentionally minimal — the versioned proof that nothing is attached anymore; it carries no path (Ktesio never constructs the managed-directory name itself, and after a detach the engine reports no attachment to quote):

```json
{
  "schema_version": 1,
  "instance": "demo"
}
```

## Memory guarantees at a glance

| Kind | Ktesio guarantees | Delegated to the agent |
| --- | --- | --- |
| `filesystem` | The managed directory exists, its contents survive restarts byte-identically, and it travels with the Agent Home | What the agent does with the delivered path |
| `native` | Only that the Agent Home persists | All memory semantics (storage, retrieval, lifecycle) |

## `kt agent config set <name> <key> <value>`

Set one config key on the Agent Instance layer. Validated at write time.

```bash
kt agent config set demo model gpt-4
kt agent config set demo budget.tokens.cumulative 500000
kt agent config set demo agent.api_key secret:OPENAI_KEY
```

A known unified key or an `agent.*` pass-through key is accepted and persisted; an unknown key **outside** `agent.*` is rejected before anything is written, with the nearest valid key suggested. The value is stored verbatim — a `secret:NAME` reference is stored as-is and resolved + masked at start/read (never resolved or echoed by this write). Setting config on a **running** instance is allowed and never touches the live process: the change takes effect on the next start (budget/cost keys are an exception — they are re-read on each usage ingestion and apply immediately).

The write is **atomic**: the updated `config.toml` is written to a temporary file in the Agent Home and renamed into place, so a crash mid-write always leaves either the complete old or the complete new bytes — never a truncated file.

The `<value>` position accepts a leading-dash value (e.g. `-x`, `--model-x`) **literally** — no `--` separator is required (the separator still works for callers who prefer it).

**Warning (warn-only).** Setting a `secret:NAME` value on a key the agent's adapter maps to a **flag** target succeeds, but prints a warning to stderr: the resolved cleartext would ride the agent's command line, where argv is readable by other local users (`ps`, `/proc/<pid>/cmdline`). Prefer an **env** or **file** target for secret-carrying keys (see Architecture — Secrets); at **start** the same fact is reported as a one-line engine diagnostic.

## `kt agent config get <name> [<key>] [--json] [--reveal]`

Read the effective (resolved) config with per-value provenance.

```bash
kt agent config get demo
kt agent config get demo model
kt agent config get demo --json
kt agent config get demo --reveal
```

- `<key>` — optional; omitted prints the whole effective config. With a key, prints just that value. A key with no effective value is rejected on stderr with a non-zero exit. When that key has no value of its own but **is a table prefix** of effective keys (e.g. `budget` while `budget.tokens.cumulative` is set), the diagnostic names the effective child leaves (the first eight, then an ellipsis) and suggests the `get` for one of them; a key that is neither a value nor a prefix gets the plain not-found diagnostic. The same diagnostic applies in `--json` mode (stderr carries it; stdout stays clean of a partial document).
- `--json` — emit a versioned document whose per-leaf objects carry `{ key, value, source, unvalidated }`.
- `--reveal` — the sole explicit un-mask for `secret:` values. It re-resolves secrets **live** (environment, then the secrets file) at read time, so a revealed value may differ from what a running instance resolved at its start. It never un-masks the persisted snapshot, logs, or events.

Values resolve across four layers — engine defaults &lt; agent-kind defaults &lt; instance config &lt; invocation overrides — and each value names its winning source layer (a **Source** column, or a `source` field with `--json`).

## Unified Config Keys

Set these with `kt agent config set <name> <key> <value>`.

| Key | Value | Meaning |
|-----|-------|---------|
| `model` | string | Model name or identifier passed to the agent through its declared config mapping (if any; the builtin `mock` maps it to the `MODEL` env var, `hermes` does not map it) |
| `budget.tokens.per_run` | integer | Per-run token ceiling |
| `budget.tokens.cumulative` | integer | Cumulative token ceiling |
| `budget.breach_action` | `pause` \| `stop` \| `warn` | Action on any budget/cap breach (default `pause`). `warn` records the breach event only — it performs no lifecycle transition |
| `cost.rate.input` | dollar string (e.g. `3.00`) | Input price per 1M tokens |
| `cost.rate.output` | dollar string | Output price per 1M tokens |
| `budget.dollars.per_run` | dollar string | Per-run dollar Cost Cap (needs a Rate to enforce) |
| `budget.dollars.cumulative` | dollar string | Cumulative dollar Cost Cap (needs a Rate to enforce) |
| `metering.upstream_base_url` | URL | Real upstream endpoint for an `engine-observed` instance |
| `agent.<key>` | any | Pass-through namespace delivered verbatim to the agent (bypasses known-key validation) |

Two additional known keys are **engine-reserved and never operator-set**: `metering.base_url` (the loopback proxy endpoint the engine injects at start for an `engine-observed` instance) and `memory.dir` (the managed Memory Backing directory the engine injects at start for a `filesystem` backing). Hand-set values are stripped from the operator layers at resolve time, so these can only ever be delivered by the engine itself.

Both Rate directions are required for dollars to be derived; with no Rate, dollar features are inert (no fabricated `$0.00`). Dollars are integer micro-dollars internally and always labeled estimates. A config value of the form `secret:NAME` (on any key) is a secret reference — resolved at start, masked everywhere Ktesio displays it.

### Budget breaches and the pause action

When a token ceiling or dollar Cost Cap is crossed, the recorded breach names its scope (`per_run`/`cumulative`), the limit, and the observed value. With the default `breach_action: pause`, the enforcement pause on a **best-effort**-pause adapter carries the budget cause (`BudgetExceeded`, naming the token or dollar scope that crossed first) — the same honest cause surfaces whether the pause was operator-initiated or enforcement-initiated. When both a token ceiling and a dollar cap are set and both breach in the same enforcement pass, the token breach wins the pause (the dollar breach is still recorded); enforcement evaluates token ceilings before dollar caps. A `pause` breach on an already-paused instance does not abort the run — the breach is recorded and a diagnostic notes the pause could not be honored.

## Global Behavior

### Exit codes

Every `kt` command returns one of these numeric exit codes, so failures can be branched on in a script without parsing stderr:

| Code | Meaning | Typical causes |
|------|---------|----------------|
| `0` | Success | The command completed; `--help` and `--version` also exit `0` |
| `1` | General error | An internal or unexpected failure: filesystem/IO, state store, config load, launch failure, an invalid or unreadable adapter manifest, an adapter manifest whose `contract_version` major does not match this engine's (the FR-30 negotiation gate — the error names both versions and the rule), an adapter declaring no capabilities or no metering source, a failed self-update |
| `2` | Usage error | An invalid invocation: an unknown flag or a missing/invalid argument, an invalid instance name, an unknown adapter kind, an unknown config key, or a duplicate instance name |
| `3` | Not found | The named Agent Instance does not exist, or no `adapter.toml` was found at the given `--manifest` path |
| `4` | Invalid state | The instance is not in a state that permits the operation: not running, an invalid lifecycle transition, removing a running instance without `--force`, attaching/detaching a Memory Backing on a non-terminal instance, attaching a different kind than the one already attached, or a stop that could not be confirmed |
| `5` | Unsupported capability | Either the agent's Capability Declaration forbids the operation on this OS (e.g. `pause` or `send` declared `unsupported`), or the operation needs a live interaction channel this session cannot reach — `kt agent send` to an instance adopted from an earlier session has no recoverable stdin pipe. `kt agent resume` of a `paused` instance whose CURRENT pause declaration reads `unsupported` lands here too (the dedicated resume diagnostic names the state + the `stop`/`start` recovery) |
| `6` | Timed out | A bounded operation exceeded its deadline (e.g. `send` when the agent is not draining its input) |

A script branches on the code directly — no stderr parsing:

```bash
kt agent show my-agent --json > status.json
code=$?
if [ $code -eq 3 ]; then
  kt agent register my-agent --kind mock
elif [ $code -eq 4 ]; then
  kt agent start my-agent
elif [ $code -ne 0 ]; then
  echo "unexpected failure (exit $code)" >&2
  exit 1
fi
```

Every command that writes machine-readable output to stdout keeps that output pure, so `kt agent logs my-agent --json | head -5` is safe: a consumer that stops reading ends the command cleanly with `0` rather than an I/O failure.

These codes are a **v1 compatibility surface**, governed by the same deprecation policy as the `--json` schemas: a breaking change is announced in the release notes, carries at least a one-minor notice window, and is removed only at a major version. Compatibility tests assert each documented condition returns its documented code, so an unannounced change fails CI.

### Environment variables

Two environment variables control `kt`'s own behavior (they are never injected into the agent's environment):

| Variable | Meaning |
|----------|---------|
| `KTESIO_STATE_DIR` | Overrides the state directory location. Precedence: explicit CLI path (where supported) → `KTESIO_STATE_DIR` → the platform data dir. A relative path is refused with a diagnostic. |
| `KTESIO_NO_UPDATE_CHECK` | Set to `1` to disable the GitHub Release update check. The check is also skipped automatically when `CI=true`. |

### Update checks

When a `kt` subcommand runs, Ktesio checks whether a newer GitHub Release is available, using an hourly cache. If an update is available, it prints a short stderr notice asking you to run `kt self-update`. Machine-readable JSON on stdout is unaffected. Disable checks with `KTESIO_NO_UPDATE_CHECK=1` (also skipped when `CI=true`).

### `kt self-update`

Update the `kt` binary itself, preserving the current install channel (Homebrew, Cargo, or a manual release binary). Running instances already hold their own process image: they keep executing the binary they were started with until their next start, so a self-update does not interrupt them — restart agents to pick up the new version.

```bash
kt self-update
```
