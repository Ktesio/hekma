//! The performance-budgets harness (story 7-5, NFR-4) — MEASURE, don't assume.
//!
//! NFR-4's budgets — read commands < 1 s on a 25-instance Fleet; supervision
//! overhead ≤ 2% CPU and ≤ 50 MB RSS per running instance — were PRD
//! placeholders marked "for architecture to validate". This harness turns
//! them into measured, gated numbers: it builds a real 25-instance Fleet
//! fixture (manifest agents on the conformance `fake_agent`, a subset
//! RUNNING), drives the PUBLIC facade only (`Engine::blocking()` — the same
//! embedding surface `kt` drives, consistent with stories 7-1/7-3), and
//! measures, in this fixed order (fixture startup is entirely OUTSIDE the
//! timed windows: setup → settle → steady-state window → re-settle → reads):
//!
//! 1. **Steady-state supervision overhead** — with the running subset idling
//!    (`fake_agent --heartbeat-ms 1000` — heartbeat only, no usage emission,
//!    so the engine is supervising, not metering), the ENGINE PROCESS's
//!    aggregate CPU% and RSS are sampled once per second over a ≥ 10 s window
//!    (the first sample is discarded — it carries the phase's residue), AFTER
//!    a settle period. On CI the window runs THREE times and the gate uses
//!    the MEDIAN window (the stability mechanism for noisy shared runners);
//!    locally it runs once. Per-instance = figure / the actually-Running
//!    count sampled at measurement time.
//! 2. **Read latency** — over `READ_ITERATIONS` iterations, each of the read
//!    kinds the CLI itself uses: `fleet()` (the `kt agent list` read),
//!    `instance_status()` (spread across the Fleet round-robin), the
//!    `effective_config()` read (`kt agent config get`; the configured value
//!    is asserted), and the per-instance usage read (`kt agent usage <name>`'s
//!    exact shape: `fleet()` + find — reported as `usage_via_fleet` because it
//!    includes the fleet listing's cost; it is NOT an independent kind).
//!    p50/p95/p99 are reported per kind; the GATE is on the overall p99 (the
//!    worst kind) < 1 s — a STRICT `<` (the budget's own prose): a p99
//!    exactly at 1000.0 ms fails. On CI a failed read gate retries ONCE
//!    after a 30 s cool-down and gates on the second run (the house flake
//!    policy: a co-tenant stall must not red an ordinary PR; local runs are
//!    single-shot).
//! 3. **Subscriber-active overhead (GATED at the ratified budgets, story
//!    10-3)** — NFR-4's gated figures above are measured with ZERO
//!    subscribers; a host using the subscription surface needs a budget
//!    covering fan-out cost, so after the gated measurements the harness
//!    attaches ONE active subscriber and re-measures a single steady-state
//!    window plus the same read kinds, and the DELTAS vs the unsubscribed
//!    baseline (CPU percentage points, RSS MiB, read p99 ms per instance)
//!    gate against the RATIFIED subscriber-active budgets — the same policy
//!    shape as the CPU gate (strict ×1.0 locally; the documented
//!    shared-runner tolerance ×1.5 in CI, since the addendum runs ONE window
//!    even on CI and the tolerance is what absorbs that single-window
//!    noise). The ratified numbers come from the observed measurements
//!    recorded in docs/testing.md (2026-09-10 local run: a −0.015 pct-point
//!    CPU delta, +0.01 MiB RSS delta, +0.7 ms read-p99 delta — fan-out cost
//!    for one subscriber is effectively zero, so the budgets carry large
//!    headroom: ~16× the observed CPU-delta magnitude and two orders of
//!    magnitude on RSS (~200×) and read p99 (~143×) — sized for measurement
//!    noise rather than headroom erosion). The budgets were ratified on a
//!    macOS local (strict) run and are enforced on ubuntu CI with the same
//!    factor shape (docs/testing.md). The addendum still runs AFTER the gated phases so
//!    the zero-subscriber figures cannot be contaminated, and the startup
//!    liveness check counts its window against the fixture's orphan bound.
//!
//! It prints a machine-readable JSON report to stdout (the measured record —
//! the CI perf-budgets job log is its canonical home, and the job uploads the
//! JSON as a workflow artifact via `PERF_BUDGETS_REPORT_PATH`) and GATES:
//!
//! | gate                              | local + CI | budget                          |
//! |-----------------------------------|------------|---------------------------------|
//! | read p99                          | hard       | < 1000 ms                       |
//! | RSS mean / instance               | hard       | ≤ 50 MiB                        |
//! | RSS max spike / instance          | hard       | ≤ 2 × 50 MiB (leak guard)       |
//! | CPU / instance                    | hard       | ≤ 2% strict (local)             |
//! | CPU / instance                    | hard       | ≤ 2% × `shared_runner_tolerance` (CI) |
//! | subscriber CPU Δ / instance       | hard       | ≤ 0.25 pct-pts strict (local)   |
//! | subscriber CPU Δ / instance       | hard       | ≤ 0.25 × tolerance (CI)         |
//! | subscriber RSS Δ / instance       | hard       | ≤ 2 MiB strict (local)          |
//! | subscriber RSS Δ / instance       | hard       | ≤ 2 × tolerance (CI)            |
//! | subscriber read-p99 Δ             | hard       | ≤ 100 ms strict (local)         |
//! | subscriber read-p99 Δ             | hard       | ≤ 100 × tolerance (CI)          |
//! | ratchet vs committed baseline     | additive   | ≤ max(baseline × 1.15, baseline + ε) — CI mode only |
//!
//! ε is the per-metric-family NOISE FLOOR ([`ratchet_noise_floor`]): a pure
//! ×1.15 window on a near-zero subscriber delta is microscopic (a +0.009
//! pct-point baseline would tolerate only +0.0014 — quantization jitter
//! reds the run), so each family also gets a small absolute window.
//!
//! ## The CI tolerance policy (named, not hidden)
//!
//! `CI_CPU_TOLERANCE_FACTOR` = 1.5. The CPU budget is strict (×1.0) on local
//! runs. GitHub shared runners are noisy neighbors — an unknown co-tenant can
//! steal cycles from under the sampling window — so in CI (the `CI` env the
//! runner sets, overridable with `PERF_BUDGETS_CI=1|0`; any other value exits
//! 2) the CPU gate runs at ×1.5 (3% per instance) AND takes the median of
//! three windows. The factor is a DOCUMENTED shared-runner tolerance, printed
//! in the report (`ci_tolerance.factor`) and in the job log; it is NOT a
//! relaxation of the budget — the 2% strict budget is what local runs gate,
//! reads and RSS gate at budget on every gate platform (ubuntu CI; strict
//! local macOS), and every report carries both the
//! strict-budget margin and the applied tolerance.
//!
//! Story 10-3 applies the SAME factor shape to the ratified subscriber-active
//! delta gates (CPU Δ / RSS Δ / read-p99 Δ): strict ×1.0 locally, ×1.5 in
//! CI — deliberately WITHOUT the median-of-three mechanism, because the
//! addendum measures ONE window even on CI (the deltas are differences taken
//! seconds apart in one process), so the tolerance is the sole noise
//! absorber there and the budgets are sized for it (docs/testing.md).
//!
//! ## Baseline trend tracking — the ratchet (story 15-5)
//!
//! The absolute budgets above catch a regression only when it crosses them;
//! they cannot see a slow multi-week DRIFT (reads creeping from 6 ms toward
//! 100 ms is green every week and worse every month). The ratchet closes
//! that gap ADDITIVELY — a gate-platform run must pass BOTH the absolute
//! budgets and the ratchet:
//!
//! * `--baseline <file>` loads a prior report JSON, the report gains a
//!   `trend` block (per gated metric: baseline, current, delta, verdict),
//!   and in CI mode a gated metric worse than the ratchet limit
//!   max(baseline × [`RATCHET_TOLERANCE`], baseline + ε) fails the run.
//!   Improvements are accepted freely; exactly-at-limit is within (the rule
//!   is "no WORSE than ×1.15"); a NON-POSITIVE baseline never ratchets (×1.15
//!   on a negative figure tightens the bound instead of tolerating noise —
//!   that verdict is `no-ratchet`, surfaced with its reason, and the
//!   metric's absolute budget still gates it); a NON-FINITE current is also
//!   a surfaced `no-ratchet` (never a silently-within NaN comparison).
//! * The ratchet gates CI MODE only — the canonical baseline records the
//!   CI ubuntu-x86_64 leg's own run, so that leg is the only place the
//!   comparison is apples-to-apples (a CI-mode run whose loaded baseline's
//!   OS differs from the current OS is WARNED on stderr — platform
//!   apples-to-apples is mechanism, not convention). Local and
//!   other-platform runs may pass `--baseline` for a REPORTING-ONLY trend
//!   block; they never ratchet-gate.
//! * The baseline's gate COVERAGE is validated immediately after load —
//!   BEFORE anything is spawned: a gated metric the baseline lacks is a
//!   named schema error + exit 2 (no wasted minute of measurement), and
//!   extra gates the baseline carries but this harness does not evaluate
//!   are surfaced on stderr (a future-harness baseline must not hard-fail
//!   today's runs).
//! * `--write-baseline <file>` writes the fresh report (trend stripped,
//!   provenance stamped, trailing newline) as the next baseline — and
//!   REFUSES red runs (a failed absolute gate or ratchet can never become
//!   canonical: loud stderr note + exit 2, the file is never written). The
//!   stdout report is unchanged.
//! * `--help` / `-h` prints plain usage (flags, defaults, the update
//!   procedure pointer); unknown arguments still exit 2.
//! * The canonical committed baseline is `benches/baselines/perf-ubuntu-x86_64.json`
//!   — ONE file, no per-platform zoo. It is updated by PR: the visible
//!   diff IS the ratification (docs/testing.md carries the procedure; CI
//!   never auto-writes a baseline). The file carries a `provenance` block
//!   (seed flag, where it was generated, the full git SHA) stamped by the
//!   writer.
//! * A missing, corrupt, or schema-mismatched baseline — including a
//!   non-finite measured value — is a LOUD named error and a non-zero
//!   exit — never a silent no-trend run. If the trend evaluation itself
//!   fails AFTER measurement, the report is printed to stdout FIRST (the
//!   measured record survives), then the named error + exit 2. The
//!   default invocation (no flags) is byte-equivalent to the pre-15-5
//!   report.
//!
//! ## The serve-shape scenario (story 17-1, `--serve-shape`)
//!
//! AD-18's reopen trigger (1) fired when AD-20 (the Service Host) was
//! ratified: a long-lived host drives the engine's PUBLIC ASYNC facade under
//! serve-shaped concurrency, and the coarse two-mutex lock model must be
//! MEASURED against NFR-4 under that shape — never argued. The scenario
//! extends this harness ADDITIVELY: the default no-flag invocation stays
//! byte-equivalent (the `serve_shape` report block exists only on
//! `--serve-shape` runs), the report `schema_version` stays 1, and the serve
//! gates get their OWN names and bind only on serve runs.
//!
//! The scenario mimics an embedded host (docs/embedding.md's "call its async
//! methods on your own runtime"): a small multi-thread tokio runtime OWNED BY
//! THE DRIVER calls the engine's async methods concurrently — no HTTP
//! transport (the lock question is engine-side; a transport would only add
//! noise), no private reach-in. On the 25/10 heartbeat fleet PLUS one
//! pathological instance it runs, concurrently, for one 12 s window:
//!
//! * `SERVE_READER_TASKS` reader tasks looping the SAME four read kinds the
//!   default phase measures (`fleet()`, `instance_status()` round-robin,
//!   `effective_config()`, the `usage_via_fleet` poll) — content-asserted
//!   exactly like the default reads, PACED so each reader carries the
//!   default phase's own read demand (`READ_ITERATIONS` per window, spread
//!   across it — `serve_reader_pace`; a host serves at demand rate, and the
//!   process-wide sampling cannot separate host handling cost from
//!   supervision, so the load model is anchored to the harness's ratified
//!   premise rather than chosen freely);
//! * ONE `--follow`-style subscribed reader over the lock-free
//!   `Engine::subscribe()` bus, counting committed events; a `Lagged`
//!   (backlog) is COUNTED AND SURFACED in the report notes — never a crash,
//!   never a silent drop (AI-18);
//! * ONE pathological instance (registered + started AFTER the default
//!   phases, whose read assertions count exactly 25 entries) stopped with a
//!   BOUNDED window mid-load. The fixture is built so the stop is genuinely
//!   slow: on Unix the agent IGNORES SIGTERM (the lifecycle suite's proven
//!   `sh trap '' TERM` shape — the conformance `fake_agent` cannot install a
//!   signal handler there, the OS-cfg gate forbids it), so the supervisor
//!   mutex is held for the whole graceful window before the SIGKILL
//!   escalation; on Windows the graceful step always elapses the window
//!   before `TerminateJobObject`, so the lingering `fake_agent` shape gives
//!   the same bounded-window stop. Both arms run through the SAME conformance
//!   `ManifestFixture` machinery; both are bounded by the stop window plus
//!   the kill confirmation.
//!
//! The serve gate set reuses the existing machinery (`GateBound`
//! strict-vs-≤, per-instance normalization over the actually-Running count,
//! the documented CI tolerance shape) at the RATIFIED NFR-4 budgets: read
//! p99 STRICT `<` 1 s; CPU ≤ 2%/instance (× the shared-runner tolerance on
//! CI); RSS ≤ 50 MiB/instance — three gates, their own names
//! (`serve_read_p99_lt_1s`, `serve_cpu_per_instance_le_2pct`,
//! `serve_rss_per_instance_le_50mib`), bound only on serve runs. The CPU/RSS
//! window is sampled with the same sysinfo discipline (first sample
//! discarded) while the load runs, so the figures cover the host's serve
//! cost, not idle supervision.
//!
//! The DECISION RULE is fixed (spec 17-1): contention over ANY of the three
//! NFR-4 budgets in this shape → the evidence recommends activating the
//! archived per-instance plan (`ai-63b-lock-model-decision-2026-08-27.md`);
//! all budgets hold → the evidence recommends re-ratifying the coarse model,
//! citing the measured headroom. The proposal never argues past the numbers.
//!
//! Scenario-scoped baseline rule: the committed canonical baseline carries NO
//! serve metrics, and the gated-metric coverage check is MODE-AWARE — a
//! SERVE run's baseline must carry the serve metrics too (their absence is
//! the same LOUD named exit-2 schema error, fired before anything spawns),
//! while a default run's set is exactly the seven default gates (the serve
//! names are not in it — pinned). A serve run trends the serve gates as
//! well: against a serve-carrying baseline (seeded per docs/testing.md's
//! procedure — a serve run's own `--write-baseline` merges the serve gates
//! into the baseline's gates array), a serve-gate regression ratchets just
//! like a default one. The CI perf job runs the serve invocation as a SECOND
//! explicit step (gated budgets CI never runs are not gates) whose report
//! file rides the SAME artifact as the default report (two files, one
//! artifact). Platform scope (the standing gate-platform rule): the serve
//! gates GATE on the ratified gate platform and are REPORT-ONLY elsewhere
//! (Windows — sysinfo's `memory()` is the working-set-size analog there).

//! ## Why an example (explicit gating, not `#[ignore]` sprawl)
//!
//! `cargo test --workspace --all-targets` COMPILES this file and RUNS its
//! `#[cfg(test)]` module (the example's unit tests) but never executes the
//! harness itself — an example's `main` runs only when invoked explicitly.
//! That is the whole gate: the wall-clock measurement stays out of ordinary
//! test runs without a single `#[ignore]`d test, while the GATE MATH
//! (percentiles, worst-kind selection, per-instance normalization, CI-mode
//! parsing, tolerance selection, pass/fail evaluation) is pinned by tests so
//! it cannot silently weaken — a budget change must consciously edit the
//! assertions. Run the harness locally:
//!
//! ```bash
//! cargo run --release --example perf-budgets -p hekma-engine
//! ```
//!
//! (From the workspace root, so the fixture's `fake_agent` exec resolves into
//! the same `target/` — the shared locator's examples/ hop plus its
//! on-demand-build fallback (story 10-1) pin the helper to this binary's
//! target root either way.) The CI perf-budgets job builds the release example
//! + helper explicitly and runs the binary — its regressions FAIL that job.
//!
//! ## Measurement honesty
//!
//! * CPU% is sysinfo's two-refresh delta for ONE pid — the harness process
//!   itself, which owns the `Engine` (its runtime threads + supervisor ARE
//!   the engine; the harness is the embedding host). Between consecutive
//!   refreshes the main thread sleeps, so the sampled CPU is the engine's
//!   idle supervision cost, not measurement-loop cost. sysinfo's process
//!   CPU% can exceed 100 on many cores (it is the per-core SUM) — that is
//!   the "aggregate" figure the budget divides. Both contamination
//!   directions are acknowledged: measuring reads BEFORE the window would
//!   inflate the CPU samples with read-phase activity (the fixed order
//!   measures steady-state FIRST, avoiding it), and the window's own
//!   background leaves the runtime warm for the reads (a short re-settle
//!   absorbs it).
//! * RSS is the engine process's resident set ONLY. The supervised agents'
//!   own memory is their own (separate processes) — per the spec's design
//!   note it is NOT supervision overhead, and the report says so. The mean
//!   is gated, the max is reported and spike-guarded at 2× budget, and the
//!   scope note acknowledges that the allocator may retain pages from
//!   earlier phases (registration/start/sampling) in the engine process.
//! * Window coverage: the 12 s window validates heartbeat-frequency
//!   supervision work (the reaper cadence + per-second log captures).
//!   LONGER-period supervision tasks (sweeps, retention) are NOT captured;
//!   a longer local window is available via `PERF_BUDGETS_WINDOW_MS` (clamped
//!   to [10 s, 1 h]).
//! * Process metrics come from `sysinfo` — the DEV-dependency the spec
//!   record sanctions (dev-only, benchmark-gated, NFR-8-compatible: the
//!   shipping `cargo tree -p ktesio -e normal,build` is unchanged). The
//!   hand-rolled `/proc` alternative would have been platform-lying; Windows
//!   needs the crate regardless.
//! * Platform scope of the gates: enforced on ubuntu CI; strict local gating
//!   on macOS; Windows is measured-and-reported (sysinfo's `memory()` is the
//!   working-set-size analog there) — NEVER a gate platform.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hekma_conformance::test_support::{fake_agent_bin_in, BinDir, ManifestFixture};
use hekma_engine::{
    broadcast, AdapterRef, Blocking, ConfigLayer, Engine, EngineEvent, FleetEntry, LifecycleState,
};
use sysinfo::{Pid, ProcessesToUpdate, System};

// ---------------------------------------------------------------------------
// Pinned fixture + budget constants (the spec's numbers, stated once)
// ---------------------------------------------------------------------------

/// The Fleet size the budgets are written against (NFR-4: "25-instance Fleet").
///
/// ## Budget assumption (conscious-edit notice)
///
/// NFR-4's per-instance figures mean MARGINAL supervision overhead: the fixed
/// engine cost (runtime + registry + reaper) amortizes over
/// [`RUNNING_COUNT`], and only the per-instance marginal work (heartbeat
/// capture, poll entries) scales with it. These two counts are PART OF THE
/// BUDGET'S MEANING — a future edit that changes [`FLEET_SIZE`] or
/// [`RUNNING_COUNT`] must consciously revisit the budget with the maintainer
/// (the spec-7.5 remedy: a documented budget-update proposal, never a silent
/// gate tweak).
const FLEET_SIZE: usize = 25;

/// How many of the registered instances are RUNNING via the facade start —
/// the steady-state denominator (see the budget assumption on [`FLEET_SIZE`]).
const RUNNING_COUNT: usize = 10;

/// The registered-instance name prefix (names are `perf-00` … `perf-24` —
/// valid `[a-z0-9][a-z0-9_-]*` instance names).
const INSTANCE_PREFIX: &str = "perf-";

/// The manifest adapter kind (a flow-local identity; nothing builtin answers
/// to it — the uj3 fixture's shape, renamed for this harness).
const MANIFEST_KIND: &str = "perfbudgets";

/// The idle agents' heartbeat cadence: the ONLY work a running instance does
/// during the steady-state window (no usage emission — the engine supervises
/// and captures, it does not meter).
const HEARTBEAT_MS: u64 = 1000;

/// The idle agents' self-exit fallback — the ORPHAN BOUND: if the harness
/// dies mid-run, any leaked agent self-exits within 60 s instead of
/// lingering. The bound is no longer enforced only by this comment: at
/// startup, [`check_liveness_schedule`] asserts it strictly covers the whole
/// measurement schedule (settle + every window — including the
/// subscriber-overhead addendum window — plus gaps and a margin); the read
/// phase does not depend on agent liveness (the read assertions check
/// returned content, never Running state) and teardown stops the idlers
/// regardless, so the bound ends at the last window, not at process exit.
const AGENT_LINGER_MS: u64 = 60_000;

/// The scheduling margin [`check_liveness_schedule`] adds on top of the
/// measurement windows: the between-window gaps plus slack for scheduler
/// jitter. Deliberately small — the read phase does not need agent liveness.
const LIVENESS_MARGIN: Duration = Duration::from_secs(5);

/// The hard cap on `PERF_BUDGETS_READ_ITERATIONS` — an absurd override must
/// not multiply the run time (and the report size) without bound. Clamping
/// is LOUD (a stderr note, via [`clamp_read_iterations`]); the cap itself is
/// pinned by a unit test.
const MAX_READ_ITERATIONS: u64 = 100_000;

/// The config leaf the setup writes to every instance so the timed
/// `effective_config` read is a REAL read whose content can be asserted.
const MODEL_KEY: &str = "model";
const MODEL_VALUE: &str = "perf-model";

/// The ratified budgets (NFR-4). These are the numbers the harness gates on.
const BUDGET_READ_P99_MS: f64 = 1000.0;
const BUDGET_CPU_PER_INSTANCE_PCT: f64 = 2.0;
const BUDGET_RSS_PER_INSTANCE_MIB: f64 = 50.0;
/// The RSS spike/leak guard: a single window sample exceeding twice the
/// per-instance budget fails the run even when the mean is fine.
const RSS_SPIKE_TOLERANCE: f64 = 2.0;

/// The RATIFIED subscriber-active budgets (story 10-3) — ceilings on the
/// ONE-active-subscriber DELTAS vs the unsubscribed baseline. Ratified from
/// the observed measurements (the 2026-09-10 local release run recorded in
/// docs/testing.md: CPU delta −0.015 pct-points, RSS delta +0.01 MiB,
/// read-p99 delta +0.7 ms — one subscriber's fan-out cost is effectively
/// zero), sized far above observed — ~16× the CPU-delta magnitude and two
/// orders of magnitude (~200× / ~143×) on the RSS / read-p99 deltas — so the
/// gate absorbs single-window measurement noise (the addendum runs ONE
/// window even on CI; the shared-runner tolerance below is its other noise
/// absorber) while any REAL fan-out regression (a per-publish broadcast
/// storm, a per-event host callback) trips it. Ratified on a macOS local
/// (strict) run; enforced on ubuntu CI with the same factor shape. A change
/// to any of these numbers is a budget re-ratification — it must consciously
/// update docs/testing.md's budget table with the new observations, never a
/// silent gate tweak.
const BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS: f64 = 0.25;
/// See [`BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS`] (per-instance MiB, mean
/// over the addendum window vs the baseline's median window mean).
const BUDGET_SUBSCRIBER_RSS_DELTA_MIB: f64 = 2.0;
/// See [`BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS`] (overall worst-kind read
/// p99, ms, with-subscriber pass vs the baseline pass).
const BUDGET_SUBSCRIBER_READ_P99_DELTA_MS: f64 = 100.0;

/// The DOCUMENTED shared-runner tolerance factor for the CPU gate in CI only
/// (see the module docs). Named here and in every report — never hidden.
const CI_CPU_TOLERANCE_FACTOR: f64 = 1.5;

/// The RATCHET tolerance (story 15-5): a gated metric may not be WORSE than
/// baseline × this factor. 1.15 (15%) absorbs CI-run-to-CI-run noise on the
/// SAME leg (where machine variance is small) while catching the slow
/// multi-week drift class the absolute budgets cannot see — reads creeping
/// from 6 ms toward 100 ms is green every week against a 1 s budget. If CI
/// proves noisier than 15% absorbs, this is a one-line change WITH a
/// baseline refresh in the same PR (the documented remedy, never a silent
/// tolerance tweak).
const RATCHET_TOLERANCE: f64 = 1.15;

/// The read-gate retry (CI mode only): one cool-down + re-measure, gated on
/// the second run — the house pattern so a co-tenant stall cannot red an
/// ordinary PR. Local runs never retry.
const READ_RETRY_COOLDOWN: Duration = Duration::from_secs(30);

/// How many steady-state windows CI mode samples (median-of-3); local runs
/// sample one.
const CI_STEADY_WINDOWS: usize = 3;

/// The report schema version (bump only on a breaking shape change; additive
/// fields keep it — the AD-14 convention).
const REPORT_SCHEMA_VERSION: u64 = 1;

// ---------------------------------------------------------------------------
// Serve-shape scenario constants (story 17-1) — pinned by the test battery
// like the NFR-4 fixture premise above
// ---------------------------------------------------------------------------

/// The serve-shape scenario's measurement window: a steady-window-shaped 12 s
/// (same 1 s sample cadence, same first-sample-discard discipline), long
/// enough to cover the mid-window bounded stop AND the reads that recover
/// after it.
const SERVE_WINDOW: Duration = Duration::from_secs(12);

/// The pathological instance's BOUNDED stop window: a host's tight bound (the
/// same bound the harness's own [`RunningGuard`] drop uses). The coarse model
/// holds the supervisor mutex for this whole window when the agent ignores
/// SIGTERM — exactly the contention term the serve shape exists to measure.
/// Changing it changes what is measured; it is pinned by a test.
const SERVE_STOP_WINDOW: Duration = Duration::from_secs(2);

/// How far into the serve window the bounded stop fires: early enough that
/// the stall lands while all readers are mid-storm, late enough that the
/// post-stop recovery reads are inside the same window's p99.
const SERVE_STOP_DELAY: Duration = Duration::from_secs(3);

/// The orphan bound ON SERVE RUNS ONLY: the serve window + the stop bound
/// extend the measurement schedule past [`AGENT_LINGER_MS`]'s 60 s, so the
/// heartbeat fixture is BUILT with this linger on serve runs and
/// [`check_liveness_schedule_serve`] asserts coverage against THIS bound.
/// Default runs keep [`AGENT_LINGER_MS`] and stay byte-equivalent.
const SERVE_AGENT_LINGER_MS: u64 = 120_000;

/// Concurrent reader tasks in the serve driver (serve-shaped concurrency:
/// several simultaneous read streams, not one).
const SERVE_READER_TASKS: usize = 4;

/// The serve readers' pace, DERIVED from the existing ratified fixture
/// premise (not a free knob): EACH reader carries the default phase's own
/// read demand — `READ_ITERATIONS` iterations per window — spread evenly
/// across [`SERVE_WINDOW`] (12 s / 200 = 60 ms between a reader's
/// iterations). Per reader that equals the default phase's demand; across
/// the [`SERVE_READER_TASKS`] concurrent readers the aggregate is ~4× the
/// default phase's (~800 iterations/window) — a serve shape, not a
/// single-pass replay. The pace is sleep-only think-time: a reader stalled
/// on the bounded stop delivers FEWER than its nominal iterations, and the
/// report records the delivered count beside the nominal demand. A host
/// serves at demand rate, and the process-wide sampling discipline cannot
/// separate the host's own handling cost from supervision — so the load
/// model IS part of the shape definition, and it is anchored to the
/// harness's own ratified read-demand premise rather than chosen after
/// seeing gate outcomes. Pure; pinned by tests.
fn serve_reader_pace(window: Duration, read_iterations: usize) -> Duration {
    window / (read_iterations.max(1) as u32)
}

/// Per-call retries for ONE serve read (review round 3): a transient read
/// error must not panic a serve reader mid-window — the read is retried
/// this many times with a short backoff, every retry is COUNTED (surfaced
/// in the report as `reader_retries`), and the retried read's latency
/// sample honestly includes the backoff it waited through.
const SERVE_READ_RETRIES: usize = 2;

/// The backoff between a serve read's retry attempts (a store-busy hiccup
/// clears in far less; the value only has to outlast scheduler jitter).
const SERVE_READ_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// How many CONSECUTIVE failed iterations one serve reader tolerates before
/// it aborts the scenario (review round 3): the abort is a NAMED error
/// through the scenario's cleanup — never a silent short window, and never
/// a first-transient panic.
const SERVE_MAX_CONSECUTIVE_FAILURES: usize = 3;

/// The driver runtime's worker threads — "a small multi-thread tokio
/// runtime" (the host's own runtime; the engine's internals ride the engine's).
const SERVE_DRIVER_WORKERS: usize = 4;

/// The pathological instance's registered name (valid
/// `[a-z0-9][a-z0-9_-]*`).
const SERVE_PATHOLOGICAL_NAME: &str = "serve-pathological";

/// The pathological fixture's manifest adapter kind (flow-local identity).
const SERVE_PATHOLOGICAL_KIND: &str = "servepathological";

// ---------------------------------------------------------------------------
// Environment (tunables + CI detection) — invalid values are LOUD, never silent
// ---------------------------------------------------------------------------

/// Progress diagnostics → stderr (CLI-first discipline: stdout is the report).
fn note(message: &str) {
    eprintln!("[perf-budgets] {message}");
}

/// Read a non-negative integer env override; an unparsable value WARNS and
/// falls back to the default (never a silent fallback, never a crash).
fn env_u64(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Err(_) => default,
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(value) => value,
            Err(_) => {
                note(&format!(
                    "invalid {key}={raw:?} (not a non-negative integer) — using default {default}"
                ));
                default
            }
        },
    }
}

/// Pure CI-mode selection: an explicit `PERF_BUDGETS_CI` of `1`/`0` wins;
/// anything ELSE is an ERROR (the caller exits 2 — a typoed override must
/// never silently pick a mode); unset falls to the GitHub-runner `CI` env.
fn ci_mode_from(explicit: Option<&str>, ambient: Option<&str>) -> Result<bool, String> {
    match explicit {
        Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(other) => Err(format!(
            "PERF_BUDGETS_CI must be \"1\" or \"0\", got {other:?}"
        )),
        None => Ok(matches!(ambient, Some("true") | Some("1"))),
    }
}

/// Pure clamp for the read-iteration override: returns the effective value
/// (at least 1 — zero iterations cannot summarize) and whether
/// [`MAX_READ_ITERATIONS`] was applied (so the caller can note it on stderr
/// — a clamp is never silent).
fn clamp_read_iterations(raw: u64) -> (usize, bool) {
    let capped = raw.min(MAX_READ_ITERATIONS);
    (capped.max(1) as usize, capped != raw)
}

/// The startup liveness assertion (the orphan bound was previously enforced
/// only by a comment): [`AGENT_LINGER_MS`] must strictly cover the ENTIRE
/// window schedule the heartbeat idlers must survive — the settle, EVERY
/// steady-state window (including the subscriber-overhead addendum window,
/// so callers pass the gated window count + 1), the 1 s gaps between
/// windows, and [`LIVENESS_MARGIN`]. An `Err` names the shortfall so `main`
/// can exit loudly instead of measuring self-exited (dead) idlers.
fn check_liveness_schedule(
    settle: Duration,
    window_count: usize,
    window: Duration,
) -> Result<(), String> {
    let gaps_ms = window_count.saturating_sub(1) as u128 * 1000; // 1 s between windows
    let schedule_ms = settle.as_millis() + window_count as u128 * window.as_millis() + gaps_ms;
    check_liveness_schedule_ms(schedule_ms, AGENT_LINGER_MS)
}

