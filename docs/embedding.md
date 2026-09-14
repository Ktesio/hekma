---
title: Embedding the Engine
description: Drive the Ktesio engine as a Rust library — the facade surface, the event bus, and the hermetic quickstart example.
---

# Embedding the Engine

Ktesio is a library first and a CLI second: the `kt` binary is itself just an
embedder that drives the engine's public Rust facade. A hosting platform can do
the same — register agents, configure them, enforce token and dollar budgets,
subscribe to lifecycle events, and supervise processes, with no CLI, no TTY, and
no prompts. This page is the quickstart; the flow-level guarantees are covered
in [Architecture](architecture.md).

## Adding the dependency

Until the crates are published to crates.io, depend on this repository pinned
to an exact revision:

```toml
[dependencies]
ktesio-engine = "0.1"
```

Pin a **full-length commit SHA**, never a branch: a pinned `rev` makes your
build reproducible and upgrades deliberate (the engine's public surface is
CI-guarded against breaking changes between freezes, but a moving target is
still a moving target).

**Which SHA to pin:** the embedding surface (the `blocking()` facade plus the
event bus) stabilized at commit `8a8b328` — story 7-3's freeze, now guarded by
the CI semver gate against that exact baseline. Pin **any commit at or after
it**; the newest `main` commit you are comfortable with is the right default.
To fetch a current full SHA to pin:

```bash
git rev-parse origin/main
```

