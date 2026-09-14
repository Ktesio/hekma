//! Shared UJ-3 flow expectations (story 7-1) — the ONE place the "host drives
//! the full flow through the library alone" journey's expected values are
//! pinned, and the seam that proves the LIBRARY path and the `kt` CLI path are
//! behaviorally identical.
//!
//! ## Role: test infrastructure, never a driving surface
//!
//! Story 7-1's intent (FR-31): a host links `ktesio-engine` and drives the full
//! UJ-3 flow — register → configure → cap → start → breach → pause → stop —
//! with no CLI, and the SAME flow through documented `kt` commands must pass
//! the SAME assertions. This module owns the shared half of that proof:
//!
//! * the fixture manifest the flow's agent registers under
//!   ([`write_flow_manifest`]),
//! * the flow's config keys/values ([`flow_config_pairs`]),
//! * the budget/rate/cap numbers and the expected token/dollar totals,
//! * assertion helpers over OBSERVED reads (state, breach/transition events,
//!   [`FleetEntry`]/[`UsageView`] rows) — each expectation stated ONCE here,
//!   never re-stated inline by either suite,
//! * the committed-state readers + poller ([`committed_state`],
//!   [`wait_for_state`], [`read_breach_events`], [`read_transition_events`])
//!   copied from the engine's `tests/budget.rs` mechanism so BOTH suites share
//!   it (the engine layout — `state.db` via the engine-published
//!   `paths::STATE_DB_FILE`, `agents/<name>/logs/*.log` — is identical under a
//!   library `Engine::open` root and a `kt` state dir),
//! * the usage-ledger reader + received-stream projection + the raw-receiver
//!   drain ([`committed_usage_rows`], [`usage_from_payload`],
//!   [`drain_receiver`] — the drain now delegating to the shared
//!   `test_support` implementation) — the ONE comparison shape for story
//!   7-2's "the received usage stream equals the committed ledger rows
//!   exactly" guarantee, consumed by both the 7-2 acceptance suite and the
//!   7-3 collision test, and
//! * assertion helpers over OBSERVED reads (state, breach/transition events,
//!   [`FleetEntry`]/[`UsageView`] rows) — each expectation stated ONCE here,
//!   never re-stated inline by either suite.
//!
//! Both consumers dev-depend on this crate; dev-deps never cross the shipping
//! boundary gate (`cargo tree -p ktesio -e normal,build` stays clean). A test
//! imports these helpers to ASSERT; the driving happens exclusively through
//! each suite's own sanctioned surface (the engine's public `Blocking` facade
//! in the host test; documented `kt` commands in the CLI journey). This module
//! is deliberately NOT a driver and offers no way to become one.
//!
//! ## The pinned flow (the numbers, and why they are exact)
//!
//! The fixture agent is the conformance `fake_agent` with `--emit-usage 5`:
//! the proven `budget.rs`/`cost.rs` emission pattern, whose FIXED sentinels are
//! 10 input + 20 output tokens per event (30/event). With the unit Rate
//! (`$1.00`/1M both directions = 1 micro/token) every event costs exactly 30
//! micro-dollars, so:
//!
//! * a cumulative token ceiling of [`TOKEN_CEILING`] (90) breaches at/after
//!   event 3 (`>= 90`) — the TOKEN breach wins the pause (the supervisor
//!   evaluates token ceilings first when both dimensions cross),
//! * a cumulative dollar cap of [`DOLLAR_CAP_DOLLARS`] ([`DOLLAR_CAP_MICROS`]
//!   micros) is armed on the same instance so the flow proves the Rate'd
//!   dollar surfaces honestly; its full enforcement matrix is `cost.rs`'s,
//!   which the shared expectations mirror rather than re-state,
//! * the instance pauses (the ratified default Breach Action) with a
//!   `BudgetExceeded` cause and exactly ONE breach event (the per-Run
//!   idempotence latch), polled against COMMITTED state — never wall-clock.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ktesio_engine::{
    broadcast, BreachAction, BreachDimension, BreachScope, BudgetBreachEvent, EngineEvent,
    EstimateLabel, FleetEntry, LifecycleState, Micros, TransitionCause, TransitionEvent,
    UsageUpdateEvent, UsageView,
};

// ---------------------------------------------------------------------------
// The pinned expectations (stated ONCE — neither suite re-states them)
// ---------------------------------------------------------------------------

/// The Fleet-unique instance name both suites drive the flow under.
pub const FLOW_INSTANCE: &str = "uj3-probe";

/// The manifest adapter kind string the fixture registers under (a flow-local
/// identity; nothing builtin answers to it).
pub const MANIFEST_KIND: &str = "uj3flow";

/// The unified config key the flow's configure leg sets, and the value it must
/// resolve to — a REAL model-shaped value so the set + effective read proves
/// the §4.3 leg end to end (not a sentinel empty string).
pub const MODEL_KEY: &str = "model";
pub const MODEL_VALUE: &str = "uj3-model-1";

/// The provenance label the configured key must carry: both suites write it at
/// the instance layer, so the effective read must name that layer.
pub const MODEL_SOURCE: &str = "instance";