/// The serve-shape extension (story 17-1): the same schedule PLUS the serve
/// window and the pathological stop's bound, checked against the RAISED
/// serve orphan bound ([`SERVE_AGENT_LINGER_MS`] — the heartbeat fixture is
/// built with that linger on serve runs). Pinned by tests, like the default
/// schedule above.
fn check_liveness_schedule_serve(
    settle: Duration,
    window_count: usize,
    window: Duration,
    serve_window: Duration,
    stop_bound: Duration,
) -> Result<(), String> {
    let gaps_ms = window_count.saturating_sub(1) as u128 * 1000; // 1 s between windows
    let schedule_ms = settle.as_millis()
        + window_count as u128 * window.as_millis()
        + gaps_ms
        + serve_window.as_millis()
        + stop_bound.as_millis();
    check_liveness_schedule_ms(schedule_ms, SERVE_AGENT_LINGER_MS)
}

/// The shared bound comparison (named once so the error text cannot drift
/// between the default and serve arms): `linger_ms` must strictly exceed the
/// schedule plus the margin.
fn check_liveness_schedule_ms(schedule_ms: u128, linger_ms: u64) -> Result<(), String> {
    let required_ms = schedule_ms + LIVENESS_MARGIN.as_millis();
    if u128::from(linger_ms) > required_ms {
        Ok(())
    } else {
        Err(format!(
            "the orphan bound linger={linger_ms} ms does not cover the \
             measurement schedule ({schedule_ms} ms of settle + windows + gaps, plus a {} ms \
             margin) — the heartbeat idlers would self-exit mid-window and the figures \
             would measure nothing; shrink PERF_BUDGETS_SETTLE_MS / PERF_BUDGETS_WINDOW_MS \
             or raise the fixture's --linger-ms with the spec record",
            LIVENESS_MARGIN.as_millis()
        ))
    }
}

/// The run configuration, resolved from the environment once. Clamps are
/// loud (a stderr note) and bounded: settle ≥ 1 s, window ∈ [10 s, 1 h]
/// (the spec's ≥ 10 s floor; the 1 h cap keeps a stray override from hanging
/// the CI job).
/// Clones/Copies so the serve scenario can derive a sampler-scoped config
/// (the clamped window) without touching the caller's.
#[derive(Clone, Copy)]
struct HarnessConfig {
    read_iterations: usize,
    settle: Duration,
    window: Duration,
    sample_interval: Duration,
    windows: usize,
    ci: bool,
}

impl HarnessConfig {
    fn from_env() -> Self {
        let (read_iterations, iterations_clamped) =
            clamp_read_iterations(env_u64("PERF_BUDGETS_READ_ITERATIONS", 200));
        if iterations_clamped {
            note(&format!(
                "PERF_BUDGETS_READ_ITERATIONS above the {MAX_READ_ITERATIONS} cap — clamped"
            ));
        }
        let settle_ms = env_u64("PERF_BUDGETS_SETTLE_MS", 2_000);
        let settle = if settle_ms < 1_000 {
            note(&format!(
                "PERF_BUDGETS_SETTLE_MS={settle_ms} below the 1000 ms floor — clamped"
            ));
            Duration::from_millis(1_000)
        } else {
            Duration::from_millis(settle_ms)
        };
        let window_ms = env_u64("PERF_BUDGETS_WINDOW_MS", 12_000);
        let window_ms = if !(10_000..=3_600_000).contains(&window_ms) {
            note(&format!(
                "PERF_BUDGETS_WINDOW_MS={window_ms} outside [10000, 3600000] — clamped"
            ));
            window_ms.clamp(10_000, 3_600_000)
        } else {
            window_ms
        };
        let ci = match ci_mode_from(
            std::env::var("PERF_BUDGETS_CI").ok().as_deref(),
            std::env::var("CI").ok().as_deref(),
        ) {
            Ok(ci) => ci,
            Err(reason) => {
                note(&format!("{reason} — exiting 2"));
                std::process::exit(2);
            }
        };
        let windows = if ci { CI_STEADY_WINDOWS } else { 1 };
        Self {
            read_iterations,
            settle,
            window: Duration::from_millis(window_ms),
            sample_interval: Duration::from_secs(1),
            windows,
            ci,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure gate math (pinned by the #[cfg(test)] module below — the alarm cannot
// silently weaken: a budget or evaluation change must edit those assertions)
// ---------------------------------------------------------------------------

/// One read kind's summarized latency statistics (milliseconds).
#[derive(Clone, Copy, Debug, PartialEq)]
struct ReadStats {
    name: &'static str,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    samples: usize,
}

/// Summarize one read kind's raw samples (sorts in place; nearest-rank).
fn summarize_read(name: &'static str, samples_ms: &mut [f64]) -> ReadStats {
    samples_ms.sort_by(|a, b| a.total_cmp(b));
    ReadStats {
        name,
        p50: percentile(samples_ms, 50.0),
        p95: percentile(samples_ms, 95.0),
        p99: percentile(samples_ms, 99.0),
        max: samples_ms.last().copied().unwrap_or(0.0),
        samples: samples_ms.len(),
    }
}

/// The worst kind's p99 — the value the read gate applies, because the budget
/// is "< 1 s for read commands", i.e. EVERY read command.
fn overall_read_p99(reads: &[ReadStats]) -> f64 {
    reads
        .iter()
        .map(|r| r.p99)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Nearest-rank percentile over an ASCENDING-sorted sample list.
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[(rank - 1).min(sorted.len() - 1)]
}

/// Median over an ASCENDING-sorted list (mean of the two middles when even).
fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    match n {
        0 => 0.0,
        odd if odd % 2 == 1 => sorted[odd / 2],
        _ => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

/// Per-instance normalization: AGGREGATE figure / actually-Running count
/// (the budget's "per running instance" division).
fn per_instance(aggregate: f64, running_count: usize) -> f64 {
    if running_count == 0 {
        return f64::INFINITY;
    }
    aggregate / running_count as f64
}

/// The normalized steady-state figures the gates consume.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SteadyFigures {
    cpu_per_instance_pct: f64,
    rss_per_instance_mib: f64,
    rss_max_per_instance_mib: f64,
}

impl SteadyFigures {
    fn new(
        median_cpu_aggregate_pct: f64,
        median_rss_bytes_mean: f64,
        rss_bytes_max: u64,
        running_count: usize,
    ) -> Self {
        const MIB: f64 = 1024.0 * 1024.0;
        Self {
            cpu_per_instance_pct: per_instance(median_cpu_aggregate_pct, running_count),
            rss_per_instance_mib: per_instance(median_rss_bytes_mean / MIB, running_count),
            rss_max_per_instance_mib: per_instance(rss_bytes_max as f64 / MIB, running_count),
        }
    }
}

/// The subscriber-overhead addendum figures: the SAME per-instance steady
/// figures and overall read p99 measured with ONE active subscriber
/// attached, plus the deltas vs the unsubscribed baseline. Pure delta math,
/// pinned by tests. The deltas GATE against the ratified subscriber-active
/// budgets (story 10-3) — strict locally, at the shared-runner tolerance on
/// CI (see `evaluate_gates`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct SubscriberOverhead {
    cpu_per_instance_pct: f64,
    rss_per_instance_mib_mean: f64,
    read_p99_overall_ms: f64,
    cpu_delta_pct_points: f64,
    rss_delta_mib: f64,
    read_p99_delta_ms: f64,
}

impl SubscriberOverhead {
    fn new(
        baseline: &SteadyFigures,
        baseline_p99_ms: f64,
        with_subscriber: &SteadyFigures,
        with_subscriber_p99_ms: f64,
    ) -> Self {
        Self {
            cpu_per_instance_pct: with_subscriber.cpu_per_instance_pct,
            rss_per_instance_mib_mean: with_subscriber.rss_per_instance_mib,
            read_p99_overall_ms: with_subscriber_p99_ms,
            cpu_delta_pct_points: with_subscriber.cpu_per_instance_pct
                - baseline.cpu_per_instance_pct,
            rss_delta_mib: with_subscriber.rss_per_instance_mib - baseline.rss_per_instance_mib,
            read_p99_delta_ms: with_subscriber_p99_ms - baseline_p99_ms,
        }
    }
}

/// The comparison direction a gate applies: `Le` (`measured ≤ budget`) or
/// `Lt` (`measured < budget`). The read budget's prose is "< 1 s" — a
/// STRICT inequality — so a p99 EXACTLY at 1000.0 ms must fail it; the
/// RSS/CPU budgets are "≤ budget" ceilings, so exactly-at passes there. The
/// direction is carried per gate (and printed in the report) so the gate
/// math cannot silently soften a "<" into a "≤".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GateBound {
    Le,
    Lt,
}

impl GateBound {
    fn holds(self, measured: f64, effective_budget: f64) -> bool {
        match self {
            GateBound::Le => measured <= effective_budget,
            GateBound::Lt => measured < effective_budget,
        }
    }

    /// The wire form printed in the report's `gates` array.
    fn as_str(self) -> &'static str {
        match self {
            GateBound::Le => "<=",
            GateBound::Lt => "<",
        }
    }
}

/// One gate's verdict. `budget` is the STRICT budget; `effective_budget` is
/// what this run gated on (the strict budget × this run's tolerance), and
/// `strict_margin` = strict budget − measured (negative on a CI pass that
/// only cleared the tolerance).
#[derive(Clone, Copy, Debug, PartialEq)]
struct GateVerdict {
    name: &'static str,
    hard: bool,
    bound: GateBound,
    measured: f64,
    budget: f64,
    effective_budget: f64,
    tolerance: f64,
    strict_margin: f64,
    passed: bool,
}

/// The complete gate evaluation for one run.
#[derive(Clone, Debug, PartialEq)]
struct GateReport {
    verdicts: Vec<GateVerdict>,
    passed: bool,
}

/// Evaluate every gate per the documented policy: reads and RSS (mean AND
/// max-spike) gate at strict budget on every gate platform (ubuntu CI; strict
/// local macOS — Windows is measured-and-reported, never a gate platform);
/// CPU gates strict (×1.0)
/// locally and at the named shared-runner tolerance (×1.5) on CI. The read
/// gate uses the STRICT `<` bound (the budget is "< 1 s" — exactly-at
/// fails); the RSS/CPU ceilings use `≤`. The subscriber-active DELTAS
/// (story 10-3) gate against the ratified budgets with the CPU gate's
/// tolerance shape — `≤` bounds, strict locally, tolerated on CI — and a
/// negative delta (the subscriber pass measured cheaper) passes trivially:
/// the budget ceilings the OVERHEAD, never a measurement sign.
fn evaluate_gates(
    overall_p99_ms: f64,
    steady: &SteadyFigures,
    subscriber: &SubscriberOverhead,
    tolerance: f64,
) -> GateReport {
    let mut verdicts = vec![
        gate(
            "read_p99_lt_1s",
            overall_p99_ms,
            BUDGET_READ_P99_MS,
            1.0,
            GateBound::Lt,
        ),
        gate(
            "rss_per_instance_le_50mib",
            steady.rss_per_instance_mib,
            BUDGET_RSS_PER_INSTANCE_MIB,
            1.0,
            GateBound::Le,
        ),
        gate(
            "rss_max_spike_le_2x_budget",
            steady.rss_max_per_instance_mib,
            BUDGET_RSS_PER_INSTANCE_MIB * RSS_SPIKE_TOLERANCE,
            1.0,
            GateBound::Le,
        ),
        gate(
            "cpu_per_instance_le_2pct",
            steady.cpu_per_instance_pct,
            BUDGET_CPU_PER_INSTANCE_PCT,
            tolerance,
            GateBound::Le,
        ),
    ];
    verdicts.extend(subscriber_gates(subscriber, tolerance));
    let passed = verdicts.iter().all(|v| v.passed);
    GateReport { verdicts, passed }
}

/// The three story-10-3 subscriber-active delta gates (appended AFTER the
/// four zero-subscriber gates, so verdict indices 0–3 keep their meaning):
/// CPU Δ pct-points, RSS Δ MiB, read-p99 Δ ms — each `≤` its ratified budget
/// × tolerance (strict ×1.0 locally; the shared-runner ×1.5 on CI, the same
/// policy shape as the CPU gate).
fn subscriber_gates(subscriber: &SubscriberOverhead, tolerance: f64) -> Vec<GateVerdict> {
    vec![
        gate(
            "subscriber_cpu_delta_le_budget",
            subscriber.cpu_delta_pct_points,
            BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS,
            tolerance,
            GateBound::Le,
        ),
        gate(
            "subscriber_rss_delta_le_budget",
            subscriber.rss_delta_mib,
            BUDGET_SUBSCRIBER_RSS_DELTA_MIB,
            tolerance,
            GateBound::Le,
        ),
        gate(
            "subscriber_read_p99_delta_le_budget",
            subscriber.read_p99_delta_ms,
            BUDGET_SUBSCRIBER_READ_P99_DELTA_MS,
            tolerance,
            GateBound::Le,
        ),
    ]
}

/// One gate: `measured` against `budget × tolerance` in the gate's own bound
/// direction (strict when tolerance is 1.0).
fn gate(
    name: &'static str,
    measured: f64,
    budget: f64,
    tolerance: f64,
    bound: GateBound,
) -> GateVerdict {
    let effective_budget = budget * tolerance;
    GateVerdict {
        name,
        hard: true,
        bound,
        measured,
        budget,
        effective_budget,
        tolerance,
        strict_margin: budget - measured,
        passed: bound.holds(measured, effective_budget),
    }
}

// ---------------------------------------------------------------------------
// Serve-shape gates (story 17-1) — the SAME machinery (`GateBound`,
// per-instance normalization, the CI tolerance shape) at the RATIFIED NFR-4
// budgets, under the serve gates' OWN names. Bound only on serve runs; kept
// OUT of the default trend evaluation (the scenario-scoped baseline rule).
// ---------------------------------------------------------------------------

/// Evaluate the three serve gates (pinned by tests): read p99 STRICT `<` 1 s
/// (the budget's own prose, on every gate platform); CPU ≤ 2%/instance with
/// the documented shared-runner tolerance shape (strict ×1.0 locally, ×1.5 on
/// CI); RSS ≤ 50 MiB/instance at budget everywhere. A zero running count
/// fails closed exactly like the default steady gates ([`per_instance`]'s
/// +inf).
fn evaluate_serve_gates(
    overall_p99_ms: f64,
    figures: &SteadyFigures,
    tolerance: f64,
) -> GateReport {
    let verdicts = vec![
        gate(
            "serve_read_p99_lt_1s",
            overall_p99_ms,
            BUDGET_READ_P99_MS,
            1.0,
            GateBound::Lt,
        ),
        gate(
            "serve_cpu_per_instance_le_2pct",
            figures.cpu_per_instance_pct,
            BUDGET_CPU_PER_INSTANCE_PCT,
            tolerance,
            GateBound::Le,
        ),
        gate(
            "serve_rss_per_instance_le_50mib",
            figures.rss_per_instance_mib,
            BUDGET_RSS_PER_INSTANCE_MIB,
            1.0,
            GateBound::Le,
        ),
    ];
    let passed = verdicts.iter().all(|v| v.passed);
    GateReport { verdicts, passed }
}

/// Merge one read kind's per-reader sample vectors into ONE sample list for
/// [`summarize_read`] (pure; pinned by tests). Order across readers is
/// irrelevant to the sorted percentile math, but the concatenation is
/// explicit so a lost vector is a length mismatch, not a silent short summary.
fn merge_kind_samples(parts: &[Vec<f64>]) -> Vec<f64> {
    let mut all = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for part in parts {
        all.extend_from_slice(part);
    }
    all
}

/// The serve scenario's CONTENTION PROOF (review round 3): the bounded stop
/// must have overlapped in-flight reads — specifically, at least one read
/// must have STARTED inside the stop's measured window. A serve window where
/// no read started during the stop measured idle supervision, not the
/// lock-contention term the shape exists to measure: its figures are vacuous
/// evidence, and the fixed decision rule must not route on them. Pure;
/// pinned. `main` maps `Err` to a NAMED exit-2 BEFORE any gate verdict is
/// computed or printed (the scenario's own teardown has already run).
fn serve_contention_exercised(reads_started_during_stop: u64) -> Result<(), String> {
    if reads_started_during_stop == 0 {
        Err(
            "serve contention NOT exercised: the bounded stop never overlapped any \
             in-flight read — the window's figures are idle-supervision numbers, not \
             the serve shape's, so no gate verdict is computed; check the serve reader \
             tasks (a dead reader is the usual cause) and re-run"
                .to_string(),
        )
    } else {
        Ok(())
    }
}

/// The serve window's SAMPLER window (review round 3): the sampling window is
/// CLAMPED to the serve load window. A raised `PERF_BUDGETS_WINDOW_MS` would
/// otherwise sample past the 12 s load into idle tail, diluting the
/// per-instance CPU/RSS figures toward passing; the clamp keeps the sampling
/// window exactly as wide as the load it claims to measure. Pure; pinned.
fn serve_sampling_window(configured: Duration) -> Duration {
    configured.min(SERVE_WINDOW)
}

/// The schedule-aware Running count at `t` into the serve window (review
/// round 3): the full sampled count until the bounded stop COMPLETES
/// ([`SERVE_STOP_DELAY`] + [`SERVE_STOP_WINDOW`] — the pathological leaves
/// the Running set when its stop lands), one fewer after. Pure in the
/// constants; pinned by tests.
fn serve_running_at(running_before_stop: usize, t: Duration) -> usize {
    if t >= SERVE_STOP_DELAY + SERVE_STOP_WINDOW {
        running_before_stop.saturating_sub(1)
    } else {
        running_before_stop
    }
}

/// The per-instance normalization divisor for the serve window (review round
/// 3): the MINIMUM Running count the schedule puts in effect across the
/// window — the post-stop count. Dividing by the pre-window count would
/// UNDERSTATE per-instance cost after the stop (the lenient direction —
/// wrong for a gate); the conservative divisor can only overstate a
/// per-instance figure, never understate one. Pure; pinned by tests.
fn serve_effective_running_instances(running_before_stop: usize) -> usize {
    serve_running_at(running_before_stop, SERVE_WINDOW)
}

/// The serve gates' PLATFORM SCOPE (review round 3), the standing rule the
/// process-metric budgets carry (docs/testing.md, gate-platform scope):
/// process figures are ratified on the gate platform (Linux CI; strict local
/// macOS) and are measured-and-reported elsewhere — Windows is NOT a
/// ratified gate platform (sysinfo's `memory()` is the working-set-size
/// analog there). On a non-gate platform the serve verdicts are REPORT-ONLY:
/// they carry `hard: false` in the report, never set the run's
/// `serve_failed`, and a named stderr note says so. Pure; pinned by tests.
fn serve_gates_bind(os: &str) -> bool {
    matches!(os, "linux" | "macos")
}

// ---------------------------------------------------------------------------
// Serve-shape scenario driver (story 17-1) — the engine's PUBLIC async facade
// under serve-shaped concurrency, exactly as an embedded host drives it
// (docs/embedding.md): the driver's OWN small multi-thread tokio runtime,
// concurrent read streams, one subscribed follow-style reader, and one
// pathological instance stopped with a BOUNDED window mid-load. No HTTP
// transport (the lock-model question is engine-side; a transport would only
// add noise), no private reach-in.
// ---------------------------------------------------------------------------

/// The pathological fixture whose BOUNDED stop the serve window measures.
/// Returns the fixture plus the honest note (surfaced in the report's
/// `serve_shape.fixture.note`) naming WHY the stop is genuinely slow on this
/// OS — both arms go through the SAME conformance `ManifestFixture`
/// machinery, and the OS branch is a RUNTIME branch (the conformance crate
/// forbids `#[cfg]`).
///
/// * Unix: the lifecycle suite's proven shape — `sh -c 'trap "" TERM; sleep
///   120'` IGNORES SIGTERM, so the supervisor's graceful step elapses the
///   whole window before the SIGKILL escalation
///   (`stop_escalates_to_forced_kill_and_records_it`). The self-exit sleep
///   is DERIVED from [`SERVE_AGENT_LINGER_MS`] (the bound only has to
///   outlive the serve window it is validated against). The conformance
///   `fake_agent` cannot install a signal handler (the OS-cfg gate forbids
///   OS-specific code), so the `sh` exec is the only Unix shape whose stop
///   is genuinely slow.
/// * Windows: the lingering `fake_agent` — the graceful step always elapses
///   the full window before `TerminateJobObject`, so the same bounded-window
///   contention is measured without needing a signal handler.
fn pathological_fixture() -> (ManifestFixture, &'static str) {
    if std::env::consts::OS == "windows" {
        (
            ManifestFixture::new(
                SERVE_PATHOLOGICAL_KIND,
                &[&format!("--linger-ms={SERVE_AGENT_LINGER_MS}")],
            )
            .exec(fake_agent_bin_in(BinDir::Examples))
            .guaranteed_on_all_oses("interaction"),
            "windows: fake_agent --linger-ms (self-exit bound); the graceful stop step \
             always elapses the full bounded window before TerminateJobObject, so the \
             supervisor mutex is held for the whole window",
        )
    } else {
        // The self-exit bound is DERIVED from the serve orphan bound (review
        // round 3): `sleep 60` would self-exit BELOW the 120 s schedule the
        // fixture is validated against — the sleep only has to outlive the
        // serve window, so it IS the serve bound (in seconds).
        (
            ManifestFixture::new(
                SERVE_PATHOLOGICAL_KIND,
                &[
                    "-c",
                    &format!("trap '' TERM; sleep {}", SERVE_AGENT_LINGER_MS / 1000),
                ],
            )
            .exec("sh")
            .guaranteed_on_all_oses("interaction"),
            "unix: sh -c 'trap \"\" TERM; sleep 120' IGNORES SIGTERM (the lifecycle \
             suite's proven shape) — the graceful window elapses before the SIGKILL \
             escalation, holding the supervisor mutex for the whole bounded window; \
             the self-exit bound IS the serve orphan bound (SERVE_AGENT_LINGER_MS, in \
             seconds), so the fixture can never self-exit inside the schedule it is \
             validated against",
        )
    }
}

/// What one serve window's driver collected (raw; `run_serve_scenario`
/// summarizes it through the same pure math the default phases use).
struct ServeLoad {
    /// Per-reader raw sample vectors for each read kind, in task order
    /// (`SERVE_READER_TASKS` parts each — merged by [`merge_kind_samples`]).
    fleet: Vec<Vec<f64>>,
    instance_status: Vec<Vec<f64>>,
    effective_config: Vec<Vec<f64>>,
    usage_via_fleet: Vec<Vec<f64>>,
    /// Events the subscribed follower counted (committed deliveries only).
    follower_events: u64,
    /// How many events the follower recovered from `Lagged` (backlog skip) —
    /// surfaced, never a crash, never a silent drop (AI-18).
    follower_lagged: u64,
    /// How long the pathological instance's bounded stop took end-to-end.
    stop_elapsed_ms: f64,
    /// The pathological instance's final state (serde-rendered, quotes
    /// trimmed — e.g. `stopped`).
    pathological_final_state: String,
    /// The stop's LAST recorded transition cause, serde-serialized verbatim.
    pathological_stop_cause: String,
    /// Delivered iterations PER READER (review round 3): the pace is
    /// sleep-only think-time, so a reader stalled on the bounded stop
    /// delivers fewer than the nominal `read_iterations` — recorded beside
    /// the nominal demand, never implied.
    reader_iterations: Vec<usize>,
    /// Reads that STARTED inside the bounded stop's measured window (review
    /// round 3) — the contention proof: zero means the window measured idle
    /// supervision, and the run fails with a named exit-2 before any gate
    /// verdict.
    reads_started_during_stop: u64,
    /// Serve-read retry attempts across all readers (review round 3) —
    /// surfaced in the report, never silent.
    reader_retries: u64,
}

/// Everything the `serve_shape` report block and the three serve gates need
/// (story 17-1) — the summarized outcome of [`run_serve_scenario`].
struct ServeShapeOutcome {
    reads: Vec<ReadStats>,
    overall_p99_ms: f64,
    steady: SteadyMeasurements,
    figures: SteadyFigures,
    running_count: usize,
    /// The schedule-aware normalization divisor (review round 3): the
    /// minimum Running count the window's schedule puts in effect (the
    /// post-stop count) — the conservative per-instance bound, disclosed in
    /// the report beside the pre-stop sampled count.
    effective_running_instances: usize,
    load: ServeLoad,
    fixture_note: &'static str,
    /// The serve driver's derived pace + per-reader demand (the load model,
    /// recorded in the report so the shape is reproducible).
    pace: Duration,
    read_iterations: usize,
    /// The sampler's clamped window (review round 3): the sampling window is
    /// clamped to the serve load window so a raised
    /// `PERF_BUDGETS_WINDOW_MS` cannot dilute the figures with idle tail.
    sampling_window_ms: u64,
}

/// Drive ONE serve window against the engine's PUBLIC async facade: a
/// subscribed follower counting the lock-free bus (a `Lagged` is counted and
/// surfaced, never a crash); `SERVE_READER_TASKS` concurrent reader tasks
/// round-robin every fixture instance (including the pathological) through
/// the SAME four content-asserted read kinds the default phase measures; and
/// ONE bounded stop of the pathological instance fires mid-load. Returns
/// `Err` (a NAMED reason) when a reader aborted on repeated consecutive
/// failures or panicked — the window's figures would be incomplete, so the
/// scenario fails loudly instead of measuring a short storm. Also records,
/// per reader: the DELIVERED iteration count (the pace is sleep-only; a
/// stalled reader delivers fewer than nominal), the retries transient read
/// errors consumed (counted, surfaced — never a first-transient panic), and
/// whether any read STARTED inside the stop's measured window (the
/// contention proof, checked by [`serve_contention_exercised`]). Pure
/// orchestration — no sysinfo (the CPU/RSS window runs on the caller's
/// separate std::thread), no report, no gates.
async fn drive_serve_load(
    engine: Arc<Engine>,
    names: Vec<String>,
    follower: broadcast::Receiver<EngineEvent>,
    follower_stop: Arc<AtomicBool>,
    pace: Duration,
) -> Result<ServeLoad, String> {
    let deadline = Instant::now() + SERVE_WINDOW;
    // The bounded stop's MEASURED window (review round 3), recorded by the
    // stop task when the stop returns; readers check whether their iteration
    // started inside it — the contention proof's raw count.
    let stop_window = Arc::new(std::sync::OnceLock::<(Instant, Instant)>::new());
    // Serve-read retries across ALL readers (review round 3) — counted and
    // surfaced in the report, never silent.
    let retries = Arc::new(AtomicU64::new(0));

    // ---- The follower FIRST (spawned before any read, so it observes every
    // committed event of the window): one --follow-style subscribed reader
    // on the lock-free `Engine::subscribe()` bus. A `Lagged` (the bus moved
    // past this receiver) is COUNTED and surfaced — never a crash, never a
    // silent drop (AI-18). It exits when `follower_stop` flips, observed
    // within one 250 ms poll. ----
    let follower_flag = Arc::clone(&follower_stop);
    let follower_handle = tokio::spawn(async move {
        let mut follower = follower;
        let mut events = 0u64;
        let mut lagged = 0u64;
        loop {
            if follower_flag.load(Ordering::Relaxed) {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(250), follower.recv()).await {
                Ok(Ok(_event)) => events += 1,
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => lagged += skipped,
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_timeout) => {} // re-check the stop flag
            }
        }
        (events, lagged)
    });

