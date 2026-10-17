---
title: 'Memory attach/detach × start atomicity: durable-store transactions closing the cross-process windows'
type: 'bugfix'
created: '2026-10-16'
status: 'implemented'
baseline_commit: '800934c0eced9e60e4d253a61b63d836650f689a'
review_loop_iteration: 0
context:
  - '{project-root}/_bmad-output/planning-artifacts/ai-63b-lock-model-decision-2026-08-27.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The memory-backing attach/detach mutations and the supervisor's start-path backing snapshot are not mutually atomic ACROSS PROCESSES — the real CLI shape is one `Engine` per command sharing a state dir, so `hekma agent start` in one terminal races `hekma agent memory attach` in another: (a) attach landing after start's backing read but before spawn (row says attached, agent launched without the dir), (b) detach clearing the row after the read but before launch (agent launched with a stale dir), (c) attach/detach's terminal-state guard check landing in the gap before the row write (a backing row lands on a running instance — the forbidden hot-swap). In-process the registry mutex already serializes everything; mutex-based fixes close NOTHING for the actual windows (Fibery 166; the 5-1 deferred finding + the never-run AI-63b replacement-model decision).

**Approach:** Close the windows at the only state the racers share — the durable store: (i) attach/detach run their guard + conflict-check + write inside ONE `BEGIN IMMEDIATE` transaction (the 11-3 migration precedent); (ii) start's AUTHORITATIVE backing read moves into the same write transaction that persists the `→ starting` transition (a conditional transition snapshotting the backing row; a mismatch rejects typed + retryable); the existing early read stays only as the pre-check feeding the self-heal/symlink/UTF-8 refusals and the delivery notice; (iii) add `busy_timeout` (absent repo-wide) as the explicit AD-17 bound so a concurrent CLI pair degrades to a bounded wait + typed busy error, never instant `SQLITE_BUSY`. The two-mutex model, the R<S total order, and AD-17's registry-lock-only discipline for memory ops are preserved verbatim.

## Boundaries & Constraints

**Always:**
- Lock discipline untouched: R before S everywhere; attach/detach stay registry-lock-only; no new locks (AD-18).
- Every race outcome is SURFACED typed (AI-18): a start-vs-attach mismatch rejects with a retryable typed error naming both sides; busy waits surface as the typed busy error after the bounded timeout; never silent last-writer-wins.
- The no-side-effect rejection contract holds: a guard failure inside the txn leaves the row byte-unchanged (the pinned registry rejection trio must stay green unchanged).
- The delivery notice, self-heal, symlink refusal, and unmapped-but-started notice re-derive from the transactional snapshot so their semantics survive the reorder.
- Crash-atomic: the guard+write txn and the transition-snapshot txn commit fully or not at all (BEGIN IMMEDIATE…COMMIT, the 11-3 `migrate_step` pattern).

**Ask First:** Nothing anticipated; if the reorder proves to break a pinned notice/self-heal semantic irreconcilably, halt and present the trade-off rather than re-pin unilaterally.

**Never:** No lock-model replacement (AD-17/AD-18 stand; this IS the scoped closure the AI-63b decision kept deferring); no flock/file-lock layer; no behavior change for the single-Engine embedding path beyond the snapshot derivation; no schema change (the txns wrap existing tables).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Two CLI: attach races start's read→spawn window | attach commits BEFORE the starting-transition txn | the txn's snapshot sees the new row; start rejects typed+retryable ("backing changed under start; retry") OR re-derives if pre-check matched — never launches with a stale view | typed mismatch error, surfaced |
| Two CLI: attach commits AFTER the transition txn | state already `starting` | the attach guard refuses side-effect-free (unchanged contract) | typed non-terminal refusal |
| Two CLI: detach races the same window | detach commits before/after the txn | symmetric: snapshot sees the clear (launch without dir) or the guard refuses on `starting` | typed |
| Guard-vs-write race inside attach | another process transitions the instance between guard and write | the txn serializes: the guard re-evaluates under the write lock; no row lands on a running instance | typed non-terminal refusal, row unchanged |
| Concurrent writers generally | any two store writers collide | bounded `busy_timeout` wait, then the typed busy error | never raw SQLITE_BUSY to the operator |
| Single-Engine embedding | all ops through one Engine | behavior identical (mutexes already serialized; txns are strictly additive) | unchanged |

</frozen-after-approval>

## Code Map

