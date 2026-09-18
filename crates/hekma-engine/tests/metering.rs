//! Integration tests for story-3-1 self-reported usage metering (AC-A / AC-B /
//! AC-C), driven end-to-end through the PUBLIC async [`Engine`] + its background
//! reaper cadence (spine AD-2/AD-13) — spawning the REAL `fake_agent` with
//! `--emit-usage` so genuine self-reported usage sentinel lines are captured and
//! ingested into the durable Usage Ledger, not mocked.
//!
//! ## Robust, cross-OS by construction (retro AI-35/37/38)
//!
//! These tests keep the engine ALIVE for the whole test (a single in-process
//! `Engine` — like `tests/lifecycle.rs`), so they need NO cross-lifetime process
//! survival and run identically on Linux, macOS, and Windows — there is NO
//! `OsId`-gated skip anywhere in this file. Determinism comes from asserting on
//! COMMITTED LEDGER STATE: the `fake_agent` emits a KNOWN number of usage events
//! with FIXED token sentinels, and each test POLLS the SQLite ledger (via the
//! engine reads / a direct read-only connection) until the expected committed row
//! count is reached — never a wall-clock sleep against a side file (the fragile
//! `_live` dump-file pattern the Epic-2 retro flagged). The engine's reaper
//! (~250ms) drains the captured output into the ledger transactionally, so the DB
//! is the deterministic source of truth.

use std::path::Path;
use std::time::{Duration, Instant};

use hekma_engine::{AdapterRef, Engine, LifecycleState};
use tempfile::TempDir;

/// The token sentinels the `fake_agent --emit-usage` emitter stamps on every
/// event (mirrors its `USAGE_INPUT_TOKENS` / `USAGE_OUTPUT_TOKENS`), so ledger
/// totals are exact-match assertions.
const USAGE_INPUT: u64 = 10;
const USAGE_OUTPUT: u64 = 20;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hekma_conformance::fake_agent_bin();
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

