//! Integration tests for the Run Key verbs (story 22-1): `hekma run keys`
//! (read-only, starts nothing), `hekma run revoke-keys [--no-cascade]`
//! (routed through the auto-started Service Host), and `hekma run open
//! --print-key` (the pair printed once, with a stderr note) — their exit
//! codes, and the revocation's durability across a hard kill of the host
//! with a byte scan of the state directory.
//!
//! Every OS: the Run tree is SQL rows, no agent process; the hard-kill leg
//! runs where the host can be killed and restarted (it is the CLI's own
//! auto-started host on every OS). Every HTTP call is bounded; no stdin.

mod helpers;

use std::path::Path;
use std::time::Duration;

use helpers::{host_pid, kill_host, run_hekma_agent, stop_host, TestContext};

/// Stops the state root's auto-started host on drop (a failing assertion
/// never leaves one behind).
struct HostGuard<'a>(&'a Path);

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        stop_host(self.0);
    }
}

/// A state dir with an open root Run `run_01JZ6Q9G3S8V2X7K4M1N5P0R2T` (no process) and a live
/// supervised Run `run_LIVE` of an agent `live` with no credential (a Run
/// that started before Run Keys).
fn seed(state_dir: &Path) {
    drop(hekma_engine::Engine::open(Some(state_dir.to_path_buf())).unwrap());
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
         process_owner, created_at, updated_at) \
         VALUES ('run_01JZ6Q9G3S8V2X7K4M1N5P0R2T', 'announced', 0, 'active', '', 0, 't', 't'); \
         INSERT INTO agent_instances (name, kind, state, agent_home, created_at, updated_at) \
         VALUES ('live', 'mock', 'running', '/h', 't', 't'); \
         INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, instance_id, \
         process_owner, created_at, updated_at) VALUES ('run_LIVE', 'supervised', 0, \
         'active', '', (SELECT id FROM agent_instances WHERE name = 'live'), 1, 't', 't');",
    )
    .unwrap();
}

/// A Control API child open under `parent` with `bearer` on the state
/// root's host; (status, error code).
fn api(state_dir: &Path, method: &str, path: &str, bearer: &str, parent: &str) -> (u16, String) {
    let discovery = hekma_host::read_discovery(state_dir).expect("a host is recorded");
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .build(),
    );
    let request = ureq::http::Request::builder()
        .method(method)
        .uri(format!("http://{}{path}", discovery.addr))
        .header("Authorization", format!("Bearer {bearer}"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::json!({ "parent_run_id": parent, "issue_credentials": false }).to_string(),
        )
        .unwrap();
    let mut response = agent.run(request).unwrap();
    let status = response.status().as_u16();
    let body: serde_json::Value =
        serde_json::from_str(&response.body_mut().read_to_string().unwrap_or_default())
            .unwrap_or_default();
    (
        status,
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    )
}

/// Every file under `root` (the db, its -wal and -shm, the logs, every
/// Agent Home, control.json, serve.log) scanned for `sentinel`.
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
                    "{} carries a token",
                    path.display()
                );
            }
        }
    }
}

