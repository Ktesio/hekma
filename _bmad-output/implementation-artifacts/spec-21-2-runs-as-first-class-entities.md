---
title: 'Runs as first-class entities'
type: 'feature'
created: '2026-11-02'
status: 'done'
review_loop_iteration: 1
baseline_commit: '8ead5ec81f7f6821eef0716a2759ae24f7291565'
context:
  - '_bmad-output/planning-artifacts/epics.md (Epic 21, Story 21.2 + readiness bullets, build order)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-25; AD-27; AD-7 Run definition; AD-14, AD-15 amendments 2026-10-30; AD-20 Run resources)'
  - '_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md (FR-49, FR-43, FR-21; Glossary Run, Control Depth, Run Status, Anomaly)'
  - '_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md (Stories 21-2 and 21-3 findings, D3)'
  - '_bmad-output/implementation-artifacts/spec-21-1-the-store-writer-and-run-scale-storage.md (the store writer and RunStore this story extends)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** The engine knows Agent Instances, not Runs. A run id is minted at every spawn, so a Restart Policy relaunch silently opens a new Run and per-run budgets, latches and the `block` fence reset on a crash; there is no Run row, no Run Status, no Anomaly, no labels, no parent/child Runs (the Host "Task" scope of the old 19.3), and nothing for the later data path (Run Keys, the shared proxy) to read without the engine locks. Every Epic 21–25 story addresses Runs by run id.

**Approach:** A Run registry in the engine core, beside the `Supervisor`, whose state lives in state.db and is written only by the store writer (AD-25, AD-27). New `runs`, `run_labels`, `run_limits` and `run_identities` tables (schema v13, with a backfill); ULID run ids (`run_` + 26 Crockford chars; legacy ids stay valid). Every supervised start creates a `supervised` Run; a relaunch keeps it; lifecycle transitions project Run Status and Anomalies in the transition's own writer job and cascade to in-process child Runs. Host-opened child Runs replace Task: open/close through the facade, the Control API and the CLI, with ancestor rollups and ancestor ceilings evaluated in the writer's pipeline (a new pure `decide_chain`, published as an additive Governance Vectors case kind). The writer publishes an immutable Run-index snapshot after each committed batch. Instance `labels` config gives supervised Runs their bound labels. A verb × Control Depth table lives in the engine core and is pinned by a conformance fixture.

## Tasks & Acceptance

**Execution:**
- [x] `domain/usage.rs` — `RunId::mint` renders `run_` + ULID (48-bit ms time, 80 bits from `getrandom`, uppercase Crockford base32); `RunId::is_ulid`; legacy ids unchanged.
- [x] `domain/runs/` (new) — `ControlDepth`, `RunStatus`, `Anomaly`, `LabelSource`, `Label`, `RunLimits`, `RunRecord`, `RunView`, `RunEvent` (+ kind), `RunError`; the verb × depth table (`RunVerb`, `VerbCell`, `verb_cell`); the label grammar; the snapshot (`RunSnapshot`, `RunEntry`, `SnapshotPublisher`); the Run Status projection from Lifecycle State.
- [x] `store/` — schema v13 (one crash-atomic step): `runs`, `run_labels`, `run_limits`, `run_identities`, `run_latches` rebuilt with a foreign key to `runs`, and the backfill (Design Notes). `RunStore` gains Run writes (create, project status, set anomalies, end, cascade, labels, limits, identity, depth) and reads (run, chain, children, open runs, current process-owning Run). The rollup fold adds each row to every ancestor's run rollup.
- [x] `domain/pipeline.rs` + `domain/governance.rs` — evaluation over the attributed Run's ancestor chain via `governance::decide_chain`; a breach sets the `breach` Anomaly on its level's Run; Run events committed with the usage.
- [x] `domain/supervisor/` — a fresh spawn creates the Run (in the spawn-record job, with bound labels); a relaunch keeps the Run and re-raises its `block` fence; transitions project Run Status with in-process cascade; terminal failure ends the Run; adoption syncs depth/wire path and labels; the relaunch "fence lifted" note goes.
- [x] `domain/config.rs` — `labels.<key>` keys (validated), resolved without the invocation layer; a `labels.*` write re-syncs the live Run's bound labels.
- [x] `engine.rs` — `open_child_run`, `close_run`, `run` (async + blocking); `EngineEvent::Run`.
- [x] `domain/governance.rs` — `ChainCase` kind, `ancestor-ceilings` cover tag, vectors 1.2.0 regenerated; ci.yml stand-in Host and `test_automation.py` pins updated.
- [x] `hekma-host` — `POST /v0/runs`, `GET /v0/runs/{id}`, `POST /v0/runs/{id}/close` (operator bearer); SSE/export carry Run events; OTLP ignores them.
- [x] `hekma` CLI — `hekma run open|close|show` (human + `--json`).
- [x] `hekma-conformance` — `fixtures/verb-depth.json` + a test pinning it to `verb_cell` and probing every `tested` cell against a live engine.
- [x] Docs, CHANGELOG + RELEASE_NOTES (Unreleased: 0.12.0, relaunch keeps the Run), deferred work, sprint status.