/// Poll the committed Usage-Ledger event count for `name` until it reaches
/// `expected` (deterministic committed state — NOT a wall-clock guess), bounded.
/// Reads through the engine's public `fleet()` cumulative totals is not enough
/// (we want the ROW count), so read the count via a direct read-only connection to
/// the same state DB the engine writes transactionally.
fn wait_for_usage_rows(state_dir: &Path, name: &str, expected: u64, within: Duration) -> u64 {
    let deadline = Instant::now() + within;
    loop {
        let count = usage_row_count(state_dir, name);
        if count >= expected {
            return count;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} committed usage rows for '{name}' (have {count})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The number of `usage_events` rows for `name`, read via a direct read-only
/// connection to the state DB (the same file the engine commits to). Used to
/// assert on committed ledger STATE deterministically.
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

/// The DISTINCT `run_id`s that appear on `name`'s ledger rows, ordered — used to
/// prove a restart opens a fresh Run (AC-B).
fn distinct_run_ids(state_dir: &Path, name: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).expect("open state db");
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT e.run_id FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1 ORDER BY e.run_id",
        )
        .unwrap();
    let rows = stmt.query_map([name], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn self_reported_usage_lands_in_the_ledger_under_the_run_id() {
    // AC-A: usage batches a `self-reported` agent emits land in the append-only
    // usage_events table with the AD-7 minimum shape (one row per event) under the
    // Run's id, and the Fleet-detail totals equal the ledger exactly.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Emit exactly 3 usage events, then linger so only our stop ends it.
    write_fake_manifest(
        manifest.path(),
        "meter",
        &["--emit-usage", "3", "--linger-ms", "600000"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "meter",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    let started = facade.start("meter").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Wait for all 3 events to be COMMITTED to the ledger (the reaper drains the
    // captured output). Deterministic: we wait for the known row count, not a sleep.
    let count = wait_for_usage_rows(state.path(), "meter", 3, Duration::from_secs(30));
    assert_eq!(count, 3, "exactly 3 usage rows (one per emitted event)");

    // Every row carries the SAME Run id (one Run so far).
    let runs = distinct_run_ids(state.path(), "meter");
    assert_eq!(runs.len(), 1, "all events share the one Run's id: {runs:?}");
    assert!(runs[0].starts_with("run-"), "run id shape: {}", runs[0]);

    // The Fleet-detail usage totals EQUAL the ledger exactly (FR-22 discipline):
    // 3 events × (10 in, 20 out).
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "meter").unwrap();
    assert_eq!(entry.usage.cumulative_input_tokens, 3 * USAGE_INPUT);
    assert_eq!(entry.usage.cumulative_output_tokens, 3 * USAGE_OUTPUT);
    // The current Run's totals equal the cumulative (a single Run, still running).
    assert_eq!(entry.usage.current_run_input_tokens, 3 * USAGE_INPUT);
    assert_eq!(entry.usage.current_run_output_tokens, 3 * USAGE_OUTPUT);
    // The active Metering Source is visible in Fleet detail (AC-C).
    assert_eq!(entry.metering_source, "self-reported");

    // Teardown.
    let _ = facade.stop("meter", Some(Duration::from_secs(5)));
}

#[test]
fn a_replayed_batch_does_not_double_count() {
    // AC-A (the security-of-billing heart): a DELAYED/replayed batch (the agent
    // re-emits `sequence 0`) must NOT double-count — the ledger total is unchanged.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Emit 3 events THEN re-emit sequence 0 once (the replay). The ledger must end
    // with exactly 3 rows and totals for 3 events, never 4.
    write_fake_manifest(
        manifest.path(),
        "replay",
        &[
            "--emit-usage",
            "3",
            "--replay-usage",
            "--linger-ms",
            "600000",
        ],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "replay",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("replay").unwrap();

    // Wait for the 3 distinct events to land.
    wait_for_usage_rows(state.path(), "replay", 3, Duration::from_secs(30));
    // Give the reaper several more polls so the replayed sequence-0 line is
    // definitely drained + classified (a duplicate → no-op). A generous settle.
    std::thread::sleep(Duration::from_millis(800));

    // The ledger still has EXACTLY 3 rows — the replay was recognized, not inserted.
    let count = usage_row_count(state.path(), "replay");
    assert_eq!(
        count, 3,
        "a replayed batch must not add a row (no double-count)"
    );
    // And the totals reflect 3 events, not 4 (the replay did not inflate them).
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "replay").unwrap();
    assert_eq!(
        entry.usage.cumulative_input_tokens,
        3 * USAGE_INPUT,
        "the replayed batch must not inflate the token total"
    );
    assert_eq!(entry.usage.cumulative_output_tokens, 3 * USAGE_OUTPUT);

    let _ = facade.stop("replay", Some(Duration::from_secs(5)));
}

#[test]
fn a_restart_opens_a_fresh_run_and_per_run_totals_do_not_bleed() {
    // AC-B: a Run is `starting`→next terminal; a stop→start opens a NEW Run with its
    // own id, and per-run totals never bleed across the boundary. Start (emit 2),
    // stop, start again (emit 2): the ledger carries TWO distinct run ids, and the
    // current-Run totals after the second start reflect ONLY the second Run.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(
        manifest.path(),
        "runs",
        &["--emit-usage", "2", "--linger-ms", "600000"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("runs", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();

    // Run 1: start, wait for its 2 events, then stop (closes the Run).
    facade.start("runs").unwrap();
    wait_for_usage_rows(state.path(), "runs", 2, Duration::from_secs(30));
    facade.stop("runs", Some(Duration::from_secs(5))).unwrap();
    let after_run1 = distinct_run_ids(state.path(), "runs");
    assert_eq!(after_run1.len(), 1, "one Run so far: {after_run1:?}");

    // Run 2: start again (a NEW Run), wait for its 2 events (total 4 rows now).
    facade.start("runs").unwrap();
    wait_for_usage_rows(state.path(), "runs", 4, Duration::from_secs(30));

    // TWO distinct Run ids now — the restart opened a fresh Run (AC-B).
    let runs = distinct_run_ids(state.path(), "runs");
    assert_eq!(
        runs.len(),
        2,
        "a restart must open a fresh Run (distinct id): {runs:?}"
    );

    // The current Run's totals reflect ONLY the second Run's 2 events — they do NOT
    // bleed in Run 1's usage. Cumulative is all 4 events.
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "runs").unwrap();
    assert_eq!(
        entry.usage.current_run_input_tokens,
        2 * USAGE_INPUT,
        "per-run totals must not bleed across the restart"
    );
    assert_eq!(entry.usage.current_run_output_tokens, 2 * USAGE_OUTPUT);
    assert_eq!(
        entry.usage.cumulative_input_tokens,
        4 * USAGE_INPUT,
        "cumulative spans both Runs"
    );
    assert_eq!(entry.usage.cumulative_output_tokens, 4 * USAGE_OUTPUT);

    let _ = facade.stop("runs", Some(Duration::from_secs(5)));
}

#[test]
fn a_final_newline_less_usage_line_is_not_stranded_on_exit() {
    // H1 (under-count): a Run's FINAL usage line, flushed WITHOUT a trailing newline
    // right before the process exits, must still land in the ledger. A mid-run drain
    // stops at the last newline (correct while live), but the TERMINAL drain-on-reap
    // consumes the newline-less tail — otherwise the next Run's cursor anchors past it
    // and it is lost forever. The `fake_agent` emits 2 normal (newline-terminated)
    // events, then ONE final line WITHOUT a newline (sequence 2), then exits: all 3
    // must be committed.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The agent stays alive past the startup readiness window (so `start` confirms
    // running), then emits the final newline-less line and EXITS — so only the
    // reaper's terminal drain-on-reap can rescue that half-line.
    write_fake_manifest(
        manifest.path(),
        "tail",
        &["--emit-usage", "2", "--final-usage-no-newline"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("tail", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start("tail").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // All THREE events land: the 2 newline-terminated ones + the final newline-less
    // one the terminal drain rescued. Deterministic on committed row count.
    let count = wait_for_usage_rows(state.path(), "tail", 3, Duration::from_secs(30));
    assert_eq!(
        count, 3,
        "the final newline-less usage line must be ingested by the terminal drain"
    );
    // The token total reflects all 3 events (the stranded one was NOT lost).
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "tail").unwrap();
    assert_eq!(entry.usage.cumulative_input_tokens, 3 * USAGE_INPUT);
    assert_eq!(entry.usage.cumulative_output_tokens, 3 * USAGE_OUTPUT);
}

#[test]
fn a_huge_u64_token_count_clamps_positive_and_is_not_a_negative_bill() {
    // C1/C2 (billing-corruption boundary), end-to-end: an agent that self-reports a
    // token count above i64::MAX (here u64::MAX) must have it SATURATE-CLAMPED into
    // SQLite's signed i64 column (a positive i64::MAX), never a raw `as i64` that
    // bit-wraps NEGATIVE and poisons the SUM (then hides under the read's `.max(0)`).
    // The surfaced cumulative must be the correct clamped POSITIVE value.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // One event whose token counts are u64::MAX, then exit (no linger needed — the
    // event is newline-terminated and drained by the running reaper before exit; a
    // generous poll covers the timing).
    write_fake_manifest(
        manifest.path(),
        "huge",
        &[
            "--emit-usage",
            "1",
            "--usage-input-tokens",
            &u64::MAX.to_string(),
            "--usage-output-tokens",
            &u64::MAX.to_string(),
            "--linger-ms",
            "600000",
        ],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("huge", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("huge").unwrap();

    // The one event commits.
    wait_for_usage_rows(state.path(), "huge", 1, Duration::from_secs(30));

    // The surfaced cumulative is the clamped POSITIVE value (i64::MAX as u64), NOT a
    // negative masked to 0, NOT a wrapped value.
    let clamped = i64::MAX as u64;
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "huge").unwrap();
    assert_eq!(
        entry.usage.cumulative_input_tokens, clamped,
        "u64::MAX input must clamp to i64::MAX (positive), not wrap negative"
    );
    assert_eq!(entry.usage.cumulative_output_tokens, clamped);
    // And the raw stored row is positive (no negative bill row exists).
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    let raw_input: i64 = conn
        .query_row(
            "SELECT input_tokens FROM usage_events e \
             JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = 'huge'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(raw_input > 0, "the stored token row must never be negative");
    assert_eq!(raw_input, i64::MAX);

    let _ = facade.stop("huge", Some(Duration::from_secs(5)));
}

#[test]
fn a_never_metered_instance_reports_zero_not_absent() {
    // The zero-vs-absent choice (AC12): a registered-but-unstarted instance reports
    // an honest all-ZERO UsageView (a truthful zero, distinct from the `budget`
    // "does not exist" null seed), with its Metering Source visible.
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("idle", "mock").unwrap();

    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "idle").unwrap();
    assert_eq!(entry.usage.cumulative_input_tokens, 0);
    assert_eq!(entry.usage.cumulative_output_tokens, 0);
    assert_eq!(entry.usage.current_run_input_tokens, 0);
    assert_eq!(entry.usage.current_run_output_tokens, 0);
    // Budget stays the honest `None` seed (budgets are 3-2); usage is a real zero.
    assert!(entry.budget.is_none());
    // The mock declares self-reported metering — visible even when never started.
    assert_eq!(entry.metering_source, "self-reported");
    // The ledger genuinely holds zero rows for it.
    assert_eq!(usage_row_count(state.path(), "idle"), 0);
    assert_eq!(entry.usage.cumulative_total_tokens(), 0);
}

#[test]
fn only_the_commit_choke_point_writes_the_usage_ledger() {
    // AC8 / AD-7 single-writer invariant (Task 8 audit guard): NO code path other
    // than the supervisor's ONE ingestion→commit choke point may write the Usage
    // Ledger. A grep-style source guard over the engine `src/` proves that every
    // CALL to `record_usage_event(` (excluding its trait declaration, the SQLite
    // impl `fn record_usage_event`, and the Registry pass-through that funnels to
    // the store) lives ONLY in `domain/supervisor.rs`. If a future change scatters a
    // second writer, this fails — the exact invariant story 3-2's enforcement relies
    // on. Pure source scan (no OS cfg); runs on every OS.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut writer_files = std::collections::BTreeSet::new();
    visit_rs(&src, &mut |path, text| {
        for line in text.lines() {
            let trimmed = line.trim_start();
            // Skip declarations / definitions / doc comments — we only care about
            // actual invocations of the method as a WRITE.
            if trimmed.starts_with("//")
                || trimmed.starts_with("///")
                || trimmed.starts_with("fn record_usage_event")
                || trimmed.contains("fn record_usage_event(")
            {
                continue;
            }
            if line.contains("record_usage_event(") {
                writer_files.insert(
                    path.strip_prefix(&src)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    });
    // The ONLY files that may CALL record_usage_event: the supervisor choke point,
    // and the Registry pass-through (which the choke point calls; it is not an
    // independent writer — it just forwards to the store in one place). The store
    // impl + the port trait are excluded above (they DEFINE the method).
    let allowed: std::collections::BTreeSet<String> = [
        "domain/supervisor.rs",
        "domain/registry.rs",
        "store/sqlite.rs",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let violations: Vec<&String> = writer_files.difference(&allowed).collect();
    assert!(
        violations.is_empty(),
        "record_usage_event is called outside the allowed ledger-commit path: {violations:?}"
    );
    // And it MUST be present in the supervisor (the choke point exists).
    assert!(
        writer_files.contains("domain/supervisor.rs"),
        "the commit choke point must call record_usage_event; callers found: {writer_files:?}"
    );
}

/// Recursively visit every `.rs` file under `dir`, calling `f(path, contents)`.
fn visit_rs(dir: &Path, f: &mut impl FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                f(&path, &text);
            }
        }
    }
}
