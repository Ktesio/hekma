---
title: 'Windows detached stdio leak fix + Linux-CI detach-e2e skip scoped to x86-64'
type: 'bugfix'
created: '2026-10-16'
status: 'in-progress'
baseline_commit: '9a2ceaa09c01fde7312a27bc55f29e0a7bdcd1fa'
review_loop_iteration: 0
context: []
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** (A, Fibery 181) On Windows, a detached agent inherits its spawning `hekma` command's stdio pipe WRITE-ENDS — any script capturing `agent start --detach` output via pipes blocks until the agent exits. Root cause RESOLVED by investigation (2026-10-16): std's default Windows spawn passes NO `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` while `bInheritHandles=TRUE`, and every stdio std provides (`Stdio::from(File)`, `null()`, `piped()`) is created/duplicated INHERITABLE — so the detached child inherits EVERY inheritable handle in the parent's table, including kt's own stdout/stderr pipe write-ends. The in-repo belief that std restricts inheritance to the child's three handles (agent_cli.rs:853 comment, deferred-work entry) is FALSE. `DETACHED_PROCESS` alone would NOT fix it (console flags don't touch handle inheritance). (B, Fibery 177) The engine-level detach e2e runtime-skips on ALL linux CI legs although #109's deadlock was x86-64-ubuntu-runner-specific — arm64 Linux never had it.

