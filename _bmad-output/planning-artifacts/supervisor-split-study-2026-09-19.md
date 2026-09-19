---
type: architecture-decision-proposal
date: 2026-09-19
author: drafted per story 13-3's spec stage (epic-11 retro F-agg2; AD-18 carried debt), for Islam
status: awaiting-ratification  # per AI-58 draft-then-ratify: this proposal touches ZERO production code; the chosen boundary is applied only after Islam ratifies
decides: "the deliberate module boundary for crates/hekma-engine/src/domain/supervisor.rs (9101 lines at HEAD 1388e79)"
scope_note: >
  MODULE boundary only — file organization inside the engine crate. The LOCK model is
  separately settled: AD-18 (2026-09-19) ratified the coarse two-mutex model; nothing here
  reopens it, and no option below changes any lock, signature, or behavior.
verified_against_code: true
verified_at_commit: 1388e79
---

# Supervisor Module Boundary — the Split Study (story 13-3, spec stage)

**The problem.** `crates/hekma-engine/src/domain/supervisor.rs` is **9101 lines** — 4773
production + 4328 test (the `#[cfg(test)] mod tests` starts at line 4774). It grew +36% in
epic-11 and +993 net in epic-12, and every retro since epic-11 has carried the same item:
the next engine epic's spec stage should propose a deliberate boundary instead of accreting.
The cost is not aesthetic: single-reviewer context for any supervisor change now spans
~4.8k production lines across six unrelated operation families; merge conflicts concentrate
on one file (epic-12's squash touched it 10 of 18 commits' worth); and the three most
recent engine defects (4-1's lock-hold, 12-1's detach surfaces, the strip gap 13-1 closes)
each required reading the whole file to safely touch one family.

**The evidence — what the file actually contains** (method inventory at `1388e79`):

| Family | Members (line anchors at HEAD) | ~Lines |
|---|---|---|
| Core: types, state, transitions | `RestartPlan` :149, `RestartDecision` :163, `ObservedPending` :498, `Supervised` :519, `Supervisor` :657, ctors + event bus + diagnostics :739–929, `transition*`/`fail_launch*`/`log_capture_for` :3293–3415 | ~1100 |
| Spawn/start | `start` :930, `start_detached` :955, `start_inner` :984–1535, `start_observed_listener` :3416, `watch_startup` :3468 | ~650 |
| Lifecycle (stop/pause/resume/restart) | `restart` :1536, `stop*` :1596–1879, `pause*`/`resume`/`suspend_or_resume` :1880–2156, `signal_backend` :2157 | ~700 |
| Reap / adopt / reconcile | `poll_once` :2593, `plan_restart` :2979, `settle_terminal_record` :3041, `adopt_orphans` :3070, `reconcile_orphan_failed` :3244, poll-fault injection :2357–2397 | ~650 |
| Usage drain + enforcement | `drain_usage_all/for` :3514/:3556, `drain_observed_all/for` :3698/:3732, `cap_observed_park_buffer` :3885, `ingest_usage` :3955, `enforce_budget` :4067, `apply_breach` :4195, `enforce_pause`/`enforce_stop` :4245/:4266, `record_*_breach` :4284/:4314, `persist_breach_event` :4353, `read_breach_events` :4418 | ~1000 |
| Interaction & observation | `send_input` :2244, `current_run_id` :2349, `read_events` :2398, `read_agent_log*` :2451/:2515, `ensure_log_dir`/`agent_log_len` :3487/:3502 | ~400 |
| Tests | `mod tests` :4774–9101 | **4328 (47.5%)** |

The families are already surgically clean at the call level: cross-family calls are
narrow and acyclic (spawn → transition helpers; reaper → `plan_restart` → lifecycle's
`stop_with_cause`; drain → `ingest_usage` → `enforce_*` → lifecycle's
`stop_with_cause`/`pause_with_cause`). The six-frame enforcement chain AD-17 documents
lives entirely inside the usage family plus one hop into lifecycle. Nothing in the file
requires a lock-model or trait change to move.

## The options