    // ---- The reader storm: SERVE_READER_TASKS concurrent tasks, each
    // looping the four read kinds round-robin (task `t` starts at offset
    // `t` and strides by the task count, so the streams interleave across
    // the whole fixture), paced at `pace` per iteration — the derived
    // host-shape load (see `serve_reader_pace`). Content-asserted EXACTLY
    // like the default reads — a read that silently degraded must not count
    // as a fast read. Transient ERRORS are retried with a short backoff
    // (review round 3: the retry machinery the default phase's CI gate has,
    // mirrored per read); the retries are counted, a retried read's sample
    // honestly includes its backoff, and only repeated CONSECUTIVE failures
    // abort the reader (a named error, never a silent short window). ----
    let mut reader_handles = Vec::with_capacity(SERVE_READER_TASKS);
    for task in 0..SERVE_READER_TASKS {
        let engine = Arc::clone(&engine);
        let names = names.clone();
        let retries = Arc::clone(&retries);
        let stop_window = Arc::clone(&stop_window);
        reader_handles.push(tokio::spawn(async move {
            let mut fleet = Vec::new();
            let mut status = Vec::new();
            let mut config = Vec::new();
            let mut usage = Vec::new();
            let mut done = 0usize;
            let mut overlaps = 0u64;
            let mut consecutive_failures = 0usize;
            let mut i = task;
            while Instant::now() < deadline {
                let name = names[i % names.len()].clone();
                // The iteration's START is the contention proof's timestamp:
                // did a read begin inside the stop's window?
                let iter_started = Instant::now();

                let attempt = async {
                    let t = Instant::now();
                    let entries =
                        retry_serve_read("serve fleet read", &retries, || engine.fleet()).await?;
                    let fleet_ms = elapsed_ms(t);
                    assert_eq!(
                        entries.len(),
                        names.len(),
                        "serve fleet read must list the whole fixture (including the pathological)"
                    );

                    let t = Instant::now();
                    let state = retry_serve_read("serve instance_status read", &retries, || {
                        engine.instance_status(&name)
                    })
                    .await?;
                    let status_ms = elapsed_ms(t);
                    assert_eq!(
                        state.instance.name.as_str(),
                        name,
                        "serve instance_status must return the requested instance"
                    );

                    let t = Instant::now();
                    let effective =
                        retry_serve_read("serve effective_config read", &retries, || {
                            engine.effective_config(&name, ConfigLayer::empty())
                        })
                        .await?;
                    let config_ms = elapsed_ms(t);
                    assert_eq!(
                        effective.value_display(MODEL_KEY).as_deref(),
                        Some(MODEL_VALUE),
                        "the seeded config leaf must read back through the serve read"
                    );

                    // The usage read, exactly as `kt agent usage <name>` performs
                    // it through the async facade: fleet() + find. (A missing
                    // entry is a CONTENT failure — the fixture is static — so it
                    // stays a loud panic, not a retry.)
                    let t = Instant::now();
                    let entry =
                        retry_serve_read("serve usage read: fleet", &retries, || engine.fleet())
                            .await?
                            .into_iter()
                            .find(|e| e.name.as_str() == name)
                            .expect("serve usage read: the instance is in the Fleet");
                    let usage_ms = elapsed_ms(t);
                    assert_eq!(
                        entry.name.as_str(),
                        name,
                        "the serve usage read found its instance"
                    );
                    Ok::<(f64, f64, f64, f64), String>((fleet_ms, status_ms, config_ms, usage_ms))
                };
                match attempt.await {
                    Ok((f, s, c, u)) => {
                        fleet.push(f);
                        status.push(s);
                        config.push(c);
                        usage.push(u);
                        done += 1;
                        consecutive_failures = 0;
                    }
                    Err(reason) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= SERVE_MAX_CONSECUTIVE_FAILURES {
                            return Err(format!(
                                "aborted after {consecutive_failures} consecutive failed \
                                 iteration(s); last error: {reason}"
                            ));
                        }
                    }
                }

                // Contention accounting (review round 3): did this iteration
                // START inside the stop's measured window? The stop task
                // records the window when the stop returns, so an iteration
                // that began inside it sees the record here.
                if let Some((start, end)) = stop_window.get() {
                    if iter_started >= *start && iter_started <= *end {
                        overlaps += 1;
                    }
                }

                // The reader's think-time: the pace derives from the default
                // phase's read demand (see `serve_reader_pace`) — the load
                // model, not a spinner's cadence.
                tokio::time::sleep(pace).await;
                i += SERVE_READER_TASKS;
            }
            Ok((fleet, status, config, usage, done, overlaps))
        }));
    }

    // ---- The bounded stop: fires SERVE_STOP_DELAY into the window (all
    // readers mid-storm), with the tight SERVE_STOP_WINDOW — the coarse
    // model holds the supervisor mutex for the whole graceful step, and
    // every read that lands there stalls with it. That stall distribution
    // IS the measurement. ----
    let stop_engine = Arc::clone(&engine);
    let stop_window_for_stop = Arc::clone(&stop_window);
    let stop_handle = tokio::spawn(async move {
        tokio::time::sleep(SERVE_STOP_DELAY).await;
        let t = Instant::now();
        let stopped = stop_engine
            .stop(SERVE_PATHOLOGICAL_NAME, Some(SERVE_STOP_WINDOW))
            .await
            .expect("the serve pathological bounded stop");
        let stop_elapsed_ms = elapsed_ms(t);
        // Record the stop's MEASURED window (review round 3): the readers'
        // contention accounting reads this when their in-flight iterations
        // complete.
        let _ = stop_window_for_stop.set((t, Instant::now()));
        let pathological_final_state = serde_json::to_string(&stopped.state)
            .unwrap_or_else(|_| "\"unknown\"".to_string())
            .trim_matches('"')
            .to_string();
        let pathological_stop_cause = stop_engine
            .transition_events(SERVE_PATHOLOGICAL_NAME)
            .await
            .ok()
            .and_then(|events| events.last().cloned())
            .map(|event| {
                serde_json::to_string(&event.cause).unwrap_or_else(|_| "\"unknown\"".to_string())
            })
            .unwrap_or_else(|| "\"unknown\"".to_string());
        (
            stop_elapsed_ms,
            pathological_final_state,
            pathological_stop_cause,
        )
    });

    // ---- Collect: readers run to the deadline, the stop completes inside
    // it, then the follower is drained (one poll interval at most). A reader
    // that aborted (repeated consecutive failures) or panicked makes the
    // whole scenario Err — AFTER the stop task is awaited, so the
    // pathological is stopped on every exit path. ----
    let mut fleet = Vec::new();
    let mut status = Vec::new();
    let mut config = Vec::new();
    let mut usage = Vec::new();
    let mut reader_iterations = Vec::with_capacity(SERVE_READER_TASKS);
    let mut reads_started_during_stop = 0u64;
    let mut reader_error: Option<String> = None;
    for (task, handle) in reader_handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok((f, s, c, u, done, overlaps))) => {
                fleet.push(f);
                status.push(s);
                config.push(c);
                usage.push(u);
                reader_iterations.push(done);
                reads_started_during_stop += overlaps;
            }
            Ok(Err(reason)) => {
                reader_error
                    .get_or_insert_with(|| format!("serve reader task {task} aborted: {reason}"));
            }
            Err(join_err) => {
                reader_error.get_or_insert_with(|| {
                    format!("serve reader task {task} panicked: {join_err}")
                });
            }
        }
    }
    let (stop_elapsed_ms, pathological_final_state, pathological_stop_cause) =
        stop_handle.await.expect("the serve stop task panicked");
    follower_stop.store(true, Ordering::Relaxed);
    let (follower_events, follower_lagged) = follower_handle
        .await
        .expect("the serve follower task panicked");

    if let Some(reason) = reader_error {
        return Err(format!(
            "{reason} — the window's figures are INCOMPLETE (the bounded stop itself \
             completed in {stop_elapsed_ms:.1} ms, so the pathological is stopped); \
             the serve scenario fails loudly rather than measure a short storm"
        ));
    }

    Ok(ServeLoad {
        fleet,
        instance_status: status,
        effective_config: config,
        usage_via_fleet: usage,
        follower_events,
        follower_lagged,
        stop_elapsed_ms,
        pathological_final_state,
        pathological_stop_cause,
        reader_iterations,
        reads_started_during_stop,
        reader_retries: retries.load(Ordering::Relaxed),
    })
}

/// One serve read with BOUNDED retries (review round 3 — the per-read mirror
/// of the default phase's CI retry machinery): `op` is attempted up to
/// `1 + SERVE_READ_RETRIES` times with [`SERVE_READ_RETRY_BACKOFF`] between
/// attempts; every retry is counted in `retries` (surfaced in the report as
/// `reader_retries`); exhaustion returns the LAST error, named — the caller's
/// consecutive-failure accounting decides the abort. The retried read's
/// latency sample honestly includes the backoff it waited through.
async fn retry_serve_read<T, E, F>(
    name: &'static str,
    retries: &AtomicU64,
    mut op: impl FnMut() -> F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut last = String::new();
    for attempt in 0..=SERVE_READ_RETRIES {
        if attempt > 0 {
            retries.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(SERVE_READ_RETRY_BACKOFF).await;
        }
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!(
        "{name} failed after {} attempt(s): {last}",
        SERVE_READ_RETRIES + 1
    ))
}

/// Run the whole serve-shape scenario (story 17-1): build + register + start
/// the pathological fixture, subscribe, drive one serve window on the
/// driver's own runtime while a separate std::thread samples the engine
/// process's CPU/RSS with the same sysinfo discipline, then summarize the
/// load through the same pure math the default phases use. Returns `Err` (a
/// NAMED reason) when the scenario cannot yield valid serve evidence — a
/// reader aborted/panicked, or the contention proof failed (`main` maps that
/// to a named exit-2 BEFORE any gate verdict; the scenario's own cleanup has
/// run either way). The pathological runs under its OWN [`RunningGuard`]:
/// defused after the scenario's successful stop, so Drop (a bounded
/// best-effort stop) covers every panic path.
fn run_serve_scenario(
    engine: &Arc<Engine>,
    facade: &Blocking<'_>,
    names: &[String],
    config: &HarnessConfig,
) -> Result<ServeShapeOutcome, String> {
    // ---- The pathological fixture: its OWN manifest dir (a fixture's
    // write() owns the directory it is given — the heartbeat fixture already
    // owns the main one). Declared BEFORE the guard so the dir outlives it
    // (the guard's stop may still need the manifest on a panic path). ----
    let (fixture, fixture_note) = pathological_fixture();
    let serve_manifest_tmp = tempfile::TempDir::new().expect("create the serve manifest dir");
    let serve_manifest = fixture.write(serve_manifest_tmp.path());

    // Register, seed the config leaf the serve readers assert on, THEN
    // subscribe (before start — a receiver sees only later commits), THEN
    // start under the guard.
    facade
        .register_with_adapter(
            SERVE_PATHOLOGICAL_NAME,
            &AdapterRef::Manifest(serve_manifest.clone()),
        )
        .unwrap_or_else(|e| panic!("register {SERVE_PATHOLOGICAL_NAME} failed: {e}"));
    facade
        .set_config(SERVE_PATHOLOGICAL_NAME, MODEL_KEY, MODEL_VALUE)
        .unwrap_or_else(|e| panic!("seed config on {SERVE_PATHOLOGICAL_NAME} failed: {e}"));
    let follower = engine.subscribe();
    let mut serve_guard = RunningGuard::new(facade, &[SERVE_PATHOLOGICAL_NAME.to_string()]);
    facade
        .start(SERVE_PATHOLOGICAL_NAME)
        .unwrap_or_else(|e| panic!("start {SERVE_PATHOLOGICAL_NAME} failed: {e}"));
    let status = facade
        .instance_status(SERVE_PATHOLOGICAL_NAME)
        .unwrap_or_else(|e| panic!("status read for {SERVE_PATHOLOGICAL_NAME} failed: {e}"));
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "{SERVE_PATHOLOGICAL_NAME} must be Running when the serve window opens"
    );

    let mut all_names = names.to_vec();
    all_names.push(SERVE_PATHOLOGICAL_NAME.to_string());
    let serve_running_count = sample_running_count(facade, &all_names);
    assert!(
        serve_running_count > 0,
        "no instance is Running — the serve budgets are meaningless over zero running instances"
    );
    note(&format!(
        "serve window: {serve_running_count} instances Running ({RUNNING_COUNT} started \
         heartbeat idlers of {FLEET_SIZE} registered + the pathological); \
         {SERVE_READER_TASKS} paced reader tasks + 1 follower for {SERVE_WINDOW:?}, \
         bounded stop at {SERVE_STOP_DELAY:?} ({SERVE_STOP_WINDOW:?} window)"
    ));

    // ---- The window: CPU/RSS sampled on a separate std::thread (the same
    // sysinfo discipline: two-refresh deltas, first sample discarded) while
    // the driver runtime drives the facade from THIS thread. ----
    let pace = serve_reader_pace(SERVE_WINDOW, config.read_iterations);
    let engine_for_driver = Arc::clone(engine);
    let driver_names = all_names.clone();
    // The SAMPLER window is CLAMPED to the serve load window (review round
    // 3): a raised PERF_BUDGETS_WINDOW_MS would otherwise sample past the
    // 12 s load into idle tail, diluting the per-instance figures toward
    // passing. (HarnessConfig is Copy; only the window field moves.)
    let sampler_config = HarnessConfig {
        window: serve_sampling_window(config.window),
        ..*config
    };
    let sampling_window_ms = sampler_config.window.as_millis() as u64;
    let (load, steady) = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| measure_steady_windows(&sampler_config, 1));
        let driver = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(SERVE_DRIVER_WORKERS)
            .enable_all()
            .build()
            .expect("build the serve driver runtime");
        let load = driver.block_on(drive_serve_load(
            engine_for_driver,
            driver_names,
            follower,
            Arc::new(AtomicBool::new(false)),
            pace,
        ));
        let steady = sampler.join().expect("the serve sampler thread panicked");
        (load, steady)
    });

    // ---- Cleanup helper shape: on ANY invalid-evidence exit path the
    // pathological is stopped (drive_serve_load awaits the stop task before
    // returning Err), so the guard defuses and the scenario's own temp dir
    // drops here — before the Err reaches main. ----
    let load = match load {
        Ok(load) => load,
        Err(reason) => {
            serve_guard.defuse_if(true);
            drop(serve_guard);
            drop(serve_manifest_tmp);
            return Err(reason);
        }
    };

    // ---- Summarize through the SAME pure math the default phases use. ----
    let reads = vec![
        summarize_read("fleet", &mut merge_kind_samples(&load.fleet)),
        summarize_read(
            "instance_status",
            &mut merge_kind_samples(&load.instance_status),
        ),
        summarize_read(
            "effective_config",
            &mut merge_kind_samples(&load.effective_config),
        ),
        summarize_read(
            "usage_via_fleet",
            &mut merge_kind_samples(&load.usage_via_fleet),
        ),
    ];
    let overall_p99_ms = overall_read_p99(&reads);
    // The per-instance normalization divisor is the schedule-aware MINIMUM
    // Running count (review round 3): the pathological leaves the Running
    // set when its stop completes, and dividing by the pre-window count
    // would understate per-instance cost after that point (the lenient
    // direction — wrong for a gate).
    let effective_running_instances = serve_effective_running_instances(serve_running_count);
    let figures = SteadyFigures::new(
        steady.median_cpu_aggregate_pct,
        steady.median_rss_bytes_mean,
        steady.rss_bytes_max,
        effective_running_instances,
    );

    // The scenario's own stop succeeded end-to-end (drive_serve_load would
    // have returned Err otherwise): the final state must read `stopped`, and
    // the guard is defused — Drop covers only the panic paths.
    assert_eq!(
        load.pathological_final_state, "stopped",
        "the pathological instance must reach `stopped` through its bounded stop"
    );

    // ---- The contention proof (review round 3), BEFORE any gate verdict:
    // a window where no read started inside the stop's measured window
    // measured idle supervision, not the lock-contention term the shape
    // exists to measure — its figures are vacuous, so the scenario fails
    // with a NAMED error (main: exit 2) instead of routing the fixed
    // decision rule on them. ----
    if let Err(reason) = serve_contention_exercised(load.reads_started_during_stop) {
        serve_guard.defuse_if(true);
        drop(serve_guard);
        drop(serve_manifest_tmp);
        return Err(reason);
    }

    serve_guard.defuse_if(true);
    drop(serve_guard);
    drop(serve_manifest_tmp);

    note(&format!(
        "serve window complete: reads p99 {overall_p99_ms:.3} ms; bounded stop {:.1} ms \
         (final state {}, last cause {}); {} read(s) started inside the stop's window; \
         follower {} event(s), {} Lagged-recovered; {} reader-retries; delivered \
         iterations per reader {:?} (nominal {})",
        load.stop_elapsed_ms,
        load.pathological_final_state,
        load.pathological_stop_cause,
        load.reads_started_during_stop,
        load.follower_events,
        load.follower_lagged,
        load.reader_retries,
        load.reader_iterations,
        config.read_iterations,
    ));

    Ok(ServeShapeOutcome {
        reads,
        overall_p99_ms,
        steady,
        figures,
        running_count: serve_running_count,
        effective_running_instances,
        load,
        fixture_note,
        pace,
        read_iterations: config.read_iterations,
        sampling_window_ms,
    })
}

/// The `serve_shape` report block (story 17-1) — present ONLY on
/// `--serve-shape` runs (the default invocation's report stays
/// byte-equivalent). `binding` is the gates' platform scope (review round
/// 3): on the ratified gate platform the verdicts gate (`hard: true`);
/// elsewhere they are report-only (`hard: false`) — measured and reported,
/// never gating. Pure.
fn serve_shape_json(
    outcome: &ServeShapeOutcome,
    gates: &GateReport,
    tolerance: f64,
    binding: bool,
) -> serde_json::Value {
    let verdicts: Vec<serde_json::Value> = gates
        .verdicts
        .iter()
        .map(|verdict| {
            let mut json = verdict_json(verdict);
            json["hard"] = serde_json::json!(binding);
            json
        })
        .collect();
    let lagged_note = if outcome.load.follower_lagged > 0 {
        format!(
            "the follower fell behind the bus and RECOVERED by skipping {} committed \
             event(s) — surfaced per AI-18, never a crash, never a silent drop",
            outcome.load.follower_lagged
        )
    } else {
        "the follower kept up (no Lagged recovery needed)".to_string()
    };
    serde_json::json!({
        "story": "17-1",
        "budget_source": "NFR-4 (the same ratified budgets, serve shape)",
        "decision_rule": "any serve budget exceeded -> the evidence recommends ACTIVATING \
             the archived per-instance plan (ai-63b-lock-model-decision-2026-08-27.md); \
             all hold -> the evidence recommends RE-RATIFYING the coarse two-mutex model, \
             citing the measured headroom",
        "gates_bind_on_this_platform": binding,
        "driver": {
            "shape": "the PUBLIC async facade on the driver's OWN small multi-thread tokio \
                 runtime (the docs/embedding.md host shape) — no HTTP transport, no \
                 private reach-in",
            "worker_threads": SERVE_DRIVER_WORKERS,
            "reader_tasks": SERVE_READER_TASKS,
            "read_iterations_per_reader": outcome.read_iterations,
            "iterations_delivered_per_reader": outcome.load.reader_iterations,
            "iterations_note": "the pace is sleep-only think-time: a reader stalled on the \
                 bounded stop delivers fewer than its nominal count — delivered recorded \
                 beside nominal, never implied",
            "reader_retries": outcome.load.reader_retries,
            "reader_pace_ms": outcome.pace.as_millis() as u64,
            "pace_derivation": "each reader carries the default phase's OWN read demand \
                 (READ_ITERATIONS per window — the story-7-5 premise), spread across the \
                 serve window: window / read_iterations; per reader this EQUALS the \
                 default phase's demand, so the SERVE_READER_TASKS concurrent readers \
                 deliver ~4x the default phase's AGGREGATE demand — a serve shape, not a \
                 single-pass replay",
            "window_secs": SERVE_WINDOW.as_secs_f64(),
            "sampling_window_ms": outcome.sampling_window_ms,
            "sampling_note": "the sampler's window is CLAMPED to the serve load window, so \
                 a raised PERF_BUDGETS_WINDOW_MS cannot dilute the figures with idle tail",
        },
        "fixture": {
            "fleet_size": FLEET_SIZE + 1,
            "running_instances": outcome.running_count,
            "effective_running_instances": outcome.effective_running_instances,
            "normalization_note": "per-instance figures divide by the schedule-aware MINIMUM \
                 Running count (the post-stop count) — the conservative bound; the \
                 pre-stop count would understate per-instance cost after the bounded stop",
            "pathological_name": SERVE_PATHOLOGICAL_NAME,
            "pathological_kind": SERVE_PATHOLOGICAL_KIND,
            "pathological_note": outcome.fixture_note,
            "orphan_bound_ms": SERVE_AGENT_LINGER_MS,
        },
        "read_latency_ms": read_kind_json(&outcome.reads),
        "read_p99_overall_ms": outcome.overall_p99_ms,
        "steady_state": steady_json(&outcome.steady, &outcome.figures),
        "contention": {
            "reads_started_during_stop": outcome.load.reads_started_during_stop,
            "note": "reads that STARTED inside the bounded stop's measured window — the \
                 contention proof; zero would mean idle supervision was measured, and the \
                 run fails with a named exit-2 before any gate verdict",
        },
        "bounded_stop": {
            "window_secs": SERVE_STOP_WINDOW.as_secs_f64(),
            "fired_into_window_secs": SERVE_STOP_DELAY.as_secs_f64(),
            "elapsed_ms": outcome.load.stop_elapsed_ms,
            "final_state": outcome.load.pathological_final_state,
            "last_transition_cause": outcome.load.pathological_stop_cause,
        },
        "subscriber": {
            "shape": "one --follow-style subscribed reader on the lock-free \
                 Engine::subscribe() bus (the story-7-2 surface), counting committed \
                 events under load — a handful of lifecycle events in the window (the \
                 pathological's start/stop transitions and any committed usage/breach \
                 events), so this is BUS LIVENESS under load, not load evidence",
            "events_counted": outcome.load.follower_events,
            "lagged_recovered": outcome.load.follower_lagged,
            "lagged_note": lagged_note,
        },
        "ci_tolerance": {
            "name": "shared_runner_tolerance",
            "factor": tolerance,
            "applies_to": "serve_cpu_per_instance_le_2pct, CI mode only",
            "applied": tolerance != 1.0,
        },
        "baseline_rule": "the serve gates bind ONLY on --serve-shape runs; the DEFAULT \
             gated-metric set never carries serve names (a default run's baseline is \
             unchanged). A SERVE run trends the serve gates too — but only against a \
             serve-carrying baseline (seeded per docs/testing.md): the mode-aware \
             coverage check hard-fails (the existing named exit-2) a serve run whose \
             --baseline lacks the serve metrics, never a silent pass",
        "gates": verdicts,
        "status": if gates.passed { "pass" } else { "fail" },
    })
}

// ---------------------------------------------------------------------------
// Baseline trend tracking + the ratchet (story 15-5) — ADDITIVE to the
// absolute gates (a gate-platform run must pass both). Pure math + parsing
// here, pinned by the #[cfg(test)] battery like the gate math above.
// ---------------------------------------------------------------------------

/// The harness's command-line surface: `--baseline <file>` and
/// `--write-baseline <file>` (each also accepted as `--flag=<file>`), plus
/// `--help`/`-h`, parsed in the example's plain std style — no dependencies,
/// no clap. Invalid usage is a LOUD stderr error + exit 2 in `main`, never a
/// silent ignore; diagnostics go to stderr (CLI-first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CliArgs {
    baseline: Option<String>,
    write_baseline: Option<String>,
    /// Story 17-1: run the serve-shape scenario AFTER the default phases and
    /// add the `serve_shape` report block + the three serve gates. A boolean
    /// flag: any `=`-carrying form (`--serve-shape=bogus`) is a named usage
    /// error + exit 2, never a silently-ignored value.
    serve_shape: bool,
    help: bool,
}

/// The plain usage text (`--help`/`-h`, CLI-first): flags, defaults, and the
/// pointer to the baseline update procedure. Plain prose on stdout — no
/// padding gymnastics, nothing hidden behind a pager.
fn usage_text() -> String {
    format!(
        "ktesio perf-budgets harness (story 7-5 / NFR-4) — usage:\n\
         \n\
         \x20 perf-budgets [--baseline <file>] [--write-baseline <file>] [--serve-shape]\n\
         \x20                (each flag also as --flag=<file>; --serve-shape takes no value)\n\
         \n\
         \x20 --baseline <file>      load a prior report JSON and add a `trend` block\n\
         \x20                          (per gated metric: baseline/current/delta/verdict).\n\
         \x20                          In CI mode the ratchet also GATES the run: no gated\n\
         \x20                          metric worse than max(baseline x {RATCHET_TOLERANCE},\n\
         \x20                          baseline + a per-metric noise floor). Other runs\n\
         \x20                          report the trend without gating.\n\
         \x20 --write-baseline <file> write this run's report (trend stripped, provenance\n\
         \x20                          stamped) as the NEXT baseline. REFUSED on a red run\n\
         \x20                          (absolute gate, serve gate, or ratchet failed): loud\n\
         \x20                          note, exit 2, the file is never written.\n\
         \x20 --serve-shape          story 17-1: ALSO run the serve-shape scenario after\n\
         \x20                          the default phases — the engine's PUBLIC async facade\n\
         \x20                          under serve-shaped concurrency (concurrent reads +\n\
         \x20                          usage polling + one subscribed follow-style reader +\n\
         \x20                          one pathological bounded stop) — and gate the NFR-4\n\
         \x20                          budgets through the serve gates (their own names;\n\
         \x20                          bound only on serve runs; not ratcheted until a\n\
         \x20                          serve-carrying baseline is seeded per docs/testing.md).\n\
         \x20 --help | -h             this text.\n\
         \n\
         \x20 No flags: the absolute budgets gate exactly as before and the report is\n\
         \x20 byte-equivalent to the pre-15-5 shape. Defaults/env tunables:\n\
         \x20 PERF_BUDGETS_CI=1|0 (force CI/local mode), PERF_BUDGETS_READ_ITERATIONS\n\
         \x20 (default 200), PERF_BUDGETS_SETTLE_MS (2000), PERF_BUDGETS_WINDOW_MS\n\
         \x20 (12000), PERF_BUDGETS_REPORT_PATH (also write the JSON report to a file).\n\
         \x20 Baseline updates ride a PR — the procedure lives in docs/testing.md\n\
         \x20 (\"Baseline trend tracking — the ratchet\")."
    )
}

/// Pure argv parse (pinned by tests): unknown flags, missing values, empty
/// values, flag-like values, bare `=`-carrying tokens, and duplicates are
/// named errors.
///
/// Parser guards (review round 1): a flag's VALUE must not start with `--`
/// (`--baseline --write-baseline` is a missing-value error, not a silent
/// flag-eating), and a bare token containing `=` that does not start with
/// `--` is an unknown-argument error outright — never a split-on-`=`
/// attempt.
fn parse_cli_args(argv: &[String]) -> Result<CliArgs, String> {
    let mut parsed = CliArgs::default();
    let mut index = 0;
    while index < argv.len() {
        let raw = argv[index].as_str();
        if raw == "--help" || raw == "-h" {
            parsed.help = true;
            index += 1;
            continue;
        }
        let (flag, inline_value) = if raw.starts_with("--") {
            match raw.split_once('=') {
                Some((flag, value)) => (flag, Some(value.to_string())),
                None => (raw, None),
            }
        } else {
            // A bare token (no leading --) is an unknown argument —
            // including one carrying '=': never a split-on-= attempt.
            return Err(format!(
                "unknown argument {raw:?} \
                 (supported: --baseline <file>, --write-baseline <file>, --serve-shape, --help)"
            ));
        };
        if flag == "--serve-shape" {
            // Story 17-1: a BOOLEAN flag — the `=`-carrying form is invalid
            // usage outright (the spec's `--serve-shape=bogus` row: a named
            // stderr error + exit 2, before anything spawns), never a
            // silently-ignored value.
            if let Some(value) = inline_value {
                return Err(format!(
                    "--serve-shape takes no value (got --serve-shape={value:?})"
                ));
            }
            if parsed.serve_shape {
                return Err("--serve-shape given more than once".to_string());
            }
            parsed.serve_shape = true;
            index += 1;
            continue;
        }
        if flag != "--baseline" && flag != "--write-baseline" {
            return Err(format!(
                "unknown argument {raw:?} \
                 (supported: --baseline <file>, --write-baseline <file>, --serve-shape, --help)"
            ));
        }
        let value = match inline_value {
            Some(value) => value,
            None => {
                index += 1;
                argv.get(index)
                    .cloned()
                    .ok_or_else(|| format!("{flag} requires a file path (usage: {flag} <file>)"))?
            }
        };
        if value.is_empty() {
            return Err(format!("{flag} requires a non-empty file path"));
        }
        if value.starts_with("--") {
            return Err(format!(
                "{flag} requires a file path — {value:?} looks like another flag, \
                 not a file (usage: {flag} <file>)"
            ));
        }
        let slot = if flag == "--baseline" {
            &mut parsed.baseline
        } else {
            &mut parsed.write_baseline
        };
        if slot.is_some() {
            return Err(format!("{flag} given more than once"));
        }
        *slot = Some(value);
        index += 1;
    }
    Ok(parsed)
}

/// A prior report loaded as the ratchet's basis: the gate-name → measured
/// map (every figure the ratchet compares) plus the provenance the trend
/// block cites back (git SHA, OS, and — when the file carries it — the
/// writer-stamped `provenance` block).
#[derive(Clone, Debug, PartialEq)]
struct BaselineFigures {
    schema_version: u64,
    /// The writer-stamped provenance SHA if present, else the report's
    /// `environment.git_sha` (older baselines) — best-effort either way.
    git_sha: Option<String>,
    os: Option<String>,
    /// The `provenance.seed` flag (true = a local stand-in, false = the
    /// canonical CI leg's own artifact) — `None` when the file predates the
    /// provenance block.
    provenance_seed: Option<bool>,
    /// The `provenance.generated_on` label ("ci-leg" | "local-ci-mode" |
    /// "local") — `None` when the file predates the block.
    provenance_generated_on: Option<String>,
    measured_by_gate: Vec<(String, f64)>,
}

impl BaselineFigures {
    fn lookup(&self, gate: &str) -> Option<f64> {
        self.measured_by_gate
            .iter()
            .find(|(name, _)| name == gate)
            .map(|(_, measured)| *measured)
    }

    fn gate_names(&self) -> Vec<&str> {
        self.measured_by_gate
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

/// Read + parse a baseline report file. Every error NAMES the file: a
/// missing or unreadable baseline must fail loudly (exit 2 in `main`) —
/// never a silent no-trend run.
fn load_baseline(path: &std::path::Path) -> Result<BaselineFigures, String> {
    let file = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the baseline file {file}: {e}"))?;
    parse_baseline(&text, &file)
}

/// The gated metrics this harness evaluates, in the canonical order
/// [`evaluate_gates`] emits — the single list the pre-measurement baseline
/// coverage validation compares against (pinned to `evaluate_gates`' own
/// output by a test, so a new gate cannot silently escape validation).
const GATED_METRICS: [&str; 7] = [
    "read_p99_lt_1s",
    "rss_per_instance_le_50mib",
    "rss_max_spike_le_2x_budget",
    "cpu_per_instance_le_2pct",
    "subscriber_cpu_delta_le_budget",
    "subscriber_rss_delta_le_budget",
    "subscriber_read_p99_delta_le_budget",
];

/// The serve gates' names (story 17-1), OUT of the default gated set: a
/// default run's gated-metric set is exactly [`GATED_METRICS`] — the serve
/// names bind only on serve runs (review round 3, pinned: they are NOT in
/// the default set). The MODE-AWARE set (review round 3): a SERVE run's
/// coverage check and trend evaluation expect the default set PLUS these —
/// but only against a serve-carrying baseline (a serve run whose baseline
/// lacks them is the existing LOUD named exit-2 schema error, never a
/// silent pass, and never a fabricated zero).
const SERVE_GATED_METRICS: [&str; 3] = [
    "serve_read_p99_lt_1s",
    "serve_cpu_per_instance_le_2pct",
    "serve_rss_per_instance_le_50mib",
];

/// Pre-measurement baseline coverage validation (review round 1): the
/// baseline must carry every gated metric (`missing` — a hard schema
/// error: today's exit-2 arm, now fired BEFORE anything is spawned), and
/// any gates it carries beyond this harness's set land in `extra` —
/// SURFACED as a warning, never a hard failure (a future-harness baseline
/// must not red today's runs). Pure; `main` maps missing → exit 2 and
/// extra → a stderr note.
fn baseline_coverage_gaps(
    baseline: &BaselineFigures,
    expected: &[&str],
) -> (Vec<String>, Vec<String>) {
    let have = baseline.gate_names();
    let missing = expected
        .iter()
        .filter(|name| !have.contains(name))
        .map(|name| (*name).to_string())
        .collect();
    let extra = have
        .into_iter()
        .filter(|name| !expected.contains(name))
        .map(str::to_string)
        .collect();
    (missing, extra)
}

/// The OS-mismatch guard (review round 1): a CI-mode run whose loaded
/// baseline was measured on a DIFFERENT OS is comparing apples to oranges
/// — surfaced as a stderr WARNING (mechanism, not convention). `None`
/// (a baseline with no environment block) never warns: absence is not a
/// mismatch.
fn baseline_os_mismatch(baseline_os: Option<&str>, current_os: &str) -> bool {
    baseline_os.is_some_and(|os| os != current_os)
}

/// Pure baseline parse + validation: valid JSON object whose
/// `schema_version` matches this harness (a future breaking shape bumps
/// [`REPORT_SCHEMA_VERSION`] and stale baselines must be refused BY NAME,
/// never mis-read), and a non-empty `gates` array whose entries each carry
/// a string `name` and a FINITE numeric `measured` (duplicates refused) —
/// a non-finite value (NaN/±inf) is a corrupt-file error, never a value
/// the ratchet quietly compares against. Unknown keys — including a stale
/// nested `trend` block from a baseline committed before the strip rule —
/// are ignored: only the gated figures are load-bearing. The
/// `environment` block and the `provenance` block are best-effort
/// provenance (the stamped block wins where present).
fn parse_baseline(text: &str, file: &str) -> Result<BaselineFigures, String> {
    let corrupt = |reason: String| format!("baseline {file} is corrupt: {reason}");
    let root: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("baseline {file} is not valid JSON: {e}"))?;
    let object = root
        .as_object()
        .ok_or_else(|| corrupt("the root is not a JSON object".to_string()))?;
    let schema_version = object
        .get("schema_version")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| {
            corrupt(format!(
                "schema_version is missing or not a non-negative integer \
                 (this harness reads {REPORT_SCHEMA_VERSION})"
            ))
        })?;
    if schema_version != REPORT_SCHEMA_VERSION {
        return Err(format!(
            "baseline {file} schema mismatch: carries schema_version \
             {schema_version}, this harness reads {REPORT_SCHEMA_VERSION} — regenerate \
             the baseline with --write-baseline per docs/testing.md"
        ));
    }
    let environment = object
        .get("environment")
        .and_then(|value| value.as_object());
    let string_field = |key: &str| {
        environment
            .and_then(|env| env.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let provenance = object.get("provenance").and_then(|value| value.as_object());
    let provenance_string = |key: &str| {
        provenance
            .and_then(|block| block.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let gates = object
        .get("gates")
        .and_then(|value| value.as_array())
        .ok_or_else(|| corrupt("the gates array is missing or not an array".to_string()))?;
    if gates.is_empty() {
        return Err(corrupt("the gates array is empty".to_string()));
    }
    let mut measured_by_gate = Vec::with_capacity(gates.len());
    for gate in gates {
        let gate_object = gate
            .as_object()
            .ok_or_else(|| corrupt("a gates entry is not an object".to_string()))?;
        let name = gate_object
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| corrupt("a gates entry lacks a string name".to_string()))?
            .to_string();
        let measured = gate_object
            .get("measured")
            .and_then(|value| value.as_f64())
            .ok_or_else(|| corrupt(format!("gate {name:?} lacks a numeric measured value")))?;
        if !measured.is_finite() {
            return Err(corrupt(format!(
                "gate {name:?} carries a non-finite measured value ({measured}) — a \
                 baseline figure the ratchet cannot compare is a corrupt file, never \
                 a quiet pass"
            )));
        }
        if measured_by_gate.iter().any(|(known, _)| *known == name) {
            return Err(corrupt(format!("gate {name:?} appears twice")));
        }
        measured_by_gate.push((name, measured));
    }
    Ok(BaselineFigures {
        schema_version,
        git_sha: provenance_string("git_sha").or_else(|| string_field("git_sha")),
        os: string_field("os"),
        provenance_seed: provenance
            .and_then(|block| block.get("seed"))
            .and_then(|value| value.as_bool()),
        provenance_generated_on: provenance_string("generated_on"),
        measured_by_gate,
    })
}

/// One gated metric's ratchet verdict. [`RatchetVerdict::NoRatchet`] is the
/// surfaced guard for a comparison the ×1.15 arithmetic cannot honestly
/// make: ×1.15 on a NEGATIVE figure tightens the bound below the baseline
/// (a tolerance must absorb noise, never add strictness), ×1.15 on zero
/// bounds everything above zero, and a NON-FINITE current cannot be
/// compared at all — in every case the trend reports the delta and the
/// reason while the metric's ABSOLUTE budget keeps gating it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RatchetVerdict {
    Improved,
    Within,
    Beyond,
    NoRatchet,
}

impl RatchetVerdict {
    fn as_str(self) -> &'static str {
        match self {
            RatchetVerdict::Improved => "improved",
            RatchetVerdict::Within => "within",
            RatchetVerdict::Beyond => "beyond",
            RatchetVerdict::NoRatchet => "no-ratchet",
        }
    }
}

/// WHY a verdict is [`RatchetVerdict::NoRatchet`] — surfaced in the report
/// and on stderr, never left to guesswork between the two arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoRatchetReason {
    NonPositiveBaseline,
    NonFiniteCurrent,
}

