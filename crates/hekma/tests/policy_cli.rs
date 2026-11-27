//! Integration tests for `hekma policy` (story 21-3): the refusals map to
//! the documented exit codes on every OS with no agent process and no
//! stdin read, and a full create → change → dry-run → delete round trip
//! runs through the local Service Host (the 21-9 label harness pattern),
//! including the default Policy's Rate table and its reserved shape.

mod helpers;

use helpers::{run_hekma_agent, TestContext};

fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"))
}

#[test]
fn policy_refusals_map_to_the_documented_exit_codes() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    // 3 — no Policy is named (read-only: no Service Host starts).
    let run = hekma(&["policy", "show", "ghost"]);
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("ghost"), "{}", run.stderr);
    assert!(
        !state_dir.join("control.json").exists(),
        "a refused read starts no Service Host"
    );

    // 2 — malformed requests, refused before anything is written.
    for args in [
        vec!["policy", "create", "Bad-Name", "--selector", "team=core"],
        vec!["policy", "create", "ok-name"],
        vec!["policy", "create", "ok-name", "--selector", "team"],
        vec![
            "policy",
            "create",
            "ok-name",
            "--selector",
            "team=core",
            "--breach-action",
            "zap",
        ],
        // An unknown --clear field is a typo, not a silent no-op.
        vec!["policy", "change", "default", "--clear", "tokens-per-rn"],
        // A flat Rate belongs to a named Policy; the default's table is
        // edited through --rate-for (a flat rate on it would never apply).
        vec![
            "policy",
            "change",
            "default",
            "--rate-input",
            "1",
            "--rate-output",
            "2",
        ],
    ] {
        let run = hekma(&args);
        assert_eq!(run.code, Some(2), "{args:?} stderr: {}", run.stderr);
    }
    // A routed WRITE to an unknown name still exits 3 (through the host).
    let run = hekma(&["policy", "change", "ghost", "--breach-action", "stop"]);
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    let run = hekma(&["policy", "delete", "ghost"]);
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);

    // --help names the verbs.
    let run = hekma(&["policy", "--help"]);
    assert!(run.success);
    for verb in ["list", "show", "create", "change", "delete"] {
        assert!(run.stdout.contains(verb), "{}", run.stdout);
    }
}

