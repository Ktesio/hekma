//! Integration tests for `hekma binding` (story 23-1): reads open the state
//! directory read-only and start nothing; writes route through the
//! auto-started Service Host, whose shared Metering Proxy port `show` turns
//! into the client base URL. Refusals land on the existing exit classes
//! (2 invalid, 3 not found), and a per-binding credential is printed once
//! and never written anywhere in the state directory.
//!
//! Every OS: no agent process; every HTTP call is the CLI's own, bounded.

mod helpers;

use std::path::Path;

use helpers::{host_pid, run_hekma_agent, stop_host, TestContext};

/// Stops the state root's auto-started host on drop (a failing assertion
/// never leaves one behind).
struct HostGuard<'a>(&'a Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

/// Every file under `root` scanned for `sentinel`.
fn assert_no_sentinel(root: &Path, sentinel: &str) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !bytes
                        .windows(sentinel.len())
                        .any(|window| window == sentinel.as_bytes()),
                    "{} carries a credential",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn reads_start_nothing_and_name_a_missing_binding_as_not_found() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());

    let run = run_hekma_agent(&["binding", "list"], &ctx.project_dir, state_dir);
    assert!(run.success, "{}", run.stderr);
    assert_eq!(run.stdout.trim(), "no bindings");
    let run = run_hekma_agent(&["binding", "list", "--json"], &ctx.project_dir, state_dir);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["bindings"].as_array().unwrap().len(), 0);

    for args in [
        &["binding", "show", "ghost"][..],
        &["binding", "credential", "list", "ghost"][..],
    ] {
        let run = run_hekma_agent(args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(3), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains("ghost"), "{}", run.stderr);
    }
    assert!(host_pid(state_dir).is_none(), "a read started a host");

    // A flag mistake is refused before anything is opened or started.
    let run = run_hekma_agent(&["binding", "set", "ghost"], &ctx.project_dir, state_dir);
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(run.stderr.contains("nothing to set"), "{}", run.stderr);
    let run = run_hekma_agent(
        &[
            "binding",
            "credential",
            "issue",
            "ghost",
            "--class",
            "admin",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(2), "{}", run.stderr);
    assert!(
        host_pid(state_dir).is_none(),
        "a refused flag started a host"
    );

    let run = run_hekma_agent(&["binding", "--help"], &ctx.project_dir, state_dir);
    for word in [
        "create",
        "list",
        "show",
        "set",
        "delete",
        "route",
        "credential",
    ] {
        assert!(run.stdout.contains(word), "{word}: {}", run.stdout);
    }
}