impl NoRatchetReason {
    fn explanation(self) -> &'static str {
        match self {
            NoRatchetReason::NonPositiveBaseline => {
                "baseline <= 0: the x1.15 ratchet is meaningless on a non-positive \
                 figure (it would tighten, not tolerate); the absolute budget \
                 still gates this metric"
            }
            NoRatchetReason::NonFiniteCurrent => {
                "current value is not finite (NaN/inf): the ratchet cannot compare \
                 it — surfaced instead of silently passing (a NaN fails every \
                 absolute bound too, so the metric still cannot slip through red)"
            }
        }
    }
}

/// The per-metric-family NOISE FLOOR ε (review round 1): the ratchet limit
/// is max(baseline × [`RATCHET_TOLERANCE`], baseline + ε), so a near-zero
/// baseline still gets a REAL absolute tolerance window instead of a
/// microscopic multiplicative one (a +0.009 pct-point subscriber-CPU
/// baseline under pure ×1.15 tolerates only +0.0014 — raw quantization
/// jitter reds the run). Each ε is sized to be noise-real but
/// regression-loud, and sits far under the metric's ABSOLUTE budget so the
/// floor can never out-tolerate the budget it complements:
///
/// | family (gate) | ε | justification |
/// |---|---|---|
/// | read p99 (`read_p99_lt_1s`) | 0.5 ms | sub-3 ms p99s jitter by scheduler quanta, not percents; 0.5 ms is ~20% of the committed 2.52 ms seed and 0.05% of the 1 s budget |
/// | CPU (`cpu_per_instance_le_2pct`) | 0.05 pct-pts | sysinfo's two-refresh CPU% resolution over an 11-sample window; 1/40 of the strict 2% budget |
/// | RSS mean + spike (both `rss_*` gates) | 0.25 MiB | page-granular retention moves in fractions of a MiB; 1/200 of the 50 MiB budget |
/// | subscriber CPU Δ | 0.05 pct-pts | a difference of two single-window means, each with the 0.05-pts resolution above; 1/5 of the 0.25-pt budget |
/// | subscriber RSS Δ | 0.25 MiB | same page-granularity argument; 1/8 of the 2 MiB budget |
/// | subscriber read-p99 Δ | 25 ms | a difference of two p99s measured seconds apart on one window each — single-window tail noise is millisecond-scale on sub-5 ms reads; 1/4 of the 100 ms budget |
///
/// An unknown gate name (a future harness's metric an older ratchet is
/// handed) floors at 0.0 — the strict pure-×1.15 rule, never a guess.
fn ratchet_noise_floor(gate: &str) -> f64 {
    match gate {
        "read_p99_lt_1s" | "serve_read_p99_lt_1s" => 0.5,
        "cpu_per_instance_le_2pct"
        | "subscriber_cpu_delta_le_budget"
        | "serve_cpu_per_instance_le_2pct" => 0.05,
        "rss_per_instance_le_50mib"
        | "rss_max_spike_le_2x_budget"
        | "subscriber_rss_delta_le_budget"
        | "serve_rss_per_instance_le_50mib" => 0.25,
        "subscriber_read_p99_delta_le_budget" => 25.0,
        _ => 0.0,
    }
}

/// One trend row — everything the spec demands a ratchet verdict name:
/// baseline value, current value, delta, and the tolerance (via `limit`).
#[derive(Clone, Debug, PartialEq)]
struct TrendEntry {
    name: String,
    baseline: f64,
    current: f64,
    delta: f64,
    /// (delta / baseline) × 100 — `None` exactly when the verdict is
    /// `no-ratchet` (a non-positive baseline has no meaningful ratio, and a
    /// non-finite current has no meaningful delta).
    delta_pct: Option<f64>,
    /// max(baseline × [`RATCHET_TOLERANCE`], baseline + ε_family) — the
    /// worst current this metric tolerates (`None` for `no-ratchet`).
    limit: Option<f64>,
    verdict: RatchetVerdict,
    /// WHY, when the verdict is `no-ratchet`.
    no_ratchet_reason: Option<NoRatchetReason>,
}

/// Pure ratchet math for one metric: improvements pass freely; a current
/// beyond the limit max(baseline × 1.15, baseline + ε) fails;
/// exactly-at-limit is WITHIN (the rule is "no WORSE than ×1.15"); a
/// non-positive baseline never ratchets; a non-finite current is a
/// surfaced no-ratchet, never a silently-within NaN comparison.
fn ratchet_entry(name: &str, baseline: f64, current: f64) -> TrendEntry {
    let delta = current - baseline;
    if !current.is_finite() {
        return TrendEntry {
            name: name.to_string(),
            baseline,
            current,
            delta,
            delta_pct: None,
            limit: None,
            verdict: RatchetVerdict::NoRatchet,
            no_ratchet_reason: Some(NoRatchetReason::NonFiniteCurrent),
        };
    }
    if baseline <= 0.0 {
        return TrendEntry {
            name: name.to_string(),
            baseline,
            current,
            delta,
            delta_pct: None,
            limit: None,
            verdict: RatchetVerdict::NoRatchet,
            no_ratchet_reason: Some(NoRatchetReason::NonPositiveBaseline),
        };
    }
    let limit = (baseline * RATCHET_TOLERANCE).max(baseline + ratchet_noise_floor(name));
    let verdict = if current < baseline {
        RatchetVerdict::Improved
    } else if current > limit {
        RatchetVerdict::Beyond
    } else {
        RatchetVerdict::Within
    };
    TrendEntry {
        name: name.to_string(),
        baseline,
        current,
        delta,
        delta_pct: Some(delta / baseline * 100.0),
        limit: Some(limit),
        verdict,
        no_ratchet_reason: None,
    }
}

/// The whole trend evaluation for one run. `gated` is true only on the
/// ratchet's gate platform — CI mode, whose canonical baseline this is
/// (local and other platforms pass `--baseline` for a reporting-only
/// block, the same platform discipline as the absolute gates). `passed`
/// is the pure math verdict (no metric Beyond); whether it FAILS the run
/// is `gated && !passed`.
#[derive(Clone, Debug, PartialEq)]
struct TrendBlock {
    baseline_path: String,
    baseline_git_sha: Option<String>,
    baseline_os: Option<String>,
    gated: bool,
    passed: bool,
    entries: Vec<TrendEntry>,
}

impl TrendBlock {
    /// The wire verdict: a non-gated run never claims pass/fail authority.
    fn verdict_str(&self) -> &'static str {
        if !self.gated {
            "report-only"
        } else if self.passed {
            "pass"
        } else {
            "fail"
        }
    }

    /// Whether this trend fails the RUN (the additive gate).
    fn fails_run(&self) -> bool {
        self.gated && !self.passed
    }
}

/// Evaluate the ratchet over EVERY gated metric: the current run's gate
/// verdicts carry the current measured values, the baseline supplies the
/// priors, matched by gate name. A current gate the baseline lacks is a
/// schema mismatch — a LOUD named error, never a silently skipped row.
fn evaluate_trend(
    gates: &GateReport,
    baseline: &BaselineFigures,
    baseline_path: &str,
    gated: bool,
) -> Result<TrendBlock, String> {
    let mut entries = Vec::with_capacity(gates.verdicts.len());
    for verdict in &gates.verdicts {
        let prior = baseline.lookup(verdict.name).ok_or_else(|| {
            format!(
                "baseline {baseline_path} schema mismatch: it lacks the gated \
                 metric {:?} — regenerate the baseline with --write-baseline",
                verdict.name
            )
        })?;
        entries.push(ratchet_entry(verdict.name, prior, verdict.measured));
    }
    let passed = !entries
        .iter()
        .any(|entry| entry.verdict == RatchetVerdict::Beyond);
    Ok(TrendBlock {
        baseline_path: baseline_path.to_string(),
        baseline_git_sha: baseline.git_sha.clone(),
        baseline_os: baseline.os.clone(),
        gated,
        passed,
        entries,
    })
}

/// The `trend` report block. ADDITIVE — the report schema stays 1 per the
/// bump policy on [`REPORT_SCHEMA_VERSION`].
fn trend_json(trend: &TrendBlock) -> serde_json::Value {
    let metrics: Vec<serde_json::Value> = trend
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "name": entry.name,
                "baseline": entry.baseline,
                "current": entry.current,
                "delta": entry.delta,
                "delta_pct": entry.delta_pct,
                "ratchet_limit": entry.limit,
                "verdict": entry.verdict.as_str(),
                "note": entry.no_ratchet_reason.map(|reason| reason.explanation()),
            })
        })
        .collect();
    serde_json::json!({
        "policy": "ADDITIVE to the absolute gates (a CI-mode run must pass both): no \
                   gated metric worse than max(baseline x RATCHET_TOLERANCE, baseline + a \
                   per-metric noise floor); improvements accepted freely; a non-positive \
                   baseline never ratchets and a non-finite current is surfaced (both \
                   no-ratchet, absolute budget still applies); the ratchet gates CI mode \
                   only — the canonical baseline's own leg — every other run reports \
                   without gating",
        "baseline_path": trend.baseline_path,
        "baseline_git_sha": trend.baseline_git_sha,
        "baseline_os": trend.baseline_os,
        "tolerance": RATCHET_TOLERANCE,
        "gated": trend.gated,
        "verdict": trend.verdict_str(),
        "metrics": metrics,
    })
}

/// ONE rendering for a missing ratchet limit, shared by the stdout trend
/// table and the stderr notes (review round 1: the two surfaces must never
/// disagree — `unwrap_or_default`'s fake `0.0000` lied about a bound that
/// does not exist).
const NO_LIMIT_TOKEN: &str = "n/a";

/// Render a ratchet limit for the human surfaces: the real figure, or the
/// one shared absence token.
fn render_limit(limit: Option<f64>) -> String {
    match limit {
        Some(value) => format!("{value:.4}"),
        None => NO_LIMIT_TOKEN.to_string(),
    }
}

/// Every ratchet verdict as note LINES (pure — `emit_trend_notes` prints
/// them), naming the four figures the spec demands (baseline, current,
/// delta, tolerance/limit), plus the block summary — a report-only Beyond
/// is called out as would-fail-there, never swallowed (surfaced, not
/// silent). Pinned by tests.
fn trend_note_lines(trend: &TrendBlock) -> Vec<String> {
    let mut lines = Vec::with_capacity(trend.entries.len() + 1);
    for entry in &trend.entries {
        match entry.verdict {
            RatchetVerdict::NoRatchet => lines.push(format!(
                "ratchet {:<34} baseline {:>10.4} -> current {:>10.4}  delta {:+.4}  \
                 limit {NO_LIMIT_TOKEN}  [{}]",
                entry.name,
                entry.baseline,
                entry.current,
                entry.delta,
                entry
                    .no_ratchet_reason
                    .map_or("no-ratchet", |reason| reason.explanation()),
            )),
            RatchetVerdict::Improved | RatchetVerdict::Within | RatchetVerdict::Beyond => {
                lines.push(format!(
                    "ratchet {:<34} baseline {:>10.4} -> current {:>10.4}  delta {:+.4} \
                     ({:+.1}% vs baseline)  tolerance x{RATCHET_TOLERANCE} (limit {}) — {}",
                    entry.name,
                    entry.baseline,
                    entry.current,
                    entry.delta,
                    // Non-no-ratchet verdicts always carry the ratio.
                    entry.delta_pct.unwrap_or_default(),
                    render_limit(entry.limit),
                    entry.verdict.as_str(),
                ))
            }
        }
    }
    let beyond = trend
        .entries
        .iter()
        .filter(|entry| entry.verdict == RatchetVerdict::Beyond)
        .count();
    if trend.gated {
        if trend.passed {
            lines.push(format!(
                "ratchet: PASS vs baseline {} (tolerance x{RATCHET_TOLERANCE} + noise floor)",
                trend.baseline_path
            ));
        } else {
            lines.push(format!(
                "ratchet: FAIL — {beyond} metric(s) beyond the limit vs {} \
                 (baseline/current/delta/limit per metric above; the report's trend \
                 block carries the same figures) — exiting 1",
                trend.baseline_path
            ));
        }
    } else {
        lines.push(format!(
            "ratchet: REPORT-ONLY on this platform (the ratchet gates CI mode — the \
             canonical baseline's own leg) vs {} — {beyond} metric(s) beyond tolerance \
             would fail there",
            trend.baseline_path
        ));
    }
    lines
}

/// Print every ratchet-verdict note line to stderr.
fn emit_trend_notes(trend: &TrendBlock) {
    for line in trend_note_lines(trend) {
        note(&line);
    }
}

/// Pure main-glue (review round 1, pinned by tests): insert the `trend`
/// block into the report, flip the run's overall `status` to `fail` when
/// the ratchet fails the run (even with every absolute gate green), and
/// return whether the ratchet fails the run.
fn apply_trend(report: &mut serde_json::Value, trend: &TrendBlock) -> bool {
    if let Some(object) = report.as_object_mut() {
        object.insert("trend".to_string(), trend_json(trend));
        if trend.fails_run() {
            // The additive gate failed: the run's overall status must say
            // fail even with every absolute gate green.
            object.insert(
                "status".to_string(),
                serde_json::Value::String("fail".to_string()),
            );
        }
    }
    trend.fails_run()
}

/// Where a run executed, for the written baseline's `provenance` block:
/// the canonical CI leg (CI mode ON the GitHub Actions runner), a local
/// run with CI mode forced (the committed seed's shape), or a plain local
/// run. The first two are the patch list's canonical pair; the third is
/// the honest label for a non-CI local write — stamping it
/// "local-ci-mode" would be a lie.
fn generated_on_label(on_ci_runner: bool, ci_mode: bool) -> &'static str {
    match (on_ci_runner, ci_mode) {
        (true, true) => "ci-leg",
        (false, true) => "local-ci-mode",
        (_, false) => "local",
    }
}

/// The `provenance` block the baseline writer stamps (review round 1):
/// whether this file is a SEED (a local stand-in, not the canonical leg's
/// own artifact), where it was generated, and the FULL git SHA of the tree
/// it measured (the report's `environment.git_sha` stays short — the
/// baseline deserves the unambiguous form).
fn baseline_provenance(ci_mode: bool, on_ci_runner: bool, full_git_sha: &str) -> serde_json::Value {
    let generated_on = generated_on_label(on_ci_runner, ci_mode);
    serde_json::json!({
        // seed: false ONLY when the file is the canonical CI leg's own run.
        "seed": generated_on != "ci-leg",
        "generated_on": generated_on,
        "git_sha": full_git_sha,
        "note": "seed: true marks a local stand-in (this harness never sees 'is this \
                 the committed file' — it sees where it ran); the canonical refresh \
                 procedure (docs/testing.md) writes seed: false, generated_on \
                 \"ci-leg\"",
    })
}

/// The report written as the NEXT baseline: the fresh report with any
/// `trend` block STRIPPED (a baseline is a self-contained measurement —
/// nesting the comparison it was produced under would grow stale data every
/// refresh; `parse_baseline` tolerates a stale one, but never writes one)
/// and the `provenance` block STAMPED (in-file provenance, review round 1).
/// On a SERVE run the serve gates are MERGED into the top-level gates array
/// (review round 3), so the written baseline CARRIES the serve figures the
/// loader reads (`load_baseline` parses exactly that array) — a serve
/// `--write-baseline` seeds a real serve-carrying baseline, exactly as
/// docs/testing.md's procedure describes. Pure; pinned by tests.
fn baseline_report_json(
    report: &serde_json::Value,
    provenance: serde_json::Value,
) -> serde_json::Value {
    let mut clean = report.clone();
    if let Some(object) = clean.as_object_mut() {
        object.remove("trend");
        object.insert("provenance".to_string(), provenance);
        // Merge the serve gates into the baseline's gates array (serve runs
        // only — the serve_shape block is absent otherwise, so a default
        // run's baseline is byte-shaped as before).
        if let Some(serve_gates) = object
            .get("serve_shape")
            .and_then(|serve| serve.get("gates"))
            .and_then(|gates| gates.as_array())
            .cloned()
        {
            if let Some(gates) = object.get_mut("gates").and_then(|g| g.as_array_mut()) {
                gates.extend(serve_gates);
            }
        }
    }
    clean
}

/// The run's exit code, folded from the three independent red sources
/// (review round 3 — extracted from `main` and pinned so each token has a
/// test that fails if it is deleted): an absolute-budget failure, a ratchet
/// failure, or a serve-gate failure reds the run (exit 1); only a fully
/// green run exits 0. Pure; pinned by tests.
fn run_exit_code(gates_passed: bool, ratchet_failed: bool, serve_failed: bool) -> i32 {
    if gates_passed && !ratchet_failed && !serve_failed {
        0
    } else {
        1
    }
}

