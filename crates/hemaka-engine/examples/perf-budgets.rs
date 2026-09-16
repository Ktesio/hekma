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
//! cargo run --release --example perf-budgets -p hemaka-engine
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

use std::time::{Duration, Instant};

use hemaka_conformance::test_support::{fake_agent_bin_in, BinDir, ManifestFixture};
use hemaka_engine::{AdapterRef, Blocking, ConfigLayer, Engine, FleetEntry, LifecycleState};
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
    let required_ms = schedule_ms + LIVENESS_MARGIN.as_millis();
    if u128::from(AGENT_LINGER_MS) > required_ms {
        Ok(())
    } else {
        Err(format!(
            "the orphan bound AGENT_LINGER_MS={AGENT_LINGER_MS} ms does not cover the \
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
// The harness
// ---------------------------------------------------------------------------

fn main() {
    let config = HarnessConfig::from_env();
    // The liveness gate BEFORE anything is spawned: the orphan bound must
    // cover the gated windows PLUS the subscriber-overhead addendum window
    // (the same heartbeat idlers are measured in both).
    if let Err(reason) = check_liveness_schedule(config.settle, config.windows + 1, config.window) {
        note(&format!("{reason} — exiting 2"));
        std::process::exit(2);
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
    let manifest_dir = ManifestFixture::heartbeat(MANIFEST_KIND, HEARTBEAT_MS, AGENT_LINGER_MS)
        .exec(fake_agent)
        .write(manifest_tmp.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open the engine");
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
    let report = build_report(
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
    );
    print_report(&report, &gates);
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

    // ---- Teardown (best-effort; a stop failure is noted, never a panic —
    // and the RunningGuard below would retry it on unwind anyway). ----
    let mut all_stopped = true;
    for name in guard.names() {
        if let Err(e) = facade.stop(name, Some(Duration::from_secs(5))) {
            all_stopped = false;
            note(&format!(
                "stop {name} failed during teardown ({e}); the agent's own \
                 --linger-ms bound ({AGENT_LINGER_MS} ms) caps any orphan"
            ));
        }
    }
    guard.defuse_if(all_stopped);
    drop(guard);
    drop(engine);
    drop(state);
    drop(manifest_tmp);
    note("teardown complete: running instances stopped, hermetic roots removed");

    if !gates.passed {
        // The measured record is already on stdout; exit nonzero so the CI
        // perf-budgets job (and any local gate runner) fails on the regression.
        std::process::exit(1);
    }
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

/// Build the machine-readable report (schema v1) from the measurements and
/// the gate evaluation. Pure — no printing, no environment access. The
/// schema stays v1: the subscriber-overhead block is ADDITIVE (the AD-14
/// convention — additive fields keep the version).
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
) -> serde_json::Value {
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
    let verdicts: Vec<serde_json::Value> = gates
        .verdicts
        .iter()
        .map(|v| {
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
        })
        .collect();
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
            "orphan_bound_ms": AGENT_LINGER_MS,
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
        "read_latency_ms": per_kind,
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
        "steady_state": {
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
        },
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
    let status = if gates.passed { "pass" } else { "fail" };
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
        assert!(err.contains("AGENT_LINGER_MS"), "names the bound: {err}");
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
}
