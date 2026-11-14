//! Integration tests for `hekma run retention` and the compacted
//! `hekma run show` (story 24-4): the read starts nothing; a change routes
//! through the auto-started Service Host, whose retention pass compacts an
//! ended Run past the window and deletes old per-call record files whole;
//! the stored Run window drives `hekma ps --all`'s cutoff; an invalid
//! window is a usage error (exit 2).
//!
//! Every OS: no agent process; every HTTP call is the CLI's own, bounded.

mod helpers;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use helpers::{host_pid, run_hekma_agent, stop_host, KtRun, TestContext};

/// Stops the state root's auto-started host on drop.
struct HostGuard<'a>(&'a Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

fn both(run: &KtRun) -> String {
    format!("{}\n{}", run.stdout, run.stderr)
}

fn json(run: &KtRun) -> serde_json::Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|err| panic!("{err}: {}", both(run)))
}

/// `days` ago in the shape the rows store.
fn days_ago(days: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    hekma_engine::ended_run_cutoff(now + 7 * 86_400 - days * 86_400)
}

/// An ended Run with a label and a rollup, `days` after its end.
fn seed_ended(state_dir: &Path, run_id: &str, days: u64) {
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let ended = days_ago(days);
    conn.execute(
        "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
         process_owner, end_cause, created_at, updated_at, ended_at) \
         VALUES (?1, 'announced', 0, 'ended', '', 0, 'closed', ?2, ?2, ?2)",
        rusqlite::params![run_id, ended],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO run_labels (run_id, key, value, source) VALUES (?1, 'team', 'red', 'caller')",
        [run_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_rollup_run (run_id, events, input_tokens, output_tokens, \
         cached_tokens, cached_unknown, cost_micros) VALUES (?1, 1, 120, 30, 0, 0, 2500)",
        [run_id],
    )
    .unwrap();
}

#[test]
fn the_read_starts_nothing_and_an_invalid_window_is_a_usage_error() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    let run = hekma(&["run", "retention"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        run.stdout
            .contains("kept in full for 7 day(s), then reduced to their totals"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("no pass yet"), "{}", run.stdout);
    let shown = json(&hekma(&["run", "retention", "--json"]));
    assert_eq!(shown["settings"]["run_retention_days"], 7, "{shown}");
    assert_eq!(shown["settings"]["calls_retention_days"], 7, "{shown}");
    assert_eq!(shown["compaction"]["runs_compacted"], 0, "{shown}");
    assert!(host_pid(state_dir).is_none(), "a read started a host");

    for bad in [["--runs", "0"], ["--runs", "4000"], ["--calls", "seven"]] {
        let run = hekma(&["run", "retention", bad[0], bad[1]]);
        assert_eq!(run.code, Some(2), "{bad:?}: {}", both(&run));
    }
    assert!(
        host_pid(state_dir).is_none(),
        "a refused change started a host"
    );

    // A Run never compacted is still not found (exit 3).
    let run = hekma(&["run", "show", "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"]);
    assert_eq!(run.code, Some(3), "{}", both(&run));
}

#[test]
fn the_stored_run_window_moves_the_listing_cutoff() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    seed_ended(state_dir, "run_TEN", 10);
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let listed = |run: &KtRun| run.stdout.contains("run_TEN");

    let run = hekma(&["ps", "--all"]);
    assert!(run.success, "{}", both(&run));
    assert!(!listed(&run), "outside the default window: {}", run.stdout);
    // The stored window (written as a host would) widens the cutoff.
    {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
        conn.execute(
            "INSERT INTO retention (id, run_retention_days, calls_retention_days, updated_at) \
             VALUES (1, 30, 7, 't')",
            [],
        )
        .unwrap();
    }
    let run = hekma(&["ps", "--all"]);
    assert!(listed(&run), "inside the stored window: {}", run.stdout);
    let shown = json(&hekma(&["run", "retention", "--json"]));
    assert_eq!(shown["settings"]["run_retention_days"], 30, "{shown}");
}

#[test]
fn a_change_routes_through_the_host_which_compacts_and_deletes_old_call_files() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    seed_ended(state_dir, "run_OLD", 40);
    seed_ended(state_dir, "run_NEW", 1);
    let calls = state_dir.join("calls");
    std::fs::create_dir_all(&calls).unwrap();
    for name in ["2020-01-01.db", "2020-01-02.db"] {
        drop(rusqlite::Connection::open(calls.join(name)).unwrap());
    }
    let _host = HostGuard(state_dir);
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    let run = hekma(&["run", "retention", "--calls", "30", "--json"]);
    assert!(run.success, "{}", both(&run));
    let set = json(&run);
    assert_eq!(set["settings"]["calls_retention_days"], 30, "{set}");
    assert_eq!(set["settings"]["run_retention_days"], 7, "{set}");
    assert!(host_pid(state_dir).is_some(), "the change started a host");

    // The host's pass at start compacts the 40-day-old Run.
    let deadline = Instant::now() + Duration::from_secs(20);
    let shown = loop {
        let run = hekma(&["run", "show", "run_OLD", "--json"]);
        if run.success {
            let document = json(&run);
            if document.get("compacted_run").is_some() {
                break document;
            }
        }
        assert!(Instant::now() < deadline, "never compacted: {}", both(&run));
        std::thread::sleep(Duration::from_millis(200));
    };
    let compacted = &shown["compacted_run"];
    assert_eq!(compacted["input_tokens"], 120, "{shown}");
    assert_eq!(compacted["cost_micros"], 2500, "{shown}");
    assert_eq!(compacted["end_cause"], "closed", "{shown}");
    assert_eq!(compacted["labels"][0]["key"], "team", "{shown}");
    let run = hekma(&["run", "show", "run_OLD"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        run.stdout.contains("(compacted)") && run.stdout.contains("team=red"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("$0.00"), "{}", run.stdout);
    // The recent Run keeps its full detail.
    let run = hekma(&["run", "show", "run_NEW", "--json"]);
    assert!(run.success, "{}", both(&run));
    assert!(json(&run).get("run").is_some());

    // The old per-call record file went whole.
    let deadline = Instant::now() + Duration::from_secs(20);
    while calls.join("2020-01-01.db").exists() {
        assert!(
            Instant::now() < deadline,
            "the old calls file was never deleted: {:?}\n{}",
            std::fs::read_dir(&calls)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>(),
            std::fs::read_to_string(state_dir.join("serve.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = json(&hekma(&["run", "retention", "--json"]));
        if state["compaction"]["runs_compacted"].as_u64() >= Some(1) {
            assert!(
                state["compaction"]["segments_dropped"].as_u64() >= Some(1),
                "{state}"
            );
            assert!(state["compaction"]["last_pass_at"].is_string(), "{state}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pass was never recorded: {state}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let run = hekma(&["run", "retention"]);
    assert!(run.stdout.contains("last pass"), "{}", run.stdout);
}