/// The value after `prefix` on the line that starts with it.
fn line_value(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn run_keys_is_read_only_and_names_a_run_that_started_before_run_keys() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);
    let run = run_hekma_agent(&["run", "keys", "run_LIVE"], &ctx.project_dir, state_dir);
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout
            .contains("This Run started before Run Keys; it gets one at its next launch."),
        "{}",
        run.stdout
    );
    let run = run_hekma_agent(
        &["run", "keys", "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["keys"].as_array().unwrap().len(), 0);
    assert!(json.get("note").is_none());
    // Starts nothing.
    assert!(host_pid(state_dir).is_none(), "run keys started a host");
    // Not found: exit 3.
    let run = run_hekma_agent(&["run", "keys", "run_ghost"], &ctx.project_dir, state_dir);
    assert_eq!(run.code, Some(3), "{}", run.stderr);
    // --help names the new verbs and flag.
    let run = run_hekma_agent(&["run", "--help"], &ctx.project_dir, state_dir);
    for word in ["keys", "revoke-keys"] {
        assert!(run.stdout.contains(word), "{}", run.stdout);
    }
    let run = run_hekma_agent(&["run", "open", "--help"], &ctx.project_dir, state_dir);
    assert!(run.stdout.contains("--print-key"), "{}", run.stdout);
}

#[test]
fn open_prints_the_pair_once_only_with_print_key_and_revoke_keys_refuses_them_at_once() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    seed(state_dir);
    let _host = HostGuard(state_dir);

    // Without --print-key: no credential is minted and none printed.
    let run = run_hekma_agent(
        &[
            "run",
            "open",
            "--parent",
            "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
            "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(json.get("credentials").is_none());
    assert!(!run.stdout.contains("hk_run_") && !run.stderr.contains("hk_run_"));
    let bare = json["run"]["run_id"].as_str().unwrap().to_string();
    let run = run_hekma_agent(
        &["run", "keys", &bare, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    let keys: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(keys["keys"].as_array().unwrap().len(), 0);

    // With it: both printed once on stdout, the note on stderr.
    let run = run_hekma_agent(
        &[
            "run",
            "open",
            "--parent",
            "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
            "--print-key",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let key = line_value(&run.stdout, "run key: ");
    let refresh = line_value(&run.stdout, "refresh credential: ");
    assert!(key.starts_with("hk_run_") && refresh.starts_with("hk_ref_"));
    assert_eq!(run.stdout.matches(&key).count(), 1);
    assert!(run.stderr.contains("shown once"), "{}", run.stderr);
    assert!(!run.stderr.contains(&key));
    let child = run
        .stdout
        .lines()
        .find_map(|line| line.split("Run ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap()
        .to_string();
    // --json --print-key carries them in the document.
    let run = run_hekma_agent(
        &[
            "run",
            "open",
            "--parent",
            "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
            "--print-key",
            "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(json["credentials"]["run_key"]
        .as_str()
        .unwrap()
        .starts_with("hk_run_"));

    // No read command shows them again.
    for args in [
        vec!["run", "show", child.as_str(), "--json"],
        vec!["run", "keys", child.as_str(), "--json"],
        vec!["run", "keys", child.as_str()],
        vec!["ps", "--all", "--json"],
    ] {
        let run = run_hekma_agent(&args, &ctx.project_dir, state_dir);
        assert!(run.success, "{args:?}: {}", run.stderr);
        assert!(!run.stdout.contains(&key) && !run.stdout.contains(&refresh));
        assert!(!run.stderr.contains(&key));
    }
    // The key works on the Control API, until it is revoked.
    assert_eq!(api(state_dir, "POST", "/v0/runs", &key, &child).0, 201);
    let run = run_hekma_agent(
        &["run", "revoke-keys", &child, "--no-cascade"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    assert!(
        run.stdout.contains(&format!(
            "Revoked 2 credential(s) for {child} and 0 under its child Runs"
        )),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains(&key));
    // Immediately — no sleep — refused.
    assert_eq!(
        api(state_dir, "POST", "/v0/runs", &key, &child),
        (401, "credential-revoked".to_string())
    );
    // --json reports token ids; a missing Run is exit 3.
    let run = run_hekma_agent(
        &["run", "revoke-keys", &child, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    let json: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["cascade"], true);
    assert_eq!(json["revoked"].as_array().unwrap().len(), 0);
    let run = run_hekma_agent(
        &["run", "revoke-keys", "run_ghost"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(3), "{}", run.stderr);

    // Durable: a hard kill of the host and a fresh one still refuse it.
    kill_host(state_dir);
    let run = run_hekma_agent(
        &["run", "open", "--parent", "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "{}", run.stderr);
    assert_eq!(
        api(state_dir, "POST", "/v0/runs", &key, &child),
        (401, "credential-revoked".to_string())
    );
    stop_host(state_dir);
    // No token on disk, before and after a WAL checkpoint.
    for secret in [&key, &refresh] {
        assert_no_sentinel(state_dir, secret);
    }
    {
        let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    for secret in [&key, &refresh] {
        assert_no_sentinel(state_dir, secret);
    }
}
