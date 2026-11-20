---
title: 'Bound labels — hekma label'
type: 'feature'
created: '2026-11-03'
status: 'in-review'
review_loop_iteration: 1
baseline_commit: '8ed253713bbabb7781ec4f90cbda1fc51e5e2bf1'
context:
  - '_bmad-output/planning-artifacts/epics.md (Epic 21, Story 21.9; Stories 21.3, 21.4, 21.6 for the seams)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-20 Run and Policy resources + authority matrix; AD-25 bound labels, Policy resolution; naming row: `hekma label`)'
  - '_bmad-output/planning-artifacts/prds/prd-ktesio-2026-07-02/prd.md (FR-49 labels, FR-50 Selectors and set resolution, FR-60 bound-only Policy Selectors; Glossary Selector, Caller)'
  - '_bmad-output/planning-artifacts/implementation-readiness-2026-10-30.md (D3: `hekma label` before 21.3)'
  - '_bmad-output/implementation-artifacts/spec-21-2-runs-as-first-class-entities.md (Run registry, `run_labels`, the `labels` config key, the writer jobs this story extends)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Policies (21.3) select Runs by bound labels only (FR-60), but the only bound-label source today is an instance's `labels` config key, which reaches only supervised Runs and only every Run of that instance. The Operator has no way to bind a label to one Run, to a child Run, or to a set of Runs, and nothing stops a Caller label write from taking over a bound key (21-2's `replace_labels` upsert flips a bound row to `caller` on conflict).

**Approach:** A label verb in the engine core, served three ways: the facade (`Engine::label_run`, `Engine::select_runs`), the Control API (`GET`/`PATCH /v0/runs/{id}/labels`, Operator bearer only) and the CLI (`hekma label set|remove`, by run id, instance or `--selector`, with `--dry-run`, `--yes`, `--json`). Each bound label records its origin (`config` or `label-verb`, schema v14) so configuration re-syncs and the verb never overwrite each other. One guard refuses any Caller write to a bound key. Every bound-label change is one `labels-changed` event and calls a Policy re-resolution hook inside the same writer job — a no-op that 21.3 fills.

## Tasks & Acceptance

**Execution:**
- [x] `domain/runs/` — `LabelOrigin`; `Label.origin`; `labels.rs` gains the pure planners (`plan_label_change`, `plan_config_sync`) and `check_caller_labels`; `selector.rs` (new): `Selector` parse + `matches`; `policy.rs` (new): the `labels_changed` hook; `ops.rs`: `label` (the verb job) and `sync_bound_labels` rebuilt on the planner; `RunError` gains `RunEnded`, `LabelFromConfig`, `LabelNotBound`, `LabelBound`, `TooManyRuns`.
- [x] `store/` — schema v14 (`run_labels.origin`, backfilled `config` for bound rows); `RunRecords` gains `set_run_labels` (one Run's whole label set) and `select_runs`; the caller upsert never overwrites a bound row.
- [x] `engine.rs` + `domain/supervisor/runs.rs` — `label_run` (async + blocking), `select_runs`; a process-owning Run's config labels are resolved before the job so a removed verb label restores a shadowed config value.
- [x] `RunEvent` gains `prior_labels`; cause `label-verb` / `config`.
- [x] `hekma-host` — `GET` and `PATCH /v0/runs/{id}/labels`, Operator bearer; error codes mapped.
- [x] `hekma` CLI — `hekma label set|remove` with target/selector resolution, confirmation, dry run, per-Run results, exit classes.
- [x] Docs (`docs/runs.md`, `docs/control-api.md`, `docs/commands.md`, `docs/events.md` as they exist, `docs/policies.md`, `README.md`), CHANGELOG + RELEASE_NOTES (Unreleased), deferred work, sprint status.

**Acceptance Criteria:**
- Given an open Run R, when the Operator runs `hekma label set R team=core`, then R carries `team=core` bound with origin `label-verb`, one `labels-changed` event (cause `label-verb`, `prior_labels` = the bound set before) is on the bus and in the outbox, the published snapshot shows it, and `GET /v0/runs/R/labels` returns it; repeating the same set changes nothing and emits nothing.
- Given a supervised Run whose instance config binds `team=payments`, when the Operator sets `team=core` on it, then `team=core` wins; a later `labels.*` config write, relaunch or adoption keeps `team=core`; `hekma label remove R team` restores `team=payments` (origin `config`) in the same change; removing a key bound only by configuration is refused (`label-from-config`, exit 4 / 409) naming the config key.
- Given a Run with Caller label `task=x`, when the Operator sets `task=y`, then the label is bound `task=y` and `prior_labels`/the per-Run result name the replaced Caller value; removing a Caller-only key with the label verb is refused (`label-not-bound`); removing an absent key is an unchanged success.
- Given any Caller-label write (child open today), when it names a key already bound on the Run, then it is refused with `label-bound` and nothing is written; the store's caller upsert never turns a bound row into a caller row.
- Given an ended Run, then `label set|remove` is refused (`run-ended`, exit 4 / 409); given no Run with the id, `run-not-found` (exit 3 / 404).
- Given `--selector team=core,env=prod`, then the matched set is every non-ended Run carrying both labels (bound or Caller); with more than one match the CLI lists the set and asks to confirm (a non-terminal stdin without `--yes` is refused with exit 2 and never blocks); `--dry-run` prints the set and each Run's planned change and writes nothing; with `--yes` it labels exactly the listed Runs and reports each Run's outcome; a Run refused mid-set does not stop the others and the exit code is the first failure's class; more than 1000 matches is refused naming the cap; no match is exit 0 with a note on stderr.
- Given `PATCH /v0/runs/{id}/labels` with `{"set":{…},"remove":[…],"dry_run":bool}`, then it answers 200 with the labels after (or as planned), `changed` and `prior_labels`; a malformed key or value, a key both set and removed, or an empty change is 400 `invalid_body`; no bearer is 401.
- Given a bound-label change from any path (verb, config write, relaunch, adoption), then the Policy hook runs in the same writer job with the Run's id (pinned by a test).
- Given a v13 state dir with config-bound labels, when the engine opens, then `user_version` is 14 and every bound row has origin `config`, and Caller rows have none.
- Given handoff: fmt, clippy `-D warnings`, the workspace tests, `check_docs.py` (+ `--self-test`), `test_automation.py` pass; changed-line coverage ≥ 95%; one independent review pass done.

</frozen-after-approval>

## Design Notes

- **Origin (schema v14).** `run_labels` gains `origin TEXT` (`config` | `label-verb`; NULL on a Caller row); the step backfills `config` on every bound row (21-2's only bound source). `Label` gains `origin: Option<LabelOrigin>`, serialized only when set (additive on `RunView`, events, the snapshot). Later sources add `run-key` (22.1) and `binding` (23.1). A v13 → v14 open is one crash-atomic step, like v13.
- **Precedence per key.** `label-verb` > `config` > Caller. The bound-over-Caller part is FR-49 ("a Caller cannot set a key that is already bound"); verb-over-config mirrors FR-60's rule that a limit set explicitly on a Run outranks inherited ones.
- **The planners (pure).** `plan_label_change(current, change, config)` returns the Run's next full label set or a refusal; `plan_config_sync(current, config)` replaces only `config`-origin rows, never a `label-verb` key, and a config key replaces a Caller row. One store write (`set_run_labels`) replaces the Run's rows; nothing is written when the set is unchanged.
- **Remove.** Removes a `label-verb` label; on a process-owning Run, if the instance config binds the same key, that value returns (origin `config`) in the same change. A `config`-origin key is refused with `label-from-config`, naming `labels.<key>` and the remediation (`hekma agent config set <instance> labels.<key> <value>`, or override it on the Run with `hekma label set`); there is no `config unset` verb. A Caller-only key is refused with `label-not-bound`. An absent key is unchanged. A request is all-or-nothing per Run.
- **Labels belong to the Run.** A Restart Policy relaunch and an adoption keep the Run and its verb labels; a fresh `start` mints a new Run without them. Docs point at the `labels` config key for labels every future Run of an instance should carry.
- **Caller guard.** `check_caller_labels(existing, caller)` refuses a Caller label whose key is bound (`label-bound`); every Caller-label write calls it (child open today — vacuous for a fresh Run — and the announce/identity-header paths of 22.x/23.x). Defense in depth: `insert`/upsert of a Caller row never replaces a bound row at the SQL level.
- **The Policy hook.** `domain::runs::policy::labels_changed(store, run, at) -> RunChanges` is called inside the writer job after each committed bound-label change (verb, config sync from a write, relaunch or adoption), before the snapshot entry is built, so 21.3's re-resolution commits and publishes with the label change. Today it returns no changes; a test-only recorder proves each path calls it.
- **Selectors.** `key=value[,key=value…]`, every term must match (AND); keys and values follow the label grammar; a key twice is refused. Verb and listing Selectors match bound and Caller labels (FR-49); Policy Selectors will match bound only (`Selector::matches` takes the scope, 21.3 uses `BoundOnly`). Resolution is one indexed store read over non-ended Runs (`idx_run_labels_kv` drives, the other terms probe the primary key), ordered by run id, capped at `MAX_SELECTED_RUNS = 1000` (1001 read to detect overflow, refused with `too-many-runs`). Set tokens and the Control API's selector resolution are 21.6's; until then the API is per Run and the CLI resolves the set locally, then acts on exactly those ids (FR-50's "acts on exactly that set").
- **The CLI.** `hekma label set <TARGET|--selector SEL> <key=value>… [--dry-run] [--yes] [--json]`, `hekma label remove <TARGET|--selector SEL> <key>… [same]`. Without `--selector` the first positional is the target: a run id (ULID or legacy shape) or an instance name (its open process-owning Run, as `run open --parent`). Confirmation is asked only when more than one Run matched, only on a terminal stdin (`IsTerminal`; never a read on a pipe, so a test or script cannot hang). Human output: one line per Run (`labeled`, `unchanged`, `would label`, or the refusal); `--json`: `{schema_version, dry_run, matched, results:[{run_id, outcome, labels?, prior_labels?, error?{code,message}}]}`. Exit: 0 when every Run succeeded (and on no match); else the class of the first failure (3 not found, 4 state, 2 invalid, 1 store).
- **Routing.** 21.4 merged first, so this story adopts it: the target, the Selector and a `--dry-run` are read on the read-only engine (a dry run plans on the read connection and starts no host); each write is one `PATCH /v0/runs/{id}/labels` routed through 21.4's host client (`route`), whose `error.code` maps to the same exit classes as the in-process errors.
- **Control API.** `GET /v0/runs/{id}/labels` → `{schema_version, run_id, labels}`; `PATCH /v0/runs/{id}/labels` with `{set: {key: value}, remove: [key], dry_run}` → `{schema_version, run_id, labels, prior_labels, changed}`. Operator bearer only (AD-20; the Run Key and announce classes, 22.1, never reach these routes). Errors: 404 `run-not-found`; 409 `run-ended`, `label-from-config`, `label-not-bound`, `label-bound`; 400 `invalid_body` (`invalid-request`).
- **Events.** `labels-changed` keeps `labels` (bound, after) and gains `prior_labels` (bound, before); cause `label-verb` for the verb and `config` for a configuration sync. Additive; Run event schema stays 1.
- **Version.** hekma-engine stays 0.12.0 (unreleased; 21-2 bumped it); new public items are additive within it. Schema v14 is one-way.

## I/O Impact Matrix

| Surface | Before | After |
|---|---|---|
| state.db | v13 | v14 (`run_labels.origin`) |
| `Label` / Run events | key, value, source | + `origin`; `labels-changed` + `prior_labels`, cause |
| Control API | `/v0/runs`, `/v0/runs/{id}`, `/close` | + `GET`/`PATCH /v0/runs/{id}/labels` |
| CLI | `hekma run` | + `hekma label set|remove` |
| Caller label writes | could replace a bound key | refused (`label-bound`) |

## Code Map

- `crates/hekma-engine/src/domain/runs/{mod,labels,selector,policy,ops,tests}.rs`, `domain/supervisor/runs.rs`, `engine.rs`, `lib.rs`.
- `crates/hekma-engine/src/store/{sqlite,runs_sqlite}.rs`, `ports/run_store.rs`.
- `crates/hekma-host/src/control/{mod,routes,dto,error}.rs`, `crates/hekma-host/tests/control_api.rs`.
- `crates/hekma/src/{lib,error,exit_code}.rs`, `crates/hekma/src/cli/label.rs` (new), `crates/hekma/tests/label_cli.rs` (new).
- Docs as above; `_bmad-output/implementation-artifacts/{deferred-work.md,sprint-status.yaml}`.

## Verification

- `cargo +stable fmt --all -- --check`; `cargo +stable clippy --workspace --all-targets -- -D warnings`; `cargo +stable nextest run --workspace --all-targets`; `python3 scripts/check_docs.py` and `--self-test`; `python3 scripts/test_automation.py`.
- Changed-line coverage from `cargo llvm-cov` over engine, host and CLI, intersected with the diff's added lines.
- Windows: label logic is proven at the store/ops level with no process; CLI and host tests that need a live supervised Run are Unix-only like 21-2's, and the refusal/exit-code tests run everywhere with no child process and no stdin read.

## Build Conventions

- User-facing text (docs, CLI help and messages, CHANGELOG, RELEASE_NOTES) never carries planning ids (no `21-9`, `FR-49`, `AD-25`, `D3`) and never the word "pivot"; the first production Host stays unnamed. Code comments may cite ids as the code around them does.
- Do not commit `.zcodeignore`. Do not push or open a PR.
- Tests must never hang on Windows: no stdin reads in tests (pass `--yes` or rely on the non-terminal refusal), no test that needs a live agent process off Unix, bounded timeouts on any HTTP call. Prove the label logic at the ops/store level with Runs inserted directly.
- After merging or switching, rebuild `cargo build -p hekma-conformance --bins` before running engine tests (they use the `fake_agent` binary). Disk is tight: clean only this worktree's `target/` if needed.
- 21-2 is a stacked, unmerged base; extend its code in place (`ops.rs`, `runs_sqlite.rs`, `run.rs` patterns) rather than duplicating it.

## Ask First

- The public-facing decisions above: origin and v14, the precedence order, refusing removal of config and Caller labels, labels belonging to the Run, the 1000-Run cap, no-match exit 0, the exit class on partial failure, the PATCH body, the event additions.

## Spec Change Log

- 2026-11-03 — spec written from the story text, AD-20/AD-25 and FR-49/50/60.
- 2026-11-03 — implemented as specified. Choices inside the spec's latitude: `Label::bound` stays the configuration constructor (origin `config`) and `Label::verb` is new; the per-Run `prior_labels` (CLI result, `PATCH` answer, `LabelOutcome`) is the whole label set before (bound and Caller, so a replaced Caller value is named) while the event's `prior_labels` is the bound set before; the `PATCH` answer also echoes `dry_run`; an instance target resolves through a new read, `Engine::instance_run`; a Selector matching more than 1000 Runs is exit 2 / 400 (`too-many-runs`); a declined interactive confirmation changes nothing and exits 0; `hekma run show` renders a bound label's origin (`team=payments (bound, config)`). Independent review pass not yet run.
- 2026-11-03 — independent review pass (one round, nothing high). Fixed: (2) the `PATCH` body refuses unknown fields, so a misspelled `dry_run` never writes; (4) an unknown agent target is `instance-not-found` (exit 3 / 404), not `no-open-run`, whose message no longer speaks of child Runs; (5) a removal on a process-owning Run whose configuration cannot be read is refused instead of dropping the key it would restore; (6) the label-route test pins exact statuses (400 for the engine's refusals and unparseable JSON, 422 for a wrong type or unknown field); (7) a declined confirmation under `--json` still prints its document. (1) Config labels resolved outside the job going stale across processes closes with routing: every write now runs in the one Service Host under its Registry lock. (3) Policy resolution at Run creation was already recorded in deferred work for 21.3. Not changed: the Selector's first term drives the index in the order typed, and a selection is one writer job per Run (fine at the measured 200k Runs; 21.5/21.6 own scale). Then merged 21-2's branch (which carries 21.4) and routed the CLI write through the Service Host (Design Notes: Routing).
