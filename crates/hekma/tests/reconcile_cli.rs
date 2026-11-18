//! `hekma run reconcile` and the reconciled line of `hekma run show` (story
//! 19-5): a submission routes through the auto-started Service Host, an
//! identical retry changes nothing, a reused reference with other figures
//! exits 4, an unknown Run exits 3, a bad source or dollar figure exits 2.
//!
//! Every OS: no agent process.

mod helpers;

use std::path::Path;

use helpers::{run_hekma_agent, stop_host, KtRun, TestContext};

struct HostGuard<'a>(&'a Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

fn both(run: &KtRun) -> String {
    format!("{}\n{}", run.stdout, run.stderr)
}

#[test]
fn a_reconciliation_routes_through_the_host_and_shows_beside_the_estimate() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             process_owner, created_at, updated_at) \
             VALUES ('run_01JZ6Q9G3S8V2X7K4M1N5P0R2T', 'announced', 0, 'active', '', 0, 't', 't');
             INSERT INTO usage_rollup_run (run_id, events, input_tokens, output_tokens, \
             cached_tokens, cached_unknown, cost_micros) \
             VALUES ('run_01JZ6Q9G3S8V2X7K4M1N5P0R2T', 1, 900, 250, 0, 1, 4000);",
        )
        .unwrap();
    }
    let _host = HostGuard(state_dir);
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let run = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";

    let first = hekma(&[
        "run",
        "reconcile",
        run,
        "--input",
        "1000",
        "--output",
        "200",
        "--dollars",
        "0.01",
        "--source",
        "provider",
        "--ref",
        "inv-1",
    ]);
    assert!(first.success, "{}", both(&first));
    assert!(
        first.stdout.contains("reconciled: 1000 in / 200 out"),
        "{}",
        first.stdout
    );
    assert!(first.stdout.contains("(reconciled)"), "{}", first.stdout);
    let again = hekma(&[
        "run",
        "reconcile",
        run,
        "--input",
        "1000",
        "--output",
        "200",
        "--dollars",
        "0.01",
        "--source",
        "provider",
        "--ref",
        "inv-1",
    ]);
    assert!(again.success, "{}", both(&again));
    assert!(again.stdout.contains("nothing changed"), "{}", again.stdout);
    let conflict = hekma(&[
        "run",
        "reconcile",
        run,
        "--input",
        "5",
        "--output",
        "5",
        "--ref",
        "inv-1",
    ]);
    assert_eq!(conflict.code, Some(4), "{}", both(&conflict));

    let shown = hekma(&["run", "show", run]);
    assert!(shown.success, "{}", both(&shown));
    assert!(shown.stdout.contains("(estimated)"), "{}", shown.stdout);
    assert!(
        shown.stdout.contains("RECONCILED")
            && shown.stdout.contains("provider, inv-1")
            && shown.stdout.contains("drift +100 in / -50 out, +$0.01"),
        "{}",
        shown.stdout
    );
    let json = hekma(&["run", "show", run, "--json"]);
    let document: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(document["run"]["reconciled"]["drift"]["cost_micros"], 6000);

    // Without --ref the figures name the submission: a retry replays.
    let unnamed = hekma(&[
        "run",
        "reconcile",
        run,
        "--input",
        "950",
        "--output",
        "250",
        "--json",
    ]);
    assert!(unnamed.success, "{}", both(&unnamed));
    let stored: serde_json::Value = serde_json::from_str(&unnamed.stdout).unwrap();
    assert_eq!(stored["replayed"], false);
    assert_eq!(stored["reconciliation"]["source"], "host");
    let unnamed = hekma(&[
        "run",
        "reconcile",
        run,
        "--input",
        "950",
        "--output",
        "250",
        "--json",
    ]);
    let stored: serde_json::Value = serde_json::from_str(&unnamed.stdout).unwrap();
    assert_eq!(stored["replayed"], true);

    let missing = hekma(&[
        "run",
        "reconcile",
        "run_01JZ6Q9G3S8V2X7K4M1N5P0R2Z",
        "--input",
        "1",
        "--output",
        "1",
    ]);
    assert_eq!(missing.code, Some(3), "{}", both(&missing));
    for bad in [vec!["--source", "vendor"], vec!["--dollars", "lots"]] {
        let mut args = vec!["run", "reconcile", run, "--input", "1", "--output", "1"];
        args.extend(bad.iter().copied());
        let refused = hekma(&args);
        assert_eq!(refused.code, Some(2), "{bad:?}: {}", both(&refused));
    }
    let shape = hekma(&["run", "reconcile", run, "--input", "-3", "--output", "1"]);
    assert_eq!(shape.code, Some(2), "{}", both(&shape));
}
