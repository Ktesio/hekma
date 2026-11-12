//! Integration tests for `hekma budget show|set`: the refusals and exit
//! codes on every OS with no agent process, and on Unix the Run-scoped pin
//! — a limit set on an agent's open Run says it is Run-scoped (human line
//! and `--json`), outranks the configuration, and is gone from the fresh
//! Run a stop and start mints.

mod helpers;

use hekma_conformance::test_support::ManifestFixture;
use helpers::{run_hekma_agent, stop_host, TestContext};

const UNKNOWN_RUN: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";

fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"))
}

#[test]
fn budget_refusals_map_to_the_documented_exit_codes() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    // 3 — not found, read before any host starts.
    for args in [
        vec!["budget", "show", UNKNOWN_RUN],
        vec!["budget", "show", "ghost-agent"],
        vec!["budget", "set", UNKNOWN_RUN, "--tokens-per-run", "5"],
        vec![
            "budget",
            "set",
            "ghost-agent",
            "--tokens-per-run",
            "5",
            "--dry-run",
        ],
    ] {
        let run = hekma(&args);
        assert_eq!(run.code, Some(3), "{args:?}: {}", run.stderr);
    }
    // 2 — malformed requests, refused before the state directory is read.
    for args in [
        vec!["budget", "set", "my-agent"],
        vec!["budget", "set", "my-agent", "--breach-action", "explode"],
        vec!["budget", "set", "my-agent", "--clear", "rate"],
        vec!["budget", "set", "my-agent", "--dollars-per-run", "-1"],
        vec![
            "budget",
            "set",
            "my-agent",
            "--tokens-per-run",
            "5",
            "--clear",
            "tokens-per-run",
        ],
        vec![
            "budget",
            "set",
            "--selector",
            "team",
            "--tokens-per-run",
            "5",
        ],
        vec!["budget", "show", "my-agent", "--cursor", "run_x"],
        vec!["budget", "show", "--selector", "team=core", "--limit", "0"],
        vec![
            "budget",
            "show",
            "--selector",
            "team=core",
            "--limit",
            "1001",
        ],
        vec!["budget", "set"],
        vec!["budget", "show", "--cursor", "not-a-cursor"],
    ] {
        let run = hekma(&args);
        assert_eq!(run.code, Some(2), "{args:?}: {}", run.stderr);
    }
    // No match: success with a note.
    let run = hekma(&["budget", "show", "--selector", "team=nobody", "--json"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(json(&run.stdout)["items"], serde_json::json!([]));
    // No target lists every Run (the listing read): none here, a note.
    let run = hekma(&["budget", "show"]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("No Run to show"), "{}", run.stderr);
    let run = hekma(&[
        "budget",
        "set",
        "--selector",
        "team=nobody",
        "--tokens-per-run",
        "5",
        "--json",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(json(&run.stdout)["verb"], "budget");
    // The help says a limit set here belongs to the Run.
    let run = hekma(&["budget", "--help"]);
    assert!(run.success);
    for needle in [
        "belongs to the Run",
        "budget.* configuration or a Policy",
        "show",
        "set",
    ] {
        assert!(run.stdout.contains(needle), "{needle}: {}", run.stdout);
    }
    assert!(
        !state_dir.join("control.json").exists(),
        "no Service Host started"
    );
}

struct Cleanup<'a> {
    ctx: &'a std::path::Path,
    state: &'a std::path::Path,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = run_hekma_agent(
            &["agent", "stop", "budgeted", "--timeout", "2"],
            self.ctx,
            self.state,
        );
        stop_host(self.state);
    }
}

