---
title: '11-1 Engine robustness batch — 11 honest-diagnostic and correctness fixes'
type: 'bugfix'
created: '2026-09-11'
status: 'done'
review_loop_iteration: 1
baseline_commit: 'f99e6702354098b69a48400be26ebbd203eb1cad'
context:
  - '{project-root}/_bmad-output/implementation-artifacts/epic-11-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Eleven retro action items (AI-4/7/8/9/12/13/14/15/16/41/44) record places where ktesio-engine is silently dishonest — errors that name the wrong path, pauses that report success they can't back, a crash-reaper that ignores persistent poll failures, a usage ledger that drops events on INSERT failure, O(N) lookups, and adoption that skips budget re-evaluation.

**Approach:** Land the 11 targeted fixes in `ktesio-engine` (plus the CLI `show --json` lookup and its error mapping), each with a test, preserving the engine's existing honest-state/event-surface patterns. The working tree already carries partial implementations (AI-4/7/8/9 complete; AI-12/13 scaffolding; AI-44 non-compiling) — finish, compile, and test them rather than restart.

## Boundaries & Constraints

**Always:** Each of the 11 items ships with a test asserting the new honest behavior. Persist-state-before-signal ordering matches `stop_inner`'s pattern. Error diagnostics name state + remediation (the epic-10 DiagnosticSink routing is untouched). The full battery passes: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, `python3 scripts/check_docs.py`.

**Ask First:** Any fix requiring a product decision beyond the retro's recorded fix intent (e.g. changing AI-7's chosen fail-fast-with-remediation into always-permitted resume). Any change to `ProcessFingerprint`'s public shape beyond what AI-14 needs.

