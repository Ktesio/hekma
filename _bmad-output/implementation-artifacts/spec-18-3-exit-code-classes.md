---
title: 'Exit-code classes and the stable-window reset — declared codes relaunch or land clean; stability resets the counter'
type: 'feature'
created: '2026-10-24'
status: 'in-progress'
review_loop_iteration: 0
baseline_commit: '2b29074'
context:
  - '_bmad-output/implementation-artifacts/epic-18-context.md'
  - '_bmad-output/implementation-artifacts/spec-18-1-readiness-probes.md (the contract-section family and validation posture)'
  - '_bmad-output/implementation-artifacts/spec-18-2-liveness-unresponsive.md (the crash-path-verbatim precedent)'
  - '_bmad-output/planning-artifacts/sprint-change-proposal-2026-09-30-hosted-platform-readiness.md (§4.4 Story 16.3 → renumbered 18.3)'
  - '_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/ARCHITECTURE-SPINE.md (AD-15: Restart Policy as data; crash-loop threshold 5)'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Every unrequested exit is a crash today (`CrashInput::Exited` → `Crashed` cause, `plan_restart` charges the consecutive-failure counter), and the only reset is a fresh operator start (spawn.rs:665). A service agent that asks its supervisor to restart it with a contract exit code (the exit-75 pattern) is indistinguishable from a crash — it charges the crash loop and can trip the 5-failure terminal; an agent that exits cleanly on a declared code is recorded as crashed; and a genuinely stable run that dies once after an hour still carries its whole failure history into the next classification.

