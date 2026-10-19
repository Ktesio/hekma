---
title: 'record_transition-after-commit: compensate the starting state on event-append failure'
type: 'bugfix'
created: '2026-10-18'
status: 'done'
review_loop_iteration: 0
baseline_commit: 'f6dbc02d62ba1ef3e15dbd451796b04d439a5ee8'
context: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** `record_transition` (crates/hekma-engine/src/domain/supervisor/mod.rs:1163) appends the durable event AFTER the store transaction has committed the new lifecycle state (start/stop/transition commit → append_event). If the append fails, the caller surfaces `EngineError::Log` — but the registry row now shows the NEW state (e.g. `running`) with NO transition event recording how it got there: the audit log and the state disagree (spec-15-6 edge-review's deferred finding; the window pre-dates 15-6, which froze the transaction-order semantics).

**Approach:** When `append_event` fails, COMPENSATE the registry row back to the `prior` state (a bounded settle-to-prior through the registry's existing write seam, best-effort with a loud cause), so the observable end-state is `prior + an error`, never `new + silence`. The `EngineError::Log` return stays the surfaced error (unchanged public behavior); if the compensation ITSELF fails, the error is augmented — a new detail line naming the double failure — so no honest path stays quiet (surfaced, not silent, AI-18).

## Boundaries & Constraints

**Always:**
- Compensation is best-effort but LOUD: on success the registry row returns to `prior` (the state the last surviving event agrees with); on compensation failure the surfaced error names BOTH the append failure and the compensation failure (a dedicated `EngineError` variant or a detail-carrying Log arm — pick whichever preserves the public error surface with an ADDITIVE variant; the existing variants must not change shape).
- The store/state and event log are the ONLY touched stores (no memory-claim, no fleet side-changes).
- Deterministic test: commit succeeds + append fails + compensation succeeds → row == prior, error == Log (or the additive variant). Commit succeeds + append fails + compensation fails → row == new (stuck), error names both (the honest stuck state).
- The bus publish + log_capture mirror (which run AFTER a successful append) are unchanged.
- 15-6's frozen semantics (transaction order) are untouched — this is failure-path only.

**Ask First:** Nothing anticipated.

**Never:** No retry loops on the append (single-shot, as today); no new public API beyond (possibly) one additive error variant; no behavior change on the success path; no changes to callers' error matching beyond what the additive variant requires (grep callers of `EngineError::Log` first).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Append fails, compensate OK | state committed `new`; log write errors | registry row settles back to `prior`; surfaced error = the Log failure (naming the compensation) | `EngineError::Log` (or additive sibling) returned |
| Append fails, compensate fails | both writes error | row stays `new` (stuck); error names BOTH failures | the augmented additive error |
| Append succeeds | normal | byte-identical behavior to today | unchanged |
| Bus publish fails after append | post-commit | unchanged (existing best-effort semantics) | unchanged |

</frozen-after-approval>

## Code Map

- `crates/hekma-engine/src/domain/supervisor/mod.rs` — `record_transition` (line ~1163): the `append_event` error arm gains the compensation call + the augmented error; a small `compensate_starting_state`-shaped helper (registry + name + prior).
- `crates/hekma-engine/src/domain/error.rs` — the additive variant if chosen (e.g. `EngineError::LogCompensationFailed { .. }` — exact shape: implementer's call within the additive constraint).
- `crates/hekma-engine/tests/lifecycle.rs` (+/or `memory_atomicity.rs`'s failure-injection harness — the 15-6 fault injector is the right tool) — the two deterministic arms (make the event log unwritable AFTER the commit: a read-only log dir mid-flight or the existing injector's append-failure point).
- `CHANGELOG.md` / `docs/RELEASE_NOTES.md` — Unreleased entries (surface-visible fix; the additive variant announced per the release-notes discipline).

## Tasks & Acceptance

**Execution:**
- [x] The compensation helper + the error arm in `record_transition`; the additive variant if needed (with caller audit).
- [x] The two deterministic test arms via the 15-6 fault-injection harness.
- [x] CHANGELOG/RELEASE_NOTES Unreleased; sprint-status 16-3 row; `deferred-work.md` — the 15-6 entry's resolved marker (the row and the marker land in this PR's bookkeeping).

**Acceptance Criteria:**
- Given a committed transition whose event append fails, then the registry row reads `prior` and the surfaced error is the Log failure.
- Given both the append and the compensation failing, then the error names both, and the row's stuck-`new` state is the documented honest outcome.
- Given a successful append, then behavior is byte-identical to today (no new events, no timing changes observable in tests).
- Gates: fmt / clippy / workspace tests / check_docs green.

## Spec Change Log

## Design Notes

- Why settle-to-prior and not settle-to-new-with-a-synthetic-event: fabricating an event after the append failed trusts the same medium that just failed; settling the row to the state the last surviving event supports keeps the invariant "state == last event's target" true without fabrication.
- The compensation runs under the supervisor lock (record_transition's caller discipline) — no new locking.

## Verification

**Commands:**
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` + fmt + clippy + check_docs — green; the two new arms visible in the lifecycle/memory_atomicity suite output.