**Acceptance Criteria:**
- Given a fresh engine, when a supervised instance starts, then exactly one `runs` row exists for it with depth `supervised`, `wire_path` true iff its metering source is `engine-observed`, status `active`, its id matches `^run_[0-9A-HJKMNP-TV-Z]{26}$`, a `run.created` event precedes its `running` transition event on the bus, and its bound labels equal the instance's `labels.*` config (instance and kind layers; an invocation override binds nothing).
- Given a running Run that crashes under `on-failure`, when the Restart Policy relaunches it, then the run id is unchanged, the Run goes `active` + `failed` Anomaly at the crash and loses the Anomaly at `running`, its per-run totals keep counting from before the crash, an already-latched per-run breach does not re-fire, and a per-run `block` fence is up on the relaunched listener.
- Given a crash the Restart Policy will not relaunch (`never` or crash loop), then the Run is `ended` with the `failed` Anomaly and end cause naming the crash; given an operator `stop`, then the Run is `ended` without an Anomaly; given `pause`/`resume`, then the Run is `held`/`active`.
- Given a supervised Run R, when a Host opens child C under R (facade, Control API or CLI) and grandchild G under C, then C and G are depth `announced`, carry R's instance id, and `run(C)` names parent R; opening under a missing or ended parent, past 8 levels, or with a directly set `pause`/`stop`/`block` is refused with a typed error naming the cause (the last naming the depth and the Metering Source); a second open with the same external id returns the same Run.
- Given usage committed against G, then G's, C's and R's run rollups each include it, the instance rollup counts it once, and C's per-run token ceiling breaching records one `warn` breach on C with the `breach` Anomaly on C, while R's per-run ceiling breaching executes R's configured action on the process.
- Given R with in-process children, when R is paused, then every non-ended in-process descendant is `held` with cause `parent-paused`; on resume they return to `active` (`parent-resumed`); on stop they are `ended` with cause `parent-stopped`; a closed child stays `ended`.
- Given a Run whose wire path changes at relaunch or adoption, then its run id is kept and one `run.depth-changed` event is emitted.
- Given a committed batch that changed any Run, then the published snapshot (before the caller is acked) holds every non-ended Run with its depth, status, Anomalies, parent and labels, and drops ended Runs.
- Given a pre-v13 state dir with a live instance on legacy run id L and history ids H1, H2 in usage, breaches and latches, when the engine opens, then L is a `supervised` Run with its live status, H1 and H2 are `ended` Runs, every latch row references a Run, and the next start mints a ULID id.
- Given the governance vectors, then a `chain` case kind pins ancestor ceilings (deepest level first, token before dollar within a level, per-level latches), `vectors_version` is 1.2.0, and the 1.1.0 cases are byte-identical.
- Given the verb × depth fixture, then it equals the engine's `verb_cell` table for every verb and depth, and every cell marked `tested` passes its live probe.
- Given handoff: fmt, clippy `-D warnings`, the workspace tests, `check_docs.py` (+ `--self-test`), `test_automation.py` pass; changed-line coverage ≥ 95%; AI-55 two-pass review done.

</frozen-after-approval>

## Design Notes

