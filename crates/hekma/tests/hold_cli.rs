//! Integration tests for `hekma hold|release|reroute|reject` (story 23-4)
//! against a live auto-started Service Host: one bare call through the
//! shared Metering Proxy creates the binding's wire Run, and each verb acts
//! on its model calls — by run id, through a Selector, dry-run and `--json`
//! — and `hekma agent pause|resume` on that Run answers the qualified note.
//!
//! Every OS: no agent process; every HTTP call is bounded.

mod helpers;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use helpers::{run_hekma_agent, stop_host, KtRun, TestContext};

/// Stops the state root's auto-started host on drop.
struct HostGuard<'a>(&'a Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

/// One bare model call through the proxy at `base` (`http://host:port/prefix`).
/// The upstream is unreachable; the call's Run exists all the same.
fn call_through(base: &str) {
    let rest = base.strip_prefix("http://").expect("an http base");
    let (authority, prefix) = rest.split_once('/').expect("a prefix");
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let body = r#"{"model":"m","messages":[]}"#;
    write!(
        stream,
        "POST /{prefix}/chat HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
}

fn both(run: &KtRun) -> String {
    format!("{}\n{}", run.stdout, run.stderr)
}

fn json(run: &KtRun) -> serde_json::Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|err| panic!("{err}: {}", both(run)))
}

#[test]
fn the_hold_family_acts_on_a_wire_runs_calls_by_id_and_through_a_selector() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let _host = HostGuard(state_dir);
    let hekma = |args: &[&str]| run_hekma_agent(args, &ctx.project_dir, state_dir);

    let run = hekma(&[
        "binding",
        "create",
        "openai",
        "--prefix",
        "/openai",
        "--upstream",
        "http://127.0.0.1:9/v1",
        "--label",
        "team=core",
    ]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&[
        "binding",
        "route",
        "add",
        "openai",
        "backup",
        "--upstream",
        "http://127.0.0.1:9/v2",
    ]);
    assert!(run.success, "{}", both(&run));
    let shown = json(&hekma(&["binding", "show", "openai", "--json"]));
    call_through(shown["client_base_url"].as_str().expect("a recorded port"));
    let listing = json(&hekma(&["ps", "--all", "--json"]));
    let run_id = listing["items"][0]["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{listing}"))
        .to_string();
    let id = run_id.as_str();

    // By id: hold, again (unchanged), dry run, --json.
    let run = hekma(&["hold", id]);
    assert!(run.success, "{}", both(&run));
    let out = both(&run);
    assert!(out.contains(&format!("Run {id} held")), "{out}");
    assert!(out.contains("dispositions: hold"), "{out}");
    assert!(out.contains("calls already sent complete"), "{out}");
    let run = hekma(&["hold", id]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("already held"), "{}", both(&run));
    let run = hekma(&["hold", id, "--dry-run"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        both(&run).contains(&format!("Run {id} would hold")),
        "{}",
        both(&run)
    );
    let document = json(&hekma(&["reroute", id, "--via", "openai/backup", "--json"]));
    let row = &document["results"][0];
    assert_eq!(row["outcome"], "rerouted", "{document}");
    assert_eq!(
        row["reasons"],
        serde_json::json!(["hold", "reroute openai/backup"]),
        "{document}"
    );
    let run = hekma(&["reroute", id, "--off"]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("dispositions: hold"), "{}", both(&run));
    let document = json(&hekma(&["release", id, "--json"]));
    assert_eq!(document["results"][0]["outcome"], "released", "{document}");
    assert_eq!(document["results"][0]["reasons"], serde_json::json!([]));
    let run = hekma(&["release", id]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("dispositions: none"), "{}", both(&run));

    // Through a Selector: dry run, act, and the set form per verb.
    let run = hekma(&["hold", "--selector", "team=core", "--dry-run"]);
    assert!(run.success, "{}", both(&run));
    assert!(
        both(&run).contains(&format!("Run {id} would hold")),
        "{}",
        both(&run)
    );
    for args in [
        vec!["reject", "--selector", "team=core", "--yes"],
        vec![
            "reroute",
            "--selector",
            "team=core",
            "--via",
            "openai/backup",
            "--yes",
        ],
        vec!["hold", "--selector", "team=core", "--yes"],
        vec!["hold", "--selector", "team=core", "--yes"],
        vec!["release", "--selector", "team=core", "--yes", "--json"],
    ] {
        let run = hekma(&args);
        assert!(run.success, "{args:?}: {}", both(&run));
        assert!(both(&run).contains(id), "{args:?}: {}", both(&run));
    }
    let run = hekma(&["release", id]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&["reject", id]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("rejected"), "{}", both(&run));
    let shown = json(&hekma(&["run", "show", id, "--json"]));
    assert_eq!(
        shown["run"]["dispositions"],
        serde_json::json!(["reject", "reroute openai/backup"]),
        "{shown}"
    );

    // The lifecycle verbs on a wire Run act on its calls, qualified.
    let run = hekma(&["agent", "pause", id]);
    assert!(run.success, "{}", both(&run));
    assert!(
        both(&run).contains("its model calls at the metering proxy"),
        "{}",
        both(&run)
    );
    let document = json(&hekma(&["agent", "resume", id, "--json"]));
    assert_eq!(document["results"][0]["acts_on"], "calls", "{document}");

    // Stop rejects its calls and ends the Run; an ended Run is then
    // refused, dry run and act alike.
    let run = hekma(&["agent", "stop", id]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("rejects"), "{}", both(&run));
    let run = hekma(&["hold", id, "--dry-run"]);
    assert!(!run.success, "{}", both(&run));
    assert!(both(&run).contains("has ended"), "{}", both(&run));
    let run = hekma(&["hold", id]);
    assert!(!run.success, "{}", both(&run));
}
