---
title: 'The store writer and Run-scale storage'
type: 'feature'
created: '2026-10-31'
status: 'in-review'
review_loop_iteration: 1
baseline_commit: 'add88aede32d8839b35cf522f56ef59ac70cf2a4'
context:
  - '_bmad-output/planning-artifacts/epics.md (Epic 21, Story 21.1 + readiness bullets, build order)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-27; AD-7 "`block` before the shared proxy"; AD-6, AD-17, AD-18, AD-25 amendments)'
  - '_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md (NFR-4, FR-10, FR-21, §4.14)'
  - '_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md (Story 21-1 findings, D1, D2)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Every state.db write runs synchronously on the Registry's one connection under the coarse `Registry → Supervisor` mutexes, the AD-7 pipeline is spread over that connection plus three file appends (`instance.log`, `breaches.log`, `outbox.jsonl`) that cannot share a transaction, the breach latch lives in Supervisor memory, usage totals re-scan `usage_events` on every evaluation, and observed usage collides with itself after an adoption (the per-listener sequence restarts under the resumed run id and is dropped as a replay). None of that holds at Run scale (NFR-4), and Epics 21–24 all build on one writer that owns storage.

**Approach:** One store writer thread owns its own SQLite connection and is the only state.db writer in the engine process; the Registry's connection becomes read-only. Every write is a message the writer runs in a bounded batch transaction (a SAVEPOINT per message) and acks after commit. The writer runs the AD-7 pipeline: usage commit, per-instance and per-Run rollups, budget evaluation against durable latches, breach records, and the outbox row commit in one transaction; it raises the `block` fence after commit and returns process Breach Actions to the caller, which applies them before releasing its locks. Transition and breach records move into `transition_events` / `breach_events` (the log files become best-effort mirrors), the outbox moves into state.db, dedup keys gain a launch generation, and the upgrade imports the old files once. A new crate-internal port carries the new reads and writes with keyset pagination.

## Tasks & Acceptance

**Execution:**
- [x] `store/writer.rs` — `StoreWriter`: a named thread owning a read-write `SqliteStore`; `call` (acked: send, wait ≤ 1 s, then abandon-or-wait — see Design Notes) and `submit` (unacked); a batch loop (acked jobs flush at once with whatever is already queued; unacked jobs coalesce ≤ 250 ms; ≤ 512 events per batch), `BEGIN IMMEDIATE` per batch, `SAVEPOINT` per job, post-commit hooks, a whole-batch commit failure answered to every taken job with a typed retryable error. Joined on drop.
- [x] `store/sqlite.rs` — schema v12 (one crash-atomic step): `transition_events`, `breach_events`, `run_latches`, `outbox` + `outbox_meta`, `usage_rollup_instance`, `usage_rollup_run`, `agent_instances.launch_generation`, and the `usage_events` rebuild (nullable `instance_id`, `call_id`, `launch_generation`; new dedup indexes). The step imports the legacy files once (per-instance `instance.log` + `breaches.log`, the outbox journal + ack), seeds the outbox sequence above the journal's high water, seeds `run_latches` for each live instance's current run, and backfills the rollups. Nested `in_immediate_txn` uses a SAVEPOINT when a transaction is already open. A `query_only` reader open.
- [x] `ports/run_store.rs` — the new crate-internal port (`RunStore`): batch-scoped writes the writer drives (usage with generation and rollups, transition and breach rows, latches, outbox append with retention, ack) and keyset reads (`Page<T>` + next cursor) for outbox, transitions and breaches; rollup reads. `StateStore` stays frozen (its totals reads now serve from the rollups).
- [x] `domain/registry.rs` — every write method routes through the writer; reads stay on the read-only connection; legacy file import wiring at open; test fault seams route through the writer.
- [x] `domain/supervisor/` — transitions record through one writer job (state write + `transition_events` + outbox); the 16-3 compensation goes away; `instance.log` is mirrored after commit. The usage drains submit one pipeline job per event; the job's resolved inputs (budget, Rate, cap, actions, the instance's wire fence) travel in the message; the Supervisor publishes the committed events in commit order, mirrors `breaches.log`, and applies the returned pause/stop decisions. `Supervised::breached_scopes` and `fence_late_block` go; adoption rebuilds the fence from `breach_events`; the Run-start fence reads totals from the rollups.
- [x] Launch generation — bumped in the writer at spawn, Restart Policy relaunch and adoption; carried on `Supervised`; stamped on every usage row.
- [x] `domain/outbox.rs`, `domain/resync.rs`, `engine.rs` — export and ack read and write state.db (same cursor rules, refusal sentences, retention semantics); `resync_events`, `transition_events` and `budget_breach_events` read the tables.
- [x] Benchmark harness — `store::writer` bench tests: a gated small seed in the normal suite and an `#[ignore]` large seed sized by env, measuring batch flush, single acked write latency and breach-to-action latency (Design Notes, NFR-4 pass criterion).
- [x] Docs, release notes, deferred work, sprint status (Design Notes, Code Map).

