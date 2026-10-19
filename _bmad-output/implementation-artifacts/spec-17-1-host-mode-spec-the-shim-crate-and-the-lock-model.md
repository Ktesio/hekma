---
title: 'Host-mode spec — the shim crate and the lock model (serve-shape measurement + bounded-options proposal)'
type: 'feature'
created: '2026-10-19'
status: 'done'
review_loop_iteration: 0
baseline_commit: 'f3e2d3910ee5fe7c9f71f6e4793cc7ff48690609'
context:
  - '_bmad-output/implementation-artifacts/epic-17-context.md'
---

<frozen-after-approval reason="human-owned intent — do not modify unless human renegotiates">

## Intent

**Problem:** Epic 17 gates every later story on three unsettled decisions — the host crate's name, its HTTP stack (NFR-8 review), and the AD-18 lock-model outcome (reopen trigger (1) fired when AD-20 was ratified). AD-18 forbids assuming the outcome: the serve shape must be MEASURED against NFR-4, not argued.

**Approach:** Extend the story 7-5 perf-budgets harness additively with a `--serve-shape` scenario driving the engine's PUBLIC async facade under serve-shaped concurrency (concurrent fleet/instance reads + usage polling + one pathological instance with a bounded stop + a `--follow`-style subscribed reader), gated on the ratified NFR-4 budgets; record this machine's measured evidence; deliver a bounded-options proposal doc for Islam (crate name, HTTP stack, AD-18 verdict). The crate itself is NOT built here (17.2's work).

## Boundaries & Constraints

**Always:**
- NFR-4 stays the ruler — reads p99 < 1 s (strict) on the 25-instance fleet; ≤ 2% CPU / ≤ 50 MiB RSS per running instance — reusing the harness's gate machinery (strict-vs-≤ `GateBound`, per-instance normalization, the documented CI tolerance shape).
- The scenario drives the PUBLIC facade only (the async methods a host drives on its own runtime); no private reach-in; no engine-internal code changes.
- Harness changes are ADDITIVE: the default no-flag invocation stays byte-equivalent; report `schema_version` stays 1 (the AD-14 convention); new pure gate/scenario math ships pinned by unit tests in the file's `#[cfg(test)]` battery (coverage ≥ 95% is CI-enforced).
- AI-58 draft-then-ratify: the proposal doc presents bounded options with trade-offs and one recommendation; ZERO edits to the spine, PRD, epics.md, or sprint-status until Islam picks.
- Measurement honesty (AI-18): diagnostics to stderr, verdicts named in the report, no silent fallback; the measured evidence lands in docs/testing.md's measured record and the proposal cites the numbers.

**Ask First:** *(HALT and ask before doing any of these)*
- Any edit to `ARCHITECTURE-SPINE.md` (AD-20's "Settled at 15.1's spec stage" note), the PRD, `epics.md`, or `sprint-status.yaml` — runs ONLY after Islam picks.
- Adding a measurement dependency — stay inside the sanctioned dev-deps (sysinfo, tempfile, hekma-conformance; tokio/serde_json already in the engine's tree).
- Changing `FLEET_SIZE`/`RUNNING_COUNT` or any budget constant — a conscious budget re-ratification with the maintainer.
- CI wiring that refreshes the canonical baseline `benches/baselines/perf-ubuntu-x86_64.json` — follows docs/testing.md's update procedure (the visible diff IS the ratification), never slipped in unmarked.

**Never:**
- No HTTP server/transport in the measurement — the lock question is engine-side; the HTTP stack is decided by argument (bounded options), not by this benchmark.
- No host-crate code, no workspace-member addition, no AD-2 allowlist edit — the proposal only DRAFTS the future entry's wording.
- No lock-model migration work — the archived plan's Steps 0–7 are out of scope even if the recommendation is to activate it.
- No engine-core fixes or optimizations — if measurement exposes a defect, surface it; do not fix it here.
- No existing gated metric weakened, renamed, or replaced; serve gates get their own names and bind only on serve runs.

## I/O & Edge-Case Matrix

| Scenario | Input / State | Expected Output / Behavior | Error Handling |
|----------|--------------|---------------------------|----------------|
| Serve run, budgets hold | `--serve-shape` on the 25/10 fixture | `serve_shape` report block: per-read-kind stats, CPU/RSS per instance, named serve-gate verdicts; exit 0 | N/A |
| Contention over budget | Pathological bounded stop stalls concurrent reads (or CPU/RSS over budget) | Failing gate names metric + measured/budget; the evidence routes the AD-18 recommendation to activating the archived ai-63b plan | Nonzero exit in gate mode; the report prints FIRST (the record survives) |
| Baseline + serve run | `--baseline` loaded for a serve run | Serve gates handled per the scenario-scoped rule (Design Notes); the seven default gates behave exactly as today | Missing/corrupt baseline stays a loud named exit 2 (unchanged) |
| Slow subscriber | The follow-style reader falls behind the bus | `Lagged`/backlog surfaced in the scenario notes (AI-18); measurement continues | Never a crash, never a silent drop |
| Invalid usage / short orphan bound | `--serve-shape=bogus`, unknown flag; or the serve schedule exceeds `AGENT_LINGER_MS` coverage | Named stderr error + exit 2 (existing parser and `check_liveness_schedule` conventions), before anything spawns | Same |

</frozen-after-approval>

## Code Map

- `crates/hekma-engine/examples/perf-budgets.rs` (3714 ln) — the harness to extend: `FLEET_SIZE`=25/RUNNING_COUNT=10 (:233/:237); budgets (:280-285); `CI_CPU_TOLERANCE_FACTOR` (:312); `RATCHET_TOLERANCE` (:322); `GateBound` (:605); `evaluate_gates` (:663); `parse_cli_args`/`usage_text` (:817/:779); `GATED_METRICS` + `baseline_coverage_gaps` (:929/:946); `check_liveness_schedule` (:393); `main` (:1511); fixture via `ManifestFixture::heartbeat` (:1628); `measure_steady_windows` (:1974); `measure_read_latency`/`usage_read` (fleet+find, the public usage read; :2051/:2115); `#[cfg(test)]` battery (:2895+).
- `benches/baselines/perf-ubuntu-x86_64.json` — canonical baseline: `schema_version` 1, seven gates, `provenance` block (lacks serve metrics — drives the scenario-scoped rule).
- `.github/workflows/ci.yml` — `perf-budgets` job (:461-562: release build, stale-helper rebuild, run with `--baseline` :551, artifact); `boundary` job (:584+): the AD-2 allowlist grep `hekma-(engine|adapter-api|adapters-hermes)` (:606-640) and the one-rustls/ring gate (:641-676) — the pattern the future host-crate entry extends.
- `scripts/test_automation.py` — `test_ci_perf_budgets_job_runs_the_harness_blocking` (:775) pins the job; `test_ci_enforces_workspace_boundary_and_semver_gates` (:535).
- `crates/hekma-engine/src/engine.rs` — the facade under test: `Engine` (:139); `EngineInner { Mutex<Registry>, Mutex<Supervisor> }` (:186-188); `EventBus` outside the locks (:192); `blocking()` (:347); async `fleet` (:585), `instance_status` (:1026), `stop(name, Option<Duration>)` — the bounded stop (:941); lock-free `subscribe` (:410); `run_blocking` (:1469); `CRASH_POLL_INTERVAL` 250 ms (:80).
- `crates/hekma-engine/tests/embed_clean.rs` — `two_engines_in_one_process_drive_the_full_uj3_flow_concurrently_without_collision` (:136): the two-engine/state-dir collision discipline the 17.2 single-owner lock builds on.
- `crates/hekma-engine/tests/uj3_library_host.rs` + `crates/hekma-conformance` (`test_support::{fake_agent_bin_in, BinDir, ManifestFixture}`, `ManifestFixture::heartbeat`, the event-subscription suites' lingering fixture presets) — fixture plumbing to reuse; `fake_agent_bin_in(BinDir::Examples)` is the harness's helper resolution.
- `Cargo.toml` (workspace) — tokio features (:46); hyper/hyper-util/http-body-util/bytes already in tree (engine-internal loopback listener, :62-65); one rustls+ring (:86-88); `sysinfo` dev-dep (:123). `crates/hekma-engine/Cargo.toml` — harness dev-deps (:86-105); examples see `[dependencies]` + `[dev-dependencies]` (tokio available to the driver).
- `docs/testing.md` — perf-budgets section (:208-266): gate table, ratchet + baseline update procedure (:231-242), "The measured record (local runs…)" (:250) — where the serve evidence lands.
- `_bmad-output/planning-artifacts/ai-63b-lock-model-decision-2026-08-27.md` — the archived per-instance plan (4 candidates, 7-step migration; Steps 0-4 zero-interleaving-risk) that ACTIVATES if measurement fails.
- `ARCHITECTURE-SPINE.md` (under `_bmad-output/planning-artifacts/architecture/architecture-ktesio-2026-07-02/`) — AD-13 (:116), AD-17 (:158), AD-18 + reopen triggers (:163-170), AD-20 (:178-196; "Settled at 15.1's spec stage" :195-196), the AD-18 amendment (:220-221). `_bmad-output/implementation-artifacts/sprint-status.yaml:180-186` — the epic-17 tracker.
- `_bmad-output/planning-artifacts/host-mode-decisions-2026-10-19.md` — the proposal doc 17.1 DELIVERS (created during implementation; this spec only names it).

## Tasks & Acceptance

**Execution:**
- [x] `crates/hekma-engine/examples/perf-budgets.rs` — ADD the `--serve-shape` scenario: flag + usage text; fixture extension (the 25/10 heartbeat fleet plus ONE lingering pathological instance from a conformance preset, so its mid-run `stop` is genuinely slow, bounded by `stop`'s window); a concurrency driver on a small multi-thread tokio runtime issuing concurrent reads + usage polling + a subscribed follow-style reader + the pathological bounded stop; a serve-scoped gate set (read p99 strict < 1 s, CPU/instance ≤ 2%, RSS/instance ≤ 50 MiB) reusing the existing machinery; a `serve_shape` report block; `#[cfg(test)]` pins for the new pure math.
- [x] `.github/workflows/ci.yml` — WIRE the serve run into the perf-budgets job (e.g. a second explicit invocation), honoring the baseline constraint in Ask First — gated budgets CI never runs are not gates.
- [x] `scripts/test_automation.py` — EXTEND the perf-job pins (and boundary pins if touched) to match the workflow in the same change.
- [x] `docs/testing.md` — DOCUMENT the scenario (what it drives, budgets, invocation, wiring) and APPEND this story's measured figures to the measured record; `scripts/check_docs.py` validates.
- [x] `_bmad-output/planning-artifacts/host-mode-decisions-2026-10-19.md` — CREATE the bounded-options proposal: ≥ 2 crate-name candidates (today's `hekma-*` convention; e.g. `hekma-host`) with drafted AD-2 allowlist-entry wording; HTTP stack candidates at minimum axum / hyper directly (already in-tree) / a no-new-heavy-dep minimal option — each with dependency footprint under NFR-8, SSE support, maintenance risk; plus the AD-18 recommendation from the measurement (re-ratify coarse WITH the numbers, or activate the archived ai-63b plan).
- [x] `ARCHITECTURE-SPINE.md` + `sprint-status.yaml` + docs — RATIFICATION-GATED: apply Islam's pick to AD-20's "Settled" note and the tracker in one change — runs ONLY after the human decision. (2026-10-19: picked D1 hekma-host / D2 axum 0.8 / D3 re-ratify coarse; applied in the same change — spine Amendments 2026-10-19, ci.yml allowlist `|host` + automation pin, docs/testing.md naming, tracker note, proposal §5 record.)

**Acceptance Criteria:**
- Given the no-flag invocation, when the harness runs, then its report and exit behavior are byte-equivalent to pre-17-1 and all seven existing gates behave identically.
- Given `--serve-shape`, when the scenario completes, then it measured the serve shape through the PUBLIC async facade — concurrent reads + usage polling + one bounded stop + a subscribed reader — and every serve-gate verdict names measured vs budget against NFR-4.
- Given a serve run over budget, when gates evaluate, then the run fails naming the metric and the evidence routes the AD-18 recommendation to the archived plan; given all budgets hold, then the recommendation is re-ratify-coarse citing the measured numbers.
- Given the new pure math, when `cargo test --workspace --all-targets` runs, then the unit battery pins it and the example compiles — the wall-clock measurement stays out of ordinary test runs.
- Given the CI perf-budgets job, when a PR runs, then the job is green with the existing canonical baseline (no unseeded coverage hard-fail) and `scripts/test_automation.py`'s pins match the workflow.
- Given the proposal doc, when Islam reads it, then every decision presents bounded options with stated trade-offs and exactly one recommendation — and no spine/PRD/epics/tracker edit preceded the pick.
- Given Islam's pick, when the ratification-gated task runs, then the spine's AD-20 note, sprint-status, and docs record the outcome in one change.
- Given handoff, when the house gates run, then `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, and `python3 scripts/check_docs.py` all pass.

## Spec Change Log

- 2026-10-19 (review round, 18 findings triaged — 16 patch / 1 defer / rest rejected
  as unreachable-or-noise): all patches applied by the implementer and
  independently re-verified. Load-bearing: the serve outcome fold (exit 1 +
  `--write-baseline` refusal) extracted into pure pinned helpers (`run_exit_code`,
  `baseline_write_refusal(serve_failed)`) — the verification-gap mutation hole is
  closed; contention is PROVEN per run (`reads_started_during_stop`, named exit-2
  on zero before any verdict); sampler window clamped to the load window;
  per-instance CPU/RSS normalized over the schedule-aware CONSERVATIVE post-stop
  count; serve CPU/RSS gates platform-scoped to the ratified gate platforms; the
  serve-baseline seeding procedure made real (mode-aware gated-metric set,
  serve-carrying baselines ratchet on serve runs, loud named error when absent);
  default liveness check no longer runs on serve runs; Unix pathological sleep
  derived from SERVE_AGENT_LINGER_MS; readers retry transient errors (counted,
  surfaced) instead of panicking; no fabricated zeros in the serve print block;
  automation pins tightened; artifact + follower-honesty wording aligned.
  Deferred: CI-runner serve headroom watch (first hosted-runner runs — if the
  ~1.03× CPU headroom flaps on 2-4 vCPU runners, the tolerance/reader-count
  needs a maintainer call; recorded in deferred-work.md). Run of record after
  hardening: p99 5.07 ms / CPU 1.94 %/instance (effective 10) / RSS 0.96 MiB /
  4 overlapping reads / 0 retries. Verdict unchanged: budgets hold.
- 2026-10-19 (ratification, human gate closed): Islam picked D1 `hekma-host` / D2 axum 0.8 /
  D3 re-ratify coarse (all recommendations as proposed). Applied verbatim in one change:
  spine Amendments (2026-10-19) + AD-20 settled note + AD-18 trigger-(1) closure;
  ci.yml boundary allowlist `|host` + the test_automation pin; docs/testing.md names the
  crate; proposal §5 records the pick; this task list is now complete. Spec → done.
- 2026-10-19 (orchestrator, review triage): clarified the Verification manual
  check — sprint-status STATUS-sync (in-progress/review flips) is the build
  workflow's own sync step and is not part of the AI-58 ratification gate,
  which covers the ratification-GATED content edits only. Known cosmetic slip
  in the frozen I/O matrix row ("AGENT_LINGER_MS" should read
  "SERVE_AGENT_LINGER_MS" for serve runs) left as-is — frozen block, human
  renegotiation required; the liveness-check family cited is correct.

## Design Notes

- **Why the async facade, not an HTTP loop:** decided at routing — the lock question is engine-side (`EngineInner`'s two coarse mutexes, `engine.rs:186-188`); a transport would only add noise. The driver mimics an embedded host: async methods called concurrently from the driver's own runtime (docs/embedding.md's "call its async methods on your own runtime").
- **Scenario-scoped gating:** the committed baseline lacks serve metrics and `baseline_coverage_gaps` hard-fails a gated metric the baseline lacks — keep the serve gate set OUT of the default evaluation: validate/compare it only on `--serve-shape` runs; default runs and the existing CI leg stay byte-equivalent. Ratchet the serve gates only after a serve-carrying baseline is seeded per the docs/testing.md procedure (report-only trend until then).
- **The pathological instance:** reuse the conformance lingering fixture shapes so the bounded `stop` holds the supervisor mutex for a real window mid-read; keep `check_liveness_schedule` covering the extended schedule (windows + stop bound + margin).
- **Decision rule (fixed):** contention over ANY NFR-4 budget in the serve shape → recommend activating the archived plan; all budgets hold → recommend re-ratifying the coarse model, citing the measured headroom. The proposal never argues past the numbers.

## Verification

**Commands:**
- `cargo fmt --all --check` — expected: clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: clean.
- `cargo test --workspace --all-targets` — expected: pass, including the new gate-math pins.
- `python3 scripts/check_docs.py` — expected: pass.
- `cargo run --release --example perf-budgets -p hekma-engine` (workspace root) — expected: the pre-17-1 report, exit 0.
- `cargo run --release --example perf-budgets -p hekma-engine -- --serve-shape` — expected: the `serve_shape` block with green serve gates on this machine (or the documented over-budget outcome, recorded as the evidence).

**Manual checks (if no CLI):**
- `git status`/diff shows NO change to the spine, PRD, epics.md, or sprint-status beyond the ratification-gated task, and no planning-artifact change except the named proposal doc — AI-58 honored.

## Suggested Review Order

**The serve scenario (entry point)**

- Entry: the whole scenario — fixture, driver runtime, stop task, follower, sampler — orchestrates here.
  [`perf-budgets.rs:1533`](../../crates/hekma-engine/examples/perf-budgets.rs#L1533)

- The concurrency driver: 4 readers + follower + pathological bounded stop on the PUBLIC async facade.
  [`perf-budgets.rs:1218`](../../crates/hekma-engine/examples/perf-budgets.rs#L1218)

**The gates and their teeth**

- Serve verdicts: three own-named NFR-4 gates, platform-scoped, conservative normalization.
  [`perf-budgets.rs:964`](../../crates/hekma-engine/examples/perf-budgets.rs#L964)

- The exit fold (mutation-pinned): red serve → exit 1; teeth, not advisory output.
  [`perf-budgets.rs:2633`](../../crates/hekma-engine/examples/perf-budgets.rs#L2633)

- `--write-baseline` refuses a red serve run by name (serve_failed arm).
  [`perf-budgets.rs:2647`](../../crates/hekma-engine/examples/perf-budgets.rs#L2647)

**Measurement honesty**

- Zero-overlap contention proof: named exit-2 before any verdict — no vacuous evidence.
  [`perf-budgets.rs:1016`](../../crates/hekma-engine/examples/perf-budgets.rs#L1016)

- Sampler clamped to the load window; schedule-aware conservative divisor.
  [`perf-budgets.rs:1035`](../../crates/hekma-engine/examples/perf-budgets.rs#L1035)

- Per-OS gate binding scope (linux/macos gate; elsewhere report-only + note).
  [`perf-budgets.rs:1070`](../../crates/hekma-engine/examples/perf-budgets.rs#L1070)

**CI wiring + pins**

- The second blocking CI step (bare `--serve-shape` last line; own report file, same artifact).
  [`ci.yml:582`](../../.github/workflows/ci.yml#L582)

- The fold's mutation pin — delete any token and this test reds.
  [`perf-budgets.rs:5328`](../../crates/hekma-engine/examples/perf-budgets.rs#L5328)

**Docs + the decision proposal**

- The serve-shape section + measured record (load model, provenance, run of record).
  [`testing.md:270`](../../docs/testing.md#L270)

- The bounded-options proposal for the ratification: D1/D2/D3, one recommendation each.
  [`host-mode-decisions-2026-10-19.md`](../planning-artifacts/host-mode-decisions-2026-10-19.md)
