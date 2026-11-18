//! Integration tests for `hekma proxy` (story 21-8): `show` reads the state
//! directory read-only and starts nothing; `set identity-headers` routes
//! through the auto-started Service Host, stores the setting and reports
//! it; `hekma binding set --identity-headers` sets one binding's own; the
//! serve flags are mutually exclusive. Refusals land on the existing exit
//! classes.
//!
//! Every OS: no agent process; every HTTP call is the CLI's own, bounded.

mod helpers;

use std::path::Path;

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

#[test]
fn show_reads_without_starting_a_host_and_set_stores_the_switch_through_one() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let _host = HostGuard(state_dir);
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    // A read starts nothing: on by default, no address recorded yet.
    let run = hekma(&["proxy", "show"]);
    assert!(run.success, "{}", both(&run));
    assert!(run.stdout.contains("identity-headers on"), "{}", run.stdout);
    assert!(run.stdout.contains("no address recorded"), "{}", run.stdout);
    assert!(host_pid(state_dir).is_none(), "a read started a host");
    let shown = json(&hekma(&["proxy", "show", "--json"]));
    assert_eq!(shown["identity_headers"], true);
    assert_eq!(shown["forced_by_flag"], false);
    assert!(shown["port"].is_null());

    // A write auto-starts the host, which stores it.
    let run = hekma(&["proxy", "set", "identity-headers", "off"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        run.stdout
            .contains("identity-headers off: applies to the next"),
        "{}",
        run.stdout
    );
    let run = hekma(&["proxy", "show"]);
    assert!(
        run.stdout.contains("identity-headers off"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("metering proxy http://127.0.0.1:"),
        "{}",
        run.stdout
    );
    let set = json(&hekma(&[
        "proxy",
        "set",
        "identity-headers",
        "on",
        "--json",
    ]));
    assert_eq!(set["identity_headers"], true, "{set}");
    assert_eq!(set["stored_identity_headers"], true);
    let shown = json(&hekma(&["proxy", "show", "--json"]));
    assert_eq!(shown["identity_headers"], true);
    assert!(shown["port"].as_u64().is_some());

    // A binding's own setting, shown on the binding.
    let run = hekma(&[
        "binding",
        "create",
        "litellm",
        "--prefix",
        "/litellm",
        "--upstream",
        "http://127.0.0.1:9/v1",
    ]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&["binding", "set", "litellm", "--identity-headers", "off"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        run.stdout.contains("identity headers off"),
        "{}",
        run.stdout
    );
    let shown = json(&hekma(&["binding", "show", "litellm", "--json"]));
    assert_eq!(shown["binding"]["identity_headers"], false);
    let run = hekma(&["binding", "set", "litellm", "--identity-headers", "inherit"]);
    assert!(run.success, "{}", both(&run));
    assert!(!run.stdout.contains("identity headers"), "{}", run.stdout);
    let shown = json(&hekma(&["binding", "show", "litellm", "--json"]));
    assert!(shown["binding"].get("identity_headers").is_none());
}

#[test]
fn bad_values_and_both_serve_flags_are_usage_errors() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    for args in [
        vec!["proxy", "set", "identity-headers", "maybe"],
        vec!["proxy", "set", "labels", "on"],
        vec!["proxy", "set", "identity-headers"],
        vec!["binding", "set", "x", "--identity-headers", "sometimes"],
        vec!["serve", "--identity-headers", "--no-identity-headers"],
    ] {
        let run = run_hekma_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(2), "{args:?}: {}", both(&run));
    }
    assert!(host_pid(state_dir).is_none());
    // --help names the command and the flags.
    let run = run_hekma_agent(&["--help"], &ctx.project_dir, state_dir);
    assert!(run.stdout.contains("proxy"), "{}", run.stdout);
    let run = run_hekma_agent(&["serve", "--help"], &ctx.project_dir, state_dir);
    assert!(run.stdout.contains("--identity-headers"), "{}", run.stdout);
    assert!(
        run.stdout.contains("--no-identity-headers"),
        "{}",
        run.stdout
    );
}