/// The env var the fixture manifest maps `model` onto (the manifest
/// `[config.model]` table) — the same target the builtin mock declares.
pub const MODEL_ENV_VAR: &str = "MODEL";

/// The flow's cumulative TOKEN ceiling: 5 events × 30 tokens crosses it at
/// event 3 (`>=`), with headroom for the rest of the batch to race the
/// suspension (the `budget.rs` observed `>=` limit discipline).
pub const TOKEN_CEILING: u64 = 90;

/// [`TOKEN_CEILING`] as the config string. A separate const (a `u64` cannot be
/// interpolated into a `&'static str`); the `values_agree_with_each_other` unit
/// test below pins the two against drift.
pub const TOKEN_CEILING_STR: &str = "90";

/// The `fake_agent` per-event token sentinel (10 in + 20 out) the shared
/// totals are computed from — mirrored as a const exactly like `budget.rs`.
pub const TOKENS_PER_EVENT: u64 = 30;

/// How many usage events the fixture emits per Run (the `--emit-usage` arg the
/// manifest bakes in).
pub const EMIT_EVENTS: u64 = 5;

/// The full per-Run batch the fixture emits: 5 events × 30 tokens — the UPPER
/// bound of what can be committed post-stop (see [`assert_stopped_usage`] for
/// why the committed figure is honestly a range, not this exact number).
pub const EMIT_TOTAL_TOKENS: u64 = EMIT_EVENTS * TOKENS_PER_EVENT;

/// The fewest events whose tokens can be committed when the ceiling fired:
/// the breach proves the committed total reached [`TOKEN_CEILING`], which at
/// [`TOKENS_PER_EVENT`]/event is the crossing event — event 3 — so events 1–3
/// at minimum were emitted AND committed before the freeze.
pub const MIN_STOPPED_EVENTS: u64 = TOKEN_CEILING / TOKENS_PER_EVENT;

/// The committed input-token bounds post-stop (the fixture's fixed 10-in
/// sentinel × the same event range as the totals).
pub const MIN_STOPPED_INPUT_TOKENS: u64 = MIN_STOPPED_EVENTS * 10;
pub const MAX_STOPPED_INPUT_TOKENS: u64 = EMIT_EVENTS * 10;

/// The committed output-token bounds post-stop (the fixture's fixed 20-out
/// sentinel × the same event range as the totals).
pub const MIN_STOPPED_OUTPUT_TOKENS: u64 = MIN_STOPPED_EVENTS * 20;
pub const MAX_STOPPED_OUTPUT_TOKENS: u64 = EMIT_EVENTS * 20;

/// How long any suite may wait for a committed state transition before
/// failing: the single polling budget both suites and the breach helper pass
/// to [`wait_for_state`] (never an inline restatement).
pub const STATE_POLL_BUDGET: Duration = Duration::from_secs(30);

/// The graceful stop window EVERY suite's stop leg uses: ZERO. The breach has
/// SIGSTOP'd (or job-suspended) the process, and a suspended process cannot
/// act on a graceful signal — the window would always fully elapse before the
/// forced kill — so all three stop legs (host facade, CLI `--timeout`, guard)
/// skip it deliberately.
pub const STOP_WINDOW: Duration = Duration::ZERO;

/// The flow's Rate: `$1.00` per 1M tokens on BOTH directions — 1 micro-dollar
/// per token, so the derived cost always equals the token total exactly.
pub const RATE_DOLLARS: &str = "1.00";

/// The flow's cumulative dollar Cost Cap as a dollar string: `$0.00009` = 90
/// micros, sized to cross on the same event as the token ceiling.
pub const DOLLAR_CAP_DOLLARS: &str = "0.00009";

/// The cap in integer micro-dollars — the wire form both suites assert.
pub const DOLLAR_CAP_MICROS: i64 = 90;

/// The Breach Action the flow arms: `pause`, the ratified default (armed
/// explicitly so the journey proves the configured value, not just the
/// default).
pub const BREACH_ACTION: &str = "pause";

/// The fixture's Metering Source — stamped on every breach event and surfaced
/// in every Fleet row the flow reads.
pub const METERING_SOURCE: &str = "self-reported";

/// The manifest `contract_version` the fixture carries (contract v1 — the
/// frozen, negotiated surface registration gates on).
pub const CONTRACT_VERSION: &str = "1.0.0";

/// The flow's config as `(key, value)` pairs in ONE pinned order. The host
/// test sets each through `Blocking::set_config`; the CLI journey runs each
/// through documented `kt agent config set`. Neither suite holds its own copy
/// of the keys or values.
pub fn flow_config_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        (MODEL_KEY, MODEL_VALUE),
        ("budget.tokens.cumulative", TOKEN_CEILING_STR),
        ("cost.rate.input", RATE_DOLLARS),
        ("cost.rate.output", RATE_DOLLARS),
        ("budget.dollars.cumulative", DOLLAR_CAP_DOLLARS),
        ("budget.breach_action", BREACH_ACTION),
    ]
}

// ---------------------------------------------------------------------------
// The fixture manifest (§4.7 — the Adapter Contract made flesh)
// ---------------------------------------------------------------------------

