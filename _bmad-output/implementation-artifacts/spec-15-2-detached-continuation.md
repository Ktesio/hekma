---
title: 'Detached continuation: run persistence, honest logs between commands, paused-stray handling, supervision indicator'
type: 'bugfix'
created: '2026-10-16'
status: 'done'
baseline_commit: '9f58225a15a6d093fd7213db0a8c2f7b9fe34522'
review_loop_iteration: 0  # implemented 2026-10-16 on feat/detached-continuation
context:
  - '{project-root}/_bmad-output/implementation-artifacts/sprint-status.yaml'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** A detached instance is not really CONTINUED across commands: every `hekma` command re-adopts it and mints a fresh Run — so per-run budgets/usage reset to 0 (Fibery 206) and an already-breached budget RE-FIRES on every command (duplicate breaches.log records + repeated actions; Fibery 205). Between commands, `agent logs --follow` shows nothing new (the attributed log is only written by a live engine's tailer; Fibery 207). A detached child paused then released stays SIGSTOPped and unsupervised forever (Fibery 178). And Fleet surfaces cannot show whether an instance is detached or session-bound (Fibery 176).

**Approach:** Persist the `run_id` on the spawn record (v9 additive migration, the v8 `acp_session_id` pattern) and RESUME it at adoption instead of minting; make the adoption-time enforcement treat breaches already recorded for that run as spent (breaches.log already keys events by run_id). Add a raw-`agent.log` fallback read with honest attribution when the attributed log is stale. On drop of a DETACHED paused handle, resume the child BEFORE release with a surfaced note (adoption keeps the pinned stays-paused contract, note-only). Add a supervision indicator to Fleet surfaces from the already-loaded `SpawnRecord.detach`.

## Boundaries & Constraints

**Always:**
- AI-18 surfaced-not-silent: the logs fallback labels raw lines honestly (un-attributed passthrough); the paused-release note and the stranded-paused adoption note go through the DiagnosticSink; no silent behavior changes.
- AI-44 intent preserved: an over-budget adopted instance still ends up acted upon — once per RUN, not once per command. A genuine restart (stop/start) still mints a fresh Run and resets the latch.
- Additive schema only: v9 nullable `run_id` on `spawn_records`; pre-v9 records → None → mint (back-compat). Cursor semantics unchanged (adoption still re-anchors at log end; park-loss note stays true).
- Frozen JSON surfaces: new FleetEntry field is additive with `skip_serializing_if` where absence is meaningful; human cells follow the AI-43 width discipline (compact marker, honest `—` absence cell); frozen key-set tests re-pinned in the same change.
- `Supervised.adopted` flag and existing adoption notes are untouched in wording unless a test pin moves with them.

**Ask First:** Any change that would auto-resume a paused ADOPTED row (breaks the pinned AI-7 stays-paused contract) — adoption is note-only for paused rows.

**Never:** No new engine-observed listener re-anchoring; no Windows paused-stray work (job-close kills them — none exist); no rotation events fabricated for the raw fallback (raw agent.log is unrotated); no billing/metering semantics change.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Re-adopt a breached detached instance | budget breached in Run R, engine exits, next command adopts | Run R RESUMES; NO new breach event for an already-recorded scope (latch = breaches already recorded for R); no duplicate breaches.log line | N/A |
| Genuine restart after breach | stop + start the same instance | fresh Run minted; latch empty; a still-over-ceiling cumulative enforces again (unchanged) | N/A |
| Per-run usage across commands | detached instance meters 40 tokens in cmd A; `show` in cmd B | per-run totals still show 40 (same run_id), cumulative unchanged | N/A |
| Pre-v9 spawn record | record without run_id adopted | run_id minted (back-compat default), one-time note | N/A |
| logs --follow between commands | detached instance writes raw agent.log after the attributed log goes stale | fallback reads raw agent.log lines, labeled un-attributed (once-per-session qualifier, not per-poll) | attributed file absent → raw only; both absent → honest empty |
| Paused detached handle released | `pause` then command exit on a detached instance | child SIGCONTed BEFORE release + surfaced note (never stranded frozen) | resume signal failure → note names it; child still released |
| Already-stranded paused row adopted | paused row from an engine that died mid-pause | adoption keeps it paused (AI-7 pin) + surfaced stranded-paused note | N/A |
| Fleet surfaces | detached + running instance; session-bound instance; stopped (record cleared) | list/show carry a compact detached marker / `—` absence; show --json carries the additive field | stopped rows never imply supervised |

</frozen-after-approval>

## Code Map

Investigated 2026-10-16 (anchors verified):
- Run minting — `crates/hekma-engine/src/domain/supervisor/spawn.rs:741` (start/restart) and `reaper.rs:640` (adoption — THE resume site); `Supervised` insert spawn.rs:753-788 (latch :778), reaper.rs:699-722.
- Adoption-time enforcement — `reaper.rs:825-831` calls `enforce_budget`; latch `Supervised::breached_scopes` keyed (dimension, scope) `usage.rs:724-729`; breach writers `usage.rs:950-1001`; breaches.log append `supervisor/mod.rs:1417-1426` via `persist_breach_event` `usage.rs:1016-1063` (events carry run_id — usable as the durable spent-check); read side `usage.rs:1081-1095`.
- SpawnRecord — `ports/state_store.rs:35-70` (detach :58, acp_session_id v8 :59-69); sqlite trio `store/sqlite.rs:958/:1087/:1104` + row builder :1264-1290; write site spawn.rs:562.
- Per-run reads — `run_usage_totals` sqlite.rs:903-923, `run_cost_totals` :942-956; fleet composition `engine.rs:692-698` (and :601-641) via `current_run_id` `interaction.rs:195-197`.
- Logs — attributed read `interaction.rs:218+` (rotated generations, cursor shape); tailer lifetime `process_backend.rs:915-923`; adopted handle `log_capture: None` `backends/unix/mod.rs:556-586` (test :2346); raw path helpers `registry.rs` ~971-999; CLI follow `crates/hekma/src/cli/agent.rs:1406+`.
- Pause — group SIGSTOP/SIGCONT `unix/mod.rs:484-505`; lifecycle persist-first `supervisor/lifecycle.rs:413-447`; adoption state-keep `reaper.rs:612-614`; AI-7 pin `tests/adoption.rs:1087` (:1124 stays paused), Windows none-exist `adoption.rs:1014`.
- Fleet surfaces — `FleetEntry` `domain/fleet.rs:430-482` (additive patterns :462/:471, honest `—` `METERING_SEED_CELL` :484-491); compose `engine.rs:643-680` (record in hand at :657; detach read today only by `reaper.rs:606-611`); CLI list `agent.rs:982-1095` (width discipline :1014-1021), show `agent.rs:478-538+`; JSON docs `agent.rs:405-477`; schema version pin `agent_cli.rs:3493`.
- Tests to re-pin — adoption.rs:1212/:1297/:1379 (AI-44 fresh-run assertions), supervisor/tests.rs:5019/:5128, agent_cli.rs:5212/:5272/:5727/:5755/:5942/:6004/:6441/:6497/:6603, fixture `sample_fleet_entry` agent_cli.rs:3255, embed_clean.rs:389, resync_events.rs:504.

## Tasks & Acceptance

**Execution:**
- [x] `ports/state_store.rs` + `store/sqlite.rs` -- v9 additive migration: nullable `run_id` on `spawn_records`; record field + row builder + trio; write site spawn.rs:562 -- durable Run identity.
- [x] `reaper.rs` -- resume the persisted run_id at adoption (:640) instead of minting; mint only when None (pre-v9) or the process is genuinely new; build the durable latch by seeding `breached_scopes` from breaches.log events already recorded for that run_id.
- [x] `reaper.rs` -- pause×detach: SIGCONT a paused DETACHED child at handle drop BEFORE release + note; adoption-time note (not auto-resume) for stranded paused rows.
- [x] `interaction.rs` + `cli/agent.rs` -- raw agent.log fallback read (labeled un-attributed, once-per-session) when the attributed log is stale/absent; follow loop surfaces the qualifier.
- [x] `domain/fleet.rs` + `engine.rs` + `cli/agent.rs` -- supervision indicator: additive FleetEntry field composed from `record.detach` + live-handle truth; compact human marker + honest `—`; re-pin frozen key-set/schema tests + fixtures.
- [x] Docs — `docs/commands.md` (detach section: run continuation, logs fallback qualifier, paused-release semantics), `docs/architecture.md` run-model note; CHANGELOG/RELEASE_NOTES announcements (additive JSON field).
- [x] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- story 15-2 row.

**Acceptance Criteria:**
- Given a detached instance breached in Run R and N subsequent commands, then breaches.log holds exactly ONE event per breached scope for R (no re-fire) and the action fired once.
- Given `stop`+`start` of that instance, then a fresh Run mints and the latch resets (unchanged restart semantics, still pinned by the control-leg test).
- Given a detached instance that metered usage in a prior command, when `show` runs in a later command, then per-run totals reflect the continued Run (non-zero), cumulative unchanged.
- Given a pre-v9 state DB, when adopted, then run_id mints with a one-time note (back-compat).
- Given a detached instance writing output between commands, when `logs --follow` runs in a new command, then new raw lines appear with the un-attributed qualifier surfaced once.
- Given `pause` on a detached instance then command exit, then the child is SIGCONTed before release with a note; given an engine death mid-pause, then adoption keeps it paused with the stranded note.
- Given list/show on detached, session-bound, and stopped instances, then the indicator distinguishes them and stopped never implies supervised.
- Full gates green: fmt, clippy -D warnings, workspace tests, check_docs; frozen JSON surfaces re-pinned.

## Spec Change Log

## Design Notes

- Durable latch: breaches.log is append-only JSON-Lines keyed by run_id (usage.rs:1016-1063); seeding the in-memory latch at adoption = read events for the resumed run_id once, collect (dimension, scope) pairs. No new persistence surface.
- Sequence dedup: resumed Run re-uses `(instance_id, run_id, sequence)` dedup (sqlite.rs:142-143); adoption re-anchors cursors at log end (reaper.rs:641) so pre-crash lines are never re-drained — totals do not double-count.
- The 205 fix deliberately does NOT gate the adoption-time enforce_budget call: it still evaluates, but the seeded latch makes already-recorded scopes no-ops — AI-44's "adopted over-budget run is acted upon" stays true the FIRST time.

## Verification

**Commands:**
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` -- green (re-pinned suites included).
- `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `python3 scripts/check_docs.py` -- green.
**Manual checks (if no CLI):**
- A live detached e2e across two `hekma` commands (breach once → second command shows no duplicate breach; usage non-zero; logs show new lines with qualifier) — recorded in the PR body.
