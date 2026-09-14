//! Integration test for the story-3-5 Fleet-WIDE aggregate (AC-A) — `totals ==
//! ledger` at Fleet scope, driven end-to-end through the PUBLIC async [`Engine`] +
//! its background reaper (spine AD-2/AD-6) by spawning real `fake_agent
//! --emit-usage` instances so genuine self-reported tokens land in the durable
//! ledger and the Fleet-wide `FleetTotals` aggregate is computed over the same rows
//! the CLI would read.
//!
//! ## Robust, cross-OS by construction (retro AI-35/37/38/49)
//!
//! Like `tests/metering.rs` / `tests/cost.rs`, this keeps a SINGLE in-process
//! `Engine` alive for the whole test — NO cross-lifetime process survival, NO
//! `OsId`-gated skip anywhere, and NO wall-clock-timing-sensitive assertion. The
//! aggregate is pure, the read is over durable committed state; determinism comes
//! from asserting on COMMITTED LEDGER STATE — the `fake_agent` emits a KNOWN number
//! of usage events with FIXED token sentinels, and the test POLLS the committed
//! ledger row count until the expected rows land, then asserts the Fleet `totals`
//! equals the EXACT sum of the per-instance ledger totals (AC-A) and that the
//! label/partial-ness is honest. The bulk of the honesty-rule coverage lives in the
//! PURE unit tests (`domain::fleet`); this proves the end-to-end WIRING + `totals ==
//! ledger` at Fleet scope.
//!
//! ## The known arithmetic (determinism lever)
//!
//! The `fake_agent --emit-usage` sentinels are 10 input + 20 output tokens per event
//! (`USAGE_INPUT`/`USAGE_OUTPUT`). A `$1.00/1M` Rate on both directions is 1
//! micro/token, so each event costs `10 + 20 = 30` micros; N events cost `30 × N`.

use std::path::Path;
use std::time::{Duration, Instant};

use ktesio_engine::{AdapterRef, Engine, EstimateLabel, FleetListing, RestartPolicy};
use tempfile::TempDir;