#[test]
fn a_run_limit_says_it_is_run_scoped_and_a_fresh_run_carries_none_unix() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // A detached agent dies with the command's job on Windows; the
        // refusals test above covers the exit codes there.
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let _cleanup = Cleanup {
        ctx: &ctx.project_dir,
        state: state_dir,
    };
    let manifest = ctx.project_dir.join("manifest");
    std::fs::create_dir_all(&manifest).unwrap();
    ManifestFixture::new("budgeted", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(&manifest);
    let run = hekma(&[
        "agent",
        "register",
        "budgeted",
        "--manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "config", "set", "budgeted", "labels.team", "core"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&[
        "agent",
        "config",
        "set",
        "budgeted",
        "budget.tokens.cumulative",
        "7000000",
    ]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "start", "budgeted", "--detach"]);
    assert!(run.success, "{}", run.stderr);

    // A dry run plans and writes nothing.
    let run = hekma(&[
        "budget",
        "set",
        "budgeted",
        "--tokens-cumulative",
        "1000000",
        "--dry-run",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["results"][0]["outcome"], "would-set");
    let run = hekma(&["budget", "show", "budgeted", "--json"]);
    assert_eq!(
        json(&run.stdout)["items"][0]["resolution"]["sources"]["tokens_cumulative"],
        "instance"
    );

    // The human line says the limit is Run-scoped …
    let run = hekma(&[
        "budget",
        "set",
        "budgeted",
        "--tokens-cumulative",
        "1000000",
    ]);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("tokens cumulative 1000000"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("set on this Run only"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("budget.* configuration or a Policy"),
        "{}",
        run.stdout
    );
    // … and so does --json.
    let run = hekma(&[
        "budget",
        "set",
        "budgeted",
        "--tokens-per-run",
        "900000",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    let result = &doc["results"][0];
    assert_eq!(result["run_scoped"], true);
    assert!(result["note"]
        .as_str()
        .unwrap()
        .contains("ends with the Run"));
    assert_eq!(
        result["limits"]["resolution"]["sources"]["tokens_cumulative"],
        "run"
    );
    let owner = result["run_id"].as_str().unwrap().to_string();
    // The Run-level limit outranks the configuration in the read.
    let run = hekma(&["budget", "show", &owner]);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("tokens cumulative  1000000 (run)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("set on this Run"), "{}", run.stdout);
    // A child's cumulative ceiling is refused; a Selector set reaches both.
    let run = hekma(&[
        "run",
        "open",
        "--parent",
        "budgeted",
        "--label",
        "team=core",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let child = json(&run.stdout)["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = hekma(&["budget", "set", &child, "--tokens-cumulative", "5"]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(run.stderr.contains("child Run"), "{}", run.stderr);
    let run = hekma(&[
        "budget",
        "set",
        "--selector",
        "team=core",
        "--tokens-per-run",
        "800000",
        "--yes",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["matched"], 2);
    assert!(doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["run_scoped"] == true));
    let run = hekma(&[
        "budget",
        "show",
        "--selector",
        "team=core",
        "--limit",
        "1",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let page = json(&run.stdout);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let run = hekma(&[
        "budget",
        "show",
        "--selector",
        "team=core",
        "--cursor",
        &cursor,
        "--json",
    ]);
    let page = json(&run.stdout);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert!(page.get("next_cursor").is_none(), "{page}");
    // The label verb rides the same fan-out: one set act, `verb: label`.
    let run = hekma(&[
        "label",
        "set",
        "--selector",
        "team=core",
        "tier=gold",
        "--yes",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["verb"], "label");
    assert_eq!(doc["matched"], 2);

    // Stop and start: the fresh Run carries no Run-level limit; the
    // configuration's ceiling applies again.
    let run = hekma(&["agent", "stop", "budgeted", "--timeout", "5"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "start", "budgeted", "--detach"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["budget", "show", "budgeted", "--json"]);
    assert!(run.success, "{}", run.stderr);
    let item = &json(&run.stdout)["items"][0];
    assert_ne!(item["run"]["run_id"], owner.as_str(), "a new Run");
    assert_eq!(
        item["limits"],
        serde_json::json!({}),
        "no run_limits row survives"
    );
    assert_eq!(
        item["resolution"]["sources"]["tokens_cumulative"],
        "instance"
    );
    assert_eq!(item["resolution"]["tokens"]["cumulative"], 7_000_000);
    let run = hekma(&["budget", "show", "budgeted"]);
    assert!(
        run.stdout.contains("set on this Run    no Run-level limit"),
        "{}",
        run.stdout
    );
}
