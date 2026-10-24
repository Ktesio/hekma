---
title: 'Liveness and the unresponsive failure — declared liveness moves running → failed (cause unresponsive)'
type: 'feature'
created: '2026-10-24'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: 'e93b8e6'
context:
  - '_bmad-output/implementation-artifacts/epic-18-context.md'
  - '_bmad-output/implementation-artifacts/spec-18-1-readiness-probes.md (the probe-runner module, contract v1.1.0, and the per-attempt evaluators this story reuses)'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 16.2 → renumbered 18.2)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-15 amendment 2026-09-30; AD-15 lifecycle-as-data)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** A hung agent is indistinguishable from a healthy one. The reaper's poll only knows process liveness — an agent that is alive but wedged (deadlocked, stuck on a socket, looped without work) stays `running` forever, metering stalls, and nothing fails (epics.md Epic 18 preamble). The unix backend's own comments name the gap: "a heartbeat stops growing" is detectable, but only for SIGSTOP, and only implicitly.

**Approach:** Adapter-DECLARED liveness rides the same reaper cadence that already detects crashes: a new `[liveness]` manifest section (Adapter Contract additive minor — the same **v1.1.0** 18-1 opened) declares ONE liveness check — a heartbeat file's maximum age, or an HTTP check — with a single unifying semantic: **unresponsive = the declared signal has been failing for `max_age_ms`**. For a heartbeat file the signal is the file's mtime age (18-1's pure `file_fresh` evaluator, reused verbatim); for HTTP it is the check endpoint failing (18-1's bounded attempt pattern, first-failure timestamp + window). A failure moves `running → failed` with the NEW cause `TransitionCause::Unresponsive` and the EXISTING Restart Policy applies (`plan_restart` charges `restart_count`; AD-15's crash-loop threshold rules as for crashes). Liveness never probes a paused instance (epic text — a SIGSTOP'd process cannot heartbeat; the supervisor's own best-effort pause would otherwise murder every paused agent). No liveness declared → byte-identical behavior to today.

## Tasks & Acceptance

**Execution:**
- [x] Manifest section — `crates/hekma-adapter-api/src/manifest.rs`: `Liveness` section struct (`#[serde(deny_unknown_fields)]`, the house style), exactly ONE check: `heartbeat_file { path, max_age_ms }` or `http_get { url, expected_status, max_age_ms }`. Validation mirrors 18-1's family exactly (as 18-1's review round ENFORCED it): `max_age_ms` 1..=600_000; `http_get.url` targets a literal loopback address (`127.0.0.0/8`, `[::1]`; port 1..=65535) — no hostnames, no DNS; `expected_status` 100..=599; no control characters; `path` non-empty. Relative `path` resolves against the Agent Home (18-1's `file_fresh` rule); a directory is never a heartbeat (18-1's `is_file` rule). Contract version STAYS `1.1.0` (18-1 opened the epic's minor; both sections ship in one release — an intermediate engine that knows `[readiness]` but not `[liveness]` never exists). The v1.0.0-engine rejection pin gains a `[liveness]` case. Manifest docs + freeze ledger updated.
- [x] Liveness evaluator — `crates/hekma-engine/src/domain/liveness.rs` NEW: per-attempt evaluators reusing 18-1's primitives (`readiness::file_fresh` for the heartbeat — re-export or lift to a shared pure fn, do NOT fork it; the HTTP attempt cap/read-cap/status-line parse extracted to a shared helper both modules call). Per-attempt result: `Fresh | Failing`. The WINDOW state (first-failure `Instant` per instance, in-memory on the supervised handle) is the supervisor's bookkeeping: `Failing` for < `max_age_ms` → still running (a transient blip inside the window recovers silently-but-honestly: recovery clears the timestamp; ONE diagnostic when a first failure is observed and ONE when it recovers, the surfaced-not-silent rule); `Failing` for ≥ `max_age_ms` → the transition.
- [x] Reaper wiring — `crates/hekma-engine/src/domain/supervisor/reaper.rs`: a new phase AFTER crash handling, for each instance the supervisor HOLDS whose persisted `LifecycleState` is exactly `Running` AND whose resolved manifest declares `[liveness]`: evaluate the check (heartbeat file stat / one bounded HTTP attempt). Skipped honestly for `Paused` (epic rule), and for every other state (starting is readiness's; stopping/failed are terminal-bound). On window expiry: the `running → failed` transition with `TransitionCause::Unresponsive { check, failing_for_ms }` (rendered description like 18-1's `describe`), then the SAME `plan_restart` path a reaper-detected crash takes (backoff, `restart_count`, crash-loop threshold at 5 — AD-15 unchanged). A restart resets the window state (new run, fresh contract). Poll errors on the handle do NOT interact with liveness (AI-12's corroboration path owns poll errors).
- [x] Event + surfaces — `TransitionCause::Unresponsive` (additive variant, wire/render per 18-1's `ReadinessTimeout` shape). No new verbs: `kt agent show`/`status` name the check + failing-for on an unresponsive failure (the existing status-detail projection); `fleet` carries it; Control API unchanged (visible through existing status routes). CLI e2e: an unresponsive fake → `show` names the check; stderr diagnostics on the window's first failure + recovery are single-shot, not per-tick spam.
- [x] Conformance fixtures — `crates/hekma-conformance/src/bin/fake_agent.rs`: `--heartbeat-file PATH:INTERVAL_MS` (rewrites/touches the file every interval, pure std), `--heartbeat-stop-after MS` (stops touching it — the hang drill), `--http-health PORT:STATUS` (a PERSISTENT tiny std TcpListener loop answering GETs — liveness polls repeatedly, unlike 18-1's one-shot `--http-ready`), `--http-health-fail-after MS` (accepts then hangs/closes). Pure std, the arg-scripted pattern.
- [x] Tests — unit: window math (first-failure + max_age boundary, exact-at and one-tick-before), validation matrix, describe rendering, the shared-evaluator non-fork pin (one test both modules call the SAME fn). Integration (`tests/liveness.rs` NEW): stale heartbeat → `running → failed(Unresponsive)` within `max_age_ms` + one reaper tick (deadline pinned, `failing_for_ms` honest); fresh heartbeat → stays running; blip-inside-window recovery → no failure + the recovery diagnostic; paused instance with a stale heartbeat → NEVER probed (pin the skip); http health pass/fail-window/fail-after drills; restart policy applied (restart_count charged, backoff schedule, 5-consecutive-unresponsive → crash-loop terminal, AD-15); no-liveness-declared → byte-identical today (pin); adopted running instance → window starts fresh (no instant false failure); v1.0.0-engine rejects `[liveness]` (pin). CLI e2e: show names the check.
- [x] Docs + release surface — `docs/manifest.md` (the `[liveness]` section reference + the unifying window semantics stated plainly), `docs/adapter-contract.md` (the section joins v1.1.0; freeze ledger entry), `CHANGELOG.md` + `RELEASE_NOTES.md` (announce; contract version unchanged from 18-1 — same minor, one release). Engine rides 0.9.0.

**Acceptance Criteria:**
- Given a running instance whose declared heartbeat file goes stale, the instance lands `failed` with cause `Unresponsive` naming the check, within `max_age_ms` + one reaper tick — never sooner than `max_age_ms` (the window is the contract).
- Given a transient failure shorter than the window that then recovers, the instance stays `running` and the recovery is surfaced (diagnostic), not silent.
- Given a paused instance, liveness is never evaluated (pinned by test), no matter how stale the signal.
- Given an unresponsive failure, the Restart Policy applies exactly as for a crash: backoff, `restart_count`, crash-loop terminal at 5 consecutive.
- Given a manifest with no `[liveness]`, behavior is byte-identical to today (pinned).
- Given handoff: the four house gates pass; coverage ≥ 95%; docs state the window semantics and the paused rule.

</frozen-after-approval>

## Design Notes

- **One semantic: failing-for-a-window.** The epic letter says "a heartbeat file's maximum age, or an HTTP check" — the file kind carries its window naturally (mtime age); HTTP needed one. Ruled: `max_age_ms` is the window for BOTH — the HTTP check's first failure starts a timestamp, expiry transitions. Rejected: immediate-on-first-HTTP-failure (a single blip on a 250 ms reaper cadence would kill healthy agents — the operational failure mode the graceful-degradation gate exists to prevent; the k8s `failureThreshold × periodSeconds` precedent is exactly this window). A one-field manifest cost, one honest semantic, symmetric failure causes.
- **Heartbeat = 18-1's `file_fresh`, not a fork.** The evaluator is lifted to a shared pure fn (or `pub(crate)` re-use) so readiness and liveness CANNOT drift — the same discipline as 19-1's one-decision-core ruling. The HTTP attempt helper likewise. A test pins the non-fork (both modules call the same symbol).
- **Where the window state lives.** In-memory on the supervised handle, reset on restart — a supervisor restart (process restart/adoption) re-derives from the next check, worst case one extra window before a failure lands: honest degradation, noted in docs. NOT persisted: the window is supervision mechanics, not instance history (registry rows stay the source of record for states).
- **The paused guard is a STATE read, not a heuristic.** The reaper already reads persisted `LifecycleState` (reaper.rs:327/:1127/:1208 pattern); the liveness phase runs only for exactly-`Running`. This also covers SIGSTOP-best-effort pause (the unix backend's heartbeat-stops-growing note) and Windows's never-suspends pause alike.
- **Unresponsive is a CAUSE, not a state.** AD-15: lifecycle-as-data, 7 states, no new edges beyond `running → failed` (an existing edge; the cause is new). The epic's "the Restart Policy applies" means `plan_restart` verbatim — 18-3's exit-code classes do NOT retrofit this cause (no exit code exists to class).
- **HTTP-in-the-reaper honesty.** A liveness HTTP attempt is capped (18-1's 1 s attempt cap / 8 KB read cap) and runs inline in the reaper pass: N instances with hung checks stretch the tick by up to N × cap. Bounded, rare (only instances whose checks are FAILING pay the cap — healthy checks are fast), and named here; if a real fleet of hung HTTP-checked instances materializes, the fix (per-tick liveness budget or round-robin) is a deferred-work trigger, recorded in deferred-work.md at implementation time if the reviewers concur it's live.
- **Contract surface: same 1.1.0.** 18-1 bumped `CONTRACT_VERSION` to 1.1.0 and neither release has shipped; `[liveness]` joins the same minor. The older-engine rejection rule (deny_unknown_fields + negotiation) covers both sections identically; the release-surface statement 18-1 wrote is extended to name both sections.
- **Naming.** `liveness.rs` module, `Liveness` manifest struct, `TransitionCause::Unresponsive`, `[liveness]` section. The backend's OS-liveness `probe_alive`/poll vocabulary stays untouched (different layer, pre-existing).

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Heartbeat goes stale | mtime age ≥ max_age_ms (window elapsed) | `running → failed(Unresponsive{check, failing_for_ms})` + restart plan | check named; failing-for honest |
| Transient blip recovers | failure < window, then Fresh | stays `running`; first-failure + recovery diagnostics (once each) | never per-tick spam |
| HTTP check down then up | fail 200 ms of a 5 s window, then 200 | stays `running` | same |
| Paused instance | LifecycleState::Paused, stale signal | liveness NOT evaluated | no diagnostic, no failure |
| Heartbeat file absent | never written / deleted | Failing from first check; window applies | absent ≡ stale (documented) |
| 5th consecutive unresponsive | crash-loop threshold | terminal `failed`, crash-loop reason (AD-15) | unchanged semantics |
| Restart after unresponsive | fresh spawn | window state reset; readiness (if declared) gates first | — |
| Supervisor restarted mid-window | adoption/process restart | window re-derives; ≤ one extra window | honest, documented |
| No `[liveness]` declared | v1.1.0 manifest without it | byte-identical today (pinned) | — |
| v1.0.0 engine + `[liveness]` | simulated negotiation | rejected, diagnostic names the section | contract-incompatible class |
| Bad manifest | max_age 0 / https / empty path | registration refused, field named | validation before any spawn |
| Hung HTTP check (attempt) | connect/read stall | attempt capped at 1 s; window accrues | tick stretch bounded + named in docs |

## Code Map

- `crates/hekma-adapter-api/src/manifest.rs` — the `Liveness` section (beside 18-1's `Readiness`), validation, `resolve` helper; `src/lib.rs` re-exports.
- `crates/hekma-engine/src/domain/liveness.rs` — NEW (per-attempt evaluators + window math; shares `file_fresh` + the HTTP attempt helper with `readiness.rs` — lift, don't fork).
- `crates/hekma-engine/src/domain/readiness.rs` — the primitives to share (:118 `file_fresh` call shape, HTTP_ATTEMPT_CAP/HTTP_READ_CAP + status-line parse); extract to a shared `probe_kinds.rs` or pub(crate) fns (implementer's choice; the non-fork test is the pin).
- `crates/hekma-engine/src/domain/supervisor/reaper.rs` — `poll_once` (:90) gains the liveness phase after crash handling; the persisted-state read pattern (:327/:1127/:1208); `plan_restart` (:515) reused verbatim.
- `crates/hekma-engine/src/domain/supervisor/mod.rs` — `Supervised` gains the per-instance window state (first-failure `Option<Instant>`); reset on restart/spawn.
- `crates/hekma-engine/src/domain/event.rs` — `TransitionCause::Unresponsive { check, failing_for_ms }` + wire/render (beside 18-1's `ReadinessTimeout` :189).
- `crates/hekma-engine/src/adapter/mod.rs` — `resolve_liveness_check` (mirrors 18-1's `resolve_readiness_probe`).
- `crates/hekma-conformance/src/bin/fake_agent.rs` — `--heartbeat-file`, `--heartbeat-stop-after`, `--http-health`, `--http-health-fail-after`.
- `crates/hekma/src/cli/agent.rs` — show/status naming (existing projections; no new verbs).
- `docs/manifest.md`, `docs/adapter-contract.md`, `CHANGELOG.md`, `docs/RELEASE_NOTES.md`.

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean (MSRV + `+stable`).
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` — pass, including the liveness matrix + the no-liveness byte-identity pin.
- `python3 scripts/check_docs.py` (+ `--self-test`).
- `python3 scripts/test_automation.py`.
- Empirical drills (record in the PR): (a) heartbeat-stop-after fake → `failed(Unresponsive)` within window + tick, `show` names the check; (b) blip-recovery fake → stays running + recovery diagnostic; (c) paused + stale → never probed.

## Ask First

None. The epic text fixes the two kinds, the unresponsive cause, the restart-policy application, and the paused rule; the window-unification semantic and the in-memory window state resolve from settled text as recorded in Design Notes (the immediate-on-first-failure alternative rejected for named reasons). The contract rides 18-1's already-open 1.1.0 minor — no new version decision.

## Spec Change Log

- 2026-10-24 (draft): authored against 18-1's implementation tip `e449914` (stacks on the 18-1 branch); autonomous mode per Islam's 2026-10-23 cadence ruling.
- 2026-10-24 (pre-dispatch consistency amendments, autonomous): baseline re-anchored to `e93b8e6` (18-1's review-round-1 tip — the branch point); the frozen validation line updated from the "`http://` only" shorthand to 18-1's ENFORCED loopback-literal-IP family (18-1's review round turned the documented posture into code; 18-2 mirrors the enforced rule — a consistency correction, not a new decision; the governing "mirrors 18-1's family exactly" clause is unchanged).
- 2026-10-24 (implementation): all Execution items complete on `feat/epic-18-2-liveness`. Implementation notes recorded here (the frozen block untouched): the shared primitives landed in `crates/hekma-engine/src/domain/probe_kinds.rs` (the spec's Code-Map offered the name as implementer's choice — `file_fresh`, the HTTP attempt helper, and the URL/status-line parsers lifted there; readiness and liveness both route through it, pinned by a `fn_addr_eq` non-fork test) and the reaper phase's plan/diagnostic shapes follow the crash path verbatim. The CLI e2e seeds the pid-0 `never` policy seed directly in the state DB (no CLI verb sets policy) so the drill is terminal at first expiry — under the shipped `on-failure` default the follow command's own engine restarts mid-drill and the row would end reconciled "orphan not found" instead of the unresponsive edge; the restart cadence itself stays covered by the engine-level crash-loop drill.
