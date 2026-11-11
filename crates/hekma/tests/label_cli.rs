//! Integration tests for `hekma label` (bound labels): the refusals map to
//! the documented exit codes on every OS with no agent process and no stdin
//! read, and on Unix a detached agent's Runs take labels by target and by
//! Selector, with confirmation, dry runs and per-Run results.

mod helpers;

use hekma_conformance::test_support::ManifestFixture;
use helpers::{run_hekma_agent, TestContext};

const UNKNOWN_RUN: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";

#[test]
fn label_refusals_map_to_the_documented_exit_codes() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // 3 — no Run has the id.
    let run = run_hekma_agent(
        &["label", "set", UNKNOWN_RUN, "team=core"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    assert!(run.stderr.contains(UNKNOWN_RUN), "{}", run.stderr);
    // 3 — no agent has the name (read-only: no Service Host is started).
    let run = run_hekma_agent(
        &["label", "remove", "ghost-agent", "team"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    assert!(
        run.stderr
            .contains("no Agent Instance is named `ghost-agent`"),
        "{}",
        run.stderr
    );
    assert!(
        !state_dir.join("control.json").exists(),
        "a refused read starts no Service Host"
    );
    // 2 — malformed requests, refused before the state directory is touched.
    for args in [
        vec!["label", "set", "my-agent", "no-equals"],
        vec!["label", "set", "my-agent", "Team=core"],
        vec!["label", "set", "my-agent", "team=a", "team=b"],
        vec!["label", "remove", "my-agent", "team=core"],
        vec!["label", "set", "my-agent"],
        vec!["label", "set", "--selector", "team", "tier=gold"],
        vec![
            "label",
            "set",
            "--selector",
            "team=core,team=x",
            "tier=gold",
        ],
        vec!["label", "set"],
    ] {
        let run = run_hekma_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(2), "{args:?} stderr: {}", run.stderr);
    }
    // No match is a success with a note on stderr.
    let run = run_hekma_agent(
        &[
            "label",
            "set",
            "--selector",
            "team=nobody",
            "tier=gold",
            "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("No open Run matches"), "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["matched"], 0);
    assert_eq!(json["results"].as_array().unwrap().len(), 0);
    // --help names the verbs.
    let run = run_hekma_agent(&["label", "--help"], &ctx.project_dir, state_dir);
    assert!(run.success);
    for verb in ["set", "remove"] {
        assert!(run.stdout.contains(verb), "{}", run.stdout);
    }
}

fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"))
}

#[test]
fn labels_bind_by_target_and_selector_on_a_detached_agent_unix() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // A detached agent dies with the command's job on Windows; the
        // refusals test above covers the exit codes there.
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let manifest = ctx.project_dir.join("manifest");
    std::fs::create_dir_all(&manifest).unwrap();
    ManifestFixture::new("labelhost", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(&manifest);
    let run = hekma(&[
        "agent",
        "register",
        "labelhost",
        "--manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&[
        "agent",
        "config",
        "set",
        "labelhost",
        "labels.team",
        "payments",
    ]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "start", "labelhost", "--detach"]);
    assert!(run.success, "{}", run.stderr);

    // The agent's Run: the verb outranks configuration, and removing it
    // restores the configured value.
    let run = hekma(&["label", "set", "labelhost", "team=core", "--json"]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["schema_version"], 1);
    assert_eq!(doc["matched"], 1);
    let result = &doc["results"][0];
    assert_eq!(result["outcome"], "labeled");
    assert_eq!(result["labels"][0]["origin"], "label-verb");
    assert_eq!(result["prior_labels"][0]["origin"], "config");
    let owner = result["run_id"].as_str().unwrap().to_string();
    let run = hekma(&["label", "set", &owner, "team=core"]);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("unchanged"), "{}", run.stdout);
    let run = hekma(&["label", "remove", &owner, "team"]);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("team=payments (bound, config)"),
        "{}",
        run.stdout
    );
    let run = hekma(&["label", "remove", "labelhost", "team"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(run.stderr.contains("labels.team"), "{}", run.stderr);

    // Two child Runs share a Caller label; one also carries `task`.
    let open = |labels: &[&str]| {
        let mut args = vec!["run", "open", "--parent", "labelhost", "--json"];
        for label in labels {
            args.push("--label");
            args.push(label);
        }
        let run = hekma(&args);
        assert!(run.success, "{}", run.stderr);
        json(&run.stdout)["run"]["run_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let a = open(&["flow=x", "task=a"]);
    let b = open(&["flow=x"]);

    // Several matches without a terminal and without --yes: refused, listed.
    let run = hekma(&["label", "set", "--selector", "flow=x", "tier=gold"]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(run.stderr.contains("--yes"), "{}", run.stderr);
    assert!(
        run.stderr.contains(&a) && run.stderr.contains(&b),
        "{}",
        run.stderr
    );
    // A dry run lists each Run's planned change and writes nothing.
    let run = hekma(&[
        "label",
        "set",
        "--selector",
        "flow=x",
        "tier=gold",
        "--dry-run",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["matched"], 2);
    for result in doc["results"].as_array().unwrap() {
        assert_eq!(result["outcome"], "would-label");
    }
    let run = hekma(&["run", "show", &a]);
    assert!(!run.stdout.contains("tier=gold"), "{}", run.stdout);
    // With --yes, exactly the listed Runs are labeled.
    let run = hekma(&["label", "set", "--selector", "flow=x", "tier=gold", "--yes"]);
    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout.matches("labeled").count(), 2, "{}", run.stdout);
    let run = hekma(&["run", "show", &b]);
    assert!(
        run.stdout.contains("tier=gold (bound, label-verb)"),
        "{}",
        run.stdout
    );
    // A Run refused mid-set does not stop the others; the exit code is the
    // first failure's class.
    let run = hekma(&[
        "label",
        "remove",
        "--selector",
        "flow=x",
        "task",
        "--yes",
        "--json",
    ]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    let doc = json(&run.stdout);
    let results = doc["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for result in results {
        if result["run_id"] == a.as_str() {
            assert_eq!(result["outcome"], "refused");
            assert_eq!(result["error"]["code"], "label-not-bound");
        } else {
            assert_eq!(result["outcome"], "unchanged");
        }
    }

    // An ended Run's labels cannot change.
    let run = hekma(&["run", "close", &a]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["label", "set", &a, "tier=silver"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(run.stderr.contains("has ended"), "{}", run.stderr);

    let run = hekma(&["agent", "stop", "labelhost", "--timeout", "5"]);
    assert!(run.success, "{}", run.stderr);
}