- **Run rows.** `runs (run_id TEXT PRIMARY KEY, depth, wire_path, status, anomalies, parent_run_id → runs ON DELETE SET NULL, instance_id → agent_instances ON DELETE CASCADE, process_owner, external_binding, end_cause, lease_expires_at, created_at, updated_at, ended_at)`. `process_owner = 1` marks the supervised Run that owns the instance's process; a partial unique index allows at most one non-ended process-owning Run per instance. `anomalies` is a sorted comma list of kebab names (`''` when none) with a partial index for exception-first reads (21.5). `lease_expires_at` stays NULL until Leases exist.
- **Run bookkeeping never blocks a transition.** The projection runs in its own SAVEPOINT inside the transition job; a failure rolls back only the projection and is a diagnostic (the Run catches up at the next transition, start or adoption). An unreadable Run row at open leaves the snapshot empty with a note instead of refusing the open.
- **One writer, no new lock.** All Run writes are store-writer jobs; the "Run registry" is `domain::runs::RunRegistry`, a handle on the writer plus the snapshot publisher, with no mutex, so the fixed lock order `Registry → Run registry → Supervisor` holds trivially. A Supervisor transition sends its Run projection inside its own writer job (never a Run lock).
- **Run ids.** `run_` + 26 uppercase Crockford base32 characters: 48-bit Unix milliseconds then 80 random bits from `getrandom` (already in the dependency tree through `ring`; added as a direct dependency, no new crate). Not monotonic within a millisecond (not needed: ordering keys are the row ids). Legacy `run-<nanos>-<nonce>` ids stay valid everywhere.
- **Run Status projection (supervised).** Spine AD-15 amendment, refining PRD FR-49 for the backoff window:

  | Lifecycle edge | Run Status | Anomaly | Run event |
  |---|---|---|---|
  | spawn record committed (fresh start) | `active` | — | `created` |
  | → `running` | `active` | clears `failed` | `status-changed` if it changed |
  | → `paused` | `held` | — | `status-changed` (children cascade `parent-paused`) |
  | `paused` → `running` | `active` | — | `status-changed` (children `parent-resumed`) |
  | → `starting`, → `stopping` | unchanged | — | none (stopping a paused agent is not a resume) |
  | → `stopped` | `ended` (cause = transition cause) | — | `ended` (children `parent-stopped`) |
  | → `failed`, relaunch planned | unchanged | sets `failed` | `anomalies-changed`, no cascade |
  | → `failed`, terminal (`never`, crash loop, launch/readiness failure, orphan reconcile) | `ended` (cause = crash detail) | sets `failed` | `ended` (children `parent-ended`) |
  | operator `start` while a previous Run is still open | previous Run `ended` (cause `replaced`) | kept | `ended` |
  | engine open finds an open process-owning Run whose instance is `failed`/`stopped`/`registered` (a planned relaunch that never ran) | `ended` (cause `relaunch-not-run`) | kept | `ended` (children `parent-ended`) |
  | relaunch or adoption of a kept Run that already ended | `active` again (cause `relaunched`) | kept | `status-changed` |