#[test]
fn create_show_set_and_delete_route_through_the_host_with_typed_refusals() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let _host = HostGuard(state_dir);

    let run = run_hekma_agent(
        &[
            "binding",
            "create",
            "openai",
            "--prefix",
            "/openai",
            "--upstream",
            "https://api.openai.com/v1",
            "--label",
            "team=core",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout
            .contains("openai: /openai -> https://api.openai.com/v1 [team=core]"),
        "{}",
        run.stdout
    );

    // `show` reads locally and prints the client base URL: the host that
    // served the write recorded the shared proxy's port.
    let run = run_hekma_agent(
        &["binding", "show", "openai", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["binding"]["prefix"], "/openai");
    assert_eq!(json["binding"]["labels"][0][0], "team");
    let base = json["client_base_url"].as_str().expect("a recorded port");
    assert!(
        base.starts_with("http://127.0.0.1:") && base.ends_with("/openai"),
        "{base}"
    );
    let run = run_hekma_agent(&["binding", "show", "openai"], &ctx.project_dir, state_dir);
    assert!(
        run.stdout.contains(&format!("client base URL {base}")),
        "{}",
        run.stdout
    );

    // Typed refusals: each names its field or its clash, on exit 2.
    for (args, needle) in [
        (
            &[
                "binding",
                "create",
                "dup",
                "--prefix",
                "/openai",
                "--upstream",
                "http://127.0.0.1:9",
            ][..],
            "already bound by `openai`",
        ),
        (
            &[
                "binding",
                "create",
                "openai",
                "--prefix",
                "/other",
                "--upstream",
                "http://127.0.0.1:9",
            ][..],
            "already exists",
        ),
        (
            &[
                "binding",
                "create",
                "inner",
                "--prefix",
                "/_hekma/x",
                "--upstream",
                "http://127.0.0.1:9",
            ][..],
            "prefix",
        ),
        (
            &[
                "binding",
                "create",
                "creds",
                "--prefix",
                "/creds",
                "--upstream",
                "https://u:p@example.com",
            ][..],
            "upstream",
        ),
        (
            &[
                "binding",
                "create",
                "keyed",
                "--prefix",
                "/keyed",
                "--upstream",
                "https://example.com",
                "--custodied-key",
                "sk-plain",
            ][..],
            "custodied",
        ),
    ] {
        let run = run_hekma_agent(args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(2), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(needle), "{args:?}: {}", run.stderr);
    }

    // set + a route, then a missing route and a missing binding are exit 3.
    let run = run_hekma_agent(
        &["binding", "set", "openai", "--clear-labels"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout
            .contains("openai: /openai -> https://api.openai.com/v1\n"),
        "{}",
        run.stdout
    );
    let run = run_hekma_agent(
        &[
            "binding",
            "route",
            "add",
            "openai",
            "backup",
            "--upstream",
            "https://example.com/v1",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let run = run_hekma_agent(&["binding", "show", "openai"], &ctx.project_dir, state_dir);
    assert!(
        run.stdout
            .contains("route backup -> https://example.com/v1"),
        "{}",
        run.stdout
    );
    for args in [
        &["binding", "route", "remove", "openai", "ghost"][..],
        &["binding", "delete", "ghost"][..],
    ] {
        let run = run_hekma_agent(args, &ctx.project_dir, state_dir);
        assert_eq!(run.code, Some(3), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains("hekma binding list"), "{}", run.stderr);
    }

    let run = run_hekma_agent(
        &["binding", "delete", "openai"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let run = run_hekma_agent(&["binding", "show", "openai"], &ctx.project_dir, state_dir);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
}

#[test]
fn a_credential_is_printed_once_rotates_on_reissue_and_is_never_stored() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let _host = HostGuard(state_dir);
    let run = run_hekma_agent(
        &[
            "binding",
            "create",
            "gw",
            "--prefix",
            "/gw",
            "--upstream",
            "http://127.0.0.1:9",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);

    let issue = |class: &str| {
        let run = run_hekma_agent(
            &["binding", "credential", "issue", "gw", "--class", class],
            &ctx.project_dir,
            state_dir,
        );
        assert!(run.success, "{}", run.stderr);
        assert!(run.stderr.contains("shown once"), "{}", run.stderr);
        let token = run.stdout.trim().to_string();
        assert!(!run.stderr.contains(&token));
        token
    };
    let first = issue("proxy-access");
    assert!(first.starts_with("hk_pxy_"), "{first}");
    let second = issue("proxy-access");
    assert_ne!(first, second);
    let announce = issue("announce");
    assert!(announce.starts_with("hk_ann_"), "{announce}");

    // The listing shows ids and states only: the reissue revoked the first.
    let run = run_hekma_agent(
        &["binding", "credential", "list", "gw", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    let states: Vec<(String, String)> = json["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["class"].as_str().unwrap().to_string(),
                c["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        states
            .iter()
            .filter(|(c, s)| c == "proxy-access" && s == "active")
            .count(),
        1,
        "{states:?}"
    );
    assert_eq!(
        states
            .iter()
            .filter(|(c, s)| c == "proxy-access" && s == "revoked")
            .count(),
        1,
        "{states:?}"
    );
    for token in [&first, &second, &announce] {
        assert!(
            !run.stdout.contains(token.as_str()),
            "the listing shows a token"
        );
    }

    let run = run_hekma_agent(
        &[
            "binding",
            "credential",
            "revoke",
            "gw",
            "--class",
            "proxy-access",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains("gw: proxy-access credential revoked"),
        "{}",
        run.stdout
    );
    let run = run_hekma_agent(
        &["binding", "credential", "list", "gw"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !run.stdout.contains("proxy-access (active"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("announce (active"), "{}", run.stdout);

    // No token reaches any file under the state directory (the db, its
    // -wal and -shm, the logs, control.json, serve.log).
    stop_host(state_dir);
    for token in [&first, &second, &announce] {
        assert_no_sentinel(state_dir, token);
    }
}