/// Write the flow's fixture manifest (`adapter.toml`) into `dir` and return the
/// directory path (both `AdapterRef::Manifest` and `kt agent register
/// --manifest` accept a directory). Since story 10-1 (issue #164 resolved for
/// the embedding suites) this is a THIN preset call: the body is built by
/// [`crate::test_support::ManifestFixture::uj3_flow`] — the ONE parameterized
/// fixture builder — with this shape:
///
/// * `contract_version = "1.0.0"` — the negotiated contract major,
/// * `pause` + `interaction` **guaranteed on all three OSes** so the default
///   pause Breach Action lands the committed `paused` state deterministically
///   everywhere — which is what the assertions pin. HONESTY about the
///   suspension itself (the engine's own honesty ceiling: a guaranteed pause
///   on Windows reads `not_applicable`): on Unix the suspension is a real
///   SIGSTOP freeze (the emitter is frozen mid-batch); on Windows it is
///   cooperative best-effort with no hard suspension (the emitter keeps
///   running through the pause) — which is exactly why the flow's usage
///   assertions are committed RANGES, not exact counts,
/// * `metering.source = "self-reported"` (a viable source — registers),
/// * `[lifecycle.start]` execs the conformance `fake_agent` with
///   `--emit-usage 5 --linger-ms 600000` (emits the known batch, then idles so
///   only the flow's own stop ends the Run),
/// * `[config.model]` mapping the unified `model` key onto the
///   [`MODEL_ENV_VAR`] env var — what makes the configure leg's set + read
///   meaningful for THIS adapter (a mapping exists to deliver through).
pub fn write_flow_manifest(dir: &Path) -> PathBuf {
    crate::test_support::ManifestFixture::uj3_flow().write(dir)
}

// ---------------------------------------------------------------------------
// Committed-state readers + poller (the budget.rs mechanism, shared)
// ---------------------------------------------------------------------------