#[test]
fn a_policy_round_trip_through_the_service_host() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    // The seeded default Policy: warn, off, no ceilings, an empty Rate
    // table — readable read-only, before any host exists.
    let run = hekma(&["policy", "show", "default", "--json"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let document = json(&run.stdout);
    assert_eq!(document["policy"]["limits"]["breach_action"], "warn");
    assert!(
        !state_dir.join("control.json").exists(),
        "a read starts no host"
    );

    // Create (routed through the auto-started host): the dry run first.
    let run = hekma(&[
        "policy",
        "create",
        "nightly",
        "--selector",
        "team=research,env=prod",
        "--tokens-per-run",
        "2000000",
        "--dollars-per-run",
        "25",
        "--breach-action",
        "pause",
        "--dry-run",
        "--json",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    let document = json(&run.stdout);
    assert_eq!(document["dry_run"], true);
    assert_eq!(document["matched"], 0);
    // Nothing was written.
    let run = hekma(&["policy", "show", "nightly"]);
    assert_eq!(run.code, Some(3), "the dry run wrote nothing");

    // The real create.
    let run = hekma(&[
        "policy",
        "create",
        "nightly",
        "--selector",
        "team=research,env=prod",
        "--tokens-per-run",
        "2000000",
        "--dollars-per-run",
        "25",
        "--breach-action",
        "pause",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("created"), "{}", run.stdout);
    let run = hekma(&["policy", "show", "nightly", "--json"]);
    let document = json(&run.stdout);
    assert_eq!(document["policy"]["selector"], "team=research,env=prod");
    assert_eq!(document["policy"]["limits"]["tokens_per_run"], 2_000_000);
    assert_eq!(
        document["policy"]["limits"]["dollars_per_run_micros"],
        25_000_000
    );
    assert_eq!(document["policy"]["limits"]["breach_action"], "pause");

    // list names both.
    let run = hekma(&["policy", "list", "--json"]);
    let document = json(&run.stdout);
    let names: Vec<&str> = document["policies"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(
        names.contains(&"default") && names.contains(&"nightly"),
        "{names:?}"
    );

    // Duplicate create: 2 with the policy-exists code.
    let run = hekma(&["policy", "create", "nightly", "--selector", "team=research"]);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("already exists"), "{}", run.stderr);

    // Change: raise the ceiling, clear the dollar cap.
    let run = hekma(&[
        "policy",
        "change",
        "nightly",
        "--tokens-per-run",
        "5000000",
        "--clear",
        "dollars-per-run",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "nightly", "--json"]);
    let document = json(&run.stdout);
    assert_eq!(document["policy"]["limits"]["tokens_per_run"], 5_000_000);
    assert_eq!(
        document["policy"]["limits"]["dollars_per_run_micros"],
        serde_json::Value::Null
    );

    // The default Policy's reserved shape.
    let run = hekma(&["policy", "delete", "default"]);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("cannot be deleted"), "{}", run.stderr);
    let run = hekma(&["policy", "change", "default", "--selector", "team=core"]);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("cannot carry a Selector"),
        "{}",
        run.stderr
    );

    // The default's Rate table: add, read, clear.
    let run = hekma(&[
        "policy",
        "change",
        "default",
        "--rate-for",
        "gpt-demo",
        "--rate-input",
        "2.00",
        "--rate-output",
        "8.00",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "default", "--json"]);
    let document = json(&run.stdout);
    assert_eq!(
        document["policy"]["rates"]["gpt-demo"]["input_micros_per_1m"],
        2_000_000
    );
    assert_eq!(
        document["policy"]["rates"]["gpt-demo"]["output_micros_per_1m"],
        8_000_000
    );
    let run = hekma(&["policy", "change", "default", "--clear", "rate:gpt-demo"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "default", "--json"]);
    let document = json(&run.stdout);
    let rates = document["policy"]["rates"].as_object();
    assert!(
        rates.is_none_or(|table| table.is_empty()),
        "the entry was removed: {rates:?}"
    );

    // Delete the named Policy; the default stays.
    let run = hekma(&["policy", "delete", "nightly"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "nightly"]);
    assert_eq!(run.code, Some(3), "deleted");
    let run = hekma(&["policy", "show", "default"]);
    assert!(run.success, "the default survives every delete");
}

#[test]
fn the_human_list_and_show_render_the_table_and_its_rates() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    let run = hekma(&[
        "policy",
        "create",
        "nightly",
        "--selector",
        "team=research,env=prod",
        "--tokens-per-run",
        "2000000",
        "--dollars-cumulative",
        "40",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);

    // The human list prints one line per Policy, the default included —
    // not only the --json document.
    let run = hekma(&["policy", "list"]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("default  (every Run)"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("nightly  team=research,env=prod"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("tokens/run 2000000"), "{}", run.stdout);
    assert!(
        run.stdout.contains("est. $40.00 cumulative"),
        "{}",
        run.stdout
    );

    // The default's Rate table, cached column included, round-trips; the
    // human show names each model's rate under the Policy line.
    let run = hekma(&[
        "policy",
        "change",
        "default",
        "--rate-for",
        "gpt-demo",
        "--rate-input",
        "2.00",
        "--rate-output",
        "8.00",
        "--rate-cached",
        "1.00",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "default", "--json"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let document = json(&run.stdout);
    assert_eq!(
        document["policy"]["rates"]["gpt-demo"]["cached_micros_per_1m"],
        1_000_000
    );
    let run = hekma(&["policy", "show", "default"]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(
        run.stdout
            .contains("rate for gpt-demo: $2.00 in / $8.00 out per 1M tokens (estimated)"),
        "{}",
        run.stdout
    );

    // The named Policy's human show carries its limits, no rates section.
    let run = hekma(&["policy", "show", "nightly"]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(run.stdout.contains("tokens/run 2000000"), "{}", run.stdout);
    assert!(!run.stdout.contains("rate for"), "{}", run.stdout);
}

#[test]
fn change_and_delete_dry_runs_write_nothing_and_say_so() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    let run = hekma(&[
        "policy",
        "create",
        "nightly",
        "--selector",
        "team=research",
        "--tokens-per-run",
        "2000000",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);

    // A change dry run resolves read-only and reports what it WOULD do.
    let run = hekma(&[
        "policy",
        "change",
        "nightly",
        "--tokens-per-run",
        "9000000",
        "--breach-action",
        "stop",
        "--dry-run",
    ]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("would change Policy `nightly`"),
        "{}",
        run.stdout
    );
    let run = hekma(&["policy", "show", "nightly", "--json"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let document = json(&run.stdout);
    assert_eq!(document["policy"]["limits"]["tokens_per_run"], 2_000_000);
    assert_eq!(
        document["policy"]["limits"]["breach_action"],
        serde_json::Value::Null
    );

    // A delete dry run re-resolves without deleting.
    let run = hekma(&["policy", "delete", "nightly", "--dry-run"]);
    assert!(run.success, "stderr: {}", run.stderr);
    assert!(
        run.stdout.contains("would delete Policy `nightly`"),
        "{}",
        run.stdout
    );
    let run = hekma(&["policy", "show", "nightly"]);
    assert!(run.success, "the dry run deleted nothing");

    // The real delete still works after both dry runs.
    let run = hekma(&["policy", "delete", "nightly"]);
    assert!(run.success, "stderr: {}", run.stderr);
    let run = hekma(&["policy", "show", "nightly"]);
    assert_eq!(run.code, Some(3), "deleted for real");
}