/// The token sentinels `fake_agent --emit-usage` stamps on every event.
const USAGE_INPUT: u64 = 10;
const USAGE_OUTPUT: u64 = 20;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = ktesio_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// The number of `usage_events` rows for `name` (committed ledger state), via a
/// direct read-only connection to the same state DB the engine commits to.
fn usage_row_count(state_dir: &Path, name: &str) -> u64 {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM usage_events e \
         JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// Poll the committed usage-row count for `name` until it reaches `expected`,
/// bounded — deterministic committed state, NOT a wall-clock guess.
fn wait_for_usage_rows(state_dir: &Path, name: &str, expected: u64, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        if usage_row_count(state_dir, name) >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} committed usage rows for '{name}' (have {})",
            usage_row_count(state_dir, name)
        );
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[test]
fn fleet_totals_equal_the_ledger_sum_across_instances_and_flag_partial() {
    // AC-A (the headline): register + meter MULTIPLE instances — one WITH a Rate +
    // accrued usage, one metered WITHOUT a Rate, one never metered — then assert the
    // Fleet-wide `FleetTotals` (composed exactly as `kt agent list` composes it, via
    // `FleetListing::new` over the engine's `fleet()` rows) equals the EXACT sum of the
    // per-instance ledger totals (tokens across ALL three; dollars only the Rate'd
    // one), is labeled `estimated`, and is flagged `dollars_partial` because a metered
    // instance had no Rate. `totals == ledger` at Fleet scope.
    let state = TempDir::new().unwrap();
    let rated_m = TempDir::new().unwrap();
    let free_m = TempDir::new().unwrap();

    // "rated": 3 events, a $1.00/1M Rate on both directions → each event 30 micros.
    write_fake_manifest(
        rated_m.path(),
        "rated",
        &["--emit-usage", "3", "--linger-ms", "600000"],
    );
    // "free": 2 events, NO Rate → real tokens, unknown dollar cost (partial driver).
    write_fake_manifest(
        free_m.path(),
        "free",
        &["--emit-usage", "2", "--linger-ms", "600000"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("rated", &AdapterRef::Manifest(rated_m.path().to_path_buf()))
        .unwrap();
    facade
        .register_with_adapter("free", &AdapterRef::Manifest(free_m.path().to_path_buf()))
        .unwrap();
    // "idle": a never-metered instance (registered, never started) — its honest 0 must
    // be COUNTED in the token sum (zero-not-absent), never omitted.
    facade.register("idle", "mock").unwrap();

    // A $1.00/1M Rate on the "rated" instance only (1 micro/token).
    facade
        .set_config("rated", "cost.rate.input", "1.00")
        .unwrap();
    facade
        .set_config("rated", "cost.rate.output", "1.00")
        .unwrap();

    facade.start("rated").unwrap();
    facade.start("free").unwrap();

    // Wait for the KNOWN committed row counts (deterministic — not a sleep).
    wait_for_usage_rows(state.path(), "rated", 3, Duration::from_secs(30));
    wait_for_usage_rows(state.path(), "free", 2, Duration::from_secs(30));

    // Compose the Fleet-wide aggregate EXACTLY as the CLI does: engine `fleet()` rows
    // → `FleetListing::new` (which computes `FleetTotals::from_entries` purely).
    let entries = facade.fleet().unwrap();
    let listing = FleetListing::new(entries);
    let totals = &listing.totals;

    // The per-instance ledger totals (the source of truth) — read straight off the
    // composed rows (these equal the ledger exactly, the 3-1/3-3 discipline).
    let sum_input: u64 = listing
        .instances
        .iter()
        .map(|e| e.usage.cumulative_input_tokens)
        .sum();
    let sum_output: u64 = listing
        .instances
        .iter()
        .map(|e| e.usage.cumulative_output_tokens)
        .sum();
    // Tokens: the Fleet total EQUALS the sum of the per-instance ledger totals (AC-A).
    assert_eq!(
        totals.total_input_tokens, sum_input,
        "Fleet input tokens must equal the per-instance ledger sum"
    );
    assert_eq!(totals.total_output_tokens, sum_output);
    // And the concrete arithmetic: rated 3 events + free 2 events + idle 0 = 5 events.
    assert_eq!(totals.total_input_tokens, 5 * USAGE_INPUT);
    assert_eq!(totals.total_output_tokens, 5 * USAGE_OUTPUT);

    // Dollars: the Fleet total EQUALS the sum of ONLY the Rate'd instance's derived
    // cost (3 events × 30 micros = 90), labeled `estimated`.
    let rated_dollars = listing
        .instances
        .iter()
        .find(|e| e.name.as_str() == "rated")
        .unwrap()
        .usage
        .cumulative_dollars
        .expect("the Rate'd instance has a derived cost");
    assert_eq!(rated_dollars.get(), 3 * 30, "3 events × 30 micros");
    assert_eq!(
        totals.total_dollars.map(|m| m.get()),
        Some(rated_dollars.get()),
        "the Fleet dollar total equals the sum of the priced instances (only 'rated')"
    );
    assert_eq!(totals.estimate_label, Some(EstimateLabel::Estimated));
    // ...and it is a labeled LOWER BOUND — "free" is metered but unpriced (AC5/SM-C3).
    assert!(
        totals.dollars_partial,
        "a metered-but-unpriced instance must flag the dollar total partial"
    );

    // Teardown.
    let _ = facade.stop("rated", Some(Duration::from_secs(5)));
    let _ = facade.stop("free", Some(Duration::from_secs(5)));
}

#[test]
fn fleet_totals_are_complete_and_labeled_when_every_metered_instance_is_rated() {
    // AC-A + AC5 complete arm: when EVERY metered instance has a Rate, the Fleet dollar
    // total is the exact labeled sum (NOT partial). Two Rate'd instances, N + M events.
    let state = TempDir::new().unwrap();
    let a_m = TempDir::new().unwrap();
    let b_m = TempDir::new().unwrap();
    write_fake_manifest(
        a_m.path(),
        "a",
        &["--emit-usage", "4", "--linger-ms", "600000"],
    );
    write_fake_manifest(
        b_m.path(),
        "b",
        &["--emit-usage", "2", "--linger-ms", "600000"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("a", &AdapterRef::Manifest(a_m.path().to_path_buf()))
        .unwrap();
    facade
        .register_with_adapter("b", &AdapterRef::Manifest(b_m.path().to_path_buf()))
        .unwrap();
    for name in ["a", "b"] {
        facade.set_config(name, "cost.rate.input", "1.00").unwrap();
        facade.set_config(name, "cost.rate.output", "1.00").unwrap();
    }
    facade.start("a").unwrap();
    facade.start("b").unwrap();
    wait_for_usage_rows(state.path(), "a", 4, Duration::from_secs(30));
    wait_for_usage_rows(state.path(), "b", 2, Duration::from_secs(30));

    let listing = FleetListing::new(facade.fleet().unwrap());
    let totals = &listing.totals;
    // 6 events total × (10 in, 20 out).
    assert_eq!(totals.total_input_tokens, 6 * USAGE_INPUT);
    assert_eq!(totals.total_output_tokens, 6 * USAGE_OUTPUT);
    // Dollars: 6 events × 30 micros = 180, labeled, COMPLETE (not partial — both priced).
    assert_eq!(totals.total_dollars.map(|m| m.get()), Some(6 * 30));
    assert_eq!(totals.estimate_label, Some(EstimateLabel::Estimated));
    assert!(
        !totals.dollars_partial,
        "all metered instances priced ⇒ complete"
    );

    let _ = facade.stop("a", Some(Duration::from_secs(5)));
    let _ = facade.stop("b", Some(Duration::from_secs(5)));
}

#[test]
fn an_empty_fleet_has_all_zero_totals_and_absent_dollars() {
    // AC9: an empty Fleet → an all-zero token total, dollars honestly absent (None),
    // not partial. The document is still valid (a `FleetListing` with an empty rows
    // vec + a zero `totals`).
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();

    let listing = FleetListing::new(facade.fleet().unwrap());
    assert!(listing.instances.is_empty());
    assert_eq!(listing.totals.total_input_tokens, 0);
    assert_eq!(listing.totals.total_output_tokens, 0);
    assert_eq!(listing.totals.total_dollars, None);
    assert_eq!(listing.totals.estimate_label, None);
    assert!(!listing.totals.dollars_partial);
}

#[test]
fn fleet_batch_reads_spawn_records_and_each_entry_keeps_its_own_ai16() {
    // AI-16 behavior preservation (the N+1 kill): `fleet()` now reads the
    // write-ahead spawn records in ONE batched, lock-held query keyed by name —
    // instead of one per-instance query — and threads the map into the row
    // composition. With MULTIPLE instances carrying DIFFERENT records, every
    // entry must still surface ITS OWN restart count/policy/cause (never
    // cross-wired, never defaulted), the Fleet row for a name must equal the
    // single-instance read (`fleet_entry`, AI-15) for the same name, and the
    // usage/cost surfaces ride the same rows as before. Runtime fields are
    // seeded directly in the state DB (deterministic — no live processes).
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    for name in ["aa", "bb", "cc"] {
        facade.register(name, "mock").unwrap();
    }

    // Seed a DISTINCT spawn record per instance straight into the store.
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    for (name, count, policy, cause) in [
        ("aa", 1, "on-failure", "crashed with code 1"),
        ("bb", 4, "never", "crash-loop: 5 consecutive failures"),
        ("cc", 0, "on-failure", "launched clean"),
    ] {
        let id: i64 = conn
            .query_row(
                "SELECT id FROM agent_instances WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO agent_runtime \
             (instance_id, pid, start_time, restart_policy, restart_count, last_known_cause) \
             VALUES (?1, 0, 0, ?2, ?3, ?4)",
            rusqlite::params![id, policy, count, cause],
        )
        .unwrap();
    }
    drop(conn);

    // ONE fleet read — the batched path under test.
    let entries = facade.fleet().unwrap();
    assert_eq!(entries.len(), 3, "one row per instance");
    for entry in &entries {
        let expected = match entry.name.as_str() {
            "aa" => (1, RestartPolicy::OnFailure, Some("crashed with code 1")),
            "bb" => (
                4,
                RestartPolicy::Never,
                Some("crash-loop: 5 consecutive failures"),
            ),
            "cc" => (0, RestartPolicy::OnFailure, Some("launched clean")),
            other => panic!("unexpected fleet row: {other}"),
        };
        assert_eq!(entry.restart_count, expected.0, "{}: own count", entry.name);
        assert_eq!(
            entry.restart_policy, expected.1,
            "{}: own policy",
            entry.name
        );
        assert_eq!(
            entry.failed_cause.as_deref(),
            expected.2,
            "{}: own cause",
            entry.name
        );
        // The single-instance read (AI-15's facade method) agrees with the
        // batched Fleet row, field for field.
        let single = facade
            .fleet_entry(entry.name.as_str())
            .unwrap_or_else(|e| panic!("fleet_entry({}) failed: {e}", entry.name));
        assert_eq!(single.restart_count, entry.restart_count);
        assert_eq!(single.restart_policy, entry.restart_policy);
        assert_eq!(single.failed_cause, entry.failed_cause);
        assert_eq!(single.state, entry.state);
        assert_eq!(
            single.usage.cumulative_input_tokens,
            entry.usage.cumulative_input_tokens
        );
        assert_eq!(single.metering_source, entry.metering_source);
        // AI-15 (loop 2 pin): byte-identity across ALL fields — the single-
        // instance `show` read and the batched `list` row serialize to the
        // IDENTICAL JSON document. The field asserts above pin the subset we
        // care about with good failure messages; this pins the WHOLE entry, so
        // a field added to one composition path but not the other fails here
        // instead of drifting silently.
        let show = serde_json::to_value(&single).unwrap();
        let list = serde_json::to_value(entry).unwrap();
        assert_eq!(
            show, list,
            "the `show` entry and its matching `list` row must be identical across \
             ALL fields ({}): show={show} list={list}",
            entry.name
        );
    }
}