/// The committed Lifecycle State for `name`, read via a direct read-only
/// connection to the SAME state DB the engine commits to (deterministic
/// committed state — never a wall-clock guess). The layout is identical under
/// a library `Engine::open(root)` and a `kt` state dir: `<root>/state.db`
/// (the engine-published `paths::STATE_DB_FILE` name).
pub fn committed_state(state_dir: &Path, name: &str) -> Option<String> {
    let conn =
        rusqlite::Connection::open(state_dir.join(ktesio_engine::paths::STATE_DB_FILE)).ok()?;
    // A SHORT busy timeout: the poller may read while another engine session
    // (e.g. the kt CLI journey's live helper) is mid-commit, and a plain
    // read hitting that lock fails instantly — silently burning the caller's
    // whole poll budget on one contended read. 50ms rides out a writer's
    // transaction without inflating the poll loop.
    let _ = conn.busy_timeout(Duration::from_millis(50));
    conn.query_row(
        "SELECT state FROM agent_instances WHERE name = ?1",
        [name],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

/// Poll the committed Lifecycle State for `name` until it equals `want`,
/// bounded. The evaluator runs synchronously inside the ingestion path, so the
/// transition commits as soon as the breaching event is ingested by the
/// reaper — this waits for the DETERMINISTIC committed state, not a duration.
pub fn wait_for_state(state_dir: &Path, name: &str, want: LifecycleState, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let state = committed_state(state_dir, name);
        if state.as_deref() == Some(want.as_str()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for '{name}' to reach {} (committed state: {state:?})",
            want.as_str()
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// The instance's committed breach-event log: `<root>/agents/<name>/logs/
/// breaches.log`, parsed as JSON-Lines into the engine's [`BudgetBreachEvent`]
/// structs — the SAME records the facade's `budget_breach_events` read returns.
/// An absent log (no breach yet) reads as empty; a torn trailing append is
/// skipped (see [`read_json_lines`]); a malformed NON-trailing line is a hard
/// error (the engine only writes well-formed records — a malformed interior
/// line means this reader is pointed at the wrong file).
pub fn read_breach_events(state_dir: &Path, name: &str) -> Vec<BudgetBreachEvent> {
    read_json_lines(
        &state_dir
            .join("agents")
            .join(name)
            .join("logs")
            .join("breaches.log"),
        "breach",
    )
}

/// The instance's committed transition-event log: `<root>/agents/<name>/logs/
/// instance.log`, parsed as JSON-Lines into [`TransitionEvent`] structs — the
/// SAME records the facade's `transition_events` read returns. Absent → empty;
/// torn-trailing/malformed-interior posture as [`read_breach_events`].
pub fn read_transition_events(state_dir: &Path, name: &str) -> Vec<TransitionEvent> {
    read_json_lines(
        &state_dir
            .join("agents")
            .join(name)
            .join("logs")
            .join("instance.log"),
        "transition",
    )
}

/// Parse an engine JSON-Lines event log with TORN-READ tolerance: the engine
/// appends while LIVE, so the file's LAST line can be a half-written record
/// caught mid-append. ONE trailing line that fails to parse is skipped and the
/// remainder parsed; a malformed NON-trailing line is a hard error (that is
/// not a race — that is the wrong file, or a corrupting engine). An absent
/// file reads as empty.
fn read_json_lines<T: serde::de::DeserializeOwned>(path: &Path, what: &str) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if let Some(last) = lines.last() {
        if serde_json::from_str::<T>(last).is_err() {
            lines.pop();
        }
    }
    lines
        .into_iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{what} log line does not parse: {e}\n{line}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Usage-ledger readers + received-stream projection + subscription drain
// (shared by the 7-2 acceptance suite and the 7-3 collision test — the ONE
// place the "received usage stream == committed ledger rows" comparison
// shape is defined; observation infrastructure, never a driving surface)
// ---------------------------------------------------------------------------

/// One committed `usage_events` row, projected onto EXACTLY the fields a
/// [`UsageUpdateEvent`] payload carries — the comparison shape for the
/// "the received usage stream equals the committed ledger rows field-for-field,
/// in commit order" guarantee (story 7-2). `PartialEq` so a whole stream can
/// be compared to the committed rows with one `Vec` equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedUsage {
    /// The Run the measurement was committed under (the strongest per-engine
    /// identity field — two engines' Run-id sets must be disjoint).
    pub run_id: String,
    /// The self-reported/engine-observed input tokens.
    pub input_tokens: u64,
    /// The self-reported/engine-observed output tokens.
    pub output_tokens: u64,
    /// The Metering Source wire string stamped on the row.
    pub metering_source: String,
    /// The agent-supplied, per-Run-monotonic dedup ordinal.
    pub sequence: u64,
    /// The RFC 3339 timestamp the engine stamped at commit.
    pub occurred_at: String,
}

/// The committed ledger rows for `name` under `state_dir`, in COMMIT ORDER
/// (SQLite `rowid` — the `metering.rs` reader mechanism, shared shape).
/// Read over `<state_dir>/<paths::STATE_DB_FILE>` — the engine-published
/// constant, never a hand-typed file name.
pub fn committed_usage_rows(state_dir: &Path, name: &str) -> Vec<CommittedUsage> {
    let conn = rusqlite::Connection::open(state_dir.join(ktesio_engine::paths::STATE_DB_FILE))
        .expect("open state db");
    let mut stmt = conn
        .prepare(
            "SELECT e.run_id, e.input_tokens, e.output_tokens, e.metering_source, \
             e.sequence, e.occurred_at \
             FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1 \
             ORDER BY e.rowid",
        )
        .expect("prepare the ledger read");
    let rows = stmt
        .query_map([name], |r| {
            Ok(CommittedUsage {
                run_id: r.get::<_, String>(0)?,
                input_tokens: r.get::<_, i64>(1)?.max(0) as u64,
                output_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                metering_source: r.get::<_, String>(3)?,
                sequence: r.get::<_, i64>(4)?.max(0) as u64,
                occurred_at: r.get::<_, String>(5)?,
            })
        })
        .expect("query the ledger");
    rows.map(|r| r.expect("ledger row")).collect()
}

/// Project a RECEIVED [`UsageUpdateEvent`] payload onto the same
/// [`CommittedUsage`] shape, so a subscriber's stream and the committed rows
/// compare with plain `Vec` equality (the payload FIDELITY check).
pub fn usage_from_payload(event: &UsageUpdateEvent) -> CommittedUsage {
    CommittedUsage {
        run_id: event.event.run_id.as_str().to_string(),
        input_tokens: event.event.input_tokens,
        output_tokens: event.event.output_tokens,
        metering_source: event.event.metering_source.clone(),
        sequence: event.event.sequence,
        occurred_at: event.event.occurred_at.clone(),
    }
}

/// A teardown `stop` that survives transient process-control failures.
/// Shared CI runners intermittently deny a process-group signal with
/// `EPERM: Operation not permitted` (observed on macOS runners, 2026-09-09:
/// `interleaved_instances_keep_per_instance_fifo` died in teardown while the
/// SAME stop succeeded everywhere else). Retry with a short backoff before
/// giving up — a teardown that flakes is a red PR for no code reason. Panics
/// with every accumulated error if all attempts fail.
pub fn stop_resilient(facade: &::ktesio_engine::Blocking<'_>, name: &str, window: Duration) {
    let mut errors = Vec::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        match facade.stop(name, Some(window)) {
            Ok(_) => return,
            Err(e) => errors.push(format!("attempt {}: {e}", attempt + 1)),
        }
    }
    panic!("stop {name} failed after 3 attempts: {}", errors.join("; "));
}

/// Multi-instance [`stop_resilient`]: stops each named instance, retrying on
/// transient failures; one instance's persistent failure is noted to stderr
/// and the loop CONTINUES so the remaining instances still attempt to stop.
pub fn stop_all_resilient(
    facade: &::ktesio_engine::Blocking<'_>,
    names: &[&str],
    window: Duration,
) {
    for name in names {
        stop_resilient(facade, name, window);
    }
}

/// Drain a RAW `Engine::subscribe()` receiver to its current tail with
/// `try_recv` (the story-7-2 helper for the async subscription surface).
/// Delegates to the ONE lag-accumulating drain in
/// [`crate::test_support::drain_raw_receiver`] (story 10-1): the Lagged math
/// is shared with the `EventSubscription`-typed form — the only difference
/// there is its `Closed` policy (panic, since a subscription holds its
/// engine's runtime). Exact, never racy: callers invoke this only AFTER every
/// publishing call has returned (each publish completes under the supervisor
/// lock before its facade call returns — after a committed-state wait plus
/// ONE supervisor-lock-taking read as the barrier, everything published is
/// already buffered). Returns the received events plus the TOTAL dropped
/// count if the receiver lagged past the bus capacity (`Lagged` is not an
/// event; counts ACCUMULATE with saturating adds).
pub fn drain_receiver(
    sub: &mut broadcast::Receiver<EngineEvent>,
) -> (Vec<EngineEvent>, Option<u64>) {
    crate::test_support::drain_raw_receiver(sub)
}

// ---------------------------------------------------------------------------
// Assertion helpers over OBSERVED reads (expectations pinned once)
// ---------------------------------------------------------------------------

/// The configure leg's shared assertion (§4.3): the `model` leaf the caller
/// resolved must carry the pinned value at the pinned source layer. The host
/// feeds `effective_config(...).value_display/source_label`; the CLI journey
/// feeds the parsed `kt agent config get --json` leaf.
pub fn assert_model_leaf(value: &str, source: &str) {
    assert_eq!(
        value, MODEL_VALUE,
        "the configured {MODEL_KEY} must resolve to the pinned value"
    );
    assert_eq!(
        source, MODEL_SOURCE,
        "the configured {MODEL_KEY} must resolve at the pinned source layer"
    );
}

/// The pre-start Fleet read's shared assertion (§4.1 + §4.5): the registered
/// instance's `FleetEntry` before any Run — the honest zero usage, the seeded
/// budget (token ceiling + dollar cap + action), and the truthful metering
/// source.
pub fn assert_pre_start_entry(entry: &FleetEntry) {
    assert_eq!(entry.name.as_str(), FLOW_INSTANCE);
    assert_eq!(entry.kind, MANIFEST_KIND, "the fixture kind round-trips");
    assert_eq!(
        entry.state,
        LifecycleState::Registered,
        "pre-start the instance is registered"
    );
    assert_pre_start_usage(&entry.usage);
    assert_eq!(
        entry.metering_source, METERING_SOURCE,
        "the metering source is surfaced honestly"
    );
    let budget = entry.budget.as_ref().expect("the flow arms a budget");
    assert_eq!(
        budget.cumulative_limit,
        Some(TOKEN_CEILING),
        "the token ceiling is seeded"
    );
    assert_eq!(
        budget.cumulative_remaining,
        Some(TOKEN_CEILING),
        "never metered ⇒ remaining equals the ceiling"
    );
    assert_eq!(
        budget.cumulative_cost_cap.map(Micros::get),
        Some(DOLLAR_CAP_MICROS),
        "the dollar cap is seeded as integer micros"
    );
    assert_eq!(
        budget.breach_action,
        BreachAction::Pause,
        "the armed Breach Action is pause"
    );
    assert_eq!(
        budget.estimate_label,
        Some(EstimateLabel::Estimated),
        "a Rate'd instance labels its dollar figures"
    );
}

/// The pre-start usage view's shared assertion: the honest all-zero token
/// totals plus — because the flow arms a Rate — the LABELED zero dollars
/// (never an absent figure, never an unlabeled one).
pub fn assert_pre_start_usage(usage: &UsageView) {
    assert_eq!(usage.cumulative_input_tokens, 0);
    assert_eq!(usage.cumulative_output_tokens, 0);
    assert_eq!(
        usage.cumulative_dollars.map(Micros::get),
        Some(0),
        "a Rate exists ⇒ a labeled $0, never absent"
    );
    assert_eq!(
        usage.estimate_label,
        Some(EstimateLabel::Estimated),
        "the zero is labeled estimated"
    );
}

/// The breach leg's shared assertion (§4.5): the flow arms BOTH the token
/// ceiling and the dollar cap, sized to cross on the same event, so the
/// committed records are EXACTLY ONE breach PER DIMENSION PER RUN (the per-Run,
/// dimension-keyed idempotence latches — every post-crossing event re-ran
/// enforcement and must have latched WITHIN its Run):
///
/// * the TOKEN breach — scope `cumulative`, limit [`TOKEN_CEILING`],
///   `observed >= limit` (events 4–5 race the suspension — the honest bound),
///   the pause action, the honest metering stamp, and NO dollar fields (a
///   token breach record carries none);
/// * the DOLLAR breach — the same scope at [`DOLLAR_CAP_MICROS`], labeled
///   `estimated` (the Rate-configured honesty), no fabricated precision.
///
/// The TOKEN breach wins the pause (the single enforcement site evaluates
/// token ceilings first; the dollar breach finds the instance already paused
/// and records only) — pinned by the lifecycle assertion below.
///
/// **Why PER RUN, not one per dimension globally (story 11-1, AI-44):** the
/// breach latch resets when a new Run starts (the ratified story-3-2 design —
/// a fresh Run gets a clean latch), and AI-44 re-evaluates budgets right
/// after ADOPTION, so a flow that adopts an already-over-budget run (the
/// `kt` CLI flow does: `stop` adopts the paused process) legitimately records
/// ONE token + ONE dollar breach for the SAME crossing in the new Run's
/// latch. The detailed field assertions below run on the LOG-ORDER FIRST
/// token breach record (the crossing the flow performed first) and on its
/// SAME-RUN dollar counterpart (matched by that record's `run_id`) — every
/// Run's pair, whichever run it is, is pinned to exactly one per dimension.
pub fn assert_flow_breaches(events: &[BudgetBreachEvent]) {
    let mut runs: Vec<&str> = events.iter().map(|b| b.run_id.as_str()).collect();
    runs.sort_unstable();
    runs.dedup();
    assert!(
        !runs.is_empty(),
        "at least one Run's breach pair must be recorded: {events:?}"
    );
    for run in &runs {
        let tokens: Vec<&BudgetBreachEvent> = events
            .iter()
            .filter(|b| b.dimension == BreachDimension::Tokens && b.run_id == *run)
            .collect();
        let dollars: Vec<&BudgetBreachEvent> = events
            .iter()
            .filter(|b| b.dimension == BreachDimension::Dollars && b.run_id == *run)
            .collect();
        assert_eq!(
            tokens.len(),
            1,
            "exactly one TOKEN breach per Run for a single crossing (run {run}); got {}: {events:?}",
            tokens.len()
        );
        assert_eq!(
            dollars.len(),
            1,
            "exactly one DOLLAR breach per Run for a single crossing (the independent \
             dimension latch; run {run}); got {}: {events:?}",
            dollars.len()
        );
    }
    assert_eq!(
        events.len(),
        2 * runs.len(),
        "the per-Run dimension records are the whole breach log: {events:?}"
    );

    let tokens: Vec<&BudgetBreachEvent> = events
        .iter()
        .filter(|b| b.dimension == BreachDimension::Tokens)
        .collect();
    let dollars: Vec<&BudgetBreachEvent> = events
        .iter()
        .filter(|b| b.dimension == BreachDimension::Dollars)
        .collect();
    let b = tokens[0];
    assert_eq!(
        b.scope,
        BreachScope::Cumulative,
        "the cumulative scope tripped"
    );
    assert_eq!(
        b.limit, TOKEN_CEILING,
        "the breach names the pinned ceiling"
    );
    assert!(
        b.observed >= TOKEN_CEILING,
        "observed {} must be >= the ceiling",
        b.observed
    );
    assert_eq!(
        b.action,
        BreachAction::Pause,
        "the armed pause action drove the breach"
    );
    assert_eq!(
        b.metering_source, METERING_SOURCE,
        "the breach event stamps the metering source"
    );
    assert!(
        b.dollar_limit.is_none() && b.dollar_observed.is_none(),
        "a token breach carries no dollar fields: {b:?}"
    );

    let d = dollars
        .iter()
        .find(|x| x.run_id == b.run_id)
        .expect("the first Run's dollar breach (pinned per-Run above)");
    assert_eq!(d.scope, BreachScope::Cumulative);
    assert_eq!(
        d.dollar_limit.map(Micros::get),
        Some(DOLLAR_CAP_MICROS),
        "the dollar breach names the pinned cap"
    );
    assert!(
        d.dollar_observed.map(Micros::get).unwrap_or(0) >= DOLLAR_CAP_MICROS,
        "the observed cost must be >= the cap: {:?}",
        d.dollar_observed
    );
    assert_eq!(
        d.estimate_label,
        Some(EstimateLabel::Estimated),
        "the dollar breach's figures are labeled estimated"
    );
    assert_eq!(
        d.action,
        BreachAction::Pause,
        "both dimensions record the armed action"
    );
}

/// The lifecycle leg's shared assertion: the recorded transitions must carry a
/// `→ paused` edge whose cause is the TOKEN `BudgetExceeded` at the pinned
/// ceiling (the lifecycle log itself explains WHY — the enforcement chain's
/// last hop: usage event → BudgetEvaluator → pause with the cause attached).
pub fn assert_paused_transition_budget_exceeded(events: &[TransitionEvent]) {
    let paused = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a → paused transition was recorded");
    match &paused.cause {
        TransitionCause::BudgetExceeded {
            scope,
            dimension,
            limit,
            observed,
            ..
        } => {
            assert_eq!(*scope, BreachScope::Cumulative);
            assert_eq!(*dimension, BreachDimension::Tokens);
            assert_eq!(*limit, TOKEN_CEILING);
            assert!(
                *observed >= TOKEN_CEILING,
                "observed {observed} >= the ceiling"
            );
        }
        other => panic!("the paused transition must carry BudgetExceeded, got {other:?}"),
    }
}

/// The post-breach Fleet read's shared assertion (the honest enforcement
/// readback): the instance is `paused`, the breached scopes report `$0`/`0`
/// remaining (saturating, never negative), the token total is at least the
/// ceiling, and the derived dollar cost equals the token total EXACTLY (the
/// unit Rate prices every row at 1 micro/token — a per-row sum, never a
/// re-priced guess). Mid-flow the current-Run totals equal the cumulative ones
/// (a single Run spans them).
pub fn assert_paused_entry(entry: &FleetEntry) {
    assert_eq!(
        entry.state,
        LifecycleState::Paused,
        "the breach fired the default pause action"
    );
    assert_eq!(entry.metering_source, METERING_SOURCE);
    let usage = &entry.usage;
    let total = usage.cumulative_total_tokens();
    assert!(
        total >= TOKEN_CEILING,
        "the committed total {total} must be >= the ceiling"
    );
    assert_eq!(
        usage.cumulative_dollars.map(Micros::get),
        Some(total as i64),
        "at the unit Rate the derived cost equals the token total exactly"
    );
    assert_eq!(
        usage.estimate_label,
        Some(EstimateLabel::Estimated),
        "the dollar figure is labeled"
    );
    assert_eq!(
        usage.cumulative_total_tokens(),
        usage
            .current_run_input_tokens
            .saturating_add(usage.current_run_output_tokens),
        "one Run spans the flow so far — current-Run == cumulative"
    );
    let budget = entry.budget.as_ref().expect("the flow arms a budget");
    assert_eq!(budget.cumulative_limit, Some(TOKEN_CEILING));
    assert_eq!(
        budget.cumulative_remaining,
        Some(0),
        "the breached scope reports 0 remaining, never negative"
    );
    assert_eq!(
        budget.cumulative_dollars_remaining.map(Micros::get),
        Some(0),
        "the breached dollar scope reports $0 remaining, never negative"
    );
}

/// The post-stop usage view's shared assertion: the ledger the stop's terminal
/// drain froze is honestly a RANGE, never pinned as exact. The breaching
/// drain can land at event 3, 4, or 5 of the fixture's 20 ms-cadence emission
/// — a loaded host stretches the cadence, and the breach's suspension is
/// immediate at the breaching drain, so whatever the emitter had flushed by
/// then (at least the crossing event's worth: [`TOKEN_CEILING`]; at most the
/// whole batch: [`EMIT_TOTAL_TOKENS`]) is what the terminal drain commits.
/// What IS exact about whatever landed: the input/output split (the fixture's
/// fixed 10/20 sentinels — output is exactly twice input, and the two sum to
/// the total), the derived dollars equal the tokens exactly (the unit Rate —
/// a per-row sum, never a re-priced guess), the `estimated` label, and the
/// terminal Run's zeroed current-Run scope.
pub fn assert_stopped_usage(usage: &UsageView) {
    let input = usage.cumulative_input_tokens;
    let output = usage.cumulative_output_tokens;
    let total = usage.cumulative_total_tokens();
    assert!(
        (MIN_STOPPED_INPUT_TOKENS..=MAX_STOPPED_INPUT_TOKENS).contains(&input),
        "committed input {input} must be the crossing event's minimum \
         {MIN_STOPPED_INPUT_TOKENS}..={MAX_STOPPED_INPUT_TOKENS} (3-5 events race \
         the suspension)"
    );
    assert!(
        (MIN_STOPPED_OUTPUT_TOKENS..=MAX_STOPPED_OUTPUT_TOKENS).contains(&output),
        "committed output {output} must be {MIN_STOPPED_OUTPUT_TOKENS}..=\
         {MAX_STOPPED_OUTPUT_TOKENS} (3-5 events race the suspension)"
    );
    assert_eq!(
        output,
        2 * input,
        "the fixed 10/20 sentinels hold for whatever landed: output is exactly \
         twice input"
    );
    assert_eq!(
        input.saturating_add(output),
        total,
        "the split sums to the committed total"
    );
    assert_eq!(
        usage.cumulative_dollars.map(Micros::get),
        Some(total as i64),
        "at the unit Rate the frozen ledger costs exactly its tokens — \
         labeling consistent with whatever landed"
    );
    assert_eq!(
        usage.estimate_label,
        Some(EstimateLabel::Estimated),
        "the dollar figure stays labeled"
    );
    assert_eq!(
        usage
            .current_run_input_tokens
            .saturating_add(usage.current_run_output_tokens),
        0,
        "a terminal Run reports no current-Run totals"
    );
}

/// The post-stop Fleet read's shared assertion: the instance is `stopped` and
/// the frozen ledger (honestly a range — see [`assert_stopped_usage`]) plus
/// the breached budget surfaces hold.
pub fn assert_stopped_entry(entry: &FleetEntry) {
    assert_eq!(
        entry.state,
        LifecycleState::Stopped,
        "the flow's stop leg landed the terminal state"
    );
    assert_stopped_usage(&entry.usage);
    let budget = entry.budget.as_ref().expect("the flow arms a budget");
    assert_eq!(budget.cumulative_limit, Some(TOKEN_CEILING));
    assert_eq!(budget.cumulative_remaining, Some(0));
    assert_eq!(
        budget.cumulative_dollars_remaining.map(Micros::get),
        Some(0)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_agree_with_each_other() {
        // The string/numeric twin pairs must never drift: the config-string
        // forms parse back to exactly the numeric expectations the assertion
        // helpers pin (the breach math depends on BOTH being the same number).
        assert_eq!(
            TOKEN_CEILING_STR.parse::<u64>().unwrap(),
            TOKEN_CEILING,
            "the ceiling's config string and numeric expectation agree"
        );
        assert_eq!(
            DOLLAR_CAP_DOLLARS
                .parse::<f64>()
                .map(|d| (d * 1_000_000.0).round() as i64)
                .unwrap_or(0),
            DOLLAR_CAP_MICROS,
            "the cap's dollar string and micros expectation agree"
        );
        assert_eq!(
            TOKEN_CEILING % TOKENS_PER_EVENT,
            0,
            "the ceiling is a whole number of events"
        );
        assert_eq!(
            MIN_STOPPED_EVENTS * TOKENS_PER_EVENT,
            TOKEN_CEILING,
            "the minimum committed events reach exactly the ceiling"
        );
        assert_eq!(
            EMIT_TOTAL_TOKENS,
            MAX_STOPPED_INPUT_TOKENS + MAX_STOPPED_OUTPUT_TOKENS,
            "the full batch is the input/output split summed"
        );
    }

    #[test]
    fn timing_constants_agree_with_each_other() {
        // The stop window's two forms (the facade Duration and the CLI's
        // whole seconds) are ONE value: the CLI journey builds its
        // `--timeout` argument from the same const the host facade passes.
        assert_eq!(STOP_WINDOW.as_secs(), 0, "a suspended process cannot act on a graceful signal, so every stop leg skips the window");
    }

    #[test]
    fn flow_config_pairs_carry_the_pinned_keys_and_values() {
        let pairs = flow_config_pairs();
        assert_eq!(pairs[0], (MODEL_KEY, MODEL_VALUE));
        assert!(pairs.contains(&("budget.tokens.cumulative", TOKEN_CEILING_STR)));
        assert!(pairs.contains(&("cost.rate.input", RATE_DOLLARS)));
        assert!(pairs.contains(&("cost.rate.output", RATE_DOLLARS)));
        assert!(pairs.contains(&("budget.dollars.cumulative", DOLLAR_CAP_DOLLARS)));
        assert!(pairs.contains(&("budget.breach_action", BREACH_ACTION)));
        assert_eq!(pairs.len(), 6, "exactly the flow's six keys, no more");
    }

    #[test]
    fn committed_log_readers_treat_an_absent_log_as_empty() {
        // No breach/transition has ever been recorded under this root: both
        // committed-log readers return an EMPTY vector, not an error — the
        // pre-breach poll leg depends on the honest absence.
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_breach_events(dir.path(), "never-registered").is_empty());
        assert!(read_transition_events(dir.path(), "never-registered").is_empty());
        assert_eq!(committed_state(dir.path(), "never-registered"), None);
    }

    #[test]
    fn committed_log_readers_tolerate_a_torn_trailing_line_only() {
        // The engine appends while LIVE, so the LAST line can be a half-written
        // record caught mid-append. The reader must skip ONE trailing bad line
        // and parse the remainder; a malformed NON-trailing line is a hard
        // error (wrong file, not a race).
        let dir = tempfile::TempDir::new().unwrap();
        let layout = dir.path().join("agents").join("probe").join("logs");
        std::fs::create_dir_all(&layout).unwrap();
        let log = layout.join("breaches.log");
        let good = r#"{"schema_version":1,"instance":"probe","run_id":"run-1","scope":"cumulative","dimension":"tokens","limit":90,"observed":90,"action":"pause","metering_source":"self-reported","at":"2026-09-07T00:00:00Z"}"#;
        // Torn trailing append (no newline after the partial line): skipped.
        std::fs::write(&log, format!("{good}\n{{\"schema_version\":1,\"inst")).unwrap();
        let events = read_breach_events(dir.path(), "probe");
        assert_eq!(
            events.len(),
            1,
            "the torn trailing line is skipped, the good record parsed"
        );
        // A malformed NON-trailing line hard-errors (the panic is the contract).
        std::fs::write(&log, format!("{{\"not\":\"a breach event\"}}\n{good}\n")).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            read_breach_events(dir.path(), "probe")
        }));
        assert!(result.is_err(), "a malformed interior line must hard-error");
    }

    #[test]
    fn write_flow_manifest_carries_the_pinned_shape() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest_dir = write_flow_manifest(dir.path());
        assert_eq!(manifest_dir, dir.path().to_path_buf());
        let text =
            std::fs::read_to_string(manifest_dir.join("adapter.toml")).expect("manifest exists");
        assert!(text.contains(&format!("contract_version = \"{CONTRACT_VERSION}\"")));
        assert!(text.contains(&format!("kind = \"{MANIFEST_KIND}\"")));
        assert!(text.contains(&format!("source = \"{METERING_SOURCE}\"")));
        assert!(text.contains(&format!("--emit-usage\", \"{EMIT_EVENTS}\"")));
        assert!(text.contains(&format!("env = \"{MODEL_ENV_VAR}\"")));
        // Pause + interaction guaranteed on all three modeled OSes: the
        // default pause Breach Action lands the committed `paused` state
        // deterministically everywhere (a real SIGSTOP freeze on Unix;
        // cooperative best-effort on Windows — hence the usage RANGE
        // assertions, never exact counts).
        for os in ["linux", "macos", "windows"] {
            assert!(
                text.matches(&format!("{os} = \"guaranteed\"")).count() >= 2,
                "{os} declares both capabilities guaranteed"
            );
        }
    }
}