- **Run Status (child).** `active` at open (`held`, cause `parent-paused`, when opened under a held parent); `held` on `parent-paused`, back to `active` on `parent-resumed` (only a child held by that cause); `ended` on `close` (cause `closed`), `parent-stopped` or `parent-ended`. A child never holds on its own before wire hold exists; `silent` and `stale` are in the enum for the wire shape and are not produced yet.
- **Anomalies in 21-2.** `breach` — set when any breach records on that Run (any action, FR-21's loud signal); it clears only when the latch re-arms (21.6), so it stays on the Run until then, ended Runs included. `failed` — set at a crash; cleared when a relaunch reaches `running`; kept on an ended Run. `depth-fallback` — set when a depth change leaves a directly set child Breach Action unexecutable (the action then acts as `warn`); cleared when the depth supports it again. Later stories add `unscented`, `bypass`, `silent`, `policy-fallback`.
- **Relaunch keeps the Run (FR-49, AD-7).** The spawn path reads the run id off the replaced spawn record for a Restart Policy relaunch and reuses it (a new launch generation still keys dedup); only an operator or Host `start` mints a new Run. Consequence (release note): per-run Token Budgets and Cost Caps no longer reset when an agent crashes and is relaunched; a per-run latch and a per-run `block` fence carry across the relaunch. The Run-start fence evaluates the kept Run's per-run totals; 19-2's "relaunch lifts the fence" diagnostic is removed. Because the latch carries, a `pause` or `stop` the kept Run already fired (under the currently configured action) is re-applied to the relaunched process with its breach's cause, so a crash while breach-paused never comes back unpaused.
- **Child Runs (FR-43, FR-49).** Opened under an existing, non-ended parent Run on this engine, ≤ 8 levels from the root (the root is level 1). A child of a supervised Run (or of its children) is depth `announced`, inherits the hosting `instance_id` (in-process), `wire_path` = the host process's wire path, no Lease (until 23.2). Labels given at open are `caller` labels (keys validated); bound labels come only from configuration now and from `hekma label` (21.9); a child does not inherit its parent's labels. Optional limits at open: per-run token ceiling and per-run dollar cap (no cumulative scope — cumulative is per instance) and a Breach Action. `warn` (or none) is accepted; `pause`, `stop` and `block` set directly on a child are refused at open: with no wire path per FR-49 (naming the Metering Source), and with a wire path because hold and reject are not built yet (the verb × depth cell is `pending`), naming the depth. An optional external id maps through `run_identities` (binding `host`) so a retried open returns the same Run (FR-52 "stays one Run"). `close` ends a child (idempotent on an ended child); closing a process-owning Run is refused (use `stop`). Children of a closed child keep running (FR-49: only in-process children of the process-owning Run end with it). Opening a child under an ended parent is refused.
- **Ancestor rollups.** `usage_rollup_run` stays keyed `(instance_id, run_id)` and becomes a subtree total: the fold adds each usage row to its own Run and to every ancestor through `parent_run_id` (each ancestor under its own `instance_id`), never by re-summing a subtree. Instance rollups sum rows by instance, so child usage counts once on the hosting instance (cumulative budgets include it).
- **Ancestor ceilings (`decide_chain`).** Input: the levels from the attributed Run up to the root, each a `GovernanceInput` (its arms against its own subtree totals and its own latches; the cumulative scope only on the process-owning level). Output: per level, `decide` on that level, in order deepest → root; within a level token before dollar. Each level's breach records on that level's Run with that level's Breach Action; a child level can only be `warn` today, the process-owning level executes its configured action on the process exactly as before (and the cascade follows from the transition). An ended process-owning level (a Run a fresh start replaced) records its breach but acts on nothing, never on the process that replaced it. A single-level chain is exactly `decide`. Child dollar ceilings price with the hosting instance's Rate stamped on the row (unpriced usage leaves a dollar ceiling inert, as for instances).
- **Governance Vectors 1.2.0.** Additive: a new `chain` case kind (`levels` in, per-level outcomes out) and the `ancestor-ceilings` cover tag, appended after the shipped cases; 1.1.0 cases unchanged; a consumer pinned to 1.1 skips the unknown kind (docs/governance.md). The CI stand-in Host learns the kind.
- **Labels config (D3).** `labels.<key> = "<value>"` in any layer, merged per key (the existing per-leaf merge, higher layer wins); the invocation override layer is excluded when resolving labels. Key: `^[a-z][a-z0-9_.-]{0,62}$`; value: 1–128 chars of `[A-Za-z0-9_.:/@+-]`, never starting `secret:`. Validated at write. Applied as bound labels at Run creation, re-synced at adoption, and on a successful `labels.*` config write while a Run is open (one `labels-changed` event when the set changed). The set_config path writes TOML first, then the label job; a failed label job is a diagnostic, the next adoption/start re-syncs.
- **Depth changes.** A Run's depth or wire path is re-derived at relaunch and adoption from the active metering source; a change keeps the id and emits `depth-changed` (prior and new depth + wire path). Latches are keyed by run id, so they survive.
- **Run events (AD-14).** One new variant `EngineEvent::Run(RunEvent)`: `{schema_version, kind ∈ created|depth-changed|status-changed|anomalies-changed|labels-changed|ended, run_id, parent_run_id?, instance?, control_depth, wire_path, status, prior_status?, prior_depth?, prior_wire_path?, anomalies, labels (bound only), cause?, at}`. Committed in the outbox with the job that caused them, published on the bus in commit order after the job's other events. No `run_events` table (the outbox and the `runs` row are the record). The OTLP v1 exporter ignores them (no run id on metrics, AD-21). Policy names join in 21.3.
- **The snapshot (AD-25).** `RunSnapshot { generation, runs: HashMap<RunId, Arc<RunEntry>>, owners: HashMap<InstanceName, RunId> }`, non-ended Runs only. The writer is the only publisher: a Run-changing job registers a post-commit hook carrying its changed entries; the hook builds the next snapshot copy-on-write and swaps it behind an `RwLock<Arc<_>>` (readers clone the `Arc`; no engine mutex, no SQLite). A publish costs O(open Runs) pointer copies; making it O(changed entries) is 24.4's scale work. Loaded once at writer open. Per engine process until 21.4.
- **Facade and wire surfaces.** `Engine::open_child_run(ChildRunRequest)`, `close_run(run_id)`, `run(run_id) -> RunView` (async + blocking). `RunView` = the Run row + bound and caller labels + limits + its subtree usage (tokens, cost) + open children count. Control API: `POST /v0/runs` → 201 + `Location`, `GET /v0/runs/{id}`, `POST /v0/runs/{id}/close`, operator bearer; `RunError` maps to 404 (`run-not-found`), 409 (`no-open-run`, `parent-ended`, `owns-process`) and 400 `invalid_body` (`nesting-too-deep`, `action-unavailable`, `invalid-request`), with the code in `error.code` beside the API's existing kinds. CLI: `hekma run open --parent <run-id|instance> [--label k=v]… [--token-budget N] [--dollar-cap D] [--breach-action warn] [--external-id X] [--json]`, `hekma run close <run-id> [--json]`, `hekma run show <run-id> [--json]`. `--parent <instance>` resolves to its open process-owning Run. Listing is 21.5.
- **Child usage before 23.1.** No public surface submits usage to a child yet (attributed calls land with 23.1). The pipeline evaluates any run id's chain, and the crate-internal `Supervisor::commit_run_usage` seam exercises it in tests.
- **Verb × Control Depth table.** `verb_cell(verb, depth, wire_path) -> VerbCell { behavior: honored|qualified|fails-fast|refused, available: bool, note }` transcribes the PRD FR-49 table, plus the `supervised (no wire)` column (wire-only cells fail fast naming the Metering Source). `available` is false for cells later stories enable (wire hold/reject, run-addressed verbs). The fixture `crates/hekma-conformance/fixtures/verb-depth.json` lists every cell with `status: tested|pending`; the test asserts parity with `verb_cell` and runs a live probe for each `tested` cell (supervised list/usage, logs, budgets/`warn`, pause/resume, stop, send; child `send` fails fast; child Breach Action `pause`/`stop`/`block` refused).
- **Upgrade backfill (v13, one crash-atomic step).** DDL, then: each `agent_runtime` row with a run id → a `supervised`, process-owning Run with its legacy id, status projected from the instance state (`failed` → `ended` with the `failed` Anomaly; a relaunch reopens it), wire path from the instance's last recorded metering source (`usage_events`, else `false`), `created_at` from its earliest usage or the record; every other distinct run id in `usage_events`, `breach_events` and `run_latches` → an `ended` Run (`end_cause = 'before-runs'`) under its instance when known; `run_latches` rebuilt with `REFERENCES runs(run_id) ON DELETE CASCADE`. Labels are not read by SQL; the next adoption or start applies them.
- **Engine version.** New public enum variants (`EngineEvent::Run`, `Case::Chain`, `CoverTag::AncestorCeilings`), the run id shape, schema v13 (one-way) and the relaunch semantics: hekma-engine 0.11.0 → 0.12.0; semver baseline SHA bump after merge (ci.yml + test_automation.py).
- **Single writer per process until 21.4.** As 21-1: each engine process has its own writer and snapshot; cross-process writes serialize through `BEGIN IMMEDIATE`. A child opened by a CLI engine is visible to `hekma serve`'s snapshot only after its next reload (none until 21.4) — recorded in deferred work.

## I/O Impact Matrix

| Surface | Before | After |
|---|---|---|
| state.db | v12 | v13 (Run tables, latch FK, backfill; one-way) |
| run id shape | `run-<nanos>-<nonce>` | `run_` + ULID (legacy accepted) |
| Restart Policy relaunch | new Run, per-run budgets/latch/fence reset | same Run, they carry |
| Event bus / outbox / SSE / export | transition, breach, usage | + `run` events |
| Control API | `/v0/instances…` | + `POST /v0/runs`, `GET /v0/runs/{id}`, `POST /v0/runs/{id}/close` |
| CLI | `hekma agent …` | + `hekma run open|close|show` |
| Instance config | — | `labels.<key>` |
| Governance Vectors | 1.1.0 | 1.2.0 (`chain` kind) |
| hekma-engine | 0.11.0 | 0.12.0 |

## Code Map

- `crates/hekma-engine/src/domain/runs/{mod,types,verbs,snapshot,labels}.rs` (new) — the Run model, table, snapshot, label grammar.
- `crates/hekma-engine/src/store/run_store_sqlite.rs` (+ a `runs_sqlite.rs` if it grows) — v13 DDL, backfill, Run reads/writes, ancestor fold.
- `crates/hekma-engine/src/ports/run_store.rs` — the new port methods.
- `crates/hekma-engine/src/domain/{pipeline,governance,usage,config,bus,registry}.rs`, `domain/supervisor/{commit,spawn,reaper,lifecycle,usage,mod}.rs`, `engine.rs`, `lib.rs`.
- `crates/hekma-engine/governance/vectors-v1.json{,.sha256}`.
- `crates/hekma-host/src/control/{mod,routes,dto,error}.rs`, `crates/hekma-host/src/otlp/*` (ignore Run events).
- `crates/hekma/src/lib.rs`, `crates/hekma/src/cli/run.rs` (new), `crates/hekma/tests/run_cli.rs` (new).
- `crates/hekma-conformance/fixtures/verb-depth.json`, `crates/hekma-conformance/tests/verb_depth.rs` (new).
- `.github/workflows/ci.yml`, `scripts/test_automation.py`, `Cargo.toml`, `crates/hekma-engine/Cargo.toml`.
- `docs/runs.md` (new), `docs/{architecture,embedding,control-api,governance,configuration,cli,events}.md` as they exist, `README.md`, `CHANGELOG.md`, `docs/RELEASE_NOTES.md`.
- `_bmad-output/implementation-artifacts/{deferred-work.md,sprint-status.yaml}`.

## Verification

- `cargo +stable fmt --all -- --check`; `cargo +stable clippy --workspace --all-targets -- -D warnings`; `cargo +stable nextest run --workspace --all-targets`; `python3 scripts/check_docs.py` and `--self-test`; `python3 scripts/test_automation.py`.
- Changed-line coverage from `cargo llvm-cov` over the engine, host and CLI, intersected with the diff's added lines.
- Regression gates: the supervisor suite (relaunch, adoption, fence), `tests/budget.rs`, `tests/adoption.rs`, `tests/resync_events.rs`, `tests/outbox_export.rs`, the v12 upgrade tests, the governance vectors verifier.

## Ask First

- The public-facing decisions above: the relaunch semantics change and its release note, the Run Status table and Anomaly rules, the child-Run refusals (non-`warn` actions, ended parent, close of a process-owning Run), caller-only labels at open and no label inheritance, the label grammar, the Run event shape, the Control API routes and error codes, the `hekma run` CLI, the snapshot's copy-on-write publish, vectors 1.2.0 and hekma-engine 0.12.0.

## Spec Change Log

- 2026-11-02 — spec written from the story text, the readiness findings for 21-2 (child-Run scope, Run Status table, Anomaly rules, backfill, release note, snapshot owner) and D3.
- 2026-11-02 — AI-55 round 1 (own pass, then one independent adversarial pass). Own pass: a relaunch or adoption of a kept Run that had already ended left it `ended` while its process ran (now reopened, cause `relaunched`). Adversarial pass, all fixed: (1, high) a crash while breach-paused relaunched unpaused with the `pause` latch spent — a kept Run's latched `pause`/`stop` is re-applied at relaunch (mutation-checked); (2) a planned relaunch that never ran left the Run `active` forever — engine open ends such Runs (`relaunch-not-run`), the backfill ends a `failed` instance's Run, and a later relaunch reopens it; (3) `paused → stopping` and a relaunching crash out of `paused` projected `active` and cascaded a false `parent-resumed` — `starting`/`stopping` keep the status and a relaunching crash only raises the Anomaly; (4) `hekma run open --parent run-bot` read an instance name as a run id — only the ULID and legacy shapes count; (5) a padded label value passed the write check but bound nothing — the raw value is validated; (6) the spec said 422 where the API answers 400 `invalid_body` — spec corrected to the API's kinds; (7) a child opened under a held parent started `active` — it starts `held`; (8) usage on a child of an ended owner could act on the replacing process — an ended owner level acts on nothing; (9) a Run read fault could fail a lifecycle transition or the engine open — the projection runs in its own savepoint and degrades to a diagnostic, and the open degrades to an empty snapshot with a note.