**Approach:** Two ADAPTER-DECLARED exit-code classes in a new `[exit_codes]` manifest section (the same v1.1.0 minor): `restart_requested = [codes]` — the agent asked for a relaunch: the exit takes the crash-path dance (failed terminal + restart plan, backoff schedule unchanged) with a NEW `TransitionCause::RestartRequested { code }`, and the consecutive-failure counter is NOT charged; `clean = [codes]` — the agent finished on purpose: `running → stopped` with `TransitionCause::CleanExit { code }`, settle like a requested stop, no restart plan, no counter action (the operator's next fresh start resets, exactly as today). The STABLE-WINDOW reset is engine-owned supervision policy (not adapter surface): a run that held `running` continuously for ≥ `STABLE_RUN_WINDOW` (30 s) resets the counter to 0 at its crash — this crash charges as failure #1. No section declared → byte-identical behavior today.

## Tasks & Acceptance

**Execution:**
- [x] Manifest section — `crates/hekma-adapter-api/src/manifest.rs`: `ExitCodes` section struct (`#[serde(deny_unknown_fields)]`), both lists optional: `restart_requested: Vec<u16>` (default empty), `clean: Vec<u16>` (default empty). Validation: each code 0..=255 (the portable exit-code domain — Windows's wider u32 codes simply never match codes > 255 on unix; documented); the two lists DISJOINT (an overlap is refused at registration, both fields named); duplicates within one list refused; control-character/validation family per 18-1/18-2. Contract STAYS `1.1.0`. The v1.0.0-engine rejection pin gains an `[exit_codes]` case. Manifest docs + freeze ledger updated; FR-9's `always` recorded as SUBSUMED in the docs (the policy layer stays `never`/`on-failure`; an operator wanting always-restart declares their codes `restart_requested`).
- [x] Classification — `crates/hekma-engine/src/domain/supervisor/reaper.rs` crash loop: when `CrashInput::Exited(Some(code))` meets an instance whose resolved manifest declares a class containing `code` (resolution mirrors `resolve_readiness_probe`/`resolve_liveness_check` — a new `resolve_exit_codes`, cached on `Supervised` at spawn/adoption like `liveness_check`; drift at start refused the same way, tested the same way): (a) `restart_requested` → the existing crash-path sequence (terminal transition + `plan_restart` plan, backoff VERBATIM) but the cause is `RestartRequested { code }`, the detail names the declared class, and `set_restart_count` is NOT called (counter unchanged); (b) `clean` → the requested-stop shape: `running → stopped` with `CleanExit { code }`, `settle_spawn_record`, no plan. `RestartPolicy::Never` + `restart_requested`: the policy still wins — no plan, terminal `failed` with the `RestartRequested` cause (the operator's policy outranks the adapter's ask; documented). Exits during STARTING (survival window / probe) are LAUNCH failures and are never classified (today's `fail_launch_detail` path, unchanged; documented). Signals/`None` exits are never classified.
- [x] Stable-window reset — the counter reset joins `plan_restart`: `Supervised` gains `running_since: Instant`, stamped at EVERY transition INTO `running` (`starting → running` commit, and `paused → running` resume — a pause-interval never counts toward stability; the resumed run re-proves it). At a crash classification (any charged class — crash or unresponsive), if the just-ended run's `running_since` shows ≥ `STABLE_RUN_WINDOW` (a NEW named const, 30 s, in `restart.rs` beside `MAX_CONSECUTIVE_FAILURES`, production-pinned + test-scaled like `BackoffSchedule`), the counter resets to 0 BEFORE this failure charges (this crash = failure #1; the reset is one line in the store + named in the crash detail: "the previous run was stable for Ns — consecutive-failure count reset"). Adopted instances have no in-memory anchor: no reset (honest, documented — adoption re-proves nothing about stability).
- [x] Causes + surfaces — `TransitionCause::RestartRequested { code }` + `TransitionCause::CleanExit { code }` (additive variants, wire/render per the `ReadinessTimeout`/`Unresponsive` templates). No new verbs: `show`/status/fleet name the class and code through existing projections; `logs`/events carry the causes. CLI e2e: an exit-75 fake → `show` names the declared class; a clean-exit fake → `stopped` with the clean cause.
- [x] Conformance fixtures — `crates/hekma-conformance/src/bin/fake_agent.rs`: `--exit-with CODE:AFTER_MS` (pure std: sleep, exit with the code; fail-fast on malformed values like the 18-1/18-2 flags). One flag covers every drill.
- [x] Tests — unit: validation matrix (range, overlap, duplicates), `STABLE_RUN_WINDOW` boundary (`>=` at exactly 30 s), the reset arithmetic. Integration (`crates/hekma-engine/tests/exit_codes.rs` NEW): restart-requested relaunches with backoff AND `restart_count` unchanged (pinned before/after); restart-requested under `never` → terminal failed, no plan; clean exit → `stopped` + `CleanExit` + settle + no plan; an exit-75 LOOP never trips the crash-loop terminal (pin ≥ MAX_CONSECUTIVE_FAILURES+1 relaunches still planning; the footgun answered by test) while backoff grows to its cap; unclassified exit → charges as today (byte-identity pin for no-section manifests: same events/counter as pre-18-3); crash after ≥ 30 s stable → counter resets-then-charges (next == 1, detail names the reset); crash before 30 s → increments; pause-interval does not count (pause 31 s, resume, crash in 1 s → NO reset — the anchor restamped at resume); exit-during-starting never classified; adopted instance no reset; v1.0.0-engine rejects `[exit_codes]`; drift refusal (`resolve_exit_codes`) unit tests shaped like the siblings. CLI e2e per the surfaces task.
- [x] Docs + release surface — `docs/manifest.md` (the `[exit_codes]` section, the two classes' semantics, the exit-75-loop honesty note: a restart-requested loop NEVER trips the crash-loop terminal — the operator asked; the backoff still grows to its cap), `docs/adapter-contract.md` (v1.1.0 ledger entry; FR-9 subsumption note), `CHANGELOG.md` + `docs/RELEASE_NOTES.md` (announce both causes' serde old-reader break like 18-1/18-2; contract unchanged). Engine rides 0.9.0; adapter-api rides 0.3.0.

**Acceptance Criteria:**
- Given a manifest declaring `restart_requested = [75]`, an exit-75 run relaunches on the standard backoff with cause `RestartRequested { code: 75 }` and the consecutive-failure counter is byte-unchanged — and 6+ consecutive exit-75 runs still plan restarts (no crash-loop terminal).
- Given `RestartPolicy::Never` and an exit-75 run, the instance lands terminal `failed` with the `RestartRequested` cause and NO restart plan.
- Given `clean = [0]`, a plain exit-0 run lands `stopped` with `CleanExit { code: 0 }`, settled like a requested stop, no plan.
- Given a run that held `running` ≥ 30 s continuously, its crash charges the counter as failure #1 (reset named in the detail); below 30 s, increments as today. A pause-interval never contributes.
- Given a manifest with no `[exit_codes]`, crash classification and counter behavior are byte-identical to today (pinned).
- Given handoff: the four house gates pass; coverage ≥ 95%; docs state the loop honesty and the FR-9 subsumption.

</frozen-after-approval>

## Design Notes

- **The exit-75 loop is answered, not hidden.** "Without charging the crash loop" literally means an exit-75 loop never trips the 5-failure terminal. That is a deliberate operator contract (they declared the class), and it is stated in docs + pinned by test; the RATE is still bounded (backoff grows to the 60 s cap). A separate restart-requested budget would invent policy the ratified text never named.
- **The stable window is ENGINE-owned, not adapter surface.** The epic declares the classes ("Declared exit-code classes") but says only "THE consecutive-failure counter resets after a stable running window" — one engine semantic, not a per-adapter knob. `STABLE_RUN_WINDOW = 30 s` sits beside `MAX_CONSECUTIVE_FAILURES` (production-pinned, test-scaled). A config surface is deferred with a trigger (an operator asks to tune it). Rejected: manifest-declared window (adapter surface for supervisor policy — wrong side of the contract; also 18-1's engine-owned-cadence precedent).
- **Clean ≠ counter reset — the reset already exists.** Today's reset is the fresh operator START (spawn.rs:665); a clean-exited instance's next start is exactly that. Adding a reset at clean-exit would be a second, redundant semantic.
- **`restart_requested` neither charges nor resets.** The agent recycling proves neither stability nor failure; the counter simply holds. (Charging-only-on-crash keeps the crash-loop terminal meaningful for real crashes interleaved with recycles: crash, 75, crash, 75 — the crashes still count 1, 2.)
- **Pause clears the stability anchor.** Any transition INTO `running` stamps a fresh `running_since` — a 10-minute pause followed by a 1-second crash must not read as "stable for 10 minutes" (the process served nothing while paused). Conservative and cheap (one stamp at the resume transition).
- **Classes classify RUNS, not launches.** An exit during `starting` (survival window, readiness probe, ACP handshake) is a launch failure — `fail_launch_detail`, no classification. The classes are service-run semantics; the launch path's honest terminals (18-1) already exist. Signals (`None`) and poll-failure exits carry no code to classify.
- **`Never` outranks the adapter's ask.** The Restart Policy is the operator's instrument; `restart_requested` is the adapter's. On conflict the operator's `never` wins (no plan), and the cause still records what the agent asked for.
- **No new states, no new edges beyond causes.** `CleanExit` rides the existing `running → stopped` edge (the requested-stop shape); `RestartRequested` rides `running → failed` + plan. AD-15's data-machine is untouched.

## I/O Impact Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Exit 75, declared restart_requested | running process exits 75 | `failed(RestartRequested{75})` + restart plan, backoff verbatim, counter UNCHANGED | detail names the declared class |
| Exit-75 loop (6+) | consecutive requested restarts | plans keep coming; NO crash-loop terminal; backoff at cap | docs state the contract honestly |
| Exit 75 under policy `never` | RestartPolicy::Never | terminal `failed(RestartRequested{75})`, no plan | — |
| Exit 0, declared clean | running process exits 0 | `stopped(CleanExit{0})`, settled, no plan, no counter action | — |
| Unclassified exit (99) | no class contains 99 | today's crash path byte-identical (charge, cause, detail) | — |
| Crash after ≥30 s stable | stable run dies | counter RESET then this crash = #1; detail names the reset | — |
| Crash after <30 s | young run dies | increments as today | — |
| Pause 31 s → resume → crash 1 s | anchor restamped at resume | NO stable reset | honest |
| Exit 75 during starting | launch-phase exit | launch failure path, never classified | unchanged |
| Adopted instance crash | no in-memory anchor | no stable reset (documented) | honest |
| Overlapping lists | code in both classes | registration refused, both fields named | validation before spawn |
| v1.0.0 engine + `[exit_codes]` | simulated negotiation | rejected, diagnostic names the section | contract-incompatible |
| No `[exit_codes]` | v1.1.0 manifest | byte-identical today (pinned) | — |

## Code Map

- `crates/hekma-adapter-api/src/manifest.rs` — `ExitCodes` section (beside `Readiness` :376 / `Liveness`), validation, `resolve` helper; `src/lib.rs` re-exports.
- `crates/hekma-engine/src/domain/supervisor/reaper.rs` — the crash loop's classification point (`CrashInput::Exited(Some(c))` detail builder ~:446; "Consult the Restart Policy FIRST" ~:407), `plan_restart` (:515) + `set_restart_count` (:717) call site (the no-charge + reset branch), the requested-stop shape reference (settle_spawn_record ~:381).
- `crates/hekma-engine/src/domain/restart.rs` — `STABLE_RUN_WINDOW` beside `MAX_CONSECUTIVE_FAILURES` (:43); the reset arithmetic helper (pure, test-scaled).
- `crates/hekma-engine/src/domain/supervisor/mod.rs` — `Supervised` gains `exit_codes: Option<ExitCodes>` + `running_since: Instant`; stamp at the `starting → running` commit (spawn.rs ~:958) and `paused → running` resume (lifecycle.rs :580 area).
- `crates/hekma-engine/src/adapter/mod.rs` — `resolve_exit_codes` (+ drift/native unit tests, the sibling pattern).
- `crates/hekma-engine/src/domain/event.rs` — `TransitionCause::RestartRequested { code }` + `CleanExit { code }` (additive, the :189/:215 template).
- `crates/hekma-conformance/src/bin/fake_agent.rs` — `--exit-with CODE:AFTER_MS`.
- `crates/hekma-engine/tests/exit_codes.rs` — NEW (the matrix).
- `crates/hekma/src/cli/agent.rs` — show/status naming via existing projections.
- `docs/manifest.md`, `docs/adapter-contract.md`, `CHANGELOG.md`, `docs/RELEASE_NOTES.md`.

## Verification

**Commands:**
- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean (MSRV + `+stable`).
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` — pass, including the exit-code matrix + byte-identity pin.
- `python3 scripts/check_docs.py` (+ `--self-test`); `python3 scripts/test_automation.py`.
- Empirical drills (record in the PR): (a) exit-75 fake → relaunch with counter unchanged, `show` names the class; (b) exit-0 fake → `stopped` + clean cause; (c) stable-run crash → counter resets to 1.

## Ask First

None. The epic text fixes both classes, the no-charge rule, and the stable-window reset; the settled sub-rulings (backoff-still-applies, clean-settles-as-requested-stop, `never`-outranks, engine-owned 30 s window, pause-clears-anchor, launches-not-classified) resolve from the ratified letter + house precedents as recorded in Design Notes. The contract rides the open 1.1.0 minor.

## Spec Change Log

- 2026-10-24 (draft): authored against 18-2's tip `a20da61` (stacks on the 18-2 branch); autonomous mode per Islam's 2026-10-23 cadence ruling.
- 2026-10-24 (pre-dispatch re-anchor, autonomous): baseline moved to `2b29074` (18-2's review-round-1 tip after the orchestrator's P8 correction — the actual branch point); Code-Map anchors re-verified at that tip (reaper.rs classification :462, `plan_restart` :886, `set_restart_count` :926; restart.rs `MAX_CONSECUTIVE_FAILURES` :33; spawn.rs fresh-start reset :665; lifecycle.rs `resume` :603).