/// The red-run refusal (review round 1, pinned by tests): `--write-baseline`
/// must NEVER turn a failed run's regressed figures into the canonical
/// baseline. Refused when any absolute gate failed, the SERVE gates failed,
/// OR the ratchet failed — each token is its own named reason (review round
/// 3 added the serve arm so the token cannot be deleted and pass), and the
/// loud reason says which and points at the procedure.
fn baseline_write_refusal(
    gates_passed: bool,
    serve_failed: bool,
    ratchet_failed: bool,
) -> Option<&'static str> {
    if !gates_passed {
        Some(
            "--write-baseline REFUSED: the absolute budgets FAILED this run — a red \
             run's regressed figures must never become the canonical baseline; fix \
             the regression first (the report above is the measured record)",
        )
    } else if serve_failed {
        Some(
            "--write-baseline REFUSED: the SERVE budgets FAILED this run — a red \
             run's regressed figures must never become the canonical baseline (a \
             serve-carrying baseline included); fix the regression first (the \
             report above is the measured record)",
        )
    } else if ratchet_failed {
        Some(
            "--write-baseline REFUSED: the ratchet FAILED this run — re-baselining \
             past a regression is exactly what the ratchet exists to catch; if the \
             drift is sanctioned, follow the update procedure in docs/testing.md \
             from a run that passes",
        )
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

fn main() {
    // Story 15-5: the flag surface (--baseline / --write-baseline). Parsed
    // FIRST so a usage error cannot waste a minute of measurement; invalid
    // usage exits 2 (the env-override convention). --help/-h is usage
    // output: plain text on stdout, exit 0 (CLI-first).
    let cli = match parse_cli_args(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(cli) => cli,
        Err(reason) => {
            note(&format!("{reason} — exiting 2"));
            std::process::exit(2);
        }
    };
    if cli.help {
        println!("{}", usage_text());
        return;
    }
    let config = HarnessConfig::from_env();
    // The baseline loads (and validates) BEFORE anything is spawned, for the
    // same fail-fast reason — a missing/corrupt/schema-mismatched baseline
    // is a loud named error + exit 2, never a silent no-trend run. Review
    // round 1 hardening: the baseline's gate COVERAGE is validated here too
    // (missing gated metrics = the same exit-2 schema error, now fired
    // before a minute of measurement is wasted; extra, future-harness
    // gates = a surfaced warning, not a hard failure), and a CI-mode run
    // WARNS when the baseline's OS differs from the current one (platform
    // apples-to-apples as mechanism, not convention). The ratchet GATES
    // only in CI mode (the canonical baseline's own leg); everywhere else
    // the trend is report-only.
    let baseline = match &cli.baseline {
        Some(path) => match load_baseline(std::path::Path::new(path)) {
            Ok(figures) => {
                // The MODE-AWARE gated set (review round 3): a serve run's
                // baseline must also carry the serve metrics — their absence
                // is the same LOUD named exit-2 schema error (fired before a
                // minute of measurement is wasted), never a silent pass and
                // never a fabricated zero. A default run's set is exactly
                // GATED_METRICS (the serve names are not in it — pinned).
                let expected_gated: Vec<&str> = if cli.serve_shape {
                    GATED_METRICS
                        .iter()
                        .chain(SERVE_GATED_METRICS.iter())
                        .copied()
                        .collect()
                } else {
                    GATED_METRICS.to_vec()
                };
                let (missing, extra) = baseline_coverage_gaps(&figures, &expected_gated);
                if !missing.is_empty() {
                    note(&format!(
                        "baseline {path} schema mismatch: it lacks the gated metric(s) \
                         {missing:?} — regenerate the baseline with --write-baseline per \
                         docs/testing.md — exiting 2"
                    ));
                    std::process::exit(2);
                }
                if !extra.is_empty() {
                    note(&format!(
                        "WARNING: baseline {path} carries gate(s) this harness does not \
                         evaluate: {extra:?} — surfaced, not gated (a future-harness \
                         baseline must not hard-fail today's runs)"
                    ));
                }
                if config.ci && baseline_os_mismatch(figures.os.as_deref(), std::env::consts::OS) {
                    note(&format!(
                        "WARNING: this CI-mode run's OS ({}) differs from the loaded \
                         baseline's OS ({}) — the comparison is not apples-to-apples; \
                         refresh the baseline per docs/testing.md if verdicts look wrong",
                        std::env::consts::OS,
                        figures.os.as_deref().unwrap_or("unknown")
                    ));
                }
                note(&format!(
                    "baseline loaded: {path} (git_sha {}, os {}, provenance: {}) — trend \
                     will be reported{}",
                    figures.git_sha.as_deref().unwrap_or("unknown"),
                    figures.os.as_deref().unwrap_or("unknown"),
                    match (
                        figures.provenance_seed,
                        figures.provenance_generated_on.as_deref(),
                    ) {
                        (Some(seed), Some(generated_on)) => {
                            format!("seed={seed}, generated_on={generated_on}")
                        }
                        _ => "absent (a pre-provenance baseline)".to_string(),
                    },
                    if config.ci {
                        " and the ratchet GATES this run (CI mode)"
                    } else {
                        " (report-only: the ratchet gates CI mode, the canonical \
                         baseline's own leg)"
                    }
                ));
                Some((path.clone(), figures))
            }
            Err(reason) => {
                note(&format!("{reason} — exiting 2"));
                std::process::exit(2);
            }
        },
        None => None,
    };
    // The liveness gate BEFORE anything is spawned: the orphan bound must
    // cover the gated windows PLUS the subscriber-overhead addendum window
    // (the same heartbeat idlers are measured in both). Serve runs skip this
    // default check (review round 3): their fixture is BUILT with the raised
    // serve bound, so the default 60 s bound does not apply — the serve
    // schedule is checked against the raised bound below, alone.
    if !cli.serve_shape {
        if let Err(reason) =
            check_liveness_schedule(config.settle, config.windows + 1, config.window)
        {
            note(&format!("{reason} — exiting 2"));
            std::process::exit(2);
        }
    }
    if cli.serve_shape {
        // The serve schedule check BEFORE anything spawns (fail-fast): the
        // serve window + the pathological stop's bound extend the schedule
        // past the default orphan bound, so serve runs check against the
        // RAISED bound the heartbeat fixture will actually be built with.
        if let Err(reason) = check_liveness_schedule_serve(
            config.settle,
            config.windows + 1,
            config.window,
            SERVE_WINDOW,
            SERVE_STOP_WINDOW,
        ) {
            note(&format!("{reason} — exiting 2"));
            std::process::exit(2);
        }
        note(&format!(
            "serve-shape run (story 17-1): the heartbeat orphan bound is RAISED to \
             SERVE_AGENT_LINGER_MS={SERVE_AGENT_LINGER_MS} ms — the serve window plus the \
             bounded stop extend the measurement schedule past the default \
             {AGENT_LINGER_MS} ms bound"
        ));
    }
    note(&format!(
        "ktesio perf-budgets harness (story 7-5 / NFR-4): fleet={FLEET_SIZE} \
         running={RUNNING_COUNT} read_iterations={} window={:?} settle={:?} \
         windows={} ci={} (plus a one-window subscriber-overhead addendum, \
         gated at the story-10-3 ratified delta budgets)",
        config.read_iterations, config.window, config.settle, config.windows, config.ci,
    ));

    // ---- Hermetic fixture root: a temp state dir + temp manifest dir, both
    // removed at teardown (TempDir drops delete the whole tree). ----
    let state = tempfile::TempDir::new().expect("create the hermetic state root");
    let manifest_tmp = tempfile::TempDir::new().expect("create the manifest dir");
    // The fixture exec resolves via the SHARED locator's `examples/` hop
    // (story 10-1): an example's `current_exe` lands in
    // `target/<profile>/examples/`, so the test-deps (`deps/`) default would
    // look one directory too deep — the parameterized
    // `fake_agent_bin_in(BinDir::Examples)` is the same resolution + the same
    // on-demand-build fallback, with the profile-matching (--release) and
    // --target-dir pinning the test-deps default lacked.
    let fake_agent = fake_agent_bin_in(BinDir::Examples);
    // The heartbeat fixture: the SHARED heartbeat preset (story 10-1) —
    // contract v1, pause + interaction guaranteed ×3, self-reported metering,
    // the `[config.model]` env mapping, and the heartbeat-only args (the
    // steady-state window measures supervision, not metering ingestion).
    // The orphan bound the idlers are BUILT with: the default on no-flag
    // runs (byte-equivalent); the RAISED serve bound on `--serve-shape` runs
    // (the serve window + the bounded stop extend the schedule).
    let orphan_bound_ms = if cli.serve_shape {
        SERVE_AGENT_LINGER_MS
    } else {
        AGENT_LINGER_MS
    };
    let manifest_dir = ManifestFixture::heartbeat(MANIFEST_KIND, HEARTBEAT_MS, orphan_bound_ms)
        .exec(fake_agent)
        .write(manifest_tmp.path());

    // Arc-wrapped (story 17-1): the serve driver's tokio tasks need 'static
    // handles to the engine's PUBLIC async methods — the Arc IS the host's
    // own shape (an embedded host owns the Engine and shares it across its
    // tasks). The sync phases keep borrowing through the Arc as before.
    let engine = Arc::new(Engine::open(Some(state.path().to_path_buf())).expect("open the engine"));
    let facade = engine.blocking();

    // ---- Setup (NOT a timed window): register 25, seed a real config leaf
    // (so the timed config read is assertable), start 10 under the guard. ----
    let names: Vec<String> = (0..FLEET_SIZE)
        .map(|i| format!("{INSTANCE_PREFIX}{i:02}"))
        .collect();
    for name in &names {
        facade
            .register_with_adapter(name, &AdapterRef::Manifest(manifest_dir.clone()))
            .unwrap_or_else(|e| panic!("register {name} failed: {e}"));
        facade
            .set_config(name, MODEL_KEY, MODEL_VALUE)
            .unwrap_or_else(|e| panic!("seed config on {name} failed: {e}"));
    }
    note(&format!(
        "registered {FLEET_SIZE} instances (config leaf seeded)"
    ));
    let mut guard = RunningGuard::new(&facade, &names[..RUNNING_COUNT]);
    for name in &names[..RUNNING_COUNT] {
        facade
            .start(name)
            .unwrap_or_else(|e| panic!("start {name} failed: {e}"));
        let status = facade
            .instance_status(name)
            .unwrap_or_else(|e| panic!("status read for {name} failed: {e}"));
        assert_eq!(
            status.instance.state,
            LifecycleState::Running,
            "{name} must be Running after start"
        );
    }
    // The per-instance normalization divides by the ACTUALLY-Running count,
    // sampled at measurement time — never the intended count.
    let running_count = sample_running_count(&facade, &names);
    assert!(
        running_count > 0,
        "no instance is Running — the steady-state budget is meaningless over zero running instances"
    );
    if running_count != RUNNING_COUNT {
        note(&format!(
            "WARNING: {running_count} instances Running (expected {RUNNING_COUNT}) — \
             normalizing over the ACTUAL count"
        ));
    }
    note(&format!(
        "{running_count} instances RUNNING (heartbeat-only idlers); settling {:?}",
        config.settle
    ));
    std::thread::sleep(config.settle);

    // ---- Measure (1): steady-state ENGINE-process CPU% / RSS (median of the
    // configured window count). ----
    let steady = measure_steady_state(&config);
    note("steady-state sampling complete");

    // A short re-settle so the window's background does not bleed into the
    // read-latency samples (see the contamination note in the report).
    std::thread::sleep(Duration::from_secs(1));

    // ---- Measure (2): read latency through the public facade. CI mode only:
    // a failed read gate retries ONCE after a cool-down and gates on the
    // second run (a co-tenant stall must not red an ordinary PR). ----
    let mut read_attempts = 1usize;
    let mut reads = measure_read_latency(&facade, &names, config.read_iterations);
    let mut p99 = overall_read_p99(&reads);
    if config.ci && p99 >= BUDGET_READ_P99_MS {
        note(&format!(
            "read gate at-or-over budget on CI (p99 {p99:.3} ms; the gate is strict <) — \
             cooling down {:?} and \
             retrying ONCE (the gate applies to the second run)",
            READ_RETRY_COOLDOWN
        ));
        std::thread::sleep(READ_RETRY_COOLDOWN);
        reads = measure_read_latency(&facade, &names, config.read_iterations);
        p99 = overall_read_p99(&reads);
        read_attempts = 2;
    }
    note("read-latency measurement complete");

    // ---- Measure (3): subscriber-active overhead — GATED at the story-10-3
    // ratified delta budgets. NFR-4's gated figures above are ZERO-subscriber
    // figures by design; a host using the subscription surface needs a budget
    // covering fan-out cost, so the harness measures the SAME per-instance
    // figures and read p99 with ONE active subscriber attached and the DELTAS
    // vs the unsubscribed baseline gate (strict locally; the shared-runner
    // tolerance on CI — see `subscriber_gates`). It still runs AFTER the
    // gated measurements (so it cannot contaminate them), uses ONE
    // steady-state window even in CI mode (the tolerance is the noise
    // absorber there), and the heartbeat idlers must still be alive — which
    // is why the startup liveness check counts this window too.
    note("subscriber-overhead addendum: attaching ONE active subscriber");
    let subscription = facade.subscribe();
    let sub_steady = measure_steady_windows(&config, 1);
    // The same re-settle discipline as the unsubscribed pass.
    std::thread::sleep(Duration::from_secs(1));
    let sub_reads = measure_read_latency(&facade, &names, config.read_iterations);
    let sub_p99 = overall_read_p99(&sub_reads);
    drop(subscription); // release the runtime handle before teardown
    note("subscriber-overhead addendum complete");

    // ---- Story 17-1: the serve-shape scenario — ONLY on --serve-shape runs
    // (the default invocation stops here and stays byte-equivalent). It runs
    // AFTER every gated phase so it cannot contaminate them. An Err (a reader
    // aborted or panicked, or the contention proof failed) is carried as a
    // NAMED scenario error: the measured record below is SKIPPED and the run
    // exits 2 after teardown — BEFORE any gate verdict exists (gates over a
    // vacuous window would dress idle-supervision numbers as serve evidence;
    // review round 3). ----
    let mut serve_scenario_error: Option<String> = None;
    let serve = if cli.serve_shape {
        match run_serve_scenario(&engine, &facade, &names, &config) {
            Ok(outcome) => Some(outcome),
            Err(reason) => {
                serve_scenario_error = Some(reason);
                None
            }
        }
    } else {
        None
    };

    // The exit fold's inputs, hoisted out of the record block so the tail can
    // fold them through the pinned `run_exit_code` (review round 3).
    let mut gates_passed = true;
    let mut ratchet_failed = false;
    let mut serve_failed = false;
    let mut baseline_write_failed = false;

    // ---- The measured record + every verdict. Breaks early on a named
    // serve-scenario error (above): teardown below still runs; the named
    // exit-2 fires after it. ----
    'record: {
        if let Some(reason) = &serve_scenario_error {
            note(reason);
            break 'record;
        }
        // ---- Report BEFORE teardown, so the measured record survives any
        // teardown trouble; the exit code still reflects the gates. ----
        let figures = SteadyFigures::new(
            steady.median_cpu_aggregate_pct,
            steady.median_rss_bytes_mean,
            steady.rss_bytes_max,
            running_count,
        );
        let sub_figures = SteadyFigures::new(
            sub_steady.median_cpu_aggregate_pct,
            sub_steady.median_rss_bytes_mean,
            sub_steady.rss_bytes_max,
            running_count,
        );
        let subscriber_overhead = SubscriberOverhead::new(&figures, p99, &sub_figures, sub_p99);
        let tolerance = if config.ci {
            CI_CPU_TOLERANCE_FACTOR
        } else {
            1.0
        };
        let gates = evaluate_gates(p99, &figures, &subscriber_overhead, tolerance);
        gates_passed = gates.passed;
        let mut report = build_report(
            &config,
            &reads,
            p99,
            &steady,
            &figures,
            running_count,
            read_attempts,
            tolerance,
            &gates,
            &subscriber_overhead,
            orphan_bound_ms,
        );
        // ---- Story 17-1: the serve gates + the serve_shape report block,
        // ADDITIVE and only on --serve-shape runs — evaluated BEFORE the
        // trend block below, because a serve run's ratchet covers them too
        // (review round 3: the mode-aware trend set; the baseline's coverage
        // was validated at load). Platform scope (review round 3), the
        // standing gate-platform rule: on the ratified gate platform the
        // serve verdicts GATE; elsewhere (Windows — sysinfo's `memory()` is
        // the working-set-size analog, not a ratified gate platform) they
        // are REPORT-ONLY with a named note, never gating the run. A
        // failure still flips the report status + the exit code, and the
        // FIXED decision rule (spec 17-1) is stated on stderr either way —
        // the evidence recommends; it never argues past the numbers. ----
        let mut serve_gates_for_trend: Option<GateReport> = None;
        if let Some(outcome) = &serve {
            let binding = serve_gates_bind(std::env::consts::OS);
            let serve_gates =
                evaluate_serve_gates(outcome.overall_p99_ms, &outcome.figures, tolerance);
            serve_failed = binding && !serve_gates.passed;
            if !binding {
                note(&format!(
                    "serve gates are REPORT-ONLY on this platform ({}) — not a ratified \
                     gate platform; the serve figures are measured and reported below and \
                     never gate this run",
                    std::env::consts::OS,
                ));
            }
            report["serve_shape"] = serve_shape_json(outcome, &serve_gates, tolerance, binding);
            if serve_failed {
                report["status"] = serde_json::json!("fail");
            }
            for verdict in &serve_gates.verdicts {
                note(&format!(
                    "serve gate {} measured {:.4} vs effective budget {:.4} — {}",
                    verdict.name,
                    verdict.measured,
                    verdict.effective_budget,
                    if verdict.passed { "PASS" } else { "FAIL" },
                ));
            }
            if serve_failed {
                note(
                    "DECISION RULE (spec 17-1): at least one serve budget was exceeded — the \
                     evidence recommends ACTIVATING the archived per-instance lock-model plan \
                     (ai-63b-lock-model-decision-2026-08-27.md)",
                );
            } else {
                note(
                    "DECISION RULE (spec 17-1): all serve budgets held — the evidence \
                     recommends RE-RATIFYING the coarse two-mutex lock model, citing the \
                     measured headroom recorded in the report's serve_shape block",
                );
            }
            serve_gates_for_trend = Some(serve_gates);
        }
        // ---- The trend block + ratchet (story 15-5), ADDITIVE to the gates:
        // inserted only when --baseline was given, so the default report stays
        // byte-equivalent. The ratchet fails the run (exit 1 below) only on its
        // gate platform — CI mode; every other run reports without gating. On a
        // serve run the trend set is MODE-AWARE (review round 3): the serve
        // gates ride the ratchet too (their noise floors sit in
        // `ratchet_noise_floor`), and the loaded baseline was already validated
        // to carry them at load time. All of the report mutation lives in the
        // pure, pinned apply_trend. On a trend-evaluation ERROR the report is
        // printed to stdout FIRST (the measured record survives the error —
        // review round 1), then the named error + exit 2. ----
        if let Some((path, figures)) = &baseline {
            let mut trend_gates = gates.clone();
            if let Some(serve_gates) = &serve_gates_for_trend {
                trend_gates
                    .verdicts
                    .extend(serve_gates.verdicts.iter().cloned());
            }
            match evaluate_trend(&trend_gates, figures, path, config.ci) {
                Ok(trend) => {
                    emit_trend_notes(&trend);
                    ratchet_failed = apply_trend(&mut report, &trend);
                }
                Err(reason) => {
                    print_report(&report, &gates);
                    note(&format!("{reason} — exiting 2"));
                    std::process::exit(2);
                }
            }
        }
        print_report(&report, &gates);
        // ---- --write-baseline: the fresh report (trend stripped, provenance
        // stamped, trailing newline) as the next baseline. A RED run is REFUSED
        // (review round 1: a failed run's regressed figures must never become
        // canonical — loud note + exit 2, the file is never written; the exit-2
        // takes precedence over the red exit-1 below so the operator sees the
        // usage error, and the note names the red-ness either way — the fold
        // itself is the pinned `baseline_write_refusal`, whose serve token is
        // its own arm since review round 3). Otherwise the operator asked for
        // the FILE, so a write failure is a loud error that fails the run (exit
        // 2 below) — unlike the optional PERF_BUDGETS_REPORT_PATH artifact
        // below, whose failure is only noted (stdout — the measured record — is
        // unaffected there). ----
        if let Some(path) = &cli.write_baseline {
            // Story 17-1: a red SERVE run refuses the write too — a failed run's
            // figures must never become canonical, whatever gated them.
            if let Some(reason) = baseline_write_refusal(gates_passed, serve_failed, ratchet_failed)
            {
                // Deferred like the write-failure flag below: the note fires
                // here, the exit happens after teardown (process::exit skips
                // destructors — the RunningGuard must get its chance).
                note(reason);
                baseline_write_failed = true;
            } else {
                let provenance =
                    baseline_provenance(config.ci, on_github_runner(), &git_full_sha());
                let fresh = baseline_report_json(&report, provenance);
                let written = serde_json::to_string_pretty(&fresh)
                    .map_err(|e| e.to_string())
                    .map(|text| format!("{text}\n"))
                    .and_then(|text| std::fs::write(path, text).map_err(|e| e.to_string()));
                match written {
                    Ok(()) => note(&format!(
                        "baseline written to {path} (the next baseline; commit it via the \
                 documented PR procedure — the visible diff IS the ratification)"
                    )),
                    Err(e) => {
                        baseline_write_failed = true;
                        note(&format!(
                            "could not write the baseline to {path} ({e}) — exiting 2"
                        ));
                    }
                }
            }
        }
        if let Ok(path) = std::env::var("PERF_BUDGETS_REPORT_PATH") {
            match serde_json::to_string_pretty(&report)
                .map_err(|e| e.to_string())
                .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()))
            {
                Ok(()) => note(&format!("JSON report written to {path}")),
                Err(e) => note(&format!(
                    "could not write the JSON report to {path} ({e}) — the stdout report \
                 (the measured record) is unaffected"
                )),
            }
        }
    } // 'record

    // ---- Teardown (best-effort; a stop failure is noted, never a panic —
    // and the RunningGuard below would retry it on unwind anyway). ----
    let mut all_stopped = true;
    for name in guard.names() {
        if let Err(e) = facade.stop(name, Some(Duration::from_secs(5))) {
            all_stopped = false;
            note(&format!(
                "stop {name} failed during teardown ({e}); the agent's own \
                 --linger-ms bound ({orphan_bound_ms} ms) caps any orphan"
            ));
        }
    }
    guard.defuse_if(all_stopped);
    drop(guard);
    drop(engine);
    drop(state);
    drop(manifest_tmp);
    note("teardown complete: running instances stopped, hermetic roots removed");

    if baseline_write_failed {
        // Exit 2 whether the write FAILED or was REFUSED (review round 1):
        // the refusal is a usage error and takes precedence over the red
        // run's exit 1 — the note above already names the red-ness.
        std::process::exit(2);
    }
    if let Some(reason) = serve_scenario_error {
        // A vacuous serve window (review round 3): the named reason is
        // already on stderr; the run fails with exit 2 — the scenario-error
        // class — and takes precedence over every other exit below.
        note(&format!("{reason} — exiting 2"));
        std::process::exit(2);
    }
    // The measured record is already on stdout; exit nonzero so the CI
    // perf-budgets job (and any local gate runner) fails on the regression —
    // an absolute-budget failure, a ratchet failure, OR a serve-gate failure
    // (story 17-1). The fold itself is the pinned `run_exit_code`.
    std::process::exit(run_exit_code(gates_passed, ratchet_failed, serve_failed));
}

// ---------------------------------------------------------------------------
// RunningGuard — teardown on ANY exit path
// ---------------------------------------------------------------------------

/// Arms the running-instance list for best-effort teardown on EVERY exit
/// path: the explicit teardown defuses it, but a panic (or an early exit
/// between start and teardown) unwinds through Drop, which stops every
/// running instance before the process dies. Best-effort: a stop failure
/// during Drop is swallowed — the fixture's `--linger-ms` bound caps any
/// orphan either way.
struct RunningGuard<'a> {
    facade: &'a Blocking<'a>,
    names: Vec<String>,
    armed: bool,
}

impl<'a> RunningGuard<'a> {
    fn new(facade: &'a Blocking<'a>, names: &[String]) -> Self {
        Self {
            facade,
            names: names.to_vec(),
            armed: true,
        }
    }

    fn names(&self) -> &[String] {
        &self.names
    }

    /// Disarm after a successful explicit teardown (Drop then does nothing).
    /// Left ARMED when any stop failed, so Drop retries them best-effort.
    fn defuse_if(&mut self, all_stopped: bool) {
        if all_stopped {
            self.armed = false;
        }
    }
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        note("unwound before explicit teardown — best-effort stopping running instances");
        for name in &self.names {
            let _ = self.facade.stop(name, Some(Duration::from_secs(2)));
        }
    }
}

// ---------------------------------------------------------------------------
// Measurement (1): steady-state engine-process CPU% / RSS
// ---------------------------------------------------------------------------

/// One steady-state window's aggregates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct WindowSample {
    /// Mean aggregate engine-process CPU% across this window's counted
    /// samples (the per-core SUM — can exceed 100 on a multicore host).
    cpu_pct_aggregate_mean: f64,
    /// Mean engine-process RSS across this window's counted samples, in bytes.
    rss_bytes_mean: f64,
    /// Max engine-process RSS across this window's counted samples, in bytes.
    rss_bytes_max: u64,
    /// How many samples were COUNTED (the first is always discarded).
    samples: usize,
}

/// All steady-state windows, reduced to the figures the report + gates use.
#[derive(Clone, Debug, PartialEq)]
struct SteadyMeasurements {
    windows: Vec<WindowSample>,
    /// The MEDIAN window's aggregate CPU% (of one window locally; of three
    /// on CI — the stability mechanism for shared-runner noise).
    median_cpu_aggregate_pct: f64,
    /// The median window's mean RSS, in bytes.
    median_rss_bytes_mean: f64,
    /// The max RSS over EVERY sample of EVERY window, in bytes (reported and
    /// spike-guarded, never mean-smoothed away).
    rss_bytes_max: u64,
    first_sample_skipped: bool,
}

/// The gated steady-state phase: `config.windows` windows (one locally;
/// median-of-three on CI).
fn measure_steady_state(config: &HarnessConfig) -> SteadyMeasurements {
    measure_steady_windows(config, config.windows)
}

/// Sample the ENGINE process (= this process: the harness owns the `Engine`)
/// for `window_count` windows. sysinfo computes a process's CPU% from the
/// delta between two refreshes, so the FIRST refresh only primes the baseline
/// and each subsequent 1 s-apart refresh yields the usage for the interval
/// since the previous one — the main thread sleeps between refreshes, so the
/// sampled figure is the engine's idle supervision cost, not loop cost. The
/// first sample of each window is DISCARDED (its interval overlaps whatever
/// ran before it — setup, or the previous window — so it is residue, not
/// steady state). The gated phase passes `config.windows`; the
/// subscriber-overhead addendum passes ONE window.
fn measure_steady_windows(config: &HarnessConfig, window_count: usize) -> SteadyMeasurements {
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    let mut windows = Vec::with_capacity(window_count);
    for window_index in 0..window_count {
        if window_index > 0 {
            // A short gap between windows so each median candidate is an
            // independent, separately-primed measurement.
            std::thread::sleep(config.sample_interval);
        }
        let primed = sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        assert_eq!(primed, 1, "the engine process (self) must be sampleable");
        let sample_count =
            (config.window.as_secs_f64() / config.sample_interval.as_secs_f64()).round() as usize;
        let mut cpu = Vec::with_capacity(sample_count);
        let mut rss = Vec::with_capacity(sample_count);
        for _ in 0..sample_count {
            std::thread::sleep(config.sample_interval);
            let updated = sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
            assert_eq!(
                updated, 1,
                "the engine process must stay alive while sampling"
            );
            let process = sys.process(pid).expect("the refreshed engine process");
            cpu.push(f64::from(process.cpu_usage()));
            rss.push(process.memory());
        }
        let _ = cpu.drain(..1.min(cpu.len()));
        let _ = rss.drain(..1.min(rss.len()));
        assert!(!cpu.is_empty(), "a window must count at least one sample");
        windows.push(WindowSample {
            cpu_pct_aggregate_mean: cpu.iter().sum::<f64>() / cpu.len() as f64,
            rss_bytes_mean: rss.iter().sum::<u64>() as f64 / rss.len() as f64,
            rss_bytes_max: *rss.iter().max().expect("non-empty samples"),
            samples: rss.len(),
        });
    }
    let mut cpu_means: Vec<f64> = windows.iter().map(|w| w.cpu_pct_aggregate_mean).collect();
    let mut rss_means: Vec<f64> = windows.iter().map(|w| w.rss_bytes_mean).collect();
    cpu_means.sort_by(|a, b| a.total_cmp(b));
    rss_means.sort_by(|a, b| a.total_cmp(b));
    SteadyMeasurements {
        median_cpu_aggregate_pct: median(&cpu_means),
        median_rss_bytes_mean: median(&rss_means),
        rss_bytes_max: windows
            .iter()
            .map(|w| w.rss_bytes_max)
            .max()
            .unwrap_or_default(),
        first_sample_skipped: true,
        windows,
    }
}

/// The actually-Running count across the whole fixture, sampled through the
/// facade (never the intended count).
fn sample_running_count(facade: &Blocking<'_>, names: &[String]) -> usize {
    names
        .iter()
        .filter(|name| {
            facade
                .instance_status(name)
                .map(|status| status.instance.state == LifecycleState::Running)
                .unwrap_or(false)
        })
        .count()
}

// ---------------------------------------------------------------------------
// Measurement (2): read latency
// ---------------------------------------------------------------------------

/// Drive `READ_ITERATIONS` iterations of the four read kinds. Each iteration
/// spreads the per-instance reads across the Fleet round-robin (iteration `i`
/// lands on instance `i % 25`) and asserts the reads actually returned the
/// right content — a read that silently degraded must not count as a fast
/// read.
fn measure_read_latency(
    facade: &Blocking<'_>,
    names: &[String],
    iterations: usize,
) -> Vec<ReadStats> {
    let mut fleet = Vec::with_capacity(iterations);
    let mut status = Vec::with_capacity(iterations);
    let mut config = Vec::with_capacity(iterations);
    let mut usage = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let name = names[i % names.len()].as_str();

        let t = Instant::now();
        let entries = facade.fleet().expect("fleet read");
        fleet.push(elapsed_ms(t));
        assert_eq!(
            entries.len(),
            FLEET_SIZE,
            "fleet read must list the whole fixture"
        );

        let t = Instant::now();
        let state = facade.instance_status(name).expect("instance_status read");
        status.push(elapsed_ms(t));
        assert_eq!(
            state.instance.name.as_str(),
            name,
            "instance_status must return the requested instance"
        );

        let t = Instant::now();
        let effective = facade
            .effective_config(name, ConfigLayer::empty())
            .expect("effective_config read");
        config.push(elapsed_ms(t));
        assert_eq!(
            effective.value_display(MODEL_KEY).as_deref(),
            Some(MODEL_VALUE),
            "the seeded config leaf must read back through the timed read"
        );

        // The per-instance usage read, EXACTLY as `kt agent usage <name>`
        // performs it through the facade: `fleet()` then find. Reported as
        // `usage_via_fleet` — it includes the fleet listing's cost and is
        // NOT an independent read kind.
        let t = Instant::now();
        let entry = usage_read(facade, name);
        usage.push(elapsed_ms(t));
        assert_eq!(
            entry.name.as_str(),
            name,
            "the usage read found its instance"
        );
    }
    vec![
        summarize_read("fleet", &mut fleet),
        summarize_read("instance_status", &mut status),
        summarize_read("effective_config", &mut config),
        summarize_read("usage_via_fleet", &mut usage),
    ]
}