**Acceptance Criteria:**
- Given an engine, when any registry, lifecycle, spawn-record, memory-backing or usage write happens, then it is committed by the store writer thread, and a write attempted on the Registry's connection fails with SQLite's read-only error.
- Given a usage event that crosses a token ceiling with `pause`, when the drain commits it, then one transaction holds the usage row, both rollups, the breach row, the latch row and the usage + breach outbox rows; the bus shows usage → breach → transition; and the instance is paused before the drain returns.
- Given a breach already latched for a Run under action A, when a later event re-crosses the same (dimension, scope) under A, then nothing is recorded; when the action is changed to B ≠ A, then the next crossing records one breach under B and executes B — live and after an adoption alike.
- Given a `block` breach committed by the writer, then the fence is raised before the caller gets the ack, and after an engine restart and adoption the fence is up again from `breach_events`.
- Given an adopted `engine-observed` instance whose listener's sequence restarts at 0 under the resumed run id, when it reports usage, then every event is inserted (no `DuplicateReplay`) and the totals count them.
- Given a pre-v12 state dir with transitions, breaches, an outbox journal of N entries and an ack at k, when the engine opens, then `resync_events`, `transition_events` and `budget_breach_events` return the same records in the same order, `export_events` from k returns entries k+1..N with their seqs, the next event gets seq N+1, a breach recorded before the upgrade for a live instance's current run does not re-fire, and the journal is renamed `outbox.jsonl.migrated`.
- Given a writer blocked on another process's write lock for more than 1 s, when a caller writes, then it gets `StoreError::Busy` within the bound and the write is not applied.
- Given one message in a batch failing its guard, then only that caller gets the error and the other messages commit.
- Given the bench at its gated seed, then the unacked batch flush is ≤ 250 ms, the single acked write p99 is ≤ 50 ms, and usage-commit-to-applied-pause is ≤ 1 s.
- Given handoff: fmt, clippy `-D warnings`, the workspace tests, `check_docs.py` and `test_automation.py` pass; coverage ≥ 95%; AI-55 two-pass review done.

</frozen-after-approval>

## Design Notes