**Never:** No new dependencies. No changes to the other per-instance fleet reads (`usage_totals`/`effective_config`/`cost_totals`) — AI-16 is spawn records only. No rewriting the locking model (AD-17 territory). Do not redo AI-3/AI-6 (already merged).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Resume, unsupported pause | instance `paused`, adapter declares pause `unsupported` on this OS | `EngineError::ResumeUnsupported` naming state + declaration + `stop`/re-resume remediation; state unchanged; no event appended | fail-fast, no signal sent |
| Guaranteed pause, no handle | row `running`, no in-memory handle (post-restart) | transition records best-effort cause naming the missing handle; no silent plain `pause` success | no signal possible; instance stays `running` in truth |
| Persistent poll errors | `poll_once` errors `MAX_CONSECUTIVE_POLL_ERRORS` times consecutively | treated as crash input → instance lands `failed` with cause naming persistent poll failure; streak resets on `Ok(Alive)` | no silent `None` forever |
| Adopted process exits, code unknown | adopted (non-child) process exits, `code: None` | crash cause says exit code unavailable (adopted process is not this engine's child) | not the generic "terminated by signal" text |
| Fingerprint read fails at spawn | `process_start_time(pid)` returns `None` | spawn does not record `start_time = 0` sentinel; fail closed | spawn fails with clear diagnostic (chosen approach) |
| Ledger INSERT fails | `record_usage_event` errors on a non-duplicate event | cursor does not advance past the failed event; event is retried on next drain; already-committed events keep the dedup-key safety | no silently dropped usage |
| Budget breach at adoption | adopted instance's run already over budget | budgets re-evaluated right after adoption → breach path fires (pause/stop per policy) | best-effort per `enforce_budget` contract |
| `show --json <name>` | single instance requested | single-instance registry lookup, error surfaced (not inherited fleet degradation); `NotFound` → exit 3 contract preserved | `RegistryError::NotFound` mapping unchanged |

</frozen-after-approval>

## Code Map

- `crates/ktesio-engine/src/domain/supervisor.rs` (~5900 lines) — almost every fix lands here. Sections: consts 92–110 (`MAX_CONSECUTIVE_POLL_ERRORS` at 106, currently unused); `Supervised` 369 (`adopted: bool` at 437, written never read); `Supervisor` 489 (`poll_error_streaks` at 523, unused); `start_inner` 732; `stop_inner` 1213 (persist-first exemplar); `suspend_or_resume` 1523 (AI-7 arm 1563–1569, AI-8 Guaranteed arm 1588–1617, AI-9 order 1613–1615); `signal_backend` 1663; `poll_once` 2021 (Err arm 2046 `Err(_) => None` — AI-12; crash-cause build 2138–2141 — AI-13; `running.remove` sites 2069/2103/2121/2137); `adopt_orphans` 2281 (`Supervised` insert 2315–2319 has the E0382 borrows; `enforce_budget` call 2367); `drain_usage_for` 2693 (cursor advance 2743–2745 BEFORE ingest 2746–2748); `ingest_usage` 2862 (`Err(_) => None` drop at 2912); `enforce_budget` 2945 (returns `()`, best-effort); tests mod 3624 (`write_fake_manifest` 3638, `setup_fake` 3658, `wait_for_crash` 3728)
- `crates/ktesio-engine/src/domain/error.rs` — `EngineError::ResumeUnsupported` at 283–306 (uncommitted); re-exported via `lib.rs:103–114`
- `crates/ktesio-engine/src/domain/registry.rs` — `Registry::open` 268–289 (AI-4 done + test `open_maps_path_resolution_failure_and_names_the_base` 1769); `spawn_record` 852 / `list_spawn_records` 874 (`pub(crate)`, AI-16)
- `crates/kt/src/cli/agent.rs` — `show()` 406, json branch 417–433 does `facade.fleet().find(...)` (AI-15); `map_engine_error` 2169–2345 matches `EngineError` EXHAUSTIVELY — **currently does not compile** until a `ResumeUnsupported` arm is added (model on `NotRunning` ~2306–2323); `NotFound` → exit 3 contract near 2794; test neighbors `show_json_wraps_one_entry_with_the_shared_schema_version` 2980
- `crates/ktesio-engine/src/engine.rs` — `fleet()` 561–574 + `fleet_entry_for` 582 (N+1: per-instance `spawn_record` at 590 — AI-16); `instance_status` 838–885 (pattern for the new single-instance facade method); `Blocking` mirror ~1282
- `crates/ktesio-engine/src/backends/unix/mod.rs` — spawn `process_start_time(pid).unwrap_or(0)` 265; `fingerprint()` fallback 398–403; consumer `if self.start_time != 0` 634; adopt compare 417–424; `process_start_time` 486/507/544; test exemplars 978/999/1059
- `crates/ktesio-engine/src/backends/windows/mod.rs` — same fingerprint fallback at 477 (keep both OSes consistent)
- `crates/ktesio-engine/src/ports/process_backend.rs` — `ProcessFingerprint` 189–199, `pub` port type (`start_time: u64`) — AI-14 decision point
- `crates/ktesio-engine/src/domain/usage.rs` — `RecordOutcome::{Inserted, DuplicateReplay}` at 175 (AI-41 dedup semantics)
- `crates/ktesio-engine/tests/pause.rs` — `unsupported_pause_fails_fast...` 302 (manifest helper at 40), `resume_on_a_running_instance...` 406, `guaranteed_pause_without_an_in_memory_handle...` 463 (extend for AI-8 cause pin), best-effort cause exemplar 240
- `crates/ktesio-engine/tests/adoption.rs` — self-spawning helper 176–312, `engine_kill_adopts_live_child_and_fails_gone_record` 317, `ai7_paused_live_process_is_adopted_and_resumable` 545 (AI-44 home)
- `crates/ktesio-engine/tests/metering.rs` — `self_reported_usage_lands_in_the_ledger_under_the_run_id` 119, `a_replayed_batch_does_not_double_count` 170, row-poll helpers 74–104 (AI-41)
- `crates/ktesio-engine/tests/fleet_totals.rs` — fleet surface net for AI-16 behavior-preservation
- `.config/nextest.toml` — engine-integration-serial group; determinism notes at `tests/crash.rs:149`

## Tasks & Acceptance

**Execution:**
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- Fix the three E0382s at 2315–2319 (clone `name`/`run_id`/`metering_source` into `Supervised`) so AI-44's post-adoption `enforce_budget` call compiles -- unblocks the whole tree
- [x] `crates/kt/src/cli/agent.rs` -- Add the exhaustive-map arm for `EngineError::ResumeUnsupported` (miette diagnostic naming state + stop/start remediation) -- restores `kt` compile
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12: consume the scaffolding — on `poll_once` Err increment `poll_error_streaks`; at `MAX_CONSECUTIVE_POLL_ERRORS` treat as crash input with a cause naming persistent poll failure; clear on Ok(Alive) and at every `running.remove` site; extract the streak decision into a small pure helper for unit testing -- kills the dead_code + silent `None`
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-13: read `adopted` before `running.remove`; when adopted and `code: None`, crash cause = "exit code unavailable — adopted process is not this engine's child" -- kills second dead_code
- [x] `crates/ktesio-engine/src/backends/unix/mod.rs` + `windows/mod.rs` + `ports/process_backend.rs` -- AI-14: make fingerprint-read failure at spawn fail the spawn with a clear diagnostic instead of recording `start_time = 0`; prefer failing at the spawn read site over reshaping the pub port type; keep the adopted-poll `start_time != 0` fallback semantics intact -- fail closed, no sentinel
- [x] `crates/ktesio-engine/src/engine.rs` -- AI-15: new single-instance facade method (Engine + Blocking mirror) that locks once, `registry.lookup` + reuse `fleet_entry_for`; synthesize `RegistryError::NotFound` for unknown names -- O(1) show + honest errors
- [x] `crates/kt/src/cli/agent.rs` -- AI-15: `show --json` calls the new facade method instead of `fleet().find` -- error surfacing parity with the human path
- [x] `crates/ktesio-engine/src/engine.rs` -- AI-16: `fleet()` builds one `HashMap` from `list_spawn_records()` under the held locks and threads it into `fleet_entry_for`; other per-instance reads untouched -- kills the N+1
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-41: `ingest_usage` reports failure; `drain_usage_for` advances the cursor only past committed/duplicate events and parks at the first store error; `drain_observed_for` keeps best-effort skip with a comment -- no dropped usage events
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-7/AI-8 polish: fix `resume()` rustdoc (1496–1502) to describe the new dedicated variant; extend `guaranteed_pause_without_an_in_memory_handle...` test to pin the best-effort cause -- docs + honesty pinned
- [x] Tests (per item): AI-7 `tests/pause.rs` `resume_under_an_unsupported_pause_declaration_names_the_state_and_remediation`; AI-9 transition-order unit test via the failure-injection pattern (`snapshot_write_failure_rejects_the_start_before_the_starting_transition`, supervisor.rs:4230); AI-12 pure-helper unit test + streak-clear assertions; AI-13 adopted-exit cause assertion in `tests/adoption.rs` subprocess pattern; AI-14 unix test that spawn fails when start-time read fails (pattern-match exemplars 978/999/1059); AI-15 `show_json_uses_single_instance_lookup` in agent.rs tests; AI-16 fleet behavior-preservation test with >1 instance; AI-41 ledger fault test at the drain/ingest seam (fake_agent usage-writing pattern, supervisor tests 5045+); AI-44 over-budget adoption test in `tests/adoption.rs` -- coverage stays ≥95%

**Re-derivation (review loop 1 — AI-12 amendment; re-derive this item against the amended requirements):**
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12 amendment (a): SYSTEMIC GUARD — a poll-error streak must only trip crash input when the failure is handle-specific; when the failure is environmental/systemic (e.g. multiple handles error in the same poll tick, or the error is a backend/environment classification rather than a per-pid miss), treat as transient: do not increment toward the threshold, emit a diagnostic naming the environmental condition, keep handles alive -- the graceful-degradation gate forbids a procfs/sysctl outage from mass-crashing the fleet (kill-on-drop would kill RUNNING agents)
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12 amendment (b): the persistent-poll-failure crash cause must carry the LAST poll error's text (truncated) so the operator gets the why, per the graceful-degradation gate -- not just "10 consecutive backend.poll errors"
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12 amendment (c): a WIRING TEST must execute the poll_once crash path — add a cfg(test) fault-injection seam on the backend (poll-error mode), drive poll_once to the threshold, assert the instance lands `failed` with the persistent-poll-failure cause and the handle is removed; assert the systemic guard keeps handles alive when the fault is flagged environmental -- today a regression to the old silent-swallow passes every test

**Review-1 patches (apply on top of the re-derived tree; each is caused by this change):**
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-9: on signal failure AFTER the transition committed, emit a diagnostic naming instance + committed state + signal error + remediation (retry `resume`/`stop`); the ledger/runtime divergence must never be silent -- mirrors stop_inner honesty
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-9 tests: extend the persist-first test to also assert the ledger still reads `running` (transition truly did not commit); add the guaranteed RESUME leg (persist `paused→running` before SIGCONT) to the same failure-injection pattern
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-8: in the no-handle case, a `Some(cause_override)` (e.g. BudgetExceeded) must STILL record the best-effort cause (wrapping the override as the detail) — a budget pause that suspended nothing must not read as a performed suspension; the breach event itself already carries the budget record
- [x] `crates/ktesio-engine/tests/pause.rs` -- AI-8: add `guaranteed_resume_without_an_in_memory_handle...` sibling asserting `"kind":"resume-best-effort"` + missing-handle detail (mirror 463's row-forcing pattern)
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-41: on the TERMINAL drain (crash/stop path), a still-failing INSERT after the existing retry must emit a diagnostic stating the batch is lost with the handle (no next drain exists) — the park-and-retry claim must not silently fail exactly where loss is likeliest
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-41: the ingest-failure diagnostic text must be factual per caller — the observed-channel caller has no cursor and never retries; reword so it doesn't claim "the drain will retry it" there
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` (tests) -- AI-41: repair the fault-test's store restoration to recreate the FULL schema including the `UNIQUE(instance_id, run_id, sequence)` index (today the repaired DB lacks the dedup invariant); extend the test to the partial-failure sequence — first event commits, second fails, repair, re-drift asserts `DuplicateReplay` and no double-count; assert the diagnostic fragment reaches the sink
- [x] `crates/ktesio-engine/tests/adoption.rs` -- AI-44: add the over-budget PAUSED-row adoption case — breach recorded, row stays `paused`, no strand, no further usage
- [x] `crates/ktesio-conformance/src/uj3.rs` -- fix `assert_flow_breaches`' "FIRST Run" claim: either tie the asserted pair to the flow's first run by construction or correct the comment to what is actually asserted (log-order first token breach)
- [x] `docs/architecture.md` -- update the pause paragraph that still says the Guaranteed dispatch "records a plain pause/resume command cause" — now false when no handle is held (best-effort qualifier) -- doc-currency gate
- [x] `README.md` -- verify the doc-currency gate: check whether README documents pause/resume semantics or exit codes touched by AI-7/AI-8; update if stale
- [x] `crates/ktesio-engine/src/backends/unix/mod.rs` + `windows/mod.rs` -- AI-14: the fail-closed spawn diagnostic must name the platform limitation ("no process start-time source on this platform; cannot guarantee pid-reuse safety") not read as a transient read failure
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` (tests) -- AI-9: fix the stray-spaces artifact in the persist-first test's assert message

**Review-2 patches (loop 2 found no bad_spec; all caused by this change; apply on the current tree):**
- [x] `crates/ktesio-engine/src/engine.rs` -- AI-16: on batched `list_spawn_records()` Err, fall back to per-instance `spawn_record` reads (restore per-row degradation; the batch read kills the N+1 in the happy path only) and fix the comment falsely claiming the batch failure degrades "exactly like" the old per-instance failure
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12: SOLE-HANDLE honesty — when a handle errors and it is the only held handle, the crash cause and a pre-crash diagnostic must state the failure could not be corroborated against other handles (single-handle fleet); detection itself stays (documented residual hole)
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12: cap consecutive environmental ticks (e.g. 40) — past the cap, resume per-handle streak credit with a diagnostic, so a persistently broken/flaky PAIR cannot defeat crash detection forever
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-12: fix the garbled environmental diagnostic (literal space runs from source wrapping) and truncate the joined per-handle error strings (200-char bound like `poll_last_errors`)
- [x] `crates/ktesio-engine/src/backends/unix/mod.rs` + `windows/mod.rs` -- AI-14: on `verified_spawn_start_time` failure, KILL the just-launched child (unix: process group) before returning the Spawn error — the raw Child drop does not kill a setsid child, orphaning it with no handle and no record; distinguish a pid-already-gone child (agent exited instantly — surface that, not a platform error) and retry the read briefly (bounded) before failing
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` (tests) -- AI-9: behavioral test for the post-commit signal-failure branch — cfg(test) knob forcing `signal_backend` to error after the persist; assert the divergence breadcrumb (instance + committed state + error + remediation) reaches the capture sink and the command returns Err
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-9: the pause-arm signal-failure remediation must NOT advise `resume` when the pause was breach-driven (`BudgetExceeded` — latch spent, resume would leave an over-budget agent running): recommend `stop` there, `resume` otherwise
- [x] `crates/ktesio-engine/tests/pause.rs` (or supervisor tests) -- AI-8: pin the override honesty wrap — `BudgetExceeded` pause with no in-memory handle records `pause-best-effort` whose detail wraps the override
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` (tests) -- AI-41: execute the Terminal-drain loss notice — stage an insert failure while a Terminal drain runs and assert the loss text (with count) reaches the sink
- [x] `crates/ktesio-engine/src/domain/supervisor.rs` -- AI-41: bound the park — after K (3) failed retries at the same cursor offset, skip the event with a loud diagnostic naming the lost event (billing honesty: announce the skip), so a permanently poisoned row cannot wedge the cursor forever
- [x] `crates/ktesio-engine/tests/adoption.rs` -- AI-44: add a warn-breach-action variant (row genuinely `running` and over budget at adoption) → adoption-time enforcement records the breach without a transition, row stays `running`
- [x] `crates/kt/src/cli/agent.rs` -- AI-7: reframe the CLI remediation — lead with the actionable `stop && start`, demote the OS suggestion to informational
- [x] `docs/architecture.md` -- document the AI-12 environmental corroboration exemption AND its single-handle residual hole in the survival paragraph (doc-currency gate)
- [x] `docs/commands.md` -- extend the no-handle honesty sentence to guaranteed RESUME and fix "the signal was best-effort" mischaracterization (nothing was signalled; the transition is recorded best-effort because no handle exists)
- [x] `crates/ktesio-engine/tests/fleet_totals.rs` -- extend the fleet-vs-show comparison to the FULL entry (all fields), pinning byte-identity beyond the current subset

**Review-2 rejects (do not act):** `fleet_entry` propagating spawn-record errors while `fleet()` degrades is the FROZEN AI-15 intent (show surfaces, fleet degrades); `list_spawn_records` pre-existed (registry.rs:874, used by adopt_orphans — no missing hunk); the no-handle cause catch-all is unreachable for non-Pause/Resume commands (`suspend_or_resume` is private, called only from pause*/resume*); the `is_linux_ci` adoption-test skips are the pre-existing #109 mitigation pattern, not this story's defect.

**Acceptance Criteria:**
- Given the working tree, when `cargo check --workspace --all-targets` runs, then it compiles with zero errors and zero dead_code warnings
- Given each of the 11 items, when its test runs, then it asserts the honest behavior described in the I/O matrix and fails against the pre-fix code's behavior where distinguishable
- Given a budget-breached run is adopted after engine restart, when the reaper settles, then a breach event exists and the instance is paused/stopped per policy without further usage
- Given a usage event whose INSERT fails, when the next drain runs, then the cursor has not advanced past it and the event commits on retry exactly once (dedup key holds)
- Given `kt agent show <name> --json` on an unknown name, when it runs, then exit code 3 with the NotFound diagnostic (M2 contract unchanged)
- Given the full battery (fmt, clippy -D warnings, tests, check_docs), when run at story end, then all green

## Spec Change Log

- 2026-09-11 — Approval note: spec approved on autopilot per the standing per-epic workflow (spec approvals delegated, memory 2026-09-07); the frozen block restates the Islam-approved sprint change proposal 2026-09-10 §3 Story 11-1 table, so the human-owned intent is the ratified proposal itself. Token count ~1.9k (above the 1600 proposal); kept whole because the user commissioned the full epic in one run ("all Epic 11 in 1 PR") — [K] pre-given.
- 2026-09-11 — LOOP 1 (bad_spec): Three-lens review of the first derivation returned one bad_spec cluster on AI-12 — the Edge-Case reviewer showed a SYSTEMIC poll failure (procfs/sysctl outage) trips every handle's streak in the same tick → fleet-wide false crashes with kill-on-drop killing RUNNING agents (a new failure mode this change introduces; the graceful-degradation gate forbids it); the verification-gap reviewer proved the poll_once crash path has no wiring test (a regression to the old silent-swallow passes everything); the blind hunter added that the crash cause drops the last error text. Root cause is the spec's AI-12 task, outside the frozen block (the frozen matrix row stays true for per-handle death; nothing in the ratified AI-12 intent commissions mass-crashing). Amended: added the AI-12 re-derivation tasks (systemic guard, last-error text, wiring test via cfg(test) fault-injection seam) + 13 patch findings (AI-9 signal-failure diagnostic + resume-leg/ledger tests, AI-8 override honesty wrap + resume test, AI-41 terminal-drain diagnostic + caller-factual text + sound fault-test repair with full-schema restore/partial-failure/dedup-replay/sink assertion, AI-44 paused-adoption test, uj3 FIRST-Run claim, architecture.md AI-8 paragraph, README currency check, AI-14 diagnostic wording, cosmetic assert message). Defers recorded in deferred-work.md: Windows adopted-exit-code retrieval + Windows AI-14 runtime test (→ 11-5), observed-channel durability (→ 11-6). Rejected: fleet-vs-show degradation asymmetry (ratified AI-15 intent — show surfaces, fleet degrades); "sprint artifacts missing" (false — diff scope excluded _bmad-output, sprint-status.yaml is updated). REVERT DECISION: full `git checkout` reversion was judged NET-HARMFUL — ten of eleven items are correct, tested, and interlocked with the CLI error mapping; instead the original implementer subagent is re-engaged with its context intact to (1) re-derive AI-12 against the amended tasks and (2) apply the patches, with the KEEP list below. KEEP (must survive re-derivation): AI-4/7/8/9/13/14/15/16/41/44 implementations and their tests as derived; the `poll_verdict` pure-helper + streak-hygiene tests (the amendment narrows the WIRING, not the helper); `EngineError::ResumeUnsupported` + CLI arm + exit-5 classification; `Engine::fleet_entry`/`Blocking::fleet_entry` + AI-16 batched map; AI-41 ingest-first cursor settlement; persist-first ordering; the uj3 per-(dimension, Run) breach assertion (fix only its comment/binding); docs/architecture.md + commands.md AI-7/8/9 updates; sprint-status annotation style with verbatim AI ids.

- 2026-09-11 — LOOP 1 IMPLEMENTED (same agent, context intact): AI-12 re-derived — the systemic guard is SAME-TICK CROSS-HANDLE CORROBORATION (chosen over error classification: the port carries no honest environment-vs-handle distinction an OS backend could report); poll_once now polls every held handle once up front, >1 errored handle in a tick = environmental (one diagnostic, no streak increment, handles alive), a lone erroring handle keeps the streak path; the crash cause carries the last error's text truncated at 200 chars; the fault-injection seam is cfg(test) ON THE SUPERVISOR (a pid set consulted at the backend poll boundary) because the embed-clean global-cell audit (with teeth, zero allowlist) forbids the OnceLock+Mutex static a backend-module seam needed — wiring tests prove both directions. All 13 patches applied as written; README verified NOT stale (generic 'honest per-OS' phrasing, no exit-code table). AI-44 tests sharpened to require a breach under the ADOPTED Run's fresh id (with engine-1's own breach made deterministic first — a fast exit could otherwise race the pause). Battery green (fmt/clippy/tests/check_docs; windows-target clippy clean). NOTE: the implementer's session died on an infrastructure credential error AFTER all edits landed and before its report — the tree was verified green (1158 tests) by the orchestrator.
- 2026-09-11 — LOOP 2 (patches only, no loopback): three-lens re-review of the full diff. No intent_gap/bad_spec. 15 patch findings routed (see "Review-2 patches"): fleet() batch-read blast radius + per-instance fallback; AI-12 sole-handle honesty + environmental-tick cap + garbled diagnostic text; AI-14 orphan-the-child-on-failed-verification leak (kill before error) + pid-gone distinction + bounded retry; AI-9 signal-failure behavioral test + budget-safe remediation; AI-8 override-wrap test; AI-41 Terminal-loss execution test + bounded park (K=3) so a poisoned row can't wedge the cursor; AI-44 warn-action variant; AI-7 CLI remediation reframe; architecture/commands doc currency; full-entry fleet-vs-show pin. 4 rejects recorded (frozen AI-15 asymmetry; pre-existing list_spawn_records; unreachable match arm; pre-existing is_linux_ci skip pattern).

- 2026-09-11 — LOOP 2 IMPLEMENTED (orchestrator + test subagent; the original implementer's session was unrecoverable after the loop-1 credential failure): all 15 patches applied. Production: fleet() distinguishes batch-failed (per-instance fallback, per-row degradation restored) from batch-ok (map-only, N+1 stays dead); AI-12 gains the sole-handle corroboration caveat in the crash cause (`CrashInput::PersistentPollFailure { sole_handle }`), the 40-tick environmental cap with a one-shot escalation diagnostic, and the fixed/truncated environmental text; AI-14 spawn read gains a 3-attempt retry, an exited-instantly-vs-platform-failure distinction, and an EXPLICIT group kill before the unix error return (std Child does not kill on drop — the leak was real); AI-9's pause remediation is budget-safe (BudgetExceeded => stop-only advice); AI-7's CLI/engine text leads with `stop && start` and demotes the supported-OS resume to informational; docs (architecture survival paragraph incl. the stated residual hole, commands pause/resume paragraph) updated in the same change. Tests: AI-9 post-commit signal-failure behavioral test (new `signal_fault_names` cfg(test) seam, budget-driven leg included), AI-8 override-wrap test (supervisor lib — `pause_with_cause` is private, no facade path), AI-41 Terminal-loss execution test, AI-44 warn-breach-action adoption variant (`budgeted_warn_survivor`), fleet-vs-show full-entry serde deep-equality pin. Full battery green: fmt, clippy -D warnings (host + x86_64-pc-windows-gnu), 1162 tests / 0 failed, check_docs (26 files).

## Design Notes

- AI-12 (loop-1 amendment): the crash-input threshold exists to catch an UN-POLLABLE HANDLE (its pid vanished into an unreadable state while others read fine). An environmental outage (procfs/sysctl unreadable) is the backend's condition, not the handle's — tripping every handle's streak then would kill healthy agents via kill-on-drop and arm restarts fleet-wide. The guard may be shaped as same-tick cross-handle corroboration or error classification; either is acceptable if a wiring test proves both directions (handle-specific → failed; environmental → alive + diagnostic).

- AI-7 is deliberately fail-fast-WITH-remediation, not always-permitted resume: SIGKILL-based `stop` genuinely works on a SIGSTOPped process, so `stop` + `start` is a real escape hatch. The retro's alternative ("always permitted resume") would need a transition+qualifier path that doesn't exist for Unsupported. Don't relitigate here.
- AI-14: reshaping `ProcessFingerprint.start_time` to `Option<u64>` is semver-visible on a `pub` port type — the retro offers both directions; choose the spawn-site failure unless a consumer forces the type change. If the type change wins anyway, update every constructor in both backends in the same commit.
- AI-41 dedup safety: the UNIQUE dedup key makes re-drifting an already-committed event safe (`DuplicateReplay`), which is why parking the cursor at a failed event cannot double-count committed neighbors.
- `scripts/check_docs.py` may pin error text — run it after error-string changes.

## Verification

**Commands:**
- `cargo check --workspace --all-targets` -- expected: clean compile, no dead_code warnings
- `cargo fmt --all --check` -- expected: no diffs
- `cargo clippy --workspace --all-targets -- -D warnings` -- expected: zero warnings
- `cargo test --workspace --all-targets` -- expected: all pass (battery ~950+; engine-integration-serial group respected via nextest config)
- `python3 scripts/check_docs.py` -- expected: pass

## Suggested Review Order

**The AI-12 crash-input pipeline (the story's deepest change)**

- Streak verdict pure helper — the threshold decision, unit-testable in isolation.
  [`supervisor.rs:214`](../../crates/ktesio-engine/src/domain/supervisor.rs#L214)
- Phase-1/2 corroboration in the reaper: multi-handle same-tick failures are environmental, not crashes.
  [`supervisor.rs:2358`](../../crates/ktesio-engine/src/domain/supervisor.rs#L2358)
- The 40-tick cap lifts environmental immunity so a broken pair can't defeat detection forever.
  [`supervisor.rs:2438`](../../crates/ktesio-engine/src/domain/supervisor.rs#L2438)
- Sole-handle honesty: a lone uncorroborated handle's crash cause says so.
  [`supervisor.rs:2530`](../../crates/ktesio-engine/src/domain/supervisor.rs#L2530)
- Wiring tests prove both directions (handle-specific → failed; environmental → alive).
  [`supervisor.rs:6893`](../../crates/ktesio-engine/src/domain/supervisor.rs#L6893)

**Pause/resume honesty (AI-7/8/9)**

- The persist-first pause/resume path: transition commits before the signal; signal failure emits the divergence breadcrumb.
  [`supervisor.rs:1714`](../../crates/ktesio-engine/src/domain/supervisor.rs#L1714)
- The dedicated resume-under-unsupported error: names state + declaration + stop/start recovery.
  [`error.rs:297`](../../crates/ktesio-engine/src/domain/error.rs#L297)
- CLI arm leads with the actionable stop && start; OS suggestion demoted to informational.
  [`agent.rs:2217`](../../crates/kt/src/cli/agent.rs#L2217)
- Behavioral test: post-commit signal failure → breadcrumb on the sink, Err returned, row committed.
  [`supervisor.rs:7341`](../../crates/ktesio-engine/src/domain/supervisor.rs#L7341)

**Billing honesty (AI-41/44)**

- Ingest-first cursor settlement: the cursor only advances past durable bytes.
  [`supervisor.rs:3250`](../../crates/ktesio-engine/src/domain/supervisor.rs#L3250)
- The bounded park: 3 failed passes at one offset → loud skip, cursor un-wedged.
  [`supervisor.rs:129`](../../crates/ktesio-engine/src/domain/supervisor.rs#L129)
- Post-adoption budget re-evaluation closes the crash-gap.
  [`supervisor.rs:2825`](../../crates/ktesio-engine/src/domain/supervisor.rs#L2825)

**Spawn fail-closed (AI-14) — look for the orphan fix**

- The verified read: 3-attempt retry, exited-instantly vs platform-failure distinction, explicit group kill before the error (std Child does NOT kill on drop).
  [`unix/mod.rs:318`](../../crates/ktesio-engine/src/backends/unix/mod.rs#L318)
- Windows mirror: job-close owns the kill; same retry/distinction.
  [`windows/mod.rs:401`](../../crates/ktesio-engine/src/backends/windows/mod.rs#L401)

**Fleet surface (AI-15/16)**

- Single-instance lookup, errors surfaced (the frozen AI-15 intent: show surfaces, fleet degrades).
  [`engine.rs:609`](../../crates/ktesio-engine/src/engine.rs#L609)
- Batched spawn-record read with per-instance fallback on batch failure — N+1 dead on the happy path, per-row degradation preserved.
  [`engine.rs:590`](../../crates/ktesio-engine/src/engine.rs#L590)
- Full-entry serde deep-equality between show row and list row.
  [`fleet_totals.rs:285`](../../crates/ktesio-engine/tests/fleet_totals.rs#L285)

**Docs + remaining tests**

- Survival paragraph: corroboration exemption + the stated single-handle residual hole.
  [`architecture.md:138`](../../docs/architecture.md#L138)
- Pause/resume paragraph: no-handle honesty extended to resume; "signal was best-effort" mischaracterization fixed.
  [`commands.md:147`](../../docs/commands.md#L147)
- Adoption tests: paused-row and warn-action variants for AI-44.
  [`adoption.rs:846`](../../crates/ktesio-engine/tests/adoption.rs#L846)
