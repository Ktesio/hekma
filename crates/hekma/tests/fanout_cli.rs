//! Integration tests for the Selector fan-out on the lifecycle verbs
//! (`hekma agent stop|pause|resume|drain`, `hekma run close`): the refusals
//! and exit codes on every OS with no agent process and no stdin read, and
//! on Unix a set of detached agents and a child Run acted on per Run —
//! confirmation over a pipe, dry runs, per-Run results, the child's refusal
//! never stopping the others, and the first failure's exit class.

mod helpers;

use hekma_conformance::test_support::ManifestFixture;
use helpers::{run_hekma_agent, stop_host, TestContext};

const UNKNOWN_RUN: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";

fn json(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout).unwrap_or_else(|err| panic!("{err}: {stdout}"))
}

/// Stops the local Service Host a test's registration started.
struct HostGuard<'a>(&'a std::path::Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

#[test]
fn an_agent_named_like_a_legacy_run_id_is_still_the_agent() {
    // `run-<digits>-<digits>` is both a legacy run id's shape and a valid
    // agent name: a registered agent of that name is resolved first, so its
    // verbs keep the agent route (no process needed: the agent is only
    // registered).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let _host = HostGuard(state_dir);
    let manifest = ctx.project_dir.join("manifest");
    std::fs::create_dir_all(&manifest).unwrap();
    ManifestFixture::new("run-1-2", &[])
        .guaranteed_on_all_oses("interaction")
        .write(&manifest);
    let run = hekma(&[
        "agent",
        "register",
        "run-1-2",
        "--manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(run.success, "{}", run.stderr);
    for verb in ["stop", "pause", "resume", "drain"] {
        // Resolved as the agent: its (missing) open Run, never a lookup of
        // a Run with that id.
        let run = hekma(&["agent", verb, "run-1-2", "--dry-run"]);
        assert_eq!(run.code, Some(4), "{verb}: {}", run.stderr);
        assert!(
            run.stderr
                .contains("Agent Instance `run-1-2` has no open Run"),
            "{verb}: {}",
            run.stderr
        );
    }
    // A legacy-shaped id no agent owns is still read as a run id.
    let run = hekma(&["agent", "stop", "run-3-4", "--dry-run"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    // Story 22-1 (the 21-6 review's N6): the real (flag-less) route keeps
    // the agent too — `pause` on a registered, not-running agent is the
    // agent route's own refusal, never a Run lookup.
    let run = hekma(&["agent", "pause", "run-1-2"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(!run.stderr.contains("no Run has the id"), "{}", run.stderr);
    // `budget` and `label` resolve the agent the same way.
    for args in [
        vec!["budget", "show", "run-1-2"],
        vec!["budget", "set", "run-1-2", "--tokens-per-run", "100"],
        vec!["label", "set", "run-1-2", "team=core"],
    ] {
        let run = hekma(&args);
        assert_eq!(run.code, Some(4), "{args:?}: {}", run.stderr);
        assert!(
            run.stderr
                .contains("Agent Instance `run-1-2` has no open Run"),
            "{args:?}: {}",
            run.stderr
        );
    }
    let run = hekma(&["budget", "show", "run-3-4"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
}

#[test]
fn the_selector_verbs_refuse_and_exit_by_class_on_every_os() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    for verb in ["stop", "pause", "resume", "drain"] {
        // No match: a success with a note, the one shared document.
        let run = hekma(&["agent", verb, "--selector", "team=nobody", "--json"]);
        assert_eq!(run.code, Some(0), "{verb}: {}", run.stderr);
        assert!(run.stderr.contains("No open Run matches"), "{}", run.stderr);
        let doc = json(&run.stdout);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["verb"], verb);
        assert_eq!(doc["matched"], 0);
        // A dry run plans on the read-only engine.
        let run = hekma(&["agent", verb, "--selector", "team=nobody", "--dry-run"]);
        assert_eq!(run.code, Some(0), "{verb}: {}", run.stderr);
        // An unknown run id is not found (3), read before any host starts.
        let run = hekma(&["agent", verb, UNKNOWN_RUN]);
        assert_eq!(run.code, Some(3), "{verb}: {}", run.stderr);
        let run = hekma(&["agent", verb, UNKNOWN_RUN, "--dry-run", "--json"]);
        assert_eq!(run.code, Some(3), "{verb}: {}", run.stderr);
        // A malformed Selector, and a target with a Selector (clap): 2.
        let run = hekma(&["agent", verb, "--selector", "team"]);
        assert_eq!(run.code, Some(2), "{verb}: {}", run.stderr);
        let run = hekma(&["agent", verb, "my-agent", "--selector", "team=core"]);
        assert_eq!(run.code, Some(2), "{verb}: {}", run.stderr);
        // An agent with no open Run under --json / --dry-run.
        let run = hekma(&["agent", verb, "ghost-agent", "--dry-run"]);
        assert_eq!(run.code, Some(3), "{verb}: {}", run.stderr);
    }
    // A stop window outside the host's bounds is served at the nearest
    // bound on a run id too — and said so, as on the agent route.
    let run = hekma(&["agent", "stop", UNKNOWN_RUN, "--timeout", "0", "--dry-run"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    assert!(
        run.stderr.contains("--timeout 0 is served as a 1s window"),
        "{}",
        run.stderr
    );
    // `run close` takes the same flags.
    let run = hekma(&[
        "run",
        "close",
        "--selector",
        "team=nobody",
        "--yes",
        "--json",
    ]);
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(json(&run.stdout)["verb"], "close");
    let run = hekma(&["run", "close", UNKNOWN_RUN, "--dry-run"]);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    // `start` and `send` take no Selector.
    let run = hekma(&["agent", "start", "--selector", "team=core"]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    // The help names the flags and why start and send do not take them.
    let run = hekma(&["agent", "stop", "--help"]);
    assert!(run.success);
    for needle in [
        "--selector",
        "--dry-run",
        "--yes",
        "--json",
        "start and send take no Selector",
    ] {
        assert!(run.stdout.contains(needle), "{needle}: {}", run.stdout);
    }
    assert!(
        !state_dir.join("control.json").exists(),
        "reads, dry runs and refusals start no Service Host"
    );
}

/// Stops the named agents and the host when dropped, so a failed assertion
/// never leaves a lingering agent behind.
struct Cleanup<'a> {
    ctx: &'a std::path::Path,
    state: &'a std::path::Path,
    agents: Vec<&'static str>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for agent in &self.agents {
            let _ = run_hekma_agent(
                &["agent", "stop", agent, "--timeout", "2"],
                self.ctx,
                self.state,
            );
        }
        stop_host(self.state);
    }
}

#[test]
fn a_selector_stops_the_agents_and_refuses_the_child_per_run_unix() {
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
        agents: vec!["fan-a", "fan-b"],
    };
    for name in ["fan-a", "fan-b"] {
        let manifest = ctx.project_dir.join(name);
        std::fs::create_dir_all(&manifest).unwrap();
        ManifestFixture::new(name, &["--linger-ms", "600000"])
            .guaranteed_on_all_oses("pause")
            .guaranteed_on_all_oses("interaction")
            .write(&manifest);
        let run = hekma(&[
            "agent",
            "register",
            name,
            "--manifest",
            manifest.to_str().unwrap(),
        ]);
        assert!(run.success, "{}", run.stderr);
        let run = hekma(&["agent", "config", "set", name, "labels.team", "core"]);
        assert!(run.success, "{}", run.stderr);
        let run = hekma(&["agent", "start", name, "--detach"]);
        assert!(run.success, "{}", run.stderr);
    }
    // A child Run under fan-a carries the label too.
    let run = hekma(&[
        "run",
        "open",
        "--parent",
        "fan-a",
        "--label",
        "team=core",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let child = json(&run.stdout)["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Over a pipe with no --yes: refused (2), naming --yes and --dry-run,
    // the set listed, nothing changed.
    let run = hekma(&["agent", "stop", "--selector", "team=core"]);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(
        run.stderr.contains("--yes") && run.stderr.contains("--dry-run"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(&child) && run.stderr.contains("CONTROL"),
        "{}",
        run.stderr
    );
    let run = hekma(&["agent", "show", "fan-a", "--json"]);
    assert_eq!(
        json(&run.stdout)["instance"]["state"],
        "running",
        "nothing changed"
    );

    // A dry run plans each Run: would-stop twice, the child would refuse.
    let run = hekma(&[
        "agent",
        "stop",
        "--selector",
        "team=core",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(
        run.code,
        Some(4),
        "the planned refusal's class: {}",
        run.stderr
    );
    let doc = json(&run.stdout);
    assert_eq!(doc["dry_run"], true);
    assert_eq!(doc["matched"], 3);
    let results = doc["results"].as_array().unwrap();
    assert_eq!(
        results
            .iter()
            .filter(|r| r["outcome"] == "would-stop")
            .count(),
        2
    );
    let planned_child = results
        .iter()
        .find(|r| r["run_id"] == child.as_str())
        .unwrap();
    assert_eq!(planned_child["error"]["code"], "verb-unavailable");
    let run = hekma(&["agent", "stop", "--selector", "team=core", "--dry-run"]);
    assert!(
        run.stdout.contains("would stop (agent fan-a)"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("would refuse (verb-unavailable)"),
        "{}",
        run.stdout
    );

    // A run id addresses the agent's own Run: pause it exactly as by name.
    let run = hekma(&["agent", "show", "fan-b", "--json"]);
    let fan_b_run = hekma(&["run", "open", "--parent", "fan-b", "--json"]);
    assert!(fan_b_run.success, "{}", fan_b_run.stderr);
    let fan_b_owner = json(&fan_b_run.stdout)["run"]["parent_run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let fan_b_child = json(&fan_b_run.stdout)["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    drop(run);
    let run = hekma(&["agent", "pause", &fan_b_owner]);
    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout.lines().last(), Some("paused"), "{}", run.stdout);
    assert!(run.stdout.contains("Paused Run"), "{}", run.stdout);
    let run = hekma(&["run", "show", &fan_b_owner, "--json"]);
    assert_eq!(
        json(&run.stdout)["run"]["status"],
        "held",
        "the Run goes held"
    );
    // A child held by its paused parent: resume names the parent.
    let run = hekma(&["agent", "resume", &fan_b_child]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(run.stderr.contains("parent-paused"), "{}", run.stderr);
    let run = hekma(&["agent", "resume", &fan_b_owner, "--json"]);
    assert!(run.success, "{}", run.stderr);
    let doc = json(&run.stdout);
    assert_eq!(doc["results"][0]["outcome"], "resumed");
    assert_eq!(doc["results"][0]["instance"]["state"], "running");
    let run = hekma(&["run", "close", &fan_b_child]);
    assert!(run.success, "{}", run.stderr);

    // --yes: the two agents stop, the child is refused per Run (its stop acts
    // on calls), the exit is the refusal's class with the count.
    let run = hekma(&[
        "agent",
        "stop",
        "--selector",
        "team=core",
        "--yes",
        "--json",
    ]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(
        run.stderr.contains("1 of 3 Runs were not stopped"),
        "{}",
        run.stderr
    );
    let doc = json(&run.stdout);
    assert_eq!(doc["verb"], "stop");
    assert_eq!(doc["matched"], 3);
    let results = doc["results"].as_array().unwrap();
    assert_eq!(
        results.iter().filter(|r| r["outcome"] == "stopped").count(),
        2,
        "{doc}"
    );
    let refused = results
        .iter()
        .find(|r| r["run_id"] == child.as_str())
        .unwrap();
    assert_eq!(refused["outcome"], "refused");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("announced"),
        "{refused}"
    );
    for name in ["fan-a", "fan-b"] {
        let run = hekma(&["agent", "show", name, "--json"]);
        assert_eq!(json(&run.stdout)["instance"]["state"], "stopped", "{name}");
    }

    // A drain over a set drains each agent with its own bound, and a child
    // Run inside the set refuses its own cell (its drain acts on calls)
    // while its agent still drains.
    let run = hekma(&["agent", "start", "fan-b", "--detach"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&[
        "run",
        "open",
        "--parent",
        "fan-b",
        "--label",
        "team=core",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let drain_child = json(&run.stdout)["run"]["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = hekma(&[
        "agent",
        "drain",
        "--selector",
        "team=core",
        "--timeout-ms",
        "1000",
        "--yes",
        "--json",
    ]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(
        run.stderr.contains("1 of 2 Runs were not drained"),
        "{}",
        run.stderr
    );
    let doc = json(&run.stdout);
    assert_eq!(doc["verb"], "drain");
    assert_eq!(doc["matched"], 2, "the fresh Run and its child: {doc}");
    let results = doc["results"].as_array().unwrap();
    let drained = results
        .iter()
        .find(|r| r["run_id"] != drain_child.as_str())
        .unwrap();
    assert_eq!(drained["outcome"], "drained", "{doc}");
    assert_eq!(drained["instance"]["state"], "stopped", "{doc}");
    let refused = results
        .iter()
        .find(|r| r["run_id"] == drain_child.as_str())
        .unwrap();
    assert_eq!(refused["outcome"], "refused", "{doc}");
    assert_eq!(refused["error"]["code"], "verb-unavailable", "{doc}");
}

#[test]
fn one_run_targets_plan_refuse_and_answer_as_json_unix() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);
    let _cleanup = Cleanup {
        ctx: &ctx.project_dir,
        state: state_dir,
        agents: vec!["solo"],
    };
    let manifest = ctx.project_dir.join("solo");
    std::fs::create_dir_all(&manifest).unwrap();
    ManifestFixture::new("solo", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(&manifest);
    let run = hekma(&[
        "agent",
        "register",
        "solo",
        "--manifest",
        manifest.to_str().unwrap(),
    ]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "config", "set", "solo", "labels.team", "solo"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&["agent", "start", "solo", "--detach"]);
    assert!(run.success, "{}", run.stderr);
    let run = hekma(&[
        "run",
        "open",
        "--parent",
        "solo",
        "--label",
        "team=solo",
        "--json",
    ]);
    assert!(run.success, "{}", run.stderr);
    let opened = json(&run.stdout);
    let child = opened["run"]["run_id"].as_str().unwrap().to_string();
    let owner = opened["run"]["parent_run_id"].as_str().unwrap().to_string();

    // One target's dry run: planned, nothing changed.
    let run = hekma(&["agent", "stop", &owner, "--dry-run"]);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("would stop (agent solo)"),
        "{}",
        run.stdout
    );
    let run = hekma(&["agent", "stop", &child, "--dry-run", "--json"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert_eq!(
        json(&run.stdout)["results"][0]["error"]["code"],
        "verb-unavailable"
    );
    // An agent name with --json acts through its Run.
    let run = hekma(&["agent", "pause", "solo", "--json"]);
    assert!(run.success, "{}", run.stderr);
    assert_eq!(json(&run.stdout)["results"][0]["outcome"], "paused");
    // The host's refusal of a child: as JSON and as a diagnostic.
    let run = hekma(&["agent", "pause", &child, "--json"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert_eq!(json(&run.stdout)["results"][0]["outcome"], "refused");
    let run = hekma(&["agent", "stop", &child]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    // The agent's own verb refusing (pause of a paused agent) as JSON.
    let run = hekma(&["agent", "pause", &owner, "--json"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert_eq!(json(&run.stdout)["results"][0]["error"]["code"], "engine");
    let run = hekma(&["agent", "resume", &owner]);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("Resumed Run"), "{}", run.stdout);

    // The budget read renders a latched breach and pages a Selector.
    let run = hekma(&["budget", "set", &owner, "--tokens-per-run", "0", "--yes"]);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("breach: tokens per-run"),
        "{}",
        run.stdout
    );
    let run = hekma(&["budget", "show", "--selector", "team=solo", "--limit", "1"]);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stderr.contains("--cursor"), "{}", run.stderr);
    let run = hekma(&["budget", "show", "--selector", "team=solo"]);
    assert!(
        run.stdout
            .contains("latched            tokens per-run (warn)"),
        "{}",
        run.stdout
    );
    assert_eq!(run.stdout.matches("Run run_").count(), 2, "{}", run.stdout);

    // Close: a dry run per target, and a Selector without --json prints
    // the refusal it met.
    let run = hekma(&["run", "close", &owner, "--dry-run", "--json"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert_eq!(
        json(&run.stdout)["results"][0]["error"]["code"],
        "owns-process"
    );
    let run = hekma(&["run", "close", &child, "--dry-run"]);
    assert!(run.success, "{}", run.stderr);
    assert!(run.stdout.contains("would close"), "{}", run.stdout);
    let run = hekma(&["run", "close", "--selector", "team=solo", "--dry-run"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    let run = hekma(&["run", "close", "--selector", "team=solo", "--yes"]);
    assert_eq!(run.code, Some(4), "{}", run.stderr);
    assert!(run.stderr.contains("not closed"), "{}", run.stderr);
    assert!(run.stdout.contains("closed"), "{}", run.stdout);
    // A run id stop and drain answer as the agent's verbs do.
    let run = hekma(&["agent", "stop", &owner, "--timeout", "2"]);
    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout.lines().last(), Some("stopped"), "{}", run.stdout);
}