/// `kt agent usage <name>`'s exact facade shape (the CLI has no narrower
/// per-instance usage query — this IS the public usage read).
fn usage_read(facade: &Blocking<'_>, name: &str) -> FleetEntry {
    facade
        .fleet()
        .expect("usage read: fleet")
        .into_iter()
        .find(|e| e.name.as_str() == name)
        .expect("usage read: the instance is in the Fleet")
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Report (pure build + print split, so tests pin the math, not the printing)
// ---------------------------------------------------------------------------

/// One gate verdict's report JSON — the ONE shape both the default gate
/// table and the serve gate table render (shared so the two tables cannot
/// drift).
fn verdict_json(v: &GateVerdict) -> serde_json::Value {
    serde_json::json!({
        "name": v.name,
        "hard": v.hard,
        "bound": v.bound.as_str(),
        "measured": v.measured,
        "budget": v.budget,
        "effective_budget": v.effective_budget,
        "tolerance": v.tolerance,
        "strict_margin": v.strict_margin,
        "status": if v.passed { "pass" } else { "fail" },
    })
}

/// The per-kind read-latency JSON — the ONE shape both the default report
/// and the serve block render.
fn read_kind_json(reads: &[ReadStats]) -> serde_json::Map<String, serde_json::Value> {
    let mut per_kind = serde_json::Map::new();
    for stats in reads {
        per_kind.insert(
            stats.name.to_string(),
            serde_json::json!({
                "p50_ms": stats.p50, "p95_ms": stats.p95, "p99_ms": stats.p99,
                "max_ms": stats.max, "samples": stats.samples,
            }),
        );
    }
    per_kind
}

/// The steady-state JSON block — the ONE shape both the default report and
/// the serve block render.
fn steady_json(steady: &SteadyMeasurements, figures: &SteadyFigures) -> serde_json::Value {
    serde_json::json!({
        "windows": steady.windows.iter().map(|w| serde_json::json!({
            "cpu_percent_aggregate_mean": w.cpu_pct_aggregate_mean,
            "rss_mib_mean": w.rss_bytes_mean / (1024.0 * 1024.0),
            "rss_mib_max": w.rss_bytes_max as f64 / (1024.0 * 1024.0),
            "samples": w.samples,
        })).collect::<Vec<_>>(),
        "cpu_percent_aggregate_median": steady.median_cpu_aggregate_pct,
        "rss_mib_aggregate_median": steady.median_rss_bytes_mean / (1024.0 * 1024.0),
        "cpu_percent_per_instance": figures.cpu_per_instance_pct,
        "rss_mib_per_instance_mean": figures.rss_per_instance_mib,
        "rss_mib_per_instance_max": figures.rss_max_per_instance_mib,
    })
}

/// Build the machine-readable report (schema v1) from the measurements and
/// the gate evaluation. Pure — no printing, no environment access. The
/// schema stays v1: the subscriber-overhead block is ADDITIVE (the AD-14
/// convention — additive fields keep the version). `orphan_bound_ms` is the
/// bound the heartbeat fixture was ACTUALLY built with (the default bound on
/// no-flag runs — byte-equivalent — the raised serve bound on `--serve-shape`
/// runs).
#[allow(clippy::too_many_arguments)]
fn build_report(
    config: &HarnessConfig,
    reads: &[ReadStats],
    overall_p99_ms: f64,
    steady: &SteadyMeasurements,
    figures: &SteadyFigures,
    running_count: usize,
    read_attempts: usize,
    tolerance: f64,
    gates: &GateReport,
    subscriber_overhead: &SubscriberOverhead,
    orphan_bound_ms: u64,
) -> serde_json::Value {
    let verdicts: Vec<serde_json::Value> = gates.verdicts.iter().map(verdict_json).collect();
    serde_json::json!({
        "schema_version": REPORT_SCHEMA_VERSION,
        "story": "7-5",
        "budget_source": "NFR-4",
        "ci_mode": config.ci,
        "environment": {
            "git_sha": git_short_sha(),
            "os": std::env::consts::OS,
            "cpu_model": host_cpu_model(),
            "logical_cores": host_logical_cores(),
            "ci": config.ci,
            "tolerance_factor": tolerance,
        },
        "fixture": {
            "fleet_size": FLEET_SIZE,
            "running_instances": running_count,
            "running_instances_sampled_at_measurement": true,
            "idle_behavior": format!("fake_agent --heartbeat-ms {HEARTBEAT_MS} (heartbeat only; no usage emission)"),
            "orphan_bound_ms": orphan_bound_ms,
            "state_root": "hermetic temp dir (removed at teardown)",
        },
        "methodology": {
            "order": "setup -> settle -> steady-state window(s) -> re-settle -> read latency -> subscriber-overhead addendum (one window + reads, reported only)",
            "read_iterations": config.read_iterations,
            "read_kinds": reads.iter().map(|k| k.name).collect::<Vec<_>>(),
            "read_spread": "per-instance reads round-robin across the fleet (iteration i -> instance i % 25)",
            "usage_kind_note": "usage_via_fleet mirrors kt agent usage <name>'s exact facade shape (fleet() + find) and therefore INCLUDES the fleet listing's cost — it is not an independent read kind",
            "steady_state_window_secs": config.window.as_secs_f64(),
            "steady_state_windows": steady.windows.len(),
            "steady_state_selection": if steady.windows.len() > 1 { "median window (CI stability mechanism)" } else { "single window (local strict)" },
            "first_sample_skipped": steady.first_sample_skipped,
            "settle_secs": config.settle.as_secs_f64(),
            "sample_interval_secs": config.sample_interval.as_secs_f64(),
            "process_metrics_source": "sysinfo (dev-dep), two-refresh CPU delta, one-second samples",
            "cpu_scope": "ENGINE process aggregate (per-core sum) — the harness process that owns the Engine runtime",
            "rss_scope": "ENGINE process resident set only; the supervised agents' own memory is their own, NOT supervision overhead; the allocator may retain pages from earlier phases (registration/start/sampling) in this figure",
            "contamination_notes": [
                "reads-before-window would inflate the CPU samples with read-phase activity — the fixed order (steady first) avoids that direction",
                "the window's background leaves the runtime warm for the reads — a short re-settle (1 s) absorbs the other direction",
            ],
            "window_coverage": "the window validates heartbeat-frequency supervision work (reaper cadence + per-second log captures); longer-period supervision tasks (sweeps, retention) are NOT captured — lengthen locally via PERF_BUDGETS_WINDOW_MS (clamped to [10s, 1h])",
        },
        "read_latency_ms": read_kind_json(reads),
        "read_p99_overall_ms": overall_p99_ms,
        "subscriber_overhead": {
            "policy": "GATED at the story-10-3 ratified subscriber-active delta budgets (strict locally; the shared-runner tolerance on CI, one window even on CI — the deltas gate, the absolutes are context); the gated zero-subscriber figures above were measured BEFORE the subscriber attached, by design",
            "subscriber_count": 1,
            "window_count": 1,
            "window_note": "one steady-state window even in CI mode — the ratified delta budgets are sized for single-window noise (docs/testing.md)",
            "cpu_percent_per_instance": subscriber_overhead.cpu_per_instance_pct,
            "cpu_delta_vs_unsubscribed_pct_points": subscriber_overhead.cpu_delta_pct_points,
            "rss_mib_per_instance_mean": subscriber_overhead.rss_per_instance_mib_mean,
            "rss_delta_vs_unsubscribed_mib": subscriber_overhead.rss_delta_mib,
            "read_p99_overall_ms": subscriber_overhead.read_p99_overall_ms,
            "read_p99_delta_vs_unsubscribed_ms": subscriber_overhead.read_p99_delta_ms,
        },
        "read_gate_retry": {
            "policy": "CI mode retries ONCE after a 30 s cool-down and gates on the second run (house pattern: a co-tenant stall must not red an ordinary PR); local runs are single-shot",
            "attempts": read_attempts,
            "retry_applied": read_attempts > 1,
        },
        "steady_state": steady_json(steady, figures),
        "budgets": {
            "read_p99_ms": BUDGET_READ_P99_MS,
            "cpu_per_instance_pct": BUDGET_CPU_PER_INSTANCE_PCT,
            "rss_per_instance_mib": BUDGET_RSS_PER_INSTANCE_MIB,
            "rss_spike_factor": RSS_SPIKE_TOLERANCE,
            "subscriber_cpu_delta_pct_points": BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS,
            "subscriber_rss_delta_mib": BUDGET_SUBSCRIBER_RSS_DELTA_MIB,
            "subscriber_read_p99_delta_ms": BUDGET_SUBSCRIBER_READ_P99_DELTA_MS,
            "subscriber_budget_source": "story 10-3 ratification over the observed 2026-09-10 runs (docs/testing.md)",
        },
        "ci_tolerance": {
            "name": "shared_runner_tolerance",
            "factor": CI_CPU_TOLERANCE_FACTOR,
            "applies_to": "cpu_per_instance gate AND the story-10-3 subscriber-active delta gates, CI mode only",
            "applied": tolerance != 1.0,
            "effective_cpu_budget_pct": BUDGET_CPU_PER_INSTANCE_PCT * tolerance,
            "policy": "reads and RSS gate at budget on every gate platform (ubuntu CI; strict local macOS — Windows is measured-and-reported, never a gate platform); CPU gates strict (x1.0) locally and at the named shared-runner tolerance (x1.5) on CI — over the median of three windows — where co-tenant noise is uncontrollable; the subscriber delta gates use the same factor shape but stay single-window (the factor is their noise absorber, and their budgets are sized for it)",
        },
        "gates": verdicts,
        "notes": [
            "units: the gate is 50 MiB/instance (derived from raw bytes); this reconciles with NFR-4's '50MB' prose intent — documented in docs/testing.md",
            "platform scope: gates are enforced on ubuntu CI; strict local gating on macOS; Windows figures are measured-and-reported (sysinfo's memory() is the working-set-size analog there), never a gate platform",
            "the per-instance budgets mean MARGINAL supervision overhead: the fixed engine cost amortizes over the running count (see the consts' budget-assumption docs)",
            "subscriber overhead is measured separately (the subscriber_overhead block, ONE active subscriber, after the gated phases) and its DELTAS gate at the story-10-3 ratified budgets (strict locally; shared-runner tolerance on CI) — a negative delta passes trivially: the budget ceilings the overhead, never the measurement's sign",
        ],
        "status": if gates.passed { "pass" } else { "fail" },
    })
}

/// Print the human summary (stdout — this is output, not diagnostics) and the
/// JSON block that ends the report.
fn print_report(report: &serde_json::Value, gates: &GateReport) {
    let overall = report["read_p99_overall_ms"].as_f64().unwrap_or(0.0);
    println!("PERF BUDGETS — reads p99 (overall worst kind): {overall:.3} ms (budget < {BUDGET_READ_P99_MS} ms)");
    if let Some(kinds) = report["read_latency_ms"].as_object() {
        for (name, stats) in kinds {
            println!(
                "  {name:<22} p50={:>9.3} ms  p95={:>9.3} ms  p99={:>9.3} ms",
                stats["p50_ms"], stats["p95_ms"], stats["p99_ms"],
            );
        }
    }
    if let Some(steady) = report["steady_state"].as_object() {
        println!(
            "PERF BUDGETS — steady state ({} window(s), median selected, first sample skipped): engine CPU {:.3}% aggregate → {:.4}%/instance; RSS mean {:.2} MiB → {:.2} MiB/instance (max {:.2} MiB/instance)",
            steady["windows"].as_array().map(Vec::len).unwrap_or(0),
            steady["cpu_percent_aggregate_median"],
            steady["cpu_percent_per_instance"],
            steady["rss_mib_aggregate_median"],
            steady["rss_mib_per_instance_mean"],
            steady["rss_mib_per_instance_max"],
        );
    }
    if let Some(sub) = report["subscriber_overhead"].as_object() {
        println!(
            "PERF BUDGETS — with ONE active subscriber (deltas GATED at the ratified budgets): \
             CPU {:.4}%/instance ({:+.4} pts vs unsubscribed, gate <= {}); RSS mean {:.2} MiB/instance \
             ({:+.2} MiB, gate <= {}); reads p99 {:.3} ms ({:+.3} ms vs unsubscribed, gate <= {}).",
            sub["cpu_percent_per_instance"],
            sub["cpu_delta_vs_unsubscribed_pct_points"],
            BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS,
            sub["rss_mib_per_instance_mean"],
            sub["rss_delta_vs_unsubscribed_mib"],
            BUDGET_SUBSCRIBER_RSS_DELTA_MIB,
            sub["read_p99_overall_ms"],
            sub["read_p99_delta_vs_unsubscribed_ms"],
            BUDGET_SUBSCRIBER_READ_P99_DELTA_MS,
        );
    }
    // The serve-shape section (story 17-1) — present only on --serve-shape
    // runs (the default invocation prints nothing here, byte-equivalent).
    if report.get("serve_shape").is_some() {
        let serve = &report["serve_shape"];
        // The block's own absent-token convention (review round 3): a
        // missing figure prints NaN (floats) or `—` (counts) — never a
        // fabricated zero that would read as a measurement.
        let f64_cell = |value: &serde_json::Value| -> String {
            value
                .as_f64()
                .map_or_else(|| "NaN".to_string(), |v| format!("{v:.3}"))
        };
        let u64_cell = |value: &serde_json::Value| -> String {
            value
                .as_u64()
                .map_or_else(|| "—".to_string(), |v| v.to_string())
        };
        let running = serve["fixture"]["running_instances"]
            .as_u64()
            .map_or_else(|| "—".to_string(), |v| v.to_string());
        let effective = serve["fixture"]["effective_running_instances"]
            .as_u64()
            .map_or_else(|| "—".to_string(), |v| v.to_string());
        println!(
            "PERF BUDGETS — serve shape (story 17-1): {SERVE_READER_TASKS} reader task(s) on a \
             {SERVE_DRIVER_WORKERS}-thread driver runtime + 1 subscribed follower + one \
             pathological bounded stop (window {SERVE_STOP_WINDOW:?}, fired at \
             {SERVE_STOP_DELAY:?}), over a {SERVE_WINDOW:?} window on {running} Running \
             instance(s) (normalizing over {effective} effective after the stop)."
        );
        println!(
            "  serve reads p99 (overall worst kind): {} ms (gate < {BUDGET_READ_P99_MS} ms)",
            f64_cell(&serve["read_p99_overall_ms"]),
        );
        if let Some(kinds) = serve["read_latency_ms"].as_object() {
            for (name, stats) in kinds {
                println!(
                    "  {name:<22} p50={:>9.3} ms  p95={:>9.3} ms  p99={:>9.3} ms",
                    stats["p50_ms"], stats["p95_ms"], stats["p99_ms"],
                );
            }
        }
        println!(
            "  serve steady: CPU {:.4}%/instance (gate <= {BUDGET_CPU_PER_INSTANCE_PCT}); RSS mean \
             {:.2} MiB/instance (gate <= {BUDGET_RSS_PER_INSTANCE_MIB})",
            serve["steady_state"]["cpu_percent_per_instance"]
                .as_f64()
                .unwrap_or(f64::NAN),
            serve["steady_state"]["rss_mib_per_instance_mean"]
                .as_f64()
                .unwrap_or(f64::NAN),
        );
        println!(
            "  serve load: {} iteration(s)/reader delivered (nominal {}), {} reader-retry \
             attempt(s), {} read(s) started inside the stop's window",
            serve["driver"]["iterations_delivered_per_reader"]
                .as_array()
                .map(|values| values.iter().map(u64_cell).collect::<Vec<_>>().join("/"))
                .unwrap_or_else(|| "—".to_string()),
            u64_cell(&serve["driver"]["read_iterations_per_reader"]),
            u64_cell(&serve["driver"]["reader_retries"]),
            u64_cell(&serve["contention"]["reads_started_during_stop"]),
        );
        println!(
            "  serve bounded stop: {} ms elapsed, final state {}, last cause {}",
            serve["bounded_stop"]["elapsed_ms"]
                .as_f64()
                .map_or_else(|| "—".to_string(), |v| format!("{v:.1}")),
            serve["bounded_stop"]["final_state"].as_str().unwrap_or("—"),
            serve["bounded_stop"]["last_transition_cause"]
                .as_str()
                .unwrap_or("—"),
        );
        println!(
            "  serve follower (bus liveness under load, not load evidence): {} event(s) \
             counted, {} Lagged-recovered — {}",
            u64_cell(&serve["subscriber"]["events_counted"]),
            u64_cell(&serve["subscriber"]["lagged_recovered"]),
            serve["subscriber"]["lagged_note"].as_str().unwrap_or("—"),
        );
        if let Some(verdicts) = serve["gates"].as_array() {
            for verdict in verdicts {
                println!(
                    "  gate {:<34} measured={:>10.4}  effective_budget={:>10.4}  {}",
                    verdict["name"].as_str().unwrap_or("?"),
                    verdict["measured"].as_f64().unwrap_or(f64::NAN),
                    verdict["effective_budget"].as_f64().unwrap_or(f64::NAN),
                    verdict["status"].as_str().unwrap_or("?").to_uppercase(),
                );
            }
        }
    }
    let tolerance_applied = report["ci_tolerance"]["applied"].as_bool().unwrap_or(false);
    if tolerance_applied {
        println!(
            "CI mode: the DOCUMENTED shared-runner tolerance factor {} applies to the CPU gate ONLY \
             (strict {}% → effective {}% per instance), over the median of {} windows. Reads and RSS gate at budget.",
            report["ci_tolerance"]["factor"],
            BUDGET_CPU_PER_INSTANCE_PCT,
            report["ci_tolerance"]["effective_cpu_budget_pct"],
            report["methodology"]["steady_state_windows"],
        );
    } else {
        println!(
            "Local mode: strict budgets gate (CPU {}%/instance, RSS {} MiB/instance, reads p99 < {} ms).",
            BUDGET_CPU_PER_INSTANCE_PCT, BUDGET_RSS_PER_INSTANCE_MIB, BUDGET_READ_P99_MS
        );
    }
    for verdict in &gates.verdicts {
        println!(
            "  gate {:<28} measured={:>10.4}  effective_budget={:>10.4}  margin(vs strict)={:>10.4}  {}",
            verdict.name,
            verdict.measured,
            verdict.effective_budget,
            verdict.strict_margin,
            if verdict.passed { "PASS" } else { "FAIL" },
        );
    }
    // The trend table (present only when --baseline was given — the default
    // invocation prints nothing here, byte-equivalent to the pre-15-5 form).
    // Values are EXTRACTED as f64 first (review round 1): serde_json's
    // Display ignores width/precision, so formatting the Value directly
    // silently dropped the column padding — and a missing limit (the
    // no-ratchet rows) renders through the ONE shared token instead of a
    // fake 0.
    if let Some(trend) = report["trend"].as_object() {
        let gated = trend["gated"].as_bool().unwrap_or(false);
        println!(
            "PERF BUDGETS — trend vs baseline {} (ratchet x{}, {}):",
            trend["baseline_path"],
            trend["tolerance"],
            if gated { "GATED" } else { "report-only" },
        );
        if let Some(metrics) = trend["metrics"].as_array() {
            for metric in metrics {
                let figure = |key: &str| metric[key].as_f64();
                println!(
                    "  trend {:<34} baseline={:>10.4}  current={:>10.4}  delta={:>9.4}  limit={:>10}  {}",
                    metric["name"].as_str().unwrap_or("?"),
                    // A null (the NaN-current no-ratchet arm) prints as NaN
                    // — honest, never a fabricated 0.
                    figure("baseline").unwrap_or(f64::NAN),
                    figure("current").unwrap_or(f64::NAN),
                    figure("delta").unwrap_or(f64::NAN),
                    render_limit(figure("ratchet_limit")),
                    metric["verdict"].as_str().unwrap_or("?"),
                );
            }
        }
    }
    // The overall status comes from the REPORT (a ratchet failure flips it
    // even with every absolute gate green — the gates table above stays the
    // absolute gates' own record).
    let status = report["status"].as_str().unwrap_or("fail");
    println!("PERF BUDGETS: {status}");
    println!("\n----- perf-budgets report (JSON, schema v{REPORT_SCHEMA_VERSION}) -----");
    println!(
        "{}",
        serde_json::to_string_pretty(report).expect("serialize the report")
    );
}

/// The short commit SHA of the tree the harness ran against (best-effort:
/// "unknown" when git is unavailable — the record must stay self-describing).
fn git_short_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The FULL 40-char commit SHA for the written baseline's `provenance`
/// block (the short form is ambiguous across forks/rewrites; the baseline
/// deserves the unambiguous one). Same best-effort honesty: "unknown"
/// rather than a fabricated value when git is unavailable.
fn git_full_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Whether this run sits on a GitHub Actions runner (the canonical CI leg's
/// own environment) — the mechanism that distinguishes a real leg artifact
/// from a local run with CI mode forced.
fn on_github_runner() -> bool {
    std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
}

/// The host CPU's brand string, for the report's environment block.
fn host_cpu_model() -> String {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string())
        .filter(|brand| !brand.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The host's logical core count, for the report's environment block.
fn host_logical_cores() -> usize {
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus().len()
}

// ---------------------------------------------------------------------------
// Gate-math tests (run by `cargo test --all-targets` — NOT the harness)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(n: usize, ms: f64) -> Vec<f64> {
        vec![ms; n]
    }

    /// n−3 fast + a 3-sample tail so nearest-rank p99 lands exactly at
    /// `p99` (rank ceil(0.99·n) hits the second-to-last tail sample) and max
    /// is a distinct value above it.
    fn kind_with_p99(name: &'static str, p99: f64, n: usize) -> ReadStats {
        let mut samples = uniform(n - 3, 1.0);
        samples.push(p99);
        samples.push(p99);
        samples.push(p99 + 100.0);
        summarize_read(name, &mut samples)
    }

    fn fast_reads() -> Vec<ReadStats> {
        vec![
            summarize_read("fleet", &mut uniform(200, 5.0)),
            summarize_read("instance_status", &mut uniform(200, 0.5)),
            summarize_read("effective_config", &mut uniform(200, 0.2)),
            summarize_read("usage_via_fleet", &mut uniform(200, 5.0)),
        ]
    }

    fn healthy_steady() -> SteadyFigures {
        SteadyFigures::new(10.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10)
    }

    /// A subscriber pass with ZERO deltas (identical figures both passes) —
    /// the default for tests that exercise the four zero-subscriber gates
    /// and must not trip the story-10-3 subscriber gates appended after
    /// them.
    fn quiet_subscriber() -> SubscriberOverhead {
        SubscriberOverhead::new(&healthy_steady(), 5.0, &healthy_steady(), 5.0)
    }

    #[test]
    fn all_fast_reads_and_healthy_steady_pass() {
        let gates = evaluate_gates(
            overall_read_p99(&fast_reads()),
            &healthy_steady(),
            &quiet_subscriber(),
            1.0,
        );
        assert!(gates.passed, "every gate must pass: {gates:?}");
    }

    #[test]
    fn one_kind_with_p99_1500ms_fails_the_overall_gate() {
        let mut reads = fast_reads();
        reads.push(kind_with_p99("slow_kind", 1500.0, 200));
        let overall = overall_read_p99(&reads);
        assert!(
            (overall - 1500.0).abs() < 1e-9,
            "the overall p99 must be the worst kind's 1500ms tail: {overall}"
        );
        let gates = evaluate_gates(overall, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert!(!gates.passed, "a 1500ms p99 kind must fail the read gate");
        assert!(!gates.verdicts[0].passed);
        assert!(
            gates.verdicts[0].strict_margin < 0.0,
            "the strict margin must be negative on the failure"
        );
    }

    #[test]
    fn rss_mean_over_budget_fails() {
        // 600 MiB aggregate over 10 running = 60 MiB/instance > 50.
        let steady = SteadyFigures::new(10.0, 600.0 * 1024.0 * 1024.0, 600_000_000, 10);
        let gates = evaluate_gates(5.0, &steady, &quiet_subscriber(), 1.0);
        assert!(
            !gates.verdicts[1].passed,
            "60 MiB/instance must fail: {gates:?}"
        );
        assert!(!gates.passed);
    }

    #[test]
    fn rss_normalization_divides_by_running_count() {
        // 400 MiB aggregate over 10 running = 40 MiB/instance — WITHIN the
        // 50 MiB mean budget: the budget is per-INSTANCE marginal overhead,
        // not an aggregate cap, and this pins exactly that division (the
        // gate consumes the divided figure, never the aggregate).
        let steady = SteadyFigures::new(10.0, 400.0 * 1024.0 * 1024.0, 400_000_000, 10);
        assert!((steady.rss_per_instance_mib - 40.0).abs() < 1e-9);
        let gates = evaluate_gates(5.0, &steady, &quiet_subscriber(), 1.0);
        assert!(gates.verdicts[1].passed);
        // The SAME aggregate over TWO running instances = 200 MiB/instance —
        // both the mean gate and the spike guard must fail.
        let few = SteadyFigures::new(10.0, 400.0 * 1024.0 * 1024.0, 400_000_000, 2);
        let gates = evaluate_gates(5.0, &few, &quiet_subscriber(), 1.0);
        assert!(!gates.verdicts[1].passed);
        assert!(!gates.verdicts[2].passed);
    }

    #[test]
    fn rss_max_spike_over_double_budget_fails_even_with_a_fine_mean() {
        // Mean 40 MiB/instance is fine; one sample spikes the max to
        // 120 MiB/instance > 2 × 50 — the leak guard must fail the run.
        let steady = SteadyFigures::new(10.0, 400.0 * 1024.0 * 1024.0, 1200 * 1024 * 1024, 10);
        assert!((steady.rss_per_instance_mib - 40.0).abs() < 1e-9);
        let gates = evaluate_gates(5.0, &steady, &quiet_subscriber(), 1.0);
        assert!(
            !gates.verdicts[2].passed,
            "a 120 MiB/instance spike must fail"
        );
        assert!(!gates.passed);
    }

    #[test]
    fn cpu_gate_is_strict_locally_and_tolerant_on_ci() {
        // 2.5%/instance: over the strict 2% budget, under the CI 2% × 1.5 = 3%.
        let steady = SteadyFigures::new(25.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let local = evaluate_gates(5.0, &steady, &quiet_subscriber(), 1.0);
        assert!(!local.verdicts[3].passed, "2.5%/instance must fail strict");
        assert!(!local.passed);
        let ci = evaluate_gates(5.0, &steady, &quiet_subscriber(), CI_CPU_TOLERANCE_FACTOR);
        assert!(ci.verdicts[3].passed, "2.5%/instance must pass at x1.5");
        assert!((ci.verdicts[3].effective_budget - 3.0).abs() < 1e-9);
        assert!((ci.verdicts[3].tolerance - 1.5).abs() < 1e-9);
        // The tolerance applies to the CPU gate ONLY.
        assert!((ci.verdicts[1].effective_budget - BUDGET_RSS_PER_INSTANCE_MIB).abs() < 1e-9);
        assert!((ci.verdicts[0].effective_budget - BUDGET_READ_P99_MS).abs() < 1e-9);
    }

    #[test]
    fn cpu_normalization_uses_the_running_count() {
        // 20% aggregate over 10 running = 2%/instance: exactly at the strict
        // budget (the gate is <=, so this passes); the same aggregate over
        // 5 running = 4%/instance fails it.
        let at_budget = SteadyFigures::new(20.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        assert!(evaluate_gates(5.0, &at_budget, &quiet_subscriber(), 1.0).verdicts[3].passed);
        let over = SteadyFigures::new(20.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 5);
        assert!(!evaluate_gates(5.0, &over, &quiet_subscriber(), 1.0).verdicts[3].passed);
    }

    #[test]
    fn ci_mode_parses_every_form() {
        assert!(ci_mode_from(Some("1"), None).unwrap());
        assert!(!ci_mode_from(Some("0"), None).unwrap());
        // Garbage is an ERROR (main exits 2) — never a silent mode pick.
        assert!(ci_mode_from(Some("garbage"), None).is_err());
        assert!(ci_mode_from(Some(""), None).is_err());
        // Unset: fall to the runner's CI env (GitHub sets "true").
        assert!(ci_mode_from(None, Some("true")).unwrap());
        assert!(ci_mode_from(None, Some("1")).unwrap());
        assert!(!ci_mode_from(None, None).unwrap());
        assert!(!ci_mode_from(None, Some("false")).unwrap());
        // An explicit value wins over the ambient one, both ways.
        assert!(!ci_mode_from(Some("0"), Some("true")).unwrap());
        assert!(ci_mode_from(Some("1"), Some("false")).unwrap());
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let mut samples: Vec<f64> = (1..=100).map(f64::from).collect();
        samples.sort_by(|a, b| a.total_cmp(b));
        assert!((percentile(&samples, 50.0) - 50.0).abs() < 1e-9);
        assert!((percentile(&samples, 95.0) - 95.0).abs() < 1e-9);
        assert!((percentile(&samples, 99.0) - 99.0).abs() < 1e-9);
        assert!((percentile(&samples, 100.0) - 100.0).abs() < 1e-9);
        assert!((percentile(&samples, 0.1) - 1.0).abs() < 1e-9);
        let mut two: Vec<f64> = vec![10.0, 20.0];
        two.sort_by(|a, b| a.total_cmp(b));
        assert!((percentile(&two, 50.0) - 10.0).abs() < 1e-9); // ceil(1) → idx 0
    }

    #[test]
    fn median_picks_the_middle_and_averages_even_counts() {
        let three = vec![1.0, 5.0, 9.0];
        assert!((median(&three) - 5.0).abs() < 1e-9);
        let four = vec![1.0, 5.0, 7.0, 9.0];
        assert!((median(&four) - 6.0).abs() < 1e-9);
        assert!(median(&[]).abs() < 1e-9);
    }

    #[test]
    fn summarize_reports_p50_p95_p99_and_max() {
        // 198 ones + 1500 + 1600 → nearest-rank p99 = 1500, max = 1600.
        let stats = kind_with_p99("probe", 1500.0, 200);
        assert_eq!(stats.name, "probe");
        assert_eq!(stats.samples, 200);
        assert!((stats.p99 - 1500.0).abs() < 1e-9);
        assert!((stats.max - 1600.0).abs() < 1e-9);
        assert!((stats.p50 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zero_running_count_fails_closed() {
        // A zero running count must never divide to a fake pass: the
        // normalization yields +inf, which fails every steady-state gate.
        let figures = SteadyFigures::new(0.0, 0.0, 0, 0);
        assert!(figures.cpu_per_instance_pct.is_infinite());
        let gates = evaluate_gates(5.0, &figures, &quiet_subscriber(), 1.0);
        assert!(!gates.passed, "zero running instances must fail closed");
    }

    #[test]
    fn the_fixture_consts_pin_the_nfr4_premise() {
        // NFR-4's premise is a 25-instance Fleet with 10 instances RUNNING —
        // the per-instance budgets mean MARGINAL overhead over exactly these
        // counts. Same precedent as the 7-2 schema pins: a budget premise is
        // a const pin with an assertion, not a comment.
        assert_eq!(FLEET_SIZE, 25);
        assert_eq!(RUNNING_COUNT, 10);
    }

    #[test]
    fn read_p99_gate_is_strict_at_the_budget_boundary() {
        // The read budget's prose is "< 1 s" — a STRICT inequality. A p99
        // EXACTLY at 1000.0 ms must FAIL (the Lt bound; a soft `<=` here
        // would pass a read that only just met the ceiling the budget
        // excludes); just under it passes. The RSS/CPU ceilings keep their
        // `≤` bounds (exactly-at-budget passes there — pinned by
        // cpu_normalization_uses_the_running_count).
        let at = evaluate_gates(1000.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert!(
            !at.verdicts[0].passed,
            "a p99 exactly at the 1000 ms budget must FAIL the strict < read gate: {at:?}"
        );
        assert_eq!(at.verdicts[0].bound, GateBound::Lt);
        assert_eq!(at.verdicts[1].bound, GateBound::Le);
        assert_eq!(at.verdicts[3].bound, GateBound::Le);
        let under = evaluate_gates(999.999, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert!(under.verdicts[0].passed, "just under the budget passes");
    }

    #[test]
    fn read_iterations_cap_is_loud_and_bounded() {
        // Under the cap: unchanged, no clamp. At the cap: allowed. One over:
        // clamped to the cap WITH the loud flag (main notes it on stderr).
        // Zero still clamps up to one (a zero-iteration run cannot
        // summarize) without claiming the cap was applied.
        assert_eq!(clamp_read_iterations(200), (200, false));
        assert_eq!(clamp_read_iterations(MAX_READ_ITERATIONS), (100_000, false));
        assert_eq!(
            clamp_read_iterations(MAX_READ_ITERATIONS + 1),
            (100_000, true)
        );
        assert_eq!(clamp_read_iterations(u64::MAX), (100_000, true));
        assert_eq!(clamp_read_iterations(0), (1, false));
    }

    #[test]
    fn the_default_schedules_fit_the_orphan_bound() {
        // CI: settle 2 s + (3 gated + 1 addendum)×12 s windows + 3×1 s gaps
        // + 5 s margin = 58 s, strictly inside the 60 s orphan bound.
        check_liveness_schedule(
            Duration::from_secs(2),
            CI_STEADY_WINDOWS + 1,
            Duration::from_secs(12),
        )
        .expect("the default CI schedule must fit the orphan bound");
        // Local: one gated window + the addendum window.
        check_liveness_schedule(Duration::from_secs(2), 1 + 1, Duration::from_secs(12))
            .expect("the default local schedule must fit the orphan bound");
    }

    #[test]
    fn a_schedule_that_outgrows_the_orphan_bound_is_rejected() {
        // 2 s settle + 2×30 s windows + 1 s gap + 5 s margin = 68 s > 60 s:
        // the idlers would self-exit mid-window, so the check must reject.
        let err = check_liveness_schedule(Duration::from_secs(2), 1 + 1, Duration::from_secs(30))
            .expect_err("a schedule past the orphan bound must be rejected");
        assert!(err.contains("linger=60000"), "names the bound: {err}");
    }

    #[test]
    fn the_serve_schedule_fits_the_raised_orphan_bound() {
        // CI: settle 2 s + (3 gated + 1 addendum)×12 s windows + 3×1 s gaps
        // + the 12 s serve window + the 2 s stop bound = 72 s, + the 5 s
        // margin = 77 s, strictly inside the raised 120 s serve bound.
        check_liveness_schedule_serve(
            Duration::from_secs(2),
            CI_STEADY_WINDOWS + 1,
            Duration::from_secs(12),
            SERVE_WINDOW,
            SERVE_STOP_WINDOW,
        )
        .expect("the serve CI schedule must fit the raised orphan bound");
        // Local: one gated window + the addendum window (46 s incl. margin).
        check_liveness_schedule_serve(
            Duration::from_secs(2),
            1 + 1,
            Duration::from_secs(12),
            SERVE_WINDOW,
            SERVE_STOP_WINDOW,
        )
        .expect("the serve local schedule must fit the raised orphan bound");
    }

    #[test]
    fn a_serve_schedule_that_outgrows_the_raised_bound_is_rejected() {
        // The same rejection math as the default arm, at the raised bound:
        // 2 s + 4×12 s + 3×1 s + a 60 s serve window + 2 s + 5 s margin =
        // exactly 120 s — and the bound must STRICTLY exceed the schedule.
        let err = check_liveness_schedule_serve(
            Duration::from_secs(2),
            CI_STEADY_WINDOWS + 1,
            Duration::from_secs(12),
            Duration::from_secs(60),
            SERVE_STOP_WINDOW,
        )
        .expect_err("a serve schedule past the raised bound must be rejected");
        assert!(err.contains("linger=120000"), "names the bound: {err}");
    }

    #[test]
    fn the_serve_scenario_consts_are_pinned() {
        // Story 17-1's fixture premise, pinned like the NFR-4 premise: a
        // change to the measured shape must consciously edit these
        // assertions (and docs/testing.md), never silently re-tune a gate.
        assert_eq!(SERVE_WINDOW, Duration::from_secs(12));
        assert_eq!(SERVE_STOP_WINDOW, Duration::from_secs(2));
        assert_eq!(SERVE_STOP_DELAY, Duration::from_secs(3));
        assert_eq!(SERVE_AGENT_LINGER_MS, 120_000);
        assert!(
            SERVE_AGENT_LINGER_MS > AGENT_LINGER_MS,
            "the serve bound RAISES the default orphan bound"
        );
        assert_eq!(SERVE_READER_TASKS, 4);
        assert_eq!(SERVE_DRIVER_WORKERS, 4);
        // The load model is DERIVED from the default phase's read-demand
        // premise, not a free knob: 200 iterations over the 12 s window =
        // 60 ms between a reader's iterations.
        assert_eq!(
            serve_reader_pace(Duration::from_secs(12), 200),
            Duration::from_millis(60)
        );
        // Degenerate demand still yields a real (max) pace — never a divide
        // by zero.
        assert_eq!(
            serve_reader_pace(Duration::from_secs(12), 0),
            Duration::from_secs(12)
        );
        assert_eq!(SERVE_PATHOLOGICAL_NAME, "serve-pathological");
        assert_eq!(SERVE_PATHOLOGICAL_KIND, "servepathological");
        // The per-read retry shape (review round 3): bounded, counted, and
        // only REPEATED consecutive failures abort — pinned so a silent
        // re-tune cannot move the reader's failure posture.
        assert_eq!(SERVE_READ_RETRIES, 2);
        assert_eq!(SERVE_READ_RETRY_BACKOFF, Duration::from_millis(100));
        assert_eq!(SERVE_MAX_CONSECUTIVE_FAILURES, 3);
        assert_ne!(
            SERVE_PATHOLOGICAL_KIND, MANIFEST_KIND,
            "the pathological fixture is a distinct flow-local identity"
        );
    }

    #[test]
    fn merge_kind_samples_concatenates_every_readers_vector() {
        // Pure concatenation (the sorted percentile math makes order across
        // readers irrelevant); an empty part contributes nothing; no parts
        // at all is empty — a lost vector is a length mismatch, not a
        // silently short summary.
        assert_eq!(
            merge_kind_samples(&[vec![1.0, 2.0], vec![], vec![3.0]]),
            vec![1.0, 2.0, 3.0]
        );
        assert!(merge_kind_samples(&[]).is_empty());
    }

    #[test]
    fn the_serve_contention_proof_names_a_vacuous_window() {
        // Review round 3: zero reads started inside the stop's window means
        // idle supervision was measured, not the lock-contention term —
        // the named error fires (main maps it to exit 2 BEFORE any gate
        // verdict), and any positive count passes.
        let err = serve_contention_exercised(0).expect_err("zero overlaps must fail");
        assert!(err.contains("contention NOT exercised"), "{err}");
        assert!(err.contains("never overlapped any in-flight read"), "{err}");
        serve_contention_exercised(1).expect("one overlapping read proves contention");
        serve_contention_exercised(1_000).expect("many do too");
    }

    #[test]
    fn the_serve_sampler_window_is_clamped_to_the_load_window() {
        // Review round 3: a raised PERF_BUDGETS_WINDOW_MS must not sample
        // past the 12 s load into idle tail (dilution toward passing); the
        // clamp keeps the sampler at the load window, and a smaller
        // configured window is left alone (the env's own 10 s floor).
        assert_eq!(
            serve_sampling_window(Duration::from_secs(12)),
            Duration::from_secs(12)
        );
        assert_eq!(
            serve_sampling_window(Duration::from_secs(3_600)),
            SERVE_WINDOW,
            "a raised window clamps DOWN to the load window"
        );
        assert_eq!(
            serve_sampling_window(Duration::from_secs(10)),
            Duration::from_secs(10),
            "a smaller window is not raised"
        );
    }

    #[test]
    fn the_serve_running_count_schedule_is_pinned_and_conservative() {
        // Review round 3: the full count until the bounded stop COMPLETES
        // (3 s delay + 2 s window), one fewer after — and the normalization
        // divisor is the MINIMUM over the window (the post-stop count), the
        // conservative bound: dividing by the pre-window count would
        // understate per-instance cost after the stop (lenient — wrong for
        // a gate).
        assert_eq!(serve_running_at(11, Duration::ZERO), 11);
        assert_eq!(
            serve_running_at(
                11,
                SERVE_STOP_DELAY + SERVE_STOP_WINDOW - Duration::from_millis(1)
            ),
            11
        );
        assert_eq!(
            serve_running_at(11, SERVE_STOP_DELAY + SERVE_STOP_WINDOW),
            10
        );
        assert_eq!(serve_running_at(11, SERVE_WINDOW), 10);
        assert_eq!(serve_effective_running_instances(11), 10);
        // Saturating: a degenerate pre-stop count never underflows.
        assert_eq!(serve_effective_running_instances(0), 0);
    }

    #[test]
    fn serve_gates_bind_only_on_the_ratified_gate_platforms() {
        // Review round 3, the standing gate-platform rule: Linux and macOS
        // are ratified gate platforms (CI-tolerated and strict-local
        // respectively); Windows is measured-and-reported ONLY (sysinfo's
        // `memory()` is the working-set-size analog there) — its serve
        // verdicts are report-only and never gate the run.
        assert!(serve_gates_bind("linux"));
        assert!(serve_gates_bind("macos"));
        assert!(!serve_gates_bind("windows"), "windows never gates");
        assert!(
            !serve_gates_bind("freebsd"),
            "unratified platforms never gate"
        );
    }

    #[test]
    fn the_serve_gated_set_is_mode_aware_and_never_leaks_into_the_default() {
        // Review round 3: (a) the DEFAULT gated set never carries a serve
        // name — a default run's baseline contract is unchanged; (b) a
        // serve run's coverage check expects the default set PLUS the serve
        // metrics — a baseline missing them is the existing LOUD named
        // exit-2 error shape (`missing`, non-empty), never a silent pass;
        // (c) against a SERVE-CARRYING baseline, the serve gates ride the
        // ratchet: within-window drift passes, drift past the x1.15 window
        // is a Beyond verdict that fails a gated (CI) run.
        for name in SERVE_GATED_METRICS.iter() {
            assert!(
                !GATED_METRICS.contains(name),
                "{name} leaked into the default gated set"
            );
        }
        // The mode-aware expected set, exactly as main builds it.
        let expected_serve: Vec<&str> = GATED_METRICS
            .iter()
            .chain(SERVE_GATED_METRICS.iter())
            .copied()
            .collect();
        assert_eq!(expected_serve.len(), 10);
        // (b) a default-shaped baseline lacks all three serve metrics.
        let figures = parse_baseline(&baseline_json_with(&SEVEN), "b.json").unwrap();
        let (missing, extra) = baseline_coverage_gaps(&figures, &expected_serve);
        assert!(extra.is_empty());
        assert_eq!(
            missing,
            SERVE_GATED_METRICS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "a serve run's coverage check hard-fails a serve-less baseline"
        );
        // (c) a serve-carrying baseline: all ten gates trend.
        let serve_measured = [5.0, 1.5, 0.8];
        let mut baseline =
            serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN)).unwrap();
        for (name, measured) in SERVE_GATED_METRICS.iter().zip(serve_measured) {
            baseline["gates"]
                .as_array_mut()
                .expect("gates array")
                .push(serde_json::json!({ "name": name, "measured": measured }));
        }
        let figures = parse_baseline(&baseline.to_string(), "serve-b.json").unwrap();
        let mut trend_gates = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        // The default seven's measureds ARE the baseline's (the same trick
        // trend_for uses — only the serve figures drift, within window).
        for (verdict, value) in trend_gates.verdicts.iter_mut().zip(SEVEN) {
            verdict.measured = value;
        }
        let mut serve_gates = evaluate_serve_gates(5.7, &healthy_steady(), 1.0);
        serve_gates.verdicts[1].measured = 1.7; // CPU: 1.5 -> 1.7 (x1.133)
        serve_gates.verdicts[2].measured = 0.9; // RSS: 0.8 -> 0.9 (x1.125)
        trend_gates
            .verdicts
            .extend(serve_gates.verdicts.iter().cloned());
        let trend = evaluate_trend(&trend_gates, &figures, "serve-b.json", true).unwrap();
        assert_eq!(trend.entries.len(), 10);
        for entry in &trend.entries[7..] {
            assert_eq!(entry.verdict, RatchetVerdict::Within, "{entry:?}");
        }
        assert!(trend.passed, "{trend:?}");
        assert!(
            !trend.fails_run(),
            "a within-window serve drift passes a gated run"
        );
        // A serve figure past the x1.15 window (5.0 -> 6.0 > 5.75) is
        // Beyond and fails the gated ratchet — the serve teeth are real.
        let mut trend_gates = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        let drifted = evaluate_serve_gates(6.0, &healthy_steady(), 1.0);
        trend_gates
            .verdicts
            .extend(drifted.verdicts.iter().cloned());
        let trend = evaluate_trend(&trend_gates, &figures, "serve-b.json", true).unwrap();
        assert_eq!(trend.entries[7].verdict, RatchetVerdict::Beyond);
        assert!(trend.fails_run(), "a drifted serve metric reds a gated run");
    }

    #[test]
    fn a_serve_write_baseline_merges_the_serve_gates_into_the_gates_array() {
        // Review round 3: the loader (`load_baseline`) reads exactly the
        // top-level gates array — so a serve `--write-baseline` must merge
        // the serve verdicts into it, or the seeded serve-carrying baseline
        // the docs describe could never load. A default report (no
        // serve_shape block) is byte-shaped as before.
        let mut report = serde_json::json!({ "status": "pass", "schema_version": REPORT_SCHEMA_VERSION,
            "environment": { "git_sha": "deadbee", "os": "linux" },
            "gates": [
            { "name": "read_p99_lt_1s", "measured": 5.0 }
        ]});
        report["serve_shape"] = serde_json::json!({ "gates": [
            { "name": "serve_read_p99_lt_1s", "measured": 5.7 },
            { "name": "serve_cpu_per_instance_le_2pct", "measured": 1.7 },
            { "name": "serve_rss_per_instance_le_50mib", "measured": 0.9 }
        ]});
        let clean = baseline_report_json(&report, baseline_provenance(true, false, "a"));
        let gates = clean["gates"].as_array().expect("gates array");
        assert_eq!(gates.len(), 4, "the serve gates merged in");
        assert_eq!(
            gates[3]["name"],
            serde_json::json!("serve_rss_per_instance_le_50mib")
        );
        // And the merged file PARSES into figures the loader resolves — the
        // serve names load (this is what a seeded serve-carrying baseline is).
        let figures = parse_baseline(&clean.to_string(), "seeded.json").unwrap();
        for name in SERVE_GATED_METRICS {
            assert!(figures.lookup(name).is_some(), "{name} must load");
        }
        // A default-shaped report gains nothing (no serve block, no merge).
        let plain = serde_json::json!({ "status": "pass", "gates": [
            { "name": "read_p99_lt_1s", "measured": 5.0 }
        ]});
        let clean = baseline_report_json(&plain, baseline_provenance(true, false, "a"));
        assert_eq!(clean["gates"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn pathological_fixture_branch_matches_the_running_os_and_bound() {
        // Review round 3: the runtime OS branch (no #[cfg] — the conformance
        // crate forbids it; every platform pins its OWN arm here) and the
        // WRITTEN adapter.toml carries the argv shape that arm promises —
        // including the Unix self-exit bound DERIVED from the serve orphan
        // bound (the fixture must never self-exit inside the schedule it is
        // validated against).
        let (fixture, note) = pathological_fixture();
        let dir = tempfile::TempDir::new().expect("temp manifest dir");
        let manifest_dir = fixture.write(dir.path());
        let written =
            std::fs::read_to_string(manifest_dir.join("adapter.toml")).expect("read adapter.toml");
        if std::env::consts::OS == "windows" {
            assert!(note.starts_with("windows:"), "{note}");
            assert!(note.contains("TerminateJobObject"), "{note}");
            assert!(
                written.contains(&format!("--linger-ms={SERVE_AGENT_LINGER_MS}")),
                "the Windows arm's self-exit bound is the serve bound: {written}"
            );
        } else {
            assert!(note.starts_with("unix:"), "{note}");
            assert!(note.contains("SIGTERM"), "{note}");
            assert!(
                written.contains(&format!(
                    "trap '' TERM; sleep {}",
                    SERVE_AGENT_LINGER_MS / 1000
                )),
                "the Unix arm's self-exit bound is the serve bound (derived, not the \
                 old fixed 60): {written}"
            );
            assert!(
                written.contains("exec = \"sh\""),
                "the Unix arm execs sh (the only Unix shape whose stop is genuinely \
                 slow): {written}"
            );
        }
    }

    #[test]
    fn the_serve_gate_names_and_order_are_pinned() {
        // Story 17-1: exactly THREE serve gates, their OWN names, bound only
        // on serve runs — the default seven's meaning never moves.
        let gates = evaluate_serve_gates(5.0, &healthy_steady(), 1.0);
        assert!(gates.passed, "healthy figures must pass: {gates:?}");
        let names: Vec<_> = gates.verdicts.iter().map(|v| v.name).collect();
        assert_eq!(
            names,
            [
                "serve_read_p99_lt_1s",
                "serve_cpu_per_instance_le_2pct",
                "serve_rss_per_instance_le_50mib",
            ]
        );
    }

    #[test]
    fn serve_read_p99_gate_is_strict_at_the_budget_boundary() {
        // p99 EXACTLY at 1000 ms fails the strict-< gate; just under passes.
        let at = evaluate_serve_gates(BUDGET_READ_P99_MS, &healthy_steady(), 1.0);
        assert!(!at.passed);
        assert!(
            !at.verdicts[0].passed,
            "strict <: the boundary itself fails"
        );
        let under = evaluate_serve_gates(BUDGET_READ_P99_MS - 0.001, &healthy_steady(), 1.0);
        assert!(under.verdicts[0].passed, "{:?}", under.verdicts[0]);
    }

    #[test]
    fn serve_cpu_gate_takes_the_ci_tolerance_like_the_default() {
        // 2.9 %/instance (aggregate 29 over 10 running): over budget
        // strictly (x1.0), inside at the shared-runner tolerance (x1.5 -> 3.0).
        let cpu = SteadyFigures::new(29.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let local = evaluate_serve_gates(5.0, &cpu, 1.0);
        assert!(!local.passed, "2.9 > 2.0 strictly: {local:?}");
        assert!(!local.verdicts[1].passed);
        let ci = evaluate_serve_gates(5.0, &cpu, CI_CPU_TOLERANCE_FACTOR);
        assert!(ci.passed, "2.9 <= 3.0 with the tolerance: {ci:?}");
    }

    #[test]
    fn serve_rss_gate_and_fail_closed_running_count() {
        // 60 MiB/instance (600 MiB aggregate over 10 running) > 50: fails
        // the RSS gate with reads and CPU healthy.
        let rss = SteadyFigures::new(10.0, 600.0 * 1024.0 * 1024.0, 600 * 1024 * 1024, 10);
        let gates = evaluate_serve_gates(5.0, &rss, 1.0);
        assert!(!gates.passed, "60 MiB/instance > 50: {gates:?}");
        assert!(!gates.verdicts[2].passed);
        // A zero Running count fails closed exactly like the default steady
        // gates (per_instance's +inf), never divides away to a fake 0.
        let zero = SteadyFigures::new(10.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 0);
        let gates = evaluate_serve_gates(5.0, &zero, 1.0);
        assert!(!gates.passed, "zero running fails closed: {gates:?}");
    }

    #[test]
    fn serve_shape_flag_parses_as_a_bare_boolean() {
        let argv: Vec<String> = ["--serve-shape"].iter().map(|s| s.to_string()).collect();
        assert!(parse_cli_args(&argv).unwrap().serve_shape);
        // Alongside the value flags, in both value forms:
        let argv: Vec<String> = [
            "--baseline",
            "b.json",
            "--serve-shape",
            "--write-baseline=w.json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_cli_args(&argv).unwrap();
        assert!(parsed.serve_shape);
        assert_eq!(parsed.baseline.as_deref(), Some("b.json"));
        assert_eq!(parsed.write_baseline.as_deref(), Some("w.json"));
    }

    #[test]
    fn serve_shape_value_and_duplicate_forms_are_named_errors() {
        // The `=`-carrying form is invalid usage OUTRIGHT — a named error
        // + exit 2 at main, never a silently-ignored value.
        let err = bad_args(&["--serve-shape=bogus"]);
        assert!(err.contains("--serve-shape takes no value"), "{err}");
        assert!(err.contains("bogus"), "names the rejected value: {err}");
        let err = bad_args(&["--serve-shape", "--serve-shape"]);
        assert!(err.contains("more than once"), "{err}");
        // The unknown-argument error names --serve-shape as supported.
        let err = bad_args(&["--nonsense"]);
        assert!(err.contains("--serve-shape"), "{err}");
        // The usage text carries the flag.
        assert!(
            usage_text().contains("--serve-shape"),
            "{usage}",
            usage = usage_text()
        );
    }

    #[test]
    fn subscriber_overhead_reports_deltas_against_the_baseline() {
        // The addendum's whole point: WITH-subscriber figures plus their
        // deltas vs the unsubscribed baseline — signed, so an improvement
        // reports negative and an overhead reports positive.
        let baseline = SteadyFigures::new(10.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let with_sub = SteadyFigures::new(12.0, 120.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let overhead = SubscriberOverhead::new(&baseline, 5.0, &with_sub, 7.5);
        assert!((overhead.cpu_per_instance_pct - 1.2).abs() < 1e-9);
        assert!((overhead.rss_per_instance_mib_mean - 12.0).abs() < 1e-9);
        assert!((overhead.read_p99_overall_ms - 7.5).abs() < 1e-9);
        assert!((overhead.cpu_delta_pct_points - 0.2).abs() < 1e-9);
        assert!((overhead.rss_delta_mib - 2.0).abs() < 1e-9);
        assert!((overhead.read_p99_delta_ms - 2.5).abs() < 1e-9);
        // A negative delta (subscriber costs nothing here) stays negative —
        // the sign is information, never abs()-ed away.
        let cheaper = SubscriberOverhead::new(&baseline, 5.0, &baseline, 4.0);
        assert!(cheaper.read_p99_delta_ms < 0.0);
    }

    #[test]
    fn the_ratified_subscriber_budgets_are_pinned() {
        // The story-10-3 ratification, pinned like the NFR-4 premise: a
        // budget change must consciously edit these assertions (and update
        // docs/testing.md's budget table with the new observations), never
        // silently tweak a gate.
        assert_eq!(BUDGET_SUBSCRIBER_CPU_DELTA_PCT_POINTS, 0.25);
        assert_eq!(BUDGET_SUBSCRIBER_RSS_DELTA_MIB, 2.0);
        assert_eq!(BUDGET_SUBSCRIBER_READ_P99_DELTA_MS, 100.0);
    }

    #[test]
    fn subscriber_deltas_within_budget_pass_and_negative_deltas_pass_trivially() {
        // Zero deltas (a quiet addendum) leave the whole report green — the
        // three subscriber gates ride indices 4-6, appended after the four
        // zero-subscriber gates whose meaning never moves.
        let gates = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert!(gates.passed, "zero deltas must pass: {gates:?}");
        assert_eq!(
            gates.verdicts.len(),
            7,
            "four NFR-4 gates + three subscriber gates"
        );
        assert_eq!(gates.verdicts[4].name, "subscriber_cpu_delta_le_budget");
        assert_eq!(gates.verdicts[5].name, "subscriber_rss_delta_le_budget");
        assert_eq!(
            gates.verdicts[6].name,
            "subscriber_read_p99_delta_le_budget"
        );
        // A NEGATIVE delta (the subscriber pass measured cheaper — pure
        // noise) passes trivially: the budget ceilings the OVERHEAD, never
        // the measurement's sign.
        let negative = SubscriberOverhead::new(&healthy_steady(), 5.0, &healthy_steady(), 4.0);
        assert!(negative.read_p99_delta_ms < 0.0);
        let gates = evaluate_gates(5.0, &healthy_steady(), &negative, 1.0);
        assert!(
            gates.passed,
            "a cheaper subscriber pass must pass: {gates:?}"
        );
    }

    #[test]
    fn subscriber_deltas_over_the_ratified_budgets_fail() {
        // Each delta driven far over its strict budget fails ITS gate — and
        // the whole run — with every other figure healthy. Budgets: 0.25
        // pct-points CPU Δ, 2 MiB RSS Δ, 100 ms read-p99 Δ.
        let cpu = SteadyFigures::new(1_010.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let subscriber = SubscriberOverhead::new(&healthy_steady(), 5.0, &cpu, 5.0);
        let gates = evaluate_gates(5.0, &healthy_steady(), &subscriber, 1.0);
        assert!(!gates.passed, "a huge CPU delta must fail the run");
        assert!(!gates.verdicts[4].passed, "the CPU delta gate: {gates:?}");

        let rss = SteadyFigures::new(10.0, 2_100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let subscriber = SubscriberOverhead::new(&healthy_steady(), 5.0, &rss, 5.0);
        let gates = evaluate_gates(5.0, &healthy_steady(), &subscriber, 1.0);
        assert!(!gates.passed, "a huge RSS delta must fail the run");
        assert!(!gates.verdicts[5].passed, "the RSS delta gate: {gates:?}");

        let subscriber =
            SubscriberOverhead::new(&healthy_steady(), 5.0, &healthy_steady(), 5_000.0);
        let gates = evaluate_gates(5.0, &healthy_steady(), &subscriber, 1.0);
        assert!(!gates.passed, "a huge read-p99 delta must fail the run");
        assert!(
            !gates.verdicts[6].passed,
            "the read-p99 delta gate: {gates:?}"
        );
    }

    #[test]
    fn subscriber_gates_take_the_ci_tolerance_like_the_cpu_gate() {
        // The policy shape: strict ×1.0 locally, ×1.5 on CI — a delta between
        // the strict budget and the tolerated budget fails locally and passes
        // on CI. CPU Δ 0.3 pct-points: over the strict 0.25, under 0.25 × 1.5
        // = 0.375.
        let with_cpu = SteadyFigures::new(13.0, 100.0 * 1024.0 * 1024.0, 150_000_000, 10);
        let subscriber = SubscriberOverhead::new(&healthy_steady(), 5.0, &with_cpu, 5.0);
        assert!((subscriber.cpu_delta_pct_points - 0.3).abs() < 1e-9);
        let local = evaluate_gates(5.0, &healthy_steady(), &subscriber, 1.0);
        assert!(
            !local.verdicts[4].passed,
            "0.3 pct-points must fail the strict 0.25 budget: {:?}",
            local.verdicts[4]
        );
        assert!(!local.passed, "the strict failure fails the whole run");
        let ci = evaluate_gates(5.0, &healthy_steady(), &subscriber, CI_CPU_TOLERANCE_FACTOR);
        assert!(
            ci.verdicts[4].passed,
            "0.3 pct-points must pass at x1.5 (0.375): {:?}",
            ci.verdicts[4]
        );
        assert!((ci.verdicts[4].effective_budget - 0.375).abs() < 1e-9);
        assert!((ci.verdicts[5].effective_budget - 3.0).abs() < 1e-9);
        assert!((ci.verdicts[6].effective_budget - 150.0).abs() < 1e-9);
        // The tolerance widens ONLY the CPU + subscriber delta gates — reads
        // and RSS keep gating at strict budget.
        assert!((ci.verdicts[0].effective_budget - BUDGET_READ_P99_MS).abs() < 1e-9);
        assert!((ci.verdicts[1].effective_budget - BUDGET_RSS_PER_INSTANCE_MIB).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // Story 15-5: the ratchet + the baseline load error arms
    // -----------------------------------------------------------------------

    /// A syntactically valid baseline covering the seven gated metrics with
    /// the given measured values, zipped onto the canonical gate order (the
    /// order `evaluate_gates` emits — pinned by the tests above). Review
    /// round 1: the lengths are ASSERTED — a short slice must fail loudly
    /// here, not silently zip-truncate into a baseline that lacks a gate
    /// (the one test that WANTS a short baseline builds it explicitly).
    fn baseline_json_with(measured: &[f64]) -> String {
        let reference = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert_eq!(
            measured.len(),
            reference.verdicts.len(),
            "baseline_json_with: one measured value per gated metric ({}), got {}",
            reference.verdicts.len(),
            measured.len()
        );
        let gates: Vec<serde_json::Value> = reference
            .verdicts
            .iter()
            .zip(measured)
            .map(|(verdict, value)| serde_json::json!({ "name": verdict.name, "measured": value }))
            .collect();
        serde_json::json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "environment": { "git_sha": "deadbee", "os": "linux" },
            "provenance": {
                "seed": true,
                "generated_on": "local-ci-mode",
                "git_sha": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            },
            "gates": gates,
        })
        .to_string()
    }

    /// Parse a baseline and evaluate the trend against a report whose gated
    /// measured values are `current` (same trick: zipped onto the canonical
    /// gate order via `evaluate_gates` inputs chosen to produce them).
    /// Lengths are ASSERTED (review round 1) — no silent zip truncation.
    fn trend_for(baseline_text: &str, current: &[f64], gated: bool) -> Result<TrendBlock, String> {
        let figures = parse_baseline(baseline_text, "test-baseline.json")?;
        // Rebuild a GateReport whose measured values ARE `current` by
        // evaluating the reference gates and overwriting each measured
        // figure (the ratchet reads only name + measured).
        let mut gates = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert_eq!(
            current.len(),
            gates.verdicts.len(),
            "trend_for: one current value per gated metric ({}), got {}",
            gates.verdicts.len(),
            current.len()
        );
        for (verdict, value) in gates.verdicts.iter_mut().zip(current) {
            verdict.measured = *value;
        }
        evaluate_trend(&gates, &figures, "test-baseline.json", gated)
    }

    /// The seven reference figures: read p99, RSS mean, RSS max, CPU, then
    /// the three subscriber deltas — all meaningfully positive so every
    /// metric exercises the real ×1.15 arithmetic.
    const SEVEN: [f64; 7] = [5.0, 10.0, 20.0, 1.0, 0.1, 0.5, 10.0];

    #[test]
    fn cli_args_parse_both_flags_in_both_value_forms() {
        let argv: Vec<String> = ["--baseline", "b.json", "--write-baseline", "w.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_cli_args(&argv).unwrap(),
            CliArgs {
                baseline: Some("b.json".into()),
                write_baseline: Some("w.json".into()),
                serve_shape: false,
                help: false,
            }
        );
        let argv: Vec<String> = ["--write-baseline=w.json", "--baseline=b.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_cli_args(&argv).unwrap(),
            CliArgs {
                baseline: Some("b.json".into()),
                write_baseline: Some("w.json".into()),
                serve_shape: false,
                help: false,
            }
        );
        assert_eq!(parse_cli_args(&[]).unwrap(), CliArgs::default());
        // Each flag alone.
        let argv: Vec<String> = ["--baseline", "only.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_cli_args(&argv).unwrap(),
            CliArgs {
                baseline: Some("only.json".into()),
                write_baseline: None,
                serve_shape: false,
                help: false,
            }
        );
    }

    #[test]
    fn cli_args_reject_unknown_missing_duplicate_and_empty_values() {
        let bad = |argv: &[&str]| {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            parse_cli_args(&owned).expect_err("must be rejected")
        };
        let err = bad(&["--nonsense"]);
        assert!(err.contains("unknown argument"), "{err}");
        let err = bad(&["--baseline"]);
        assert!(err.contains("--baseline requires a file path"), "{err}");
        let err = bad(&["--baseline=", "--write-baseline", "w.json"]);
        assert!(err.contains("non-empty file path"), "{err}");
        let err = bad(&["--baseline", "a.json", "--baseline", "b.json"]);
        assert!(err.contains("more than once"), "{err}");
        let err = bad(&["b.json"]);
        assert!(err.contains("unknown argument"), "{err}");
    }

    #[test]
    fn cli_args_help_flag_and_parser_guards() {
        // --help / -h set the flag (main prints usage to stdout, exit 0) —
        // alone or alongside other flags.
        for argv in [
            vec!["--help"],
            vec!["-h"],
            vec!["--baseline", "b.json", "-h"],
        ] {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(parse_cli_args(&owned).unwrap().help, "{argv:?}");
        }
        // A flag's VALUE must not look like another flag: the classic
        // `--baseline --write-baseline` must NOT silently eat the second
        // flag as a path.
        let err = parse_cli_args(
            &["--baseline", "--write-baseline"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect_err("a flag-like value must be rejected");
        assert!(err.contains("looks like another flag"), "{err}");
        let err = parse_cli_args(&["--baseline=--x.json".to_string()])
            .expect_err("an inline flag-like value must be rejected");
        assert!(err.contains("looks like another flag"), "{err}");
        // A bare token containing '=' (no leading --) is an unknown
        // argument outright — never a split-on-= attempt.
        let err = bad_args(&["path.json=else"]);
        assert!(err.contains("unknown argument"), "{err}");
        assert!(err.contains("path.json=else"), "names the raw token: {err}");
        // The usage text carries the flags, the defaults, and the update
        // procedure pointer.
        let usage = usage_text();
        assert!(usage.contains("--baseline <file>"), "{usage}");
        assert!(usage.contains("--write-baseline <file>"), "{usage}");
        assert!(usage.contains("PERF_BUDGETS_CI"), "{usage}");
        assert!(usage.contains("docs/testing.md"), "{usage}");
    }

    /// Shared tiny helper: parse argv strings and expect an error.
    fn bad_args(argv: &[&str]) -> String {
        let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        parse_cli_args(&owned).expect_err("must be rejected")
    }

    #[test]
    fn missing_baseline_file_is_a_loud_named_error() {
        // A path that cannot exist (never created); the error must NAME it.
        let missing = std::env::temp_dir().join("perf-budgets-15-5-absent-baseline.json");
        let err = load_baseline(&missing).expect_err("a missing baseline must fail loudly");
        assert!(
            err.contains(&missing.display().to_string()),
            "the error must name the file: {err}"
        );
        assert!(err.contains("cannot read the baseline file"), "{err}");
    }

    #[test]
    fn corrupt_and_schema_mismatched_baselines_are_named_errors() {
        let err = parse_baseline("{ not json", "corrupt.json").expect_err("must fail");
        assert!(
            err.contains("corrupt.json") && err.contains("not valid JSON"),
            "{err}"
        );
        let err = parse_baseline("[1,2]", "corrupt.json").expect_err("must fail");
        assert!(err.contains("corrupt"), "{err}");
        // Schema mismatch (a future breaking shape bumps the version and old
        // baselines must be refused BY NAME, never mis-read).
        let future = baseline_json_with(&SEVEN).replace(
            &format!("\"schema_version\":{REPORT_SCHEMA_VERSION}"),
            "\"schema_version\":2",
        );
        let err = parse_baseline(&future, "future.json").expect_err("must fail");
        assert!(
            err.contains("schema mismatch") && err.contains("future.json"),
            "{err}"
        );
        // Missing schema_version, missing gates, empty gates, gate without a
        // numeric measured, duplicate gate — every arm named.
        let err = parse_baseline("{}", "empty.json").expect_err("must fail");
        assert!(err.contains("schema_version"), "{err}");
        let no_gates = serde_json::json!({ "schema_version": REPORT_SCHEMA_VERSION }).to_string();
        let err = parse_baseline(&no_gates, "no-gates.json").expect_err("must fail");
        assert!(err.contains("gates array is missing"), "{err}");
        let empty_gates = serde_json::json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "gates": [],
        })
        .to_string();
        let err = parse_baseline(&empty_gates, "empty-gates.json").expect_err("must fail");
        assert!(err.contains("gates array is empty"), "{err}");
        let unmeasured = serde_json::json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "gates": [{ "name": "read_p99_lt_1s" }],
        })
        .to_string();
        let err = parse_baseline(&unmeasured, "unmeasured.json").expect_err("must fail");
        assert!(err.contains("lacks a numeric measured value"), "{err}");
        let duplicated = serde_json::json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "gates": [
                { "name": "read_p99_lt_1s", "measured": 5.0 },
                { "name": "read_p99_lt_1s", "measured": 6.0 },
            ],
        })
        .to_string();
        let err = parse_baseline(&duplicated, "dup.json").expect_err("must fail");
        assert!(err.contains("appears twice"), "{err}");
        // A stale nested trend key is IGNORED (only the gated figures are
        // load-bearing) — a refresh committed before the strip rule loads.
        let mut stale = serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN))
            .expect("valid json");
        stale["trend"] = serde_json::json!({ "stale": true });
        assert!(parse_baseline(&stale.to_string(), "stale.json").is_ok());
    }

    #[test]
    fn baseline_parse_extracts_figures_and_provenance() {
        let figures = parse_baseline(&baseline_json_with(&SEVEN), "ok.json").expect("must parse");
        assert_eq!(figures.schema_version, REPORT_SCHEMA_VERSION);
        // The stamped provenance block WINS over the environment block's
        // older short form, and carries the seed/generated_on pair.
        assert_eq!(
            figures.git_sha.as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
        assert_eq!(figures.os.as_deref(), Some("linux"));
        assert_eq!(figures.provenance_seed, Some(true));
        assert_eq!(
            figures.provenance_generated_on.as_deref(),
            Some("local-ci-mode")
        );
        assert_eq!(figures.lookup("read_p99_lt_1s"), Some(5.0));
        assert_eq!(figures.lookup("cpu_per_instance_le_2pct"), Some(1.0));
        assert_eq!(
            figures.lookup("subscriber_read_p99_delta_le_budget"),
            Some(10.0)
        );
        assert_eq!(figures.lookup("no_such_gate"), None);
        assert_eq!(figures.measured_by_gate.len(), 7);
        // A pre-provenance baseline (the original 15-5 seed shape) still
        // loads: the block is best-effort, environment.git_sha is the
        // fallback, and the provenance arms read as absent — never as a
        // mismatch.
        let mut legacy = serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN))
            .expect("valid json");
        legacy.as_object_mut().expect("object").remove("provenance");
        let figures = parse_baseline(&legacy.to_string(), "legacy.json").expect("must parse");
        assert_eq!(figures.git_sha.as_deref(), Some("deadbee"));
        assert_eq!(figures.provenance_seed, None);
        assert_eq!(figures.provenance_generated_on, None);
    }

    #[test]
    fn ratchet_improvements_pass_freely() {
        let improved: [f64; 7] = [4.0, 9.0, 18.0, 0.5, 0.05, 0.4, 9.0];
        let trend = trend_for(&baseline_json_with(&SEVEN), &improved, true).unwrap();
        assert!(trend.passed && trend.gated && !trend.fails_run());
        assert!(trend
            .entries
            .iter()
            .all(|e| e.verdict == RatchetVerdict::Improved));
        let entry = &trend.entries[0];
        assert!((entry.delta - (-1.0)).abs() < 1e-9);
        assert!((entry.delta_pct.unwrap() - (-20.0)).abs() < 1e-9);
        assert!((entry.limit.unwrap() - 5.0 * RATCHET_TOLERANCE).abs() < 1e-9);
        assert_eq!(trend.verdict_str(), "pass");
    }

    #[test]
    fn ratchet_within_tolerance_passes_and_the_boundary_is_inclusive() {
        // +10% on every metric — inside 1.15.
        let drifted: [f64; 7] = SEVEN.map(|value| value * 1.10);
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, true).unwrap();
        assert!(trend.passed && !trend.fails_run(), "{trend:?}");
        assert!(trend
            .entries
            .iter()
            .all(|e| e.verdict == RatchetVerdict::Within));
        // EXACTLY at baseline × 1.15 is still within: the rule is "no WORSE
        // than ×1.15" (the ratchet's own bound discipline — the read gate's
        // strict < does not apply here).
        let at_limit: [f64; 7] = SEVEN.map(|value| value * RATCHET_TOLERANCE);
        let trend = trend_for(&baseline_json_with(&SEVEN), &at_limit, true).unwrap();
        assert!(trend.passed, "exactly-at-limit must be within: {trend:?}");
        assert!(trend
            .entries
            .iter()
            .all(|e| e.verdict == RatchetVerdict::Within));
    }

    #[test]
    fn ratchet_beyond_tolerance_fails_naming_all_four_figures() {
        // +20% on every metric: the four ×1.15-dominated metrics (read p99,
        // RSS mean/max, CPU — baselines well above their floors) go Beyond
        // and the gated run fails, while the three subscriber deltas land
        // INSIDE their noise floors (0.1→limit 0.15, 0.5→0.75, 10→35 — the
        // floor engaging, exactly its purpose). Each entry carries the
        // baseline, the current, the delta, and the tolerance's limit (the
        // four figures every verdict must name).
        let drifted: [f64; 7] = SEVEN.map(|value| value * 1.20);
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, true).unwrap();
        assert!(!trend.passed && trend.fails_run(), "{trend:?}");
        assert_eq!(trend.verdict_str(), "fail");
        for index in 0..4 {
            assert_eq!(
                trend.entries[index].verdict,
                RatchetVerdict::Beyond,
                "entry {index}: {:?}",
                trend.entries[index]
            );
        }
        for index in 4..7 {
            assert_eq!(
                trend.entries[index].verdict,
                RatchetVerdict::Within,
                "the subscriber deltas ride their noise floors: {:?}",
                trend.entries[index]
            );
        }
        let entry = &trend.entries[0];
        assert!((entry.baseline - 5.0).abs() < 1e-9);
        assert!((entry.current - 6.0).abs() < 1e-9);
        assert!((entry.delta - 1.0).abs() < 1e-9);
        assert!(
            (entry.limit.unwrap() - 5.75).abs() < 1e-9,
            "limit = max(baseline x 1.15, baseline + floor)"
        );
        assert!((entry.delta_pct.unwrap() - 20.0).abs() < 1e-9);
        // One drifted metric alone fails the whole ratchet (the additive
        // gate is per-metric, like the absolute gates).
        let mut one = SEVEN;
        one[3] *= 1.20;
        let trend = trend_for(&baseline_json_with(&SEVEN), &one, true).unwrap();
        assert!(!trend.passed, "{trend:?}");
        assert_eq!(trend.entries[3].verdict, RatchetVerdict::Beyond);
        assert_eq!(trend.entries[0].verdict, RatchetVerdict::Within);
    }

    #[test]
    fn non_positive_baselines_never_ratchet() {
        // ×1.15 on a negative figure TIGHTENS the bound below the baseline
        // (a tolerance must absorb noise, never add strictness), and ×1.15
        // on zero bounds everything above zero — both noise arithmetic. The
        // verdict is the surfaced no-ratchet; the absolute budget still
        // gates the metric (the report's note says exactly that).
        for baseline_value in [0.0, -0.015] {
            let entry = ratchet_entry("subscriber_cpu_delta_le_budget", baseline_value, 0.25);
            assert_eq!(entry.verdict, RatchetVerdict::NoRatchet, "{entry:?}");
            assert_eq!(entry.limit, None);
            assert_eq!(entry.delta_pct, None);
            assert!((entry.delta - (0.25 - baseline_value)).abs() < 1e-9);
        }
        // In the block: a zero-baseline metric whose current is far above it
        // does NOT fail even the GATED ratchet — the other six stay within.
        let mut baseline = SEVEN;
        baseline[4] = 0.0; // subscriber CPU Δ measured exactly zero
        let trend = trend_for(&baseline_json_with(&baseline), &SEVEN, true).unwrap();
        assert!(trend.passed && !trend.fails_run(), "{trend:?}");
        assert_eq!(trend.entries[4].verdict, RatchetVerdict::NoRatchet);
        // The JSON arm surfaces the reason (surfaced, not silent).
        let json = trend_json(&trend);
        let note = json["metrics"][4]["note"].as_str().unwrap_or_default();
        assert!(
            note.contains("no-ratchet is meaningless") || note.contains("baseline <= 0"),
            "{note}"
        );
        assert_eq!(json["metrics"][4]["ratchet_limit"].as_null(), Some(()));
    }

    #[test]
    fn non_gate_platforms_report_without_gating() {
        // The same +20% drift on a NON-gate platform (local/other-OS run):
        // the trend names every Beyond verdict (the four ×1.15-dominated
        // metrics; the three subscriber deltas ride their noise floors),
        // but the run does not gate — verdict_str is report-only and
        // fails_run is false.
        let drifted: [f64; 7] = SEVEN.map(|value| value * 1.20);
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, false).unwrap();
        assert!(!trend.passed, "the math verdict still names the drift");
        assert!(
            !trend.fails_run(),
            "a non-gate platform never ratchet-gates"
        );
        assert_eq!(trend.verdict_str(), "report-only");
        assert_eq!(trend.entries[0].verdict, RatchetVerdict::Beyond);
        assert_eq!(trend.entries[3].verdict, RatchetVerdict::Beyond);
        assert_eq!(trend.entries[4].verdict, RatchetVerdict::Within);
    }

    #[test]
    fn round_trip_against_a_baseline_of_itself_is_all_within() {
        // The spec's own verification shape: a report compared against a
        // baseline carrying its own figures — every delta zero, every
        // verdict within, the gated ratchet passes.
        let trend = trend_for(&baseline_json_with(&SEVEN), &SEVEN, true).unwrap();
        assert!(trend.passed && !trend.fails_run(), "{trend:?}");
        assert_eq!(trend.entries.len(), 7);
        assert!(trend
            .entries
            .iter()
            .all(|e| e.verdict == RatchetVerdict::Within));
        assert!(trend.entries.iter().all(|e| (e.delta).abs() < 1e-9));
        assert!(trend
            .entries
            .iter()
            .all(|e| (e.delta_pct.unwrap()).abs() < 1e-9));
    }

    #[test]
    fn a_baseline_missing_a_gated_metric_is_a_schema_mismatch() {
        // Six of the seven gates present: the missing one is a LOUD named
        // schema error — never a silently skipped trend row. This test
        // INTENDS truncation, so it removes a gate from the parsed JSON
        // explicitly (the baseline_json_with helper now asserts full
        // length — review round 1's no-silent-zip-truncation rule).
        let mut short = serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN))
            .expect("valid json");
        short["gates"]
            .as_array_mut()
            .expect("gates array")
            .pop()
            .expect("the last gate (subscriber read-p99 delta) removed");
        let err = trend_for(&short.to_string(), &SEVEN, true).expect_err("must fail");
        assert!(err.contains("lacks the gated metric"), "{err}");
        assert!(err.contains("subscriber_read_p99_delta_le_budget"), "{err}");
        assert!(err.contains("test-baseline.json"), "names the file: {err}");
    }

    #[test]
    fn written_baselines_strip_the_trend_block_and_stamp_provenance() {
        let mut report = serde_json::json!({ "status": "pass", "gates": [] });
        report["trend"] = serde_json::json!({ "verdict": "pass" });
        let provenance = baseline_provenance(true, false, "a".repeat(40).as_str());
        let clean = baseline_report_json(&report, provenance);
        assert!(
            clean.get("trend").is_none(),
            "the written baseline must not nest a trend"
        );
        assert_eq!(clean["provenance"]["seed"], serde_json::json!(true));
        assert_eq!(
            clean["provenance"]["generated_on"],
            serde_json::json!("local-ci-mode")
        );
        assert_eq!(
            clean["provenance"]["git_sha"],
            serde_json::json!("a".repeat(40))
        );
        assert!(clean.get("status").is_some());
        // A report without a trend still gains the provenance stamp.
        let plain = serde_json::json!({ "status": "pass" });
        let clean = baseline_report_json(
            &plain,
            baseline_provenance(true, true, "b".repeat(40).as_str()),
        );
        assert_eq!(clean["provenance"]["seed"], serde_json::json!(false));
        assert_eq!(
            clean["provenance"]["generated_on"],
            serde_json::json!("ci-leg")
        );
        assert!(clean.get("trend").is_none());
    }

    #[test]
    fn trend_json_carries_the_spec_fields() {
        let trend = trend_for(&baseline_json_with(&SEVEN), &SEVEN, true).unwrap();
        let json = trend_json(&trend);
        assert_eq!(json["tolerance"].as_f64(), Some(RATCHET_TOLERANCE));
        assert_eq!(json["gated"].as_bool(), Some(true));
        assert_eq!(json["verdict"].as_str(), Some("pass"));
        assert_eq!(
            json["baseline_git_sha"].as_str(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
        assert_eq!(json["baseline_os"].as_str(), Some("linux"));
        assert_eq!(json["metrics"].as_array().map(Vec::len), Some(7));
        for field in [
            "name",
            "baseline",
            "current",
            "delta",
            "delta_pct",
            "ratchet_limit",
            "verdict",
        ] {
            assert!(
                json["metrics"][0].get(field).is_some(),
                "every trend row must carry {field}"
            );
        }
    }

    #[test]
    fn the_ratchet_tolerance_is_pinned_at_1_15() {
        // The spec-15.5 ratification: 15% absorbs same-leg CI noise while
        // catching the multi-week drift class. Changing this number is a
        // re-ratification that must edit this pin AND refresh the committed
        // baseline in the same PR (docs/testing.md's update procedure).
        assert_eq!(RATCHET_TOLERANCE, 1.15);
    }

    // -----------------------------------------------------------------------
    // Review round 1: the noise floor, the write refusal, provenance, the
    // OS guard, pre-measurement coverage, non-finite handling, apply_trend,
    // the no-flag report shape, and the note lines.
    // -----------------------------------------------------------------------

    #[test]
    fn the_noise_floor_gives_near_zero_baselines_a_real_window() {
        // The per-family ε table itself (a change is a documented
        // re-ratification — the table's doc comment carries each value's
        // justification).
        assert_eq!(ratchet_noise_floor("read_p99_lt_1s"), 0.5);
        assert_eq!(ratchet_noise_floor("cpu_per_instance_le_2pct"), 0.05);
        assert_eq!(ratchet_noise_floor("rss_per_instance_le_50mib"), 0.25);
        assert_eq!(ratchet_noise_floor("rss_max_spike_le_2x_budget"), 0.25);
        assert_eq!(ratchet_noise_floor("subscriber_cpu_delta_le_budget"), 0.05);
        assert_eq!(ratchet_noise_floor("subscriber_rss_delta_le_budget"), 0.25);
        assert_eq!(
            ratchet_noise_floor("subscriber_read_p99_delta_le_budget"),
            25.0
        );
        // An unknown (future-harness) gate floors at 0.0 — the strict
        // pure-×1.15 rule, never a guess.
        assert_eq!(ratchet_noise_floor("some_future_gate"), 0.0);

        // The floor ENGAGES on a near-zero subscriber delta: the committed
        // seed's +0.009 pct-point CPU baseline under pure ×1.15 tolerates
        // only +0.0014 (quantization jitter reds it); with ε=0.05 the limit
        // is baseline + 0.05 — a real window a +0.04 drift fits through.
        let entry = ratchet_entry("subscriber_cpu_delta_le_budget", 0.009, 0.049);
        assert_eq!(entry.verdict, RatchetVerdict::Within, "{entry:?}");
        assert!((entry.limit.unwrap() - 0.059).abs() < 1e-12);
        // Past the floored limit it is still Beyond (the floor widens the
        // window, it never disables the gate).
        let entry = ratchet_entry("subscriber_cpu_delta_le_budget", 0.009, 0.06);
        assert_eq!(entry.verdict, RatchetVerdict::Beyond, "{entry:?}");
        // Exactly at the floored limit is within (the inclusive boundary).
        let entry = ratchet_entry("subscriber_cpu_delta_le_budget", 0.009, 0.059);
        assert_eq!(entry.verdict, RatchetVerdict::Within, "{entry:?}");
        // The floor is IRRELEVANT wherever the baseline is meaningfully
        // positive: ×1.15 dominates (a 5 ms read baseline limits at 5.75,
        // not 5.5).
        let entry = ratchet_entry("read_p99_lt_1s", 5.0, 5.6);
        assert!((entry.limit.unwrap() - 5.75).abs() < 1e-12);
        assert_eq!(entry.verdict, RatchetVerdict::Within, "{entry:?}");
        // …but it DOES engage on the committed seed's real read figure
        // (2.523 ms: 2.90 from ×1.15 vs 3.02 from +0.5 — the floor wins).
        let entry = ratchet_entry("read_p99_lt_1s", 2.523, 2.95);
        assert!((entry.limit.unwrap() - 3.023).abs() < 1e-12);
        assert_eq!(entry.verdict, RatchetVerdict::Within, "{entry:?}");
        // Block-level: a +400% drift on a 0.009-pt subscriber-CPU baseline
        // (0.045 current) stays within ONLY because of the floor — and the
        // gated ratchet still passes.
        let mut baseline = SEVEN;
        baseline[4] = 0.009;
        let mut current = SEVEN;
        current[4] = 0.045;
        let trend = trend_for(&baseline_json_with(&baseline), &current, true).unwrap();
        assert_eq!(trend.entries[4].verdict, RatchetVerdict::Within);
        assert!(trend.passed && !trend.fails_run(), "{trend:?}");
    }

    #[test]
    fn write_baseline_refuses_red_runs() {
        // The pure refusal decision (main wires it to a loud note + exit 2
        // and never reaches the write): every red combination refuses, and
        // each reason names WHICH gate failed and the remedy. Review round
        // 3: the serve token is its OWN arm — deleting it cannot pass the
        // battery.
        let gates_red =
            baseline_write_refusal(false, false, false).expect("a red absolute run refuses");
        assert!(gates_red.contains("absolute budgets FAILED"), "{gates_red}");
        let serve_red = baseline_write_refusal(true, true, false).expect("a red serve run refuses");
        assert!(serve_red.contains("SERVE budgets FAILED"), "{serve_red}");
        let ratchet_red = baseline_write_refusal(true, false, true).expect("a red ratchet refuses");
        assert!(ratchet_red.contains("ratchet FAILED"), "{ratchet_red}");
        assert!(ratchet_red.contains("docs/testing.md"), "{ratchet_red}");
        // Both absolute families red at once names the default-budget
        // failure first; red-both-with-ratchet never reaches the ratchet arm.
        let both = baseline_write_refusal(false, true, true).expect("red-both refuses");
        assert!(both.contains("absolute budgets FAILED"), "{both}");
        // A fully green run is the only write path.
        assert_eq!(baseline_write_refusal(true, false, false), None);
    }

    #[test]
    fn run_exit_code_folds_all_three_red_sources() {
        // Review round 3, pinned so each token has a test that fails if it
        // is deleted: only a fully green run exits 0; ANY one of the three
        // red sources (absolute gates, ratchet, serve gates) reds the run.
        assert_eq!(run_exit_code(true, false, false), 0);
        assert_eq!(run_exit_code(false, false, false), 1, "absolute gates red");
        assert_eq!(run_exit_code(true, true, false), 1, "ratchet red");
        assert_eq!(run_exit_code(true, false, true), 1, "serve gates red");
        assert_eq!(run_exit_code(false, true, true), 1, "all red");
    }

    #[test]
    fn provenance_labels_distinguish_the_ci_leg_from_local_runs() {
        // The canonical pair from the review: the CI leg (CI mode ON the
        // GitHub runner) is seed:false / "ci-leg"; a local run with CI mode
        // forced (the committed seed's shape) is seed:true /
        // "local-ci-mode".
        assert_eq!(generated_on_label(true, true), "ci-leg");
        assert_eq!(generated_on_label(false, true), "local-ci-mode");
        // The honest third arm: a NON-CI local write is "local" — stamping
        // it "local-ci-mode" would be a lie (recorded as a surface decision
        // in the spec change log).
        assert_eq!(generated_on_label(false, false), "local");
        assert_eq!(generated_on_label(true, false), "local");
        let leg = baseline_provenance(true, true, &"c".repeat(40));
        assert_eq!(leg["seed"], serde_json::json!(false));
        assert_eq!(leg["generated_on"], serde_json::json!("ci-leg"));
        assert_eq!(leg["git_sha"], serde_json::json!("c".repeat(40)));
        let seed = baseline_provenance(true, false, &"d".repeat(40));
        assert_eq!(seed["seed"], serde_json::json!(true));
        assert_eq!(seed["generated_on"], serde_json::json!("local-ci-mode"));
    }

    #[test]
    fn the_os_guard_warns_only_on_a_real_mismatch() {
        assert!(!baseline_os_mismatch(Some("linux"), "linux"));
        assert!(baseline_os_mismatch(Some("macos"), "linux"));
        assert!(baseline_os_mismatch(Some("linux"), "macos"));
        assert!(baseline_os_mismatch(Some("windows"), "linux"));
        // An absent OS is not a mismatch — absence never warns.
        assert!(!baseline_os_mismatch(None, "linux"));
    }

    #[test]
    fn pre_measurement_coverage_names_missing_and_extra_gates() {
        // The expected list IS what evaluate_gates emits — lengths asserted
        // first (the same no-silent-zip-truncation rule), so a new gate
        // cannot silently escape pre-measurement validation.
        let reference = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        assert_eq!(GATED_METRICS.len(), reference.verdicts.len());
        for (expected, verdict) in GATED_METRICS.iter().zip(&reference.verdicts) {
            assert_eq!(*expected, verdict.name);
        }
        // A full baseline: no gaps either way.
        let figures = parse_baseline(&baseline_json_with(&SEVEN), "cov.json").unwrap();
        let (missing, extra) = baseline_coverage_gaps(&figures, &GATED_METRICS);
        assert!(missing.is_empty() && extra.is_empty());
        // A baseline missing a gate names it in `missing`.
        let mut short = serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN))
            .expect("valid json");
        short["gates"].as_array_mut().expect("gates").pop();
        let figures = parse_baseline(&short.to_string(), "short.json").unwrap();
        let (missing, extra) = baseline_coverage_gaps(&figures, &GATED_METRICS);
        assert_eq!(missing, vec!["subscriber_read_p99_delta_le_budget"]);
        assert!(extra.is_empty());
        // A FUTURE-harness baseline (an extra gate today's run does not
        // evaluate) lands in `extra` — surfaced, never a hard failure.
        let mut future = serde_json::from_str::<serde_json::Value>(&baseline_json_with(&SEVEN))
            .expect("valid json");
        future["gates"]
            .as_array_mut()
            .expect("gates")
            .push(serde_json::json!({ "name": "gpu_per_instance_le_1pct", "measured": 0.1 }));
        let figures = parse_baseline(&future.to_string(), "future.json").unwrap();
        let (missing, extra) = baseline_coverage_gaps(&figures, &GATED_METRICS);
        assert!(missing.is_empty());
        assert_eq!(extra, vec!["gpu_per_instance_le_1pct"]);
    }

    #[test]
    fn non_finite_currents_surface_no_ratchet_with_a_reason() {
        // A NaN current is a SURFACED no-ratchet — never a silently-within
        // NaN comparison (every NaN comparison is false, so "within by
        // default" would be a lie of omission).
        let entry = ratchet_entry("read_p99_lt_1s", 5.0, f64::NAN);
        assert_eq!(entry.verdict, RatchetVerdict::NoRatchet, "{entry:?}");
        assert_eq!(
            entry.no_ratchet_reason,
            Some(NoRatchetReason::NonFiniteCurrent)
        );
        assert_eq!(entry.limit, None);
        assert_eq!(entry.delta_pct, None);
        assert!(entry.delta.is_nan(), "the delta stays honestly NaN");
        // Block-level: the JSON note names the non-finiteness, the row's
        // limit is null, and the gated ratchet does not fail on it (the
        // metric's ABSOLUTE gate is what a NaN reds — pinned by the gate
        // tests: every NaN bound comparison is false).
        let mut current = SEVEN;
        current[0] = f64::NAN;
        let trend = trend_for(&baseline_json_with(&SEVEN), &current, true).unwrap();
        assert_eq!(trend.entries[0].verdict, RatchetVerdict::NoRatchet);
        assert!(trend.passed && !trend.fails_run(), "{trend:?}");
        let json = trend_json(&trend);
        let note = json["metrics"][0]["note"].as_str().unwrap_or_default();
        assert!(note.contains("not finite"), "{note}");
        assert_eq!(json["metrics"][0]["ratchet_limit"].as_null(), Some(()));
        // The stderr note line surfaces the same reason.
        let lines = trend_note_lines(&trend);
        assert!(
            lines.iter().any(|line| line.contains("not finite")),
            "{lines:?}"
        );
        // A NON-FINITE BASELINE never loads at all (the corrupt-file arm
        // below) — this arm is only about the CURRENT side.
    }

    #[test]
    fn non_finite_baseline_values_are_corrupt_file_errors() {
        // JSON cannot spell NaN, but an out-of-range literal (1e999) either
        // fails serde_json's parse or parses to inf — BOTH must surface as
        // a loud named error, never as a value the ratchet compares. The
        // literal is spliced into the TEXT (the json! macro cannot carry a
        // non-finite f64 — it renders null — and the compiler denies
        // 1e999 literals), which is exactly the hostile-file shape.
        let hostile_text =
            baseline_json_with(&SEVEN).replacen("\"measured\":5.0", "\"measured\":1e999", 1);
        let err = parse_baseline(&hostile_text, "inf.json").expect_err("must fail");
        assert!(err.contains("inf.json"), "names the file: {err}");
        assert!(
            err.contains("not valid JSON") || err.contains("non-finite"),
            "either the parse error or the corrupt arm, both loud: {err}"
        );
        // The direct corrupt arm: a Value carrying inf into `measured`
        // cannot even be BUILT programmatically (serde_json's Number API
        // refuses non-finite f64s), so the remaining real hostile shape is
        // a huge INTEGER literal — beyond u64/i64, serde_json falls back to
        // f64, which saturates to inf. Either way the load must refuse.
        let huge = format!("\"measured\":1{}", "0".repeat(400));
        let hostile_text = baseline_json_with(&SEVEN).replacen("\"measured\":5.0", &huge, 1);
        let err = parse_baseline(&hostile_text, "bigint.json").expect_err("must fail");
        assert!(
            err.contains("not valid JSON") || err.contains("non-finite"),
            "either the parse error or the corrupt arm, both loud: {err}"
        );
    }

    #[test]
    fn apply_trend_flips_status_and_returns_the_failure() {
        // Beyond + gated: apply_trend returns true (fails the run) and the
        // report's overall status flips to fail EVEN with every absolute
        // gate green.
        let drifted: [f64; 7] = SEVEN.map(|value| value * 1.20);
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, true).unwrap();
        let mut report = serde_json::json!({ "status": "pass" });
        assert!(apply_trend(&mut report, &trend));
        assert_eq!(report["status"], serde_json::json!("fail"));
        assert!(report.get("trend").is_some());
        // Within + gated: returns false, the status stays whatever the
        // gates derived (unchanged — apply_trend never touches it).
        let trend = trend_for(&baseline_json_with(&SEVEN), &SEVEN, true).unwrap();
        let mut report = serde_json::json!({ "status": "pass" });
        assert!(!apply_trend(&mut report, &trend));
        assert_eq!(report["status"], serde_json::json!("pass"));
        // Beyond but NOT gated (report-only): no status flip, no failure.
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, false).unwrap();
        let mut report = serde_json::json!({ "status": "pass" });
        assert!(!apply_trend(&mut report, &trend));
        assert_eq!(report["status"], serde_json::json!("pass"));
        assert_eq!(report["trend"]["verdict"], serde_json::json!("report-only"));
    }

    #[test]
    fn the_no_flag_report_shape_is_pinned() {
        // build_report WITHOUT --baseline (the byte-equivalence observer
        // gap, closed): no `trend` key, and the status derived purely from
        // the gates — pass when they pass, fail when they do not.
        let config = HarnessConfig {
            read_iterations: 200,
            settle: Duration::from_secs(2),
            window: Duration::from_secs(12),
            sample_interval: Duration::from_secs(1),
            windows: 1,
            ci: false,
        };
        let steady_measurements = SteadyMeasurements {
            windows: vec![WindowSample {
                cpu_pct_aggregate_mean: 10.0,
                rss_bytes_mean: 100.0 * 1024.0 * 1024.0,
                rss_bytes_max: 150_000_000,
                samples: 11,
            }],
            median_cpu_aggregate_pct: 10.0,
            median_rss_bytes_mean: 100.0 * 1024.0 * 1024.0,
            rss_bytes_max: 150_000_000,
            first_sample_skipped: true,
        };
        let green = evaluate_gates(5.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        let report = build_report(
            &config,
            &fast_reads(),
            5.0,
            &steady_measurements,
            &healthy_steady(),
            10,
            1,
            1.0,
            &green,
            &quiet_subscriber(),
            AGENT_LINGER_MS,
        );
        assert!(report.get("trend").is_none(), "no --baseline, no trend");
        assert_eq!(report["status"], serde_json::json!("pass"));
        assert!(
            report.get("provenance").is_none(),
            "provenance is writer-only"
        );
        assert!(
            report.get("serve_shape").is_none(),
            "story 17-1: the serve block exists only on --serve-shape runs"
        );
        // The same report with a failed gate says fail — the status is the
        // gates' own derivation, nothing else.
        let red = evaluate_gates(1500.0, &healthy_steady(), &quiet_subscriber(), 1.0);
        let report = build_report(
            &config,
            &fast_reads(),
            1500.0,
            &steady_measurements,
            &healthy_steady(),
            10,
            1,
            1.0,
            &red,
            &quiet_subscriber(),
            AGENT_LINGER_MS,
        );
        assert!(report.get("trend").is_none());
        assert_eq!(report["status"], serde_json::json!("fail"));
    }

    #[test]
    fn serve_shape_json_carries_the_spec_fields() {
        // The serve block's pinned shape (story 17-1): the decision-rule
        // provenance, the measured serve figures, the bounded-stop record,
        // the surfaced subscriber counts, and the serve gates' OWN names —
        // plus the red arm flipping the block status.
        let outcome = ServeShapeOutcome {
            reads: fast_reads(),
            overall_p99_ms: 5.0,
            steady: SteadyMeasurements {
                windows: vec![WindowSample {
                    cpu_pct_aggregate_mean: 10.0,
                    rss_bytes_mean: 100.0 * 1024.0 * 1024.0,
                    rss_bytes_max: 150_000_000,
                    samples: 11,
                }],
                median_cpu_aggregate_pct: 10.0,
                median_rss_bytes_mean: 100.0 * 1024.0 * 1024.0,
                rss_bytes_max: 150_000_000,
                first_sample_skipped: true,
            },
            figures: healthy_steady(),
            running_count: FLEET_SIZE + 1,
            effective_running_instances: FLEET_SIZE,
            load: ServeLoad {
                fleet: vec![vec![1.0]],
                instance_status: vec![vec![1.0]],
                effective_config: vec![vec![1.0]],
                usage_via_fleet: vec![vec![1.0]],
                follower_events: 3,
                follower_lagged: 0,
                stop_elapsed_ms: 2010.0,
                pathological_final_state: "stopped".to_string(),
                pathological_stop_cause: "{\"kind\":\"stop-forced\"}".to_string(),
                reader_iterations: vec![198, 197, 198, 196],
                reads_started_during_stop: 128,
                reader_retries: 2,
            },
            fixture_note: "unix: test note",
            pace: Duration::from_millis(60),
            read_iterations: 200,
            sampling_window_ms: 12_000,
        };
        let gates = evaluate_serve_gates(5.0, &healthy_steady(), 1.0);
        let json = serve_shape_json(&outcome, &gates, 1.0, true);
        assert_eq!(json["story"], serde_json::json!("17-1"));
        assert_eq!(json["status"], serde_json::json!("pass"));
        assert!(
            json["decision_rule"].as_str().unwrap().contains("ai-63b"),
            "the block carries the FIXED decision rule's pointer: {json}"
        );
        assert_eq!(
            json["fixture"]["running_instances"],
            serde_json::json!(FLEET_SIZE + 1)
        );
        assert_eq!(
            json["fixture"]["orphan_bound_ms"],
            serde_json::json!(SERVE_AGENT_LINGER_MS)
        );
        assert_eq!(
            json["driver"]["reader_tasks"],
            serde_json::json!(SERVE_READER_TASKS)
        );
        // Review round 3 disclosures: delivered iterations beside the
        // nominal demand, the retry count, the contention proof's count,
        // the effective (post-stop) normalization divisor, and the clamped
        // sampling window.
        assert_eq!(
            json["driver"]["iterations_delivered_per_reader"],
            serde_json::json!([198, 197, 198, 196])
        );
        assert_eq!(json["driver"]["reader_retries"], serde_json::json!(2));
        assert_eq!(
            json["driver"]["sampling_window_ms"],
            serde_json::json!(12_000)
        );
        assert_eq!(
            json["contention"]["reads_started_during_stop"],
            serde_json::json!(128)
        );
        assert_eq!(
            json["fixture"]["effective_running_instances"],
            serde_json::json!(FLEET_SIZE)
        );
        // The load-model honesty (review round 3): the derivation names the
        // per-reader EQUALITY and the ~4x AGGREGATE — never "the same
        // demand" unqualified.
        let derivation = json["driver"]["pace_derivation"].as_str().unwrap();
        assert!(derivation.contains("EQUALS the"), "{derivation}");
        assert!(derivation.contains("~4x"), "{derivation}");
        // The follower's honesty: bus liveness under load, not load evidence.
        let sub_shape = json["subscriber"]["shape"].as_str().unwrap();
        assert!(sub_shape.contains("BUS LIVENESS"), "{sub_shape}");
        // The binding flag rides each verdict (true on the gate platform).
        assert_eq!(json["gates"][0]["hard"], serde_json::json!(true));
        // Report-only elsewhere: hard flips to false, the block says so.
        let json = serve_shape_json(&outcome, &gates, 1.0, false);
        assert_eq!(json["gates"][0]["hard"], serde_json::json!(false));
        assert_eq!(
            json["gates_bind_on_this_platform"],
            serde_json::json!(false)
        );
        let json = serve_shape_json(&outcome, &gates, 1.0, true);
        assert_eq!(
            json["bounded_stop"]["final_state"],
            serde_json::json!("stopped")
        );
        assert_eq!(
            json["bounded_stop"]["elapsed_ms"],
            serde_json::json!(2010.0)
        );
        assert_eq!(json["subscriber"]["events_counted"], serde_json::json!(3));
        assert_eq!(json["subscriber"]["lagged_recovered"], serde_json::json!(0));
        assert_eq!(json["gates"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            json["gates"][0]["name"],
            serde_json::json!("serve_read_p99_lt_1s")
        );
        // The red arm flips the block's own status (the REPORT's status is
        // main's job — the pure builder only reflects its inputs).
        let red = evaluate_serve_gates(1500.0, &healthy_steady(), 1.0);
        let json = serve_shape_json(&outcome, &red, 1.0, true);
        assert_eq!(json["status"], serde_json::json!("fail"));
        // A Lagged recovery is surfaced verbatim in the block (AI-18).
        let lagged_outcome = ServeShapeOutcome {
            load: ServeLoad {
                fleet: vec![vec![1.0]],
                instance_status: vec![vec![1.0]],
                effective_config: vec![vec![1.0]],
                usage_via_fleet: vec![vec![1.0]],
                follower_events: 30,
                follower_lagged: 7,
                stop_elapsed_ms: 2010.0,
                pathological_final_state: "stopped".to_string(),
                pathological_stop_cause: "{\"kind\":\"stop-forced\"}".to_string(),
                reader_iterations: vec![198, 197, 198, 196],
                reads_started_during_stop: 128,
                reader_retries: 2,
            },
            ..outcome
        };
        let json = serve_shape_json(&lagged_outcome, &gates, 1.0, true);
        assert_eq!(json["subscriber"]["lagged_recovered"], serde_json::json!(7));
        assert!(
            json["subscriber"]["lagged_note"]
                .as_str()
                .unwrap()
                .contains("RECOVERED"),
            "the Lagged note names the recovery: {json}"
        );
    }

    #[test]
    fn trend_note_lines_carry_the_fail_and_report_only_summaries() {
        // The FAIL summary (gated + beyond) — the line the CI job log leads
        // with on a ratchet red. At +20% the four ×1.15-dominated metrics
        // are Beyond and the three subscriber deltas ride their floors.
        let drifted: [f64; 7] = SEVEN.map(|value| value * 1.20);
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, true).unwrap();
        let lines = trend_note_lines(&trend);
        assert_eq!(lines.len(), 7 + 1, "seven metric rows + one summary");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("ratchet: FAIL — 4 metric(s)")),
            "{lines:?}"
        );
        // The beyond rows name all four figures; the floored rows say within.
        assert!(lines.iter().take(4).all(|line| line.contains("— beyond")));
        assert!(lines
            .iter()
            .skip(4)
            .take(3)
            .all(|line| line.contains("— within")));
        // The REPORT-ONLY summary (non-gated + beyond).
        let trend = trend_for(&baseline_json_with(&SEVEN), &drifted, false).unwrap();
        let lines = trend_note_lines(&trend);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("ratchet: REPORT-ONLY")),
            "{lines:?}"
        );
        // A missing limit renders through the ONE shared token — never a
        // fake 0.0000 (the no-ratchet rows).
        let mut baseline = SEVEN;
        baseline[4] = 0.0;
        let trend = trend_for(&baseline_json_with(&baseline), &SEVEN, true).unwrap();
        let lines = trend_note_lines(&trend);
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("limit {NO_LIMIT_TOKEN}"))),
            "{lines:?}"
        );
        assert_eq!(render_limit(None), NO_LIMIT_TOKEN);
        assert_eq!(render_limit(Some(0.059)), "0.0590");
    }
}