- **The writer is a thread, not a tokio task.** The store is rusqlite (sync); a dedicated OS thread keeps blocking SQLite off the async workers (AD-13) and lets the sync Registry wait on a std channel. It takes no engine mutex and sits outside the lock order (AD-27, D2). The trait stays sync behind the thread; a later async backend wraps itself inside the writer.
- **Every write is a closure message.** A job is `FnOnce(&mut WriterCtx) -> Result<R, StoreError>` run on the writer's connection, so the existing `StateStore` write methods (and their guards) run unchanged inside the batch: `in_immediate_txn` opens a SAVEPOINT when a transaction is already open, so a guard re-checks inside the batch and a refused guard rolls back only its own job.
- **Ack bound and "Busy means not applied".** A caller waits ≤ 1 s. A job carries an atomic state Pending → Taken | Abandoned. On timeout the caller CASes Pending → Abandoned and returns `StoreError::Busy { waited_ms: 1000 }` (the existing typed retryable error, so no new public variant); the writer skips an abandoned job. If the writer already took the job, the caller keeps waiting: once taken, the remaining work is bounded local SQLite work inside a held write lock. So a `Busy` always means the write did not land. The batch's `BEGIN IMMEDIATE` keeps the 5 s busy timeout for cross-process contention (only that wait can exceed the bound).
- **Batching.** The writer blocks for one job, then takes what is already queued (acked jobs never wait for company), up to 512 events (a usage job weighs 1, others 1). If every job taken so far is unacked, it waits for more up to 250 ms from the first job or until an acked job arrives. Production writes in 21-1 are all acked; the unacked path is the seam the shared proxy's per-call records use (23.1) and what the bench measures. A failed COMMIT rolls back and every taken job gets a typed error (`Busy` for SQLITE_BUSY, else `Backend`), and its post-commit hooks never run. Drained usage stays one acked job per event (the drain's park-and-retry is per event).
- **The pipeline in the writer.** A usage job carries the event, the launch generation, the resolved token budget and action, the Rate, the cost cap and action, whether the instance is supervised, and its `WireFence` when it has a listener. In one SAVEPOINT the writer inserts the row (dedup), updates both rollups, appends the usage outbox row, reads the totals from the rollups and the latches from `run_latches`, runs `governance::decide`, and for each decision inserts the breach row, the latch row and the breach outbox row. After COMMIT a post-commit hook raises the fence for each `block` decision, before the ack. The ack returns the committed events with their seqs and the decisions; the caller publishes them in commit order, mirrors `breaches.log`, and applies pause/stop through the normal lifecycle (each a transition job). An unsupervised instance commits its usage but fires no decision (today's `apply_breach` decline). The Run-start fence (19-2) stays a read-side evaluation with no commit.
- **Event order changes once.** Usage and every breach it causes commit in one transaction, so the order on the bus and in the outbox becomes usage → token breach → dollar breach → transition (was usage → token breach → pause → dollar breach). Bus order still equals commit order, and the outbox seq is now global across engine processes (one AUTOINCREMENT), closing the two-engines-interleave caveat.
- **Latches are durable and keyed by action.** `run_latches (run_id, dimension, scope, action, latched_at)`, PRIMARY KEY on all four of the first, no foreign key until 21.2. A decision is latched when a row exists for its run, dimension and scope under the currently configured action. This is the adoption rule 15-2 already used, now applied live too: changing the action mid-Run fires once under the new action. It also replaces 19-2's `fence_late_block` (which raised a fence with no durable record, so a restart dropped it): moving to `block` after a breach now records a `block` breach, which raises the fence and survives restart. Re-arm on a raised ceiling stays with 21.6.
- **Transition and breach records (D1).** One writer job does the state write (plain, guarded, or snapshotting the memory backing), the `transition_events` row and the outbox row. `instance.log` and `breaches.log` are appended after commit as best-effort mirrors (a failed mirror is a diagnostic, never a failed transition), so the 16-3 compensation is gone. Rows carry the event's JSON verbatim plus indexed columns; they cascade with the instance row (removing an instance drops its history; a retained Agent Home keeps the mirror files). `resync_events` keeps its per-family count cursor: a position is the row ordinal per instance, in commit order, which equals the old line order because the import preserves file order.
- **The outbox in state.db.** `outbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, event TEXT, bytes)` and `outbox_meta (ack, total_bytes)`. Same cursor rules, refusal sentences, `retained_from`, ack monotonicity and 30 MB cap, now counted as stored event bytes. Eviction runs at append in the same transaction: acked prefix first, then the oldest un-acked, never the last row; an un-acked eviction is returned to the caller, which emits the same diagnostic. `torn_tail_skipped` and `blank_lines_skipped` stay on the wire and are always false/0. An outbox failure now fails the family commit with it (they are one transaction); that replaces "the outbox append never fails the family commit".
- **Upgrade (one crash-atomic step).** The v12 step runs DDL and the import in one `BEGIN IMMEDIATE`: each registered instance's `instance.log` and `breaches.log` (tolerant read; a torn tail or unparseable line is skipped and reported as an open note on the engine's diagnostic surface, the file is left in place as the mirror), the outbox journal entries with their seqs, the ack, the sequence seeded to max(journal tail, ack), latches for each live instance's current run, and the rollups. An unreadable outbox journal or ack refuses the open (today's `OutboxOpenRefused` contract). After commit the journal and ack files are renamed `*.migrated`. A crash mid-step leaves v11 and the files untouched.
- **Contention never skips usage.** A `Busy` refusal does not spend one of the drains' three park attempts (they bound a poisoned row, not contention), so another process holding the write lock parks usage until it commits instead of skipping it.
- **A batch SQLite rolls back on its own** (a full disk, an I/O error) answers every job in it "not applied", and no later job of that batch runs outside a transaction. A panicking post-commit hook is caught; the writer keeps running.
- **Rows another writer inserted are still counted.** The rollups fold every `usage_events` row above a `folded_through` watermark (`usage_events.id` becomes AUTOINCREMENT so ids are never reused below it), on each commit and at each writer open, so usage an older engine inserts after the upgrade is not lost from the totals. The rest of what an older engine writes after the upgrade (its transitions, breaches and journal) is not read, so the release notes say to stop older engines before upgrading.
- **Launch generation.** `agent_instances.launch_generation` is bumped by the writer at spawn, relaunch and adoption, and every usage row carries the generation it was metered under. Self-reported dedup keys on (instance, run, generation, sequence), proxied on (run, call id) once 23.1 sets call ids. The instance id stays in the self-reported key as a guard (legacy run ids are not guaranteed unique across instances in tests). An agent that re-emits a pre-adoption line after an adoption now counts it again: an overcount (AD-8's safe direction); the drain already anchors at end of file on adoption, so the engine itself never re-reads one.
- **Single writer per process until 21.4.** Each engine process (every CLI command, `hekma serve`) runs its own writer; cross-process serialization stays SQLite's `BEGIN IMMEDIATE` plus the 5 s busy timeout, and guarded transactions keep their guards inside the batch. `memory_atomicity.rs` is the regression gate.
- **The port.** `RunStore` is `pub(crate)`: no embedder can supply a store today, and 21.2–21.3 still add Run and Policy tables, so freezing it now buys nothing. It exposes batch-scoped writes the writer drives and keyset reads returning `Page { items, next }`, with a bounded page (1000 default cap). Making it public is a later, additive step.
- **NFR-4 pass criterion for 21-1.** Run-scale numbers (1M Runs, proxy p99) need Runs (21.2) and the shared proxy (23.1); they are measured from 21.5 and 24.4 on. 21-1 gates: an unacked batch flushes within 250 ms of its first job; a single acked write is ≤ 50 ms p99; usage commit to applied pause ≤ 1 s (FR-21's metering interval + 1 s); the existing 7.5 perf budgets do not regress (CI perf-budgets job). Seed shape: M instances (default 25; the large run 1,000) × N usage events (default 10,000; the large run 1,000,000) spread round-robin over one Run each, with one ceiling armed per instance.
- **Engine version.** The schema step is one-way (an older engine refuses a v12 state dir as too new), the event order and the outbox failure contract change, so hekma-engine goes 0.10.0 → 0.11.0, announced in CHANGELOG and RELEASE_NOTES. No public type changes.

## I/O Impact Matrix

| Surface | Before | After |
|---|---|---|
| state.db | v11 | v12 (new tables, `usage_events` rebuilt; one-way) |
| `instance.log`, `breaches.log` | the record | best-effort mirrors, written after commit |
| `outbox.jsonl`, `outbox-ack.json` | the outbox | imported once, renamed `*.migrated` |
| Bus / outbox order on a breach | usage, token breach, pause, dollar breach | usage, token breach, dollar breach, pause |
| Outbox seq across two engines | per-process counters, may interleave | one global sequence |
| Mid-Run Breach Action change | suppressed until the next Run (adoption excepted) | fires once under the new action |
| Concurrent writer wait | up to 5 s, then `Busy { waited_ms: 5000 }` | up to 1 s, then `Busy { waited_ms: 1000 }`, never applied |
| `export_events` flags | torn/blank counts from the journal | always false / 0 |
| hekma-engine | 0.10.0 | 0.11.0 |

## Code Map

- `crates/hekma-engine/src/store/writer.rs` (new) — the writer thread, jobs, batching, bench tests.
- `crates/hekma-engine/src/store/sqlite.rs` — v12 step, import, new tables' reads and writes, rollup-served totals, savepoint-aware transactions, read-only open.
- `crates/hekma-engine/src/ports/run_store.rs` (new) — the `RunStore` port and `Page`.
- `crates/hekma-engine/src/domain/registry.rs` — writer routing, open wiring.
- `crates/hekma-engine/src/domain/supervisor/{mod,usage,reaper,spawn,lifecycle,interaction}.rs` — transitions, the pipeline caller, latch removal, adoption, generation.
- `crates/hekma-engine/src/domain/{outbox,resync}.rs`, `crates/hekma-engine/src/engine.rs` — DB-backed export, ack, resync.
- `docs/architecture.md`, `docs/embedding.md`, `docs/hosting.md`, `docs/policies.md`, `docs/control-api.md`, `docs/testing.md` — storage, order, outbox, concurrency bound.
- `CHANGELOG.md`, `docs/RELEASE_NOTES.md`, `crates/hekma-engine/Cargo.toml`, `Cargo.toml` — 0.11.0.
- `_bmad-output/implementation-artifacts/deferred-work.md`, `sprint-status.yaml`.

## Verification

- `cargo +stable fmt --all -- --check`; `cargo +stable clippy --workspace --all-targets -- -D warnings`; `cargo +stable nextest run --workspace --all-targets`; `python3 scripts/check_docs.py`; `python3 scripts/test_automation.py`.
- Changed-line coverage from `cargo llvm-cov -p hekma-engine --lib`, intersected with the diff's added lines.
- Regression gates: `tests/memory_atomicity.rs` (two engines, one root), `tests/resync_events.rs`, `tests/outbox_export.rs`, `tests/budget.rs`, `tests/adoption.rs`, the supervisor suite.

## Ask First

- The public-facing decisions in Design Notes: the changed event order, the action-keyed latch, the outbox failure contract, the 1 s Busy bound, removed instances dropping their event rows, and the 0.11.0 bump.

## Spec Change Log

- 2026-10-31 — spec written from the story text, the readiness findings for 21-1 and D1/D2.
- 2026-10-31 — AI-55 round 1 (own pass, then one independent adversarial pass). Fixed: rollups missed rows an older engine inserted after the upgrade (watermark fold, AUTOINCREMENT ids, release-note warning); `Busy` spent park attempts, so ~3.5 s of contention skipped usage (Busy no longer counts); a batch SQLite rolled back on its own let later jobs autocommit and be reported failed (batch aborted, later jobs refused); a panicking hook killed the writer (caught); two openers racing the v12 step (version re-checked under the lock); the eviction scan read the whole outbox at the cap (stops at the first kept row); the `.migrated` backup could be overwritten (never overwritten). Deferred: instance-less rows never dedup (NULL in the UNIQUE key), no instance-less writer before 21.2/23.1. Own pass: the action dedupe in `apply_evaluation` was dropped to keep the pre-21-1 per-breach action behaviour.
