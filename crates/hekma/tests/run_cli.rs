//! Integration tests for `hekma run` (Runs as first-class entities): the
//! refusals map to the documented exit codes on every OS, and on Unix a
//! detached agent's Run takes a child through open → show → close.

mod helpers;

use hekma_conformance::test_support::ManifestFixture;
use helpers::{run_hekma_agent, TestContext};

#[test]
fn run_refusals_map_to_the_documented_exit_codes() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // 3 — no Run has the id.
    let run = run_hekma_agent(&["run", "show", "run_ghost"], &ctx.project_dir, state_dir);
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("run_ghost"), "{}", run.stderr);
    let run = run_hekma_agent(
        &["run", "close", "run_ghost", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(3), "stderr: {}", run.stderr);
    // 4 — the agent has no open Run to open a child under.
    let run = run_hekma_agent(
        &["run", "open", "--parent", "ghost-agent"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(4), "stderr: {}", run.stderr);
    assert!(run.stderr.contains("no open Run"), "{}", run.stderr);
    // 2 — malformed flags, refused before the state directory is touched.
    for args in [
        vec!["run", "open", "--parent", "a", "--label", "no-equals"],
        vec!["run", "open", "--parent", "a", "--breach-action", "explode"],
        vec!["run", "open", "--parent", "a", "--dollar-cap", "1.0000001"],
    ] {
        let run = run_hekma_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(2), "{args:?} stderr: {}", run.stderr);
    }
    // --help names the verbs.
    let run = run_hekma_agent(&["run", "--help"], &ctx.project_dir, state_dir);
    assert!(run.success);
    for verb in ["open", "close", "show"] {
        assert!(run.stdout.contains(verb), "{}", run.stdout);
    }
}

#[test]
fn a_child_run_opens_shows_and_closes_under_a_detached_agent_unix() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // A detached agent dies with the command's job on Windows; the
        // refusals test above covers the exit codes there.
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = ctx.project_dir.join("manifest");
    std::fs::create_dir_all(&manifest).unwrap();
    ManifestFixture::new("runhost", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(&manifest);
    let run = run_hekma_agent(
        &[
            "agent",
            "register",
            "runhost",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let run = run_hekma_agent(
        &[
            "agent",
            "config",
            "set",
            "runhost",
            "labels.team",
            "payments",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let run = run_hekma_agent(
        &["agent", "start", "runhost", "--detach"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);

    let run = run_hekma_agent(
        &[
            "run",
            "open",
            "--parent",
            "runhost",
            "--label",
            "task=review",
            "--token-budget",
            "500",
            "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    let child = json["run"]["run_id"].as_str().unwrap().to_string();
    assert!(child.starts_with("run_"), "{child}");
    assert_eq!(json["run"]["control_depth"], "announced");
    let parent = json["run"]["parent_run_id"].as_str().unwrap().to_string();

    // The parent: supervised (no wire), with the configured bound label.
    let run = run_hekma_agent(&["run", "show", &parent], &ctx.project_dir, state_dir);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("supervised (no wire)"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("team=payments (bound)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("1 open"), "{}", run.stdout);

    // Closing the process-owning Run is refused; the child closes.
    let run = run_hekma_agent(&["run", "close", &parent], &ctx.project_dir, state_dir);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    let run = run_hekma_agent(&["run", "close", &child], &ctx.project_dir, state_dir);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("closed"), "{}", run.stdout);
    assert!(run.stdout.contains("ended"), "{}", run.stdout);

    let run = run_hekma_agent(
        &["agent", "stop", "runhost", "--timeout", "5"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let run = run_hekma_agent(
        &["run", "show", &parent, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["run"]["status"], "ended");
}