Investigated 2026-10-16 (anchors verified):
- The lock map — `EngineInner { registry: Mutex, supervisor: Mutex, events }` engine.rs:186-193; start holds R for ALL of start_inner engine.rs:910-913 → spawn.rs:15-17; attach/detach/memory_status R-only engine.rs:1221-1227/1240-1246/1264-1270 (rationale :1209-1213); R<S total order everywhere (no S→R exists); `with_diagnostics` S-only engine.rs:381-387.
- The start path — backing read spawn.rs:255 → memory_dir :256-259 → invocation override layer :321-329 → mapping application :382-389 → SpawnSpec :495-534 → backend.spawn :538; terminal until the `→ starting` transition :487-493 (registry.set_state mod.rs:1119 → autocommit UPDATE sqlite.rs:758). Everything between read and transition is guard-invisible (mapping resolve :242, self-heal I/O :260-296, loopback bind :310-311, secrets :372-374, renders :382-389, snapshot write :451-453, log dirs :467).
- The attach/detach paths — lookup registry.rs:647 → guard :650 (`ensure_terminal_for_memory_backing` :1511-1522) → dir-create :675-677 → upsert :678-683; detach guard :704-706; upsert read-then-write internally sqlite.rs:1264-1292.
- Store precedents — per-step BEGIN IMMEDIATE migrations sqlite.rs:14, 507-516 (the crash-atomic pattern to reuse); busy_timeout ABSENT repo-wide (the AI-63a correction, ai-63b doc :21).
- Pinning tests — memory.rs:136/:183/:246/:309/:361/:511/:560/:606/:672/:739/:843; registry.rs unit trio :3580 (attach non-terminal no-side-effect), :3622 (detach), :3641; supervisor/tests.rs:2388/:2453/:4506/:4559 + start-ordering pins :417/:789; sqlite.rs:3423-3552; agent_cli.rs:7170/:7275/:7339/:7408/:7437/:7457/:7479 + JSON freezes :6163-6297. Fixture users of attach/detach: hermes.rs:267/600/1045, acp_lifecycle.rs:899/970, diagnostic_sink.rs:172+, hermes_tck.rs:285-332, tck.rs:2054-2109.
- Considered-and-rejected (record in the architecture doc): PC-5a routing attach/detach through the supervisor mutex — deadlock-free but closes nothing cross-process and queues memory ops behind stop's ≤35s graceful window (AD-17-hostile); a third lock/flock — process-local or portability-fragile, declined by AD-18.

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-engine/src/store/sqlite.rs` (+ `ports/state_store.rs`) -- transactional seams: a `BEGIN IMMEDIATE` wrapper for the attach/detach guard+write, and a conditional `transition_with_backing_snapshot` persisting `→ starting` + reading the authoritative backing row in ONE txn; `busy_timeout` pragma at connection open (bounded, named in the docs).
- [x] `crates/hekma-engine/src/domain/registry.rs` -- attach_memory/detach_memory move guard+conflict-check+write into the txn; rejection arms stay side-effect-free.
- [x] `crates/hekma-engine/src/domain/supervisor/spawn.rs` -- the authoritative read becomes the txn snapshot; the early read demoted to pre-check (self-heal/symlink/UTF-8/notice inputs); a snapshot-vs-precheck mismatch rejects typed+retryable; the delivery notice derives from the snapshot.
- [x] New integration test -- a two-`Engine` same-root race harness pinning all three windows closed (attach-during-start, detach-during-start, transition-inside-guard-gap) using the hermetic two-engine shape; plus the busy-timeout typed error under a held write txn.
- [x] `docs/architecture.md` -- the locking paragraph gains the transactional boundary statement (what the store txns close, what mutexes still cover, the considered-and-rejected PC-5a/flock options, the busy_timeout bound).
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- story 15-6 row; `deferred-work.md` 5-1 entry resolved-marker at merge.

**Acceptance Criteria:**
- Given two Engines on one root with attach interleaved anywhere in start's read→transition window, then EITHER start launches with the attached dir (attach committed before the snapshot) OR start rejects typed+retryable — never a launched agent contradicting the row; and an attach committing after the transition refuses side-effect-free.
- Given detach interleaved symmetrically, then the same dichotomy holds.
- Given a lifecycle transition landing inside attach's guard→write gap, then the attach refuses with the row unchanged (the pinned no-side-effect trio stays green).
- Given two concurrent store writers, then the loser waits the bounded busy_timeout and surfaces the typed busy error (never raw SQLITE_BUSY).
- Given the single-Engine battery, then every pinned memory/registry/supervisor/CLI test above stays green unmodified (except deliberate re-derivation pins named in the PR).
- Gates: fmt/clippy/workspace tests/check_docs green.

## Spec Change Log

- (Review round 1, 2026-10-17) 31 findings triaged (blind 18, edge 10, vg 3), all patches except one DEFER (record_transition failure after the transition commit — a PRE-EXISTING window the old autocommit path shared; the extraction made it visible; deferred entry, not a regression of this story). The bricking bug leads: commit-failure in the three txn wrappers now ROLLS BACK (an open txn on the long-lived connection could kill every later store call). CLI honesty: Busy/StateConflict gain dedicated retry-class arms (never the "database may be inaccessible" text); NotFound-in-txn keeps its class; the raced-start arm stays exit-code 4 (frozen table; the class fits "invalid state at command time") WITH the mapper-test case pinning it (the adversarial-retarget hole the suite exists for) and the retry remediation; the three new surfaces' binary naming aligns to ONE name (hekma); backend()'s busy wording stops asserting a full 5s elapsed ("remained busy within the bounded 5000ms window"). busy_timeout installs BEFORE the WAL pragma (first-open races can't bypass the typed error). The post-commit symlink/ensure failure reports the COMMITTED state honestly (attach succeeded at the store; the dir problem named; the self-heal path named) — no fake side-effect-freeness; the attach doc's numbered ordering prose matches the code; rejected-start messages stop overclaiming ("nothing was launched" → names the inert config-snapshot residue). Tests: the dead second Engine removed + docs wording scoped (deterministic windows pin the store seam; the unsynchronized race drives two Engines); the vacuous probe assertion replaced; the two-concurrent-starts engine-level pin + the dichotomy test's attach-loses arm (E7) + the scheduler-stall Busy tolerance (E8) added; the no-notice-on-rejection sink-emptiness pin (vg). Records: CHANGELOG gains the 15-6 surface in the unreleased 0.7.0 block (new StoreError variants are exhaustive arms for external implementors); "zero existing tests modified" corrected (the BareStore mock gained the three trait impls; no existing ASSERTIONS moved); the architecture sentence scoped to the → starting seam; upsert/clear carry the bypass-warning doc; MemoryBackingTxn's retention documented; testing.md gains the memory_atomicity suite (incl. the deliberate 2×5s busy-wait note — a test-fidelity choice, no env lever); the sprint row gains the Fibery URL.
- (Closeout, 2026-10-17) Merged as 7c73d60 (PR #286) after the review round (commit-failure rollback proven fails-without-fix; dedicated CLI retry arms; honest committed-state reporting). Zero existing test assertions modified; the full pin inventory green on every CI leg.

- 2026-10-17 (implementation, loop 0): landed as specified — store txns
  (`attach_memory_backing_txn`/`detach_memory_backing_txn`/`set_state_snapshotting_memory_backing`),
  the registry pre-check + in-txn authority split, the start-path reorder
  (early read → pre-check; txn snapshot authoritative; delivery notice moved
  post-transition deriving from the snapshot), `busy_timeout` 5000ms at open
  with the typed `StoreError::Busy` classification, typed
  `EngineError::MemoryBackingChangedUnderStart` (retryable, names both
  sides). Two deliberate orderings worth naming for review: (1) the attach's
  symlink refusal stays a PURE PRE-TXN READ so a symlinked managed path
  still refuses with no row committed (the pinned re-attach refusal keeps
  its exact semantics); (2) the registry keeps a side-effect-free pre-check
  guard ahead of the txn so the no-side-effect rejection trio stays green
  UNMODIFIED — the authoritative guard re-runs inside the txn (the race case
  the pre-check cannot see). The row now commits BEFORE the managed-dir
  creation (the guard must precede any side effect and the guard lives in
  the txn); both failure residues remain the documented inert, self-healing
  class (no rollback ever deletes operator-shaped data). Pin inventory
  (review round 1 correction): no existing test ASSERTIONS were modified —
  the one touched existing test artifact is the `BareStore` test-support
  mock in `ports/state_store.rs`, which gained the three new trait-method
  impls (`unimplemented!` bodies, compile-necessary for the exhaustive
  impl); all green. New: tests/memory_atomicity.rs
  (two-Engine same-root harness, all three windows, the busy bound) + 9
  store-level pins.

## Design Notes

- Why transactions and not locks: the racers that matter are two PROCESSES (the CLI's one-Engine-per-command shape); the only shared state is SQLite. std mutexes are process-local by definition — any lock-shaped fix is cosmetic for the real window. This is the scoped closure the AI-63b "replacement locking model" kept deferring; it lands WITHOUT touching the model.
- The start reorder is the delicate part: the early read stays (its refusal inputs need to run BEFORE any state transition), but it is no longer authoritative — the txn snapshot is, and disagreement between them means someone mutated the backing mid-start, which is exactly the race: reject, don't guess.
- busy_timeout value: pick from the stop ladder's bounded-wait precedent (e.g. 5s) — long enough for a realistic attach-vs-start collision, short enough to never feel hung; named in docs.

## Verification

**Commands:**
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` -- green (the full memory/registry/supervisor/CLI pin inventory).
- The new two-Engine race test, run with `--nocapture --test-threads=1` where the harness needs it.
- fmt / clippy / check_docs -- green.