### Option A — Tests out first (production file untouched)
Move `mod tests` (4328 lines) to a sibling `domain/supervisor/tests.rs` submodule tree
(or a `supervisor/tests/` directory split by family, mirroring the production split below).
Tested private items widen to `pub(super)` — mechanical, crate-internal only, **zero
public-API change, zero behavior change, compile-gated**.
- **Wins:** the file halves immediately; the production boundary below becomes reviewable
  at all; test-side conflicts (the majority of epic-12's churn) stop colliding with
  production edits.
- **Costs:** visibility bumps on every tested private fn (one-time, ~30–60 items);
  `super::*` imports re-pointed.

### Option B — Directory split by operation family (pure moves, after A)
`domain/supervisor/` becomes a directory: `mod.rs` (the `Supervisor` struct,
`Supervised`/`ObservedPending`, ctors, diagnostics, transition helpers — the Core family),
plus `spawn.rs`, `lifecycle.rs`, `reaper.rs`, `usage.rs`, `interaction.rs` holding the
families above **as additional `impl Supervisor` blocks** (idiomatic Rust; the type does
not move; no trait introduced). Cross-family private fns widen to `pub(super)`; the
enforcement chain's `pub(crate)` surface is unchanged.
- **Wins:** each family becomes an independently reviewable ~400–1000-line unit; future
  epics touch the family file, not the monolith; the debt items AD-18 carries land *in
  the family that owns them* (usage rollups → `usage.rs`; the bare `child.wait()` bound
  → wherever the Drop lives, `spawn.rs`'s neighborhood); grep-ability of a subsystem
  becomes whole-file.
- **Costs:** a large mechanical diff (git rename-tracking keeps history if done as
  move-then-trim commits); visibility bumps; one-time import churn. **No behavior risk:
  moves and visibility only, compile-gated per commit.**

### Option C — Subsystem extraction with explicit ports (NOT recommended now)
Promote usage/enforcement (or the reaper) to its own type with a defined input surface
(e.g. free functions over `&mut Supervised` + `&Registry`, or a `UsageEngine` struct) —
the "vertical" cut that enables independent subsystem testing and would anticipate the
archived AI-63b Step-4 shape (`SupervisorShared`/per-instance state).
- **Why not now:** it re-architects seams AD-18 just declined to pay for; the enforcement
  chain's correctness argument (AD-17's six-frame chain) is currently *documented against
  the single-type shape*, and re-proving it after extraction is real work for no driving
  need; Option B delivers the reviewability wins without moving a single responsibility.
- **When it reopens:** if a fleet-wide budget or global concurrency cap is ever ratified
  (AD-18 prohibition (a) already requires a new proof for exactly that), do the
  extraction as part of it — the boundary proposal below deliberately leaves room.

## Recommendation

**Ratify A + B as one story (13-3 implementation), staged in three compile-gated,
individually revertible commits:**
1. **Commit 1 (Option A):** tests → `domain/supervisor/tests/` split by the same
   families. Production bytes untouched.
2. **Commit 2 (Option B):** production directory split — `mod.rs`, `spawn.rs`,
   `lifecycle.rs`, `reaper.rs`, `usage.rs`, `interaction.rs`. Moves + visibility only.
3. **Commit 3 (hygiene rider, optional):** land AD-18's carried Step-0/Step-7 free wins
   *in their new family homes* (agent-log reads off the guard + log rotation notes →
   `interaction.rs`; usage rollups + the double `effective_config` dedupe → `usage.rs`).
   Separately revertible; skippable without blocking 1–2.

Each commit runs the full house gates (`fmt`, `clippy -D warnings`, workspace `test`,
`check_docs`); no `Cargo.toml`, public-API, or lock change at any step; `git log --follow`
preserves history (moves committed as pure renames first, edits second).

**Explicitly out of scope:** anything from the archived AI-63b migration Steps 1–6
(TimedMutex, cells, StoreGate) — those are lock-model work behind AD-18's reopen
triggers, and this boundary deliberately does not preclude any of them (a later
`SupervisorShared`/InstanceState cut would land *inside* `mod.rs`'s Core family).

## The ask (AI-58 step 2)

Islam ratifies one of: **(1) A+B as staged above (recommended)**; (2) A only;
(3) A+B+C — not recommended, see Option C's "why not now"; (4) your own variant.
On ratification, the implementation applies the chosen text verbatim as story 13-3's
dev stage.