(paste the full 40-character output as your `rev`). After the publish executes
(see [the release runbook](release-process.md#publishing-the-engine-crates)),
switch to the versioned crates.io form — the facade you compile against does
not change:

```toml
[dependencies]
ktesio-engine = "0.1"
```

Two things to know before depending: the engine's minimum supported Rust is
**1.96.1** (the workspace `rust-version`; any toolchain at or above it
builds), and Ktesio is **source-available**, not open source — the
[Ktesio Noncommercial-Attribution License 1.0.0](../LICENSE) keeps
noncommercial use free and requires the author's written approval for
commercial use.

Only `ktesio-engine` is needed. It is a normal Rust dependency — the engine
never touches your TTY, never reads stdin, holds no global process state, and
several engines can live in one process side by side, each rooted at its own
state directory.

## The facade surface

Open an engine with `Engine::open(base)` and either call its `async` methods on
your own runtime or take `engine.blocking()` for a synchronous view — the same
surface `kt` uses. The capabilities you will reach for first:

| Facade | Purpose |
|--------|---------|
| `Engine::open(base)` | Open (or create) an engine rooted at a state directory; `None` uses the OS default. |
| `register` / `register_with_adapter` | Register an instance under a built-in adapter kind or a manifest (`adapter.toml`) directory. |
| `set_config` / `effective_config` | Write and read the unified configuration (budgets, rates, model keys) with per-leaf provenance. |
| `start` / `stop` / `pause` / `resume` | Drive the lifecycle; `stop` takes a graceful-shutdown window and kills the whole process group. |
| `subscribe` / `Blocking::subscribe` | Receive the event stream (below). |
| `resync_events` / `Blocking::resync_events` | Backfill the committed events a subscriber missed (below). |
| `with_diagnostics` / `Blocking::with_diagnostics` | Route the engine's two stderr diagnostics into your own writer (below). |
| `fleet` / `instance_status` | Read per-instance rows — state, usage, budget remaining, metering source — what `kt agent list` renders. |
| `budget_breach_events` / `transition_events` / `read_agent_log` | Query the durable records directly (a `subscribe` sees only later commits; the query APIs reach the past). |
| `send_input` / `attach_memory` / `detach_memory` | Interaction and memory wiring, where the adapter declares support. |

Every method returns a typed `Result` — the engine reports partial failures
with a reason and a remediation instead of panicking.

## The event bus

`engine.subscribe()` hands you a receiver over a bounded, ordered event bus.
Four rules cover the whole contract:

1. **Subscribe before it happens.** A receiver observes only events committed
   after it subscribed, in commit order, per-instance FIFO. Anything earlier is
   readable through the query APIs.
2. **Payloads are versioned structs.** Every event carries a `schema_version`
   and is one of: a lifecycle transition, a budget breach, or a committed
   usage measurement — the exact wire shapes `kt --json` documents.
3. **A slow subscriber never stalls supervision.** The bus is bounded; if you
   fall more than its capacity behind, your next receive observes `Lagged` and
   resynchronizes at the tail — the dropped events stay readable in the
   durable logs. Drain promptly or poll `try_recv` on your own cadence.
4. **Delivery is at-most-once in the crash window — and recoverable.** Events
   are appended to the durable record first, then published; a process crash
   between the two loses that one event from the *stream*. The durable record
   stays complete, and `resync_events` (below) heals the window in one call.
   Treat the stream as a live notification surface, with the resync as your
   gap remedy.

## Healing the crash window: `resync_events`

Rule 4 above leaves a gap: events committed while nobody was subscribed (or
while your subscriber's process was down) never reach the stream. The engine
ships the remedy — one call reads the instance's COMMITTED event records (the
same truth the query APIs serve: transitions, breaches, ledger rows) and
returns them as the exact event payloads the live bus delivers:

```rust
use ktesio_engine::{Blocking, ResyncCursor};

// 1. Subscribe live FIRST — the gap-free order. Every commit from this
//    moment reaches the stream, whatever the backfill does afterwards.
let mut events = facade.subscribe();

// 2. …then backfill everything committed before it. The backfill overlaps
//    the live stream, so drop the overlap yourself: skip each family's
//    first `cursor.<family>` backfilled records against your own position
//    bookkeeping (or key on the records' own identity fields).
let batch = facade.resync_events("my-agent", ResyncCursor::START)?;
```

The contract, in five rules:

1. **Committed truth only.** A read-side helper over the same durable records
   the query APIs return — the bus is untouched. An event whose append failed
   never appears in a backfill either.
2. **Ordering is exact per family; the combined order is your choice, with
   named tradeoffs.** Transitions come in `instance.log` order, breaches in
   `breaches.log` order, usage updates in ledger commit order — the same
   orders the stream guarantees. Across families the batch is family-major
   (transitions, then breaches, then usage): the durable record carries no
   global cross-family sequence, and the engine does not fabricate one.
   * **Subscribe first, then resync** is the *gap-free* order: every commit
     after the subscribe reaches the live stream by construction, and the
     backfill merely overlaps it — you see duplicates, never gaps, and you
     dedup the overlap per family with your own cursor bookkeeping (the
     sample above). **Gap-sensitive hosts should use this order.**
   * **Resync first, then subscribe** has no seam duplicates — the backfill
     is precisely the prefix and the live stream precisely the suffix — but
     it is NOT gap-free: a commit landing between the `resync_events` call
     returning and your `subscribe()` is delivered by *neither*. Use it only
     across a quiescent agent (stopped/paused, no traffic) or accept the
     window consciously.
3. **Cursor-based and idempotent.** The returned batch carries a
   `ResyncCursor`; pass it to the next call and the already-consumed prefix is
   skipped, so re-running a resync never re-delivers. Persist the cursor
   across your own restarts if you like (it serializes snake_case). A cursor
   below a family's committed count backfills the suffix; a cursor ABOVE one
   (the log was truncated or the ledger rotated away) is a typed error naming
   the family and both counts — never a silent clamp, which would re-deliver
   from zero or silently skip records. Resetting to `ResyncCursor::START` is
   your deliberate choice.
4. **Crash-recovery read posture, with the skip surfaced.** The helper is
   called most often right after the crash it heals — and that crash can tear
   the log's trailing append. One unparseable trailing line per log is
   skipped — and only when it carries the torn-append signature (the file
   does not end with a newline, the mark of a write cut mid-append) — and the
   skip is never silent: the batch's `torn_tail_skipped` flag is set,
   because the engine's next append fuses onto the torn fragment and the
   skipped line may be carrying a good post-crash record you did not
   receive. Every other malformed line is a typed error worth investigating:
   an interior line, a newline-terminated trailing line (corruption, or
   exactly that fused line), or a trailing line that is valid JSON of the
   wrong shape (the wrong file).
5. **Per-instance — and per-family honest.** `name` scopes the read; a
   Fleet-wide backfill is your loop over instances. An unregistered name
   fails `NotFound` rather than reading as a silent empty backfill. And the
   batch is per-family exact, not a cross-family snapshot: the three family
   reads (transitions, breaches, ledger) are sequential, so a commit landing
   between them skews the families relative to each other. Each family's
   slice stays exact and cursor-lossless — the next call picks up exactly the
   stragglers — but if you need one coherent point-in-time view, make the
   agent quiescent (stop/pause) across the read.

## The diagnostic sink

The engine writes exactly two operational diagnostics — a DC-10 notice when an
attached filesystem memory backing cannot be delivered to the agent, and an
enforcement breadcrumb when a budget-breach action (pause/stop) could not be
honored. With no sink installed they go to **stderr**, byte-for-byte as they
always have; nothing is written to stdout, ever. A host that owns its stderr
(for a daemon, a GUI, a log pipeline) can route both into its own writer:

```rust
use ktesio_engine::{DiagnosticSink, Engine};
use std::sync::{Arc, Mutex};

// Any std::io::Write works — a file, a channel, an in-memory buffer.
let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(std::io::sink())));

// Either install at open (the airtight form — in place before any
// supervision work, including orphan adoption and the crash reaper):
let engine = Engine::open_with_diagnostics(Some(state_dir), sink.clone())?;

// …or install/rotate on an already-open engine:
engine.blocking().with_diagnostics(sink);
```

The contract, in five rules:

1. **Exact texts.** Each diagnostic arrives as one full line — the same
   `[ktesio] `-prefixed text stderr would have received, `\n`-terminated. A
   sink that mirrors its input reproduces the default output byte-for-byte.
2. **Opt-in, additive — and one-way.** Installing nothing changes nothing;
   the default path is pinned by CI as byte-identical to the historical
   engine. Installing REPLACES whatever sink is installed (that is how you
   rotate writers mid-flight; the outgoing writer is flushed before the swap
   so its buffered bytes are not lost), but there is **no uninstall** back to
   the stderr default — a host that wants the default back re-opens the
   engine, or installs its own writer that emits to the process's stderr.
3. **Thread-safe by construction.** The sink is an `Arc<Mutex<Box<dyn Write +
   Send>>>`, so the engine can emit from any supervision thread and you can
   clone the `Arc` to share one sink across engines. Diagnostics are rare —
   the lock is never a hot path.
4. **Never re-enter the engine from the writer.** Emissions happen while the
   engine's supervisor lock is held; a `write` that calls back into the engine
   would deadlock. Forwarding the line to your own channel or lock is fine.
5. **Best-effort, like the diagnostics themselves.** A write error is
   swallowed — and so is a PANIC in your writer's `write`/`flush` (the engine
   catches it: a host bug must never unwind through supervision, and the same
   sink keeps receiving later diagnostics) — so supervision never fails or
   blocks on a broken sink. No diagnostic is ever the durable record of
   anything — the transition, breach, and usage logs remain the authoritative
   copies, readable via the query APIs.

## The quickstart example

A complete, runnable host lives at
[`crates/ktesio-engine/examples/embedding-quickstart.rs`](https://github.com/iMagdy/ktesio/blob/main/crates/ktesio-engine/examples/embedding-quickstart.rs).
Run it from the repository root:

```bash
cargo run -p ktesio-engine --example embedding-quickstart
```

It is deliberately dependency-free — no `kt`, no test fixtures, no helper
crates, not even `tempfile` — so it shows exactly what a host depends on. The
seven legs, matching the numbered steps in the file:

1. **Open**: a scratch state root (a per-process directory under the OS temp
   dir, cleaned up best-effort at the end) and `Engine::open(Some(root))`
   with the blocking facade.
2. **Register** a manifest adapter: the example writes a minimal
   `contract_version = "1.0.0"` `adapter.toml` (see
   [the manifest reference](manifest.md) and the
   [Adapter Contract](adapter-contract.md)) whose `[lifecycle.start]` command
   re-executes the example binary itself as a stand-in agent — a real host
   points that field at its own agent executable. (The engine's built-in
   kinds, such as `hermes`, register with `AdapterRef::Native` instead.)
3. **Configure** one budget key — `budget.tokens.cumulative = 100000` — and
   assert the `effective_config` read-back (value and provenance layer).
4. **Subscribe** before starting, so the transition events are observable.
5. **Start**: the engine spawns the manifest's `[lifecycle.start]` command
   and reaches `running` before the call returns.
6. **Observe**: drain the committed events (`Lagged` resyncs at the tail and
   keeps draining), then assert `instance_status` is `running` and the
   `fleet` row carries the configured budget ceiling.
7. **Stop** with a five-second graceful window, then remove the scratch root
   (best-effort).

CI compiles the example on all three OS legs and **runs** it hermetically on
ubuntu on every push — the quickstart cannot silently rot.

## The boundary is the guarantee

What a host can reach is enforced by the compiler, not by convention: the
engine's private modules are Rust-private, so **if you cannot name it, you
cannot call it** — a host compiles against exactly the same public API `kt`
does. Four instruments keep that statement honest:

- **The dependency-shape gate** — CI's `boundary` job allowlists the internal
  edges of the shipped CLI's graph (`cargo tree`), so `kt` cannot quietly grow
  a dependency on anything but the engine, the adapter-contract types, and the
  built-in hermes adapter; a future internal crate fails the gate
  automatically ([the workflow](https://github.com/iMagdy/ktesio/blob/main/.github/workflows/ci.yml)).
- **The facade audits** — the embed-clean suites prove `kt` consumes only the
  blocking facade (no async APIs, no runtime of its own) and that every async
  method has a blocking counterpart, with no TTY, prompt, or global-state
  escapes.
- **The library-alone flow proof** — a host test drives the full
  register → configure → cap → start → breach → pause → stop journey through
  the facade alone and shares its assertions with the CLI suite, proving the
  library path and the CLI path behave identically
  ([the host test](https://github.com/iMagdy/ktesio/blob/main/crates/ktesio-engine/tests/uj3_library_host.rs)).
- **The dependency-audit checkpoint (story 11-6, AI-48)** — when reviewing or
  bumping HTTP-stack dependencies (`hyper`/`hyper-util`/`reqwest`-family),
  check the tracing exposure: `hyper-util` links `tracing`, and its
  connection-pool events would carry the upstream host:port and timing IF a
  global `tracing-subscriber` were ever installed. The engine ships NO
  subscriber (events are no-ops) and hyper's own tracing feature is OFF, so
  today nothing is emitted; the exposure never carries the `Authorization`
  header, body, or key. The checkpoint: any future story that installs a
  global DEBUG/TRACE subscriber must re-audit what the HTTP stack logs at
  that level before it ships (a model-call endpoint is operator-sensitive
  context, even without credentials).
- **The semver gate** — CI diffs both public crates' surfaces against their
  freeze baselines, so a breaking change cannot land unnoticed.

## Availability

**Published**: `ktesio-engine` 0.1, `ktesio-adapter-api` 0.1, and
`ktesio-adapters-hermes` 0.1 are on [crates.io](https://crates.io) (first
release v0.7.0, 2026-09-09). Depend on `ktesio-engine = "0.1"` — no git
dependency needed. The crates are source-available (noncommercial free;
commercial use requires the author's written approval — see the license).

The publish runbook's historical HELD state is retained in
[the release process](release-process.md) decision log.