**Approach:** (A) In `WindowsBackend::spawn`, when `spec.detach`, transiently clear `HANDLE_FLAG_INHERIT` on the spawner's own three stdio handles around `command.spawn()`, restoring them after — the detached child then inherits only the handles std deliberately dups for its stdio (the capture files). Windows-hosted regression test inverts the forensic shape (piped parent → detached spawn → parent exits → both pipes EOF within a bound); the CLI e2e's `mem::forget` leak workaround becomes the canary. (B) Scope `is_linux_ci()` to `x86_64` (matching #109's documented trigger), so arm64 Linux CI runs the family affirmatively; the full x86 unskip stays blocked on #109 (documented, not silently dropped).

## Boundaries & Constraints

**Always:**
- The un-flag/restore window wraps ONLY the detached spawn; a panic path restores via a guard (no permanently-mangled stdio handles in the spawning process).
- The fix is Windows-backend-local: no Unix behavior change, no cross-crate surface change.
- Honest docs: correct the false "std restricts inheritance" claims (the agent_cli comment + the deferred entry's expectation) and document the residual risk (any OTHER inheritable handle in kt would still leak — none known).
- The `is_linux_ci()` change is a one-liner matching the documented #109 trigger; the skip family's runtime-skip (no cfg) posture is unchanged.
- Windows-proof split: `cargo check -p hekma-engine --lib --profile test --target x86_64-pc-windows-gnu` + clippy cross-target + fmt locally; the new regression test executes ONLY on windows-latest (cfg(test), hosted there).

**Ask First:** Nothing anticipated; if the Windows CI leg proves Fix 1 insufficient, halt and report rather than escalating to a raw `CreateProcessW` reimplementation (Fix 3) autonomously.

**Never:** No raw CreateProcessW reimplementation in this story; no blind unskip of the x86 family; no changes to the job-object kill-on-close semantics; no `DETACHED_PROCESS` flag addition (insufficient and changes console semantics).

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Captured detached start (Windows) | script pipes kt's stdout/stderr; kt detaches an agent; kt exits | both pipes EOF within a bounded window after kt exits (the child no longer holds the write-ends) | timeout → regression test red |
| Attached (non-detached) spawn | normal start under a live engine | unchanged (no un-flag window; kill-on-close job unchanged) | unchanged |
| Panic inside the window | spawn panics | guard restores the three handles before unwinding continues | restored, verified by the guard's own test arm |
| linux CI, arm64 | adoption/detach family | runs (no skip) — matches the never-affected arch | unchanged |
| linux CI, x86-64 | same family | still runtime-skips (documented #109 mitigation, now arch-scoped) | unchanged |
| macOS / Windows CI | same family | unchanged (never skipped) | unchanged |

</frozen-after-approval>

## Code Map

Investigated 2026-10-16 (anchors verified):
- `crates/hekma-engine/src/backends/windows/mod.rs` — job create/guard :279-291; kill-on-close only if `!spec.detach` :293-304; capture trio :333-342; capture files opened :352-370; stdout wiring `Stdio::from(file)` :380-387; stderr :388-391; stdin :402-406; flags `CREATE_NEW_PROCESS_GROUP` only :412; spawn :414-417; job assign :426-448; `Drop` disarm :194-235. NO handle-inheritance management anywhere in-tree (no SetHandleInformation uses).
- windows-sys FFI already a dependency (CreateJobObjectW etc.) — `SetHandleInformation`/`GetStdHandle` available through it.
- False-belief anchors to correct: `crates/hekma/tests/agent_cli.rs:853-854` (comment), `deferred-work.md` 12-1 entry wording; the leak workaround `mem::forget(reader threads)` `agent_cli.rs:859-864`.
- Skip family: `crates/hekma-engine/tests/adoption.rs:219-221` (`is_linux_ci` = Linux + CI env), the detach e2e skip :675-679 (test :653); mirrored skips interaction.rs:414/554, logs.rs:758/949/1183, acp_lifecycle.rs:1532-1539 (~11 fns total).
- #109 record: sprint-status.yaml:768 (AI-50, x86-64-ubuntu-only D-state wedge, Islam-approved runtime skip), epic-9-retro §6.5.
- Test inventory to keep green: windows/mod.rs:1054/:1089/:1101 (hosted windows-latest), unix/mod.rs:1197 detached parity + detached-reaper test, adoption.rs:653, agent_cli.rs:890 (the compensating CLI e2e, hosts the forensic harness `run_hekma_agent_bounded` :752, LEG_BOUND :743), :1065 observed-detach refusal, :1154 help honesty.

## Tasks & Acceptance

**Execution:**
- [ ] `crates/hekma-engine/src/backends/windows/mod.rs` -- the detached-spawn inheritable-stdio guard: a RAII guard clearing `HANDLE_FLAG_INHERIT` on the spawner's three stdio handles around `command.spawn()` when `spec.detach`, restoring on drop (panic-safe); a hosted unit arm proving clear-then-restore.
- [ ] `crates/hekma-engine/src/backends/windows/mod.rs` -- the Windows-hosted regression test: spawn the test binary with piped stdout/stderr → inside, spawn a detached instance (fake agent, linger) → exit the inner process → assert both pipes EOF within a bound.
- [ ] `crates/hekma/tests/agent_cli.rs` -- correct the false std-inheritance comment at :853; tighten the `mem::forget` leak workaround ONLY if the Windows leg proves the fix (otherwise leave + note).
- [ ] `crates/hekma-engine/tests/adoption.rs` -- `is_linux_ci()` gains `std::env::consts::ARCH == "x86_64"`; a comment naming #109 as the reason and arm64 as never-affected.
- [ ] Docs — `docs/testing.md` (the skip family now x86-64-scoped, the Windows leak root cause + residual-risk note); `deferred-work.md` 12-1 entry gains the resolved root-cause note (fix ref at merge).
- [ ] `_bmad-output/implementation-artifacts/sprint-status.yaml` -- story 15-4 row.

**Acceptance Criteria:**
- Given a Windows host with a piped parent, when a detached start's command exits, then the parent's pipes EOF within the bound (the regression test's assertion) — and the non-detached paths are byte-unchanged.
- Given a panic between clear and restore, then the three handles are restored (the guard's test arm).
- Given linux CI on arm64, when the adoption/detach family runs, then the detach e2e executes (no skip); given x86-64, it still skips with #109 named.
- Given the local battery, then `cargo check/clippy/fmt --target x86_64-pc-windows-gnu` + the full Unix gates pass; no Unix behavior change.
- Docs carry the corrected mechanism (no false "std restricts inheritance" claims remain).

## Spec Change Log

## Design Notes

- Why Fix 1 over alternatives: `DETACHED_PROCESS` doesn't touch handle inheritance (insufficient); `bInheritHandles=FALSE` is unstable std API; raw `CreateProcessW` with an explicit handle list is correct but heavy (re-implement quoting/env + std's attribute-list trap: std does NOT add its own stdio dups to a user list). The transient un-flag is 100% stable-API, backend-local, and precise: it removes exactly the three handles that matter (kt's own stdio), leaving the intended capture-file inheritance intact.
- Concurrent-spawn safety: this backend never uses `Stdio::inherit` (its stdio is always piped/file/null), so un-flagging the parent handles cannot break a concurrent spawn's std wiring; std's own dup window is mutex-guarded internally.
- The CLI e2e's `mem::forget` canary: the fix makes the forgotten reader threads unnecessary; tighten only on Windows-leg evidence (this box is Linux — do not assume).

## Verification

**Commands:**
- `cargo check -p hekma-engine --lib --profile test --target x86_64-pc-windows-gnu` + `cargo clippy -p hekma-engine --lib --profile test --target x86_64-pc-windows-gnu -- -D warnings` + `cargo fmt --all --check` -- green locally.
- `HEKMA_TEST_REAL_AGENT_SMOKES=0 cargo test --workspace --all-targets` + `python3 scripts/check_docs.py` -- green (Unix unchanged).
- Windows-latest CI leg green on the PR (the regression test executes there).
