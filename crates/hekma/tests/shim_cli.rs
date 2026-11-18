//! The Shim preview through the CLI (story 24-1 phase 2): with
//! `HEKMA_SHIM=1`, the auto-started Service Host launches agents through
//! this binary's hidden `hekma shim`, records the Shim, and a stop ends the
//! agent through it. Without it nothing changes.
//!
//! Unix only for now (data-driven early return elsewhere).

mod helpers;

use std::path::Path;

use helpers::{run_hekma_agent_with_env, stop_host, KtRun, TestContext};

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
fn hekma_shim_1_launches_agents_through_a_shim_and_a_stop_ends_them() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let env = [("HEKMA_SHIM", "1")];
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    let run = hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&["agent", "start", "svc"]);
    assert!(run.success, "{}", both(&run));
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let (pid, socket): (i64, Option<String>) = conn
        .query_row("SELECT pid, shim_socket FROM agent_runtime", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    let socket = socket.expect("the host launched the agent through a Shim");
    assert!(Path::new(&socket).exists());
    let run = hekma(&["agent", "stop", "svc"]);
    assert!(run.success, "{}", both(&run));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent outlived its stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// SM-7: every stop, pause or block in the state directory's durable
/// record maps to an NFR-9 clause, and the actions taken are exactly
/// `expected` (in order).
fn sm7_holds(state_dir: &Path, expected: &[hekma_conformance::sm7::Clause]) {
    let actions = hekma_conformance::sm7::audit_state_dir(state_dir);
    let unauthorized = hekma_conformance::sm7::unauthorized(&actions);
    assert!(unauthorized.is_empty(), "unauthorized: {unauthorized:?}");
    let taken: Vec<_> = actions.iter().filter_map(|action| action.clause).collect();
    assert_eq!(taken, expected, "{actions:?}");
}

fn alive(pid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

/// Every file under `dir` whose text contains `needle` (the host's log).
fn logged(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            logged(&path, needle)
        } else {
            path.extension().is_some_and(|ext| ext == "log")
                && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        }
    })
}

/// Story 24-2: in Handoff Mode a stopping host releases a Shim-held agent
/// instead of stopping it; the next host reconnects to it, and its stop
/// still ends it.
#[test]
fn a_handoff_mode_host_leaves_shim_agents_running_for_the_next_host() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let env = [("HEKMA_SHIM", "1"), ("HEKMA_HANDOFF", "on")];
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    let run = hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&["agent", "start", "svc"]);
    assert!(run.success, "{}", both(&run));
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let pid: i64 = conn
        .query_row("SELECT pid FROM agent_runtime", [], |row| row.get(0))
        .unwrap();
    // SIGTERM the host: the agent is released, not stopped.
    stop_host(state_dir);
    assert!(
        alive(pid),
        "a Handoff-Mode shutdown must not stop a Shim-held agent"
    );
    sm7_holds(state_dir, &[]);
    assert!(
        logged(state_dir, "svc released to its Shim"),
        "the host notes the release"
    );
    // The next host (auto-started by this write) reconnects to it: a send
    // reaches the agent through its Shim again.
    let run = hekma(&["agent", "send", "svc", "ping"]);
    assert!(run.success, "{}", both(&run));
    assert!(alive(pid));
    let run = hekma(&["agent", "stop", "svc"]);
    assert!(run.success, "{}", both(&run));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent outlived its stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn control_doc(state_dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(state_dir.join("control.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Story 24-2: `hekma serve --upgrade` hands the running host over in
/// place: a poller dialing the metering proxy the whole time is never
/// refused, the Shim-held agent keeps running, `control.json` names the
/// successor, and the successor reaches the agent.
#[test]
fn serve_upgrade_hands_the_host_over_without_a_refused_connection() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let env = [("HEKMA_SHIM", "1")];
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    assert!(hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]).success);
    let run = hekma(&["agent", "start", "svc"]);
    assert!(run.success, "{}", both(&run));
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let agent: i64 = conn
        .query_row("SELECT pid FROM agent_runtime", [], |row| row.get(0))
        .unwrap();
    let before = control_doc(state_dir);
    let old_pid = before["pid"].as_i64().unwrap();
    let proxy = before["metering_proxy_url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .to_string();
    // Dial the proxy continuously across the whole upgrade.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = std::thread::spawn({
        let stop = std::sync::Arc::clone(&stop);
        move || {
            let (mut ok, mut refused) = (0u32, 0u32);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match std::net::TcpStream::connect(&proxy) {
                    Ok(_) => ok += 1,
                    Err(_) => refused += 1,
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            (ok, refused)
        }
    });
    let run = hekma(&["serve", "--upgrade"]);
    assert!(run.success, "{}", both(&run));
    assert!(both(&run).contains("upgraded in place"), "{}", both(&run));
    // The predecessor exits once the successor serves.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while alive(old_pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the old host never exited"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (ok, refused) = poller.join().unwrap();
    assert!(ok > 0, "the poller reached the proxy");
    assert_eq!(
        refused, 0,
        "no connection refused across the upgrade ({ok} reached it)"
    );
    sm7_holds(state_dir, &[]);
    let after = control_doc(state_dir);
    assert_ne!(
        after["pid"].as_i64().unwrap(),
        old_pid,
        "control.json names the successor"
    );
    assert!(alive(agent), "the agent kept running");
    assert!(
        logged(state_dir, "handed over to the successor"),
        "the predecessor notes the hand-over"
    );
    let run = hekma(&["agent", "send", "svc", "ping"]);
    assert!(run.success, "{}", both(&run));
    let run = hekma(&["agent", "stop", "svc"]);
    assert!(run.success, "{}", both(&run));
}

/// Story 24-2: with no host serving, `--upgrade` starts nothing and says so.
#[test]
fn serve_upgrade_without_a_host_refuses_and_starts_none() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let run = run_hekma_agent_with_env(&["serve", "--upgrade"], &ctx.project_dir, state_dir, &[]);
    assert!(!run.success);
    assert_eq!(run.code, Some(2), "{}", both(&run));
    assert!(both(&run).contains("nothing to upgrade"), "{}", both(&run));
    assert!(
        !state_dir.join("control.json").exists(),
        "no host was started"
    );
}

/// Story 24-3: SIGKILL a host holding a Shim agent, then let the next
/// `hekma` write start a host, under the dead host's recorded mode.
/// Returns the agent's pid, the end cause of its Run (`None` while open),
/// and whether the agent is still alive.
/// `GET /v0/host` from the running host, with the local Operator token.
fn host_doc(state_dir: &Path) -> serde_json::Value {
    use std::io::{Read, Write};
    let control = control_doc(state_dir);
    let addr = control["addr"].as_str().unwrap().to_string();
    let token = control["token"].as_str().unwrap().to_string();
    let mut stream = std::net::TcpStream::connect(&addr).unwrap();
    write!(
        stream,
        "GET /v0/host HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let body = &answer[answer.find("\r\n\r\n").unwrap() + 4..];
    serde_json::from_str(body.trim()).unwrap()
}

fn crash_with(mode: Option<&str>) -> (i64, Option<String>, bool, String) {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let mut env = vec![("HEKMA_SHIM", "1")];
    if let Some(mode) = mode {
        env.push(("HEKMA_HANDOFF", mode));
    }
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    assert!(hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]).success);
    let run = hekma(&["agent", "start", "svc"]);
    assert!(run.success, "{}", both(&run));
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let pid: i64 = conn
        .query_row("SELECT pid FROM agent_runtime", [], |row| row.get(0))
        .unwrap();
    helpers::kill_host(state_dir);
    assert!(alive(pid), "a SIGKILLed host leaves its Shim agent running");
    // The next write starts a host, which reads the dead host's record.
    let run = hekma(&["agent", "register", "other", "--manifest", &manifest_arg]);
    assert!(run.success, "{}", both(&run));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while mode == Some("off") && alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let cause: Option<String> = conn
        .query_row(
            "SELECT r.end_cause FROM runs r JOIN agent_instances a ON a.id = r.instance_id \
             WHERE a.name = 'svc' ORDER BY r.created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let log = std::fs::read_to_string(state_dir.join("serve.log")).unwrap_or_default();
    // SM-7: the only stop the crash recovery may take is the cleanup of
    // an explicit off-mode host's agent.
    let cleanup: &[hekma_conformance::sm7::Clause] = if mode == Some("off") {
        &[hekma_conformance::sm7::Clause::OrphanCleanup]
    } else {
        &[]
    };
    sm7_holds(state_dir, cleanup);
    // Story 24-3: the new host's one summary of what it reconciled.
    let recovery = &host_doc(state_dir)["recovery"];
    let (posture, kept, cleaned) = match mode {
        Some("off") => ("off", 0, 1),
        Some("on") => ("on", 1, 0),
        _ => ("unknown", 1, 0),
    };
    assert_eq!(recovery["change"], "recovered", "{recovery}");
    assert_eq!(recovery["posture"], posture, "{recovery}");
    assert_eq!(recovery["runs_adopted"], kept, "{recovery}");
    assert_eq!(recovery["runs_cleaned"], cleaned, "{recovery}");
    let still = alive(pid);
    if still {
        assert!(
            hekma(&["agent", "send", "svc", "ping"]).success,
            "reconnected"
        );
        assert!(hekma(&["agent", "stop", "svc"]).success);
    }
    (pid, cause, still, log)
}

/// Story 24-3: Handoff Mode explicitly off — the crashed host's agent is
/// stopped by the next host, the ordinary way, and its Run ends
/// `engine-crash`, named in the notice.
#[test]
fn after_a_crash_with_handoff_off_the_next_host_cleans_up_its_agents() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let (_pid, cause, alive_after, log) = crash_with(Some("off"));
    assert!(!alive_after, "cleaned up");
    assert_eq!(cause.as_deref(), Some("engine-crash"));
    assert!(
        log.contains("their Runs end with cause engine-crash"),
        "{log}"
    );
}

/// Story 24-3: Handoff Mode on — reconnected and kept; and with the mode
/// never chosen — adopted, never stopped.
#[test]
fn after_a_crash_with_handoff_on_or_unchosen_the_next_host_keeps_its_agents() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let (_pid, cause, alive_after, log) = crash_with(Some("on"));
    assert!(alive_after && cause.is_none(), "kept: {cause:?}");
    assert!(log.contains("reconnected to its 1 agent(s): svc"), "{log}");
    let (_pid, cause, alive_after, log) = crash_with(None);
    assert!(alive_after && cause.is_none(), "kept: {cause:?}");
    assert!(log.contains("adopted, not stopped: svc"), "{log}");
}

/// SM-7 (story 24-3): SIGTERM with Handoff Mode off stops the agent — the
/// one action NFR-9's shutdown clause authorizes, and nothing else.
#[test]
fn sigterm_with_handoff_off_stops_its_agents_by_the_shutdown_clause_only() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let env = [("HEKMA_SHIM", "1"), ("HEKMA_HANDOFF", "off")];
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    assert!(hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]).success);
    assert!(hekma(&["agent", "start", "svc"]).success);
    let conn = rusqlite::Connection::open(state_dir.join("state.db")).unwrap();
    let pid: i64 = conn
        .query_row("SELECT pid FROM agent_runtime", [], |row| row.get(0))
        .unwrap();
    stop_host(state_dir);
    assert!(
        !alive(pid),
        "Handoff Mode off: the shutdown stops the agent"
    );
    sm7_holds(state_dir, &[hekma_conformance::sm7::Clause::VerbOrShutdown]);
}

/// Story 24-3 (T5, R2): everything committed before a `kill -9` survives
/// it exactly — a child Run's labels, its limits and a hold set on it, and
/// the agent's own Run — read back unchanged through the next host, whose
/// first snapshot already holds the hold.
#[test]
fn a_kill_9_loses_nothing_committed_before_it() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = tempfile::tempdir().unwrap();
    // Engine-observed, so its Runs have a wire path (and can be held).
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .metering("engine-observed")
        .write(manifest.path());
    let _host = HostGuard(state_dir);
    let env = [
        ("HEKMA_SHIM", "1"),
        ("HEKMA_HANDOFF", "on"),
        ("DRILL_PROVIDER_KEY", "sk-drill-not-a-real-key"),
    ];
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &env);
    let manifest_arg = manifest.path().to_string_lossy().into_owned();
    assert!(hekma(&["agent", "register", "svc", "--manifest", &manifest_arg]).success);
    let set = hekma(&[
        "agent",
        "config",
        "set",
        "svc",
        "provider.api_key",
        "secret:DRILL_PROVIDER_KEY",
    ]);
    assert!(set.success, "{}", both(&set));
    let set = hekma(&[
        "agent",
        "config",
        "set",
        "svc",
        "metering.upstream_base_url",
        "http://127.0.0.1:9/v1",
    ]);
    assert!(set.success, "{}", both(&set));
    // Custody off: the (fake) key is delivered to the agent as is.
    let set = hekma(&["agent", "config", "set", "svc", "provider.custody", "off"]);
    assert!(set.success, "{}", both(&set));
    let started = hekma(&["agent", "start", "svc"]);
    assert!(started.success, "{}", both(&started));
    let opened = hekma(&[
        "run", "open", "--parent", "svc", "--label", "job=7", "--json",
    ]);
    assert!(opened.success, "{}", both(&opened));
    let doc: serde_json::Value = serde_json::from_str(&opened.stdout).unwrap();
    let child = doc["run"]["run_id"].as_str().unwrap().to_string();
    for args in [
        vec!["label", "set", child.as_str(), "team=core"],
        vec!["budget", "set", child.as_str(), "--tokens-per-run", "1000"],
        vec!["hold", child.as_str()],
    ] {
        let run = hekma(&args);
        assert!(run.success, "{args:?}: {}", both(&run));
    }
    let show = |id: &str| -> serde_json::Value {
        let run = hekma(&["run", "show", id, "--json"]);
        assert!(run.success, "{}", both(&run));
        serde_json::from_str(&run.stdout).unwrap()
    };
    let before_child = show(&child);
    let parent = before_child["run"]["parent_run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let before_parent = show(&parent);
    assert_eq!(before_child["run"]["status"], "held");

    // A call on the held Run parks at the proxy (O4's crash contract: the
    // dying host sends it nothing; the connection just ends).
    let proxy = control_doc(state_dir)["metering_proxy_url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .to_string();
    let path = format!("/_hekma/runs/{child}/v1/chat/completions");
    let parked = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(&proxy).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .unwrap();
        let body = r#"{"model":"m","messages":[]}"#;
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        answer
    });
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert!(!parked.is_finished(), "the call is parked by the hold");

    helpers::kill_host(state_dir);
    let answer = parked.join().unwrap();
    assert!(
        answer.is_empty(),
        "a SIGKILLed host answers nothing: {}",
        String::from_utf8_lossy(&answer)
    );
    // The next write starts a host over the same state directory.
    assert!(hekma(&["agent", "register", "other", "--manifest", &manifest_arg]).success);
    let after_child = show(&child);
    let after_parent = show(&parent);
    assert_eq!(
        after_child, before_child,
        "the child Run is exactly as committed"
    );
    // The parked call's record closes once, at the next start, as ended by
    // the crash.
    let listed = hekma(&["calls", "list", "--run", child.as_str(), "--json"]);
    assert!(listed.success, "{}", both(&listed));
    let doc: serde_json::Value = serde_json::from_str(&listed.stdout).unwrap();
    let records = doc["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(records.len(), 1, "{doc}");
    assert_eq!(records[0]["outcome"], "crash", "{doc}");
    assert_eq!(records[0]["disposition"], "hold", "{doc}");
    assert_eq!(records[0]["hold_end"], "crash", "{doc}");
    assert_eq!(records[0]["status_class"], "transport-error", "{doc}");
    for field in [
        "run_id",
        "status",
        "anomalies",
        "labels",
        "limits",
        "dispositions",
        "custody",
    ] {
        assert_eq!(
            after_parent["run"][field], before_parent["run"][field],
            "the agent's Run kept {field}"
        );
    }
    // SM-7: the crash took no action (the hold is an Operator verb).
    let actions = hekma_conformance::sm7::audit_state_dir(state_dir);
    assert!(
        hekma_conformance::sm7::unauthorized(&actions).is_empty(),
        "{actions:?}"
    );
    assert!(hekma(&["release", child.as_str()]).success);
    assert!(hekma(&["agent", "stop", "svc"]).success);
}

/// Story 24-3 (T5, R2): an announced Run's Lease survives a `kill -9`
/// unchanged — the next host shows the same Run, status and Lease.
#[test]
fn an_announced_runs_lease_survives_a_kill_9() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let _host = HostGuard(state_dir);
    let hekma = |args: &[&str]| run_hekma_agent_with_env(args, &ctx.project_dir, state_dir, &[]);
    let created = hekma(&[
        "binding",
        "create",
        "gw",
        "--prefix",
        "/gw",
        "--upstream",
        "http://127.0.0.1:9/v1",
    ]);
    assert!(created.success, "{}", both(&created));
    let issued = hekma(&[
        "binding",
        "credential",
        "issue",
        "gw",
        "--class",
        "announce",
        "--json",
    ]);
    assert!(issued.success, "{}", both(&issued));
    let doc: serde_json::Value = serde_json::from_str(&issued.stdout).unwrap();
    let credential = ["credential", "value", "token"]
        .iter()
        .find_map(|key| {
            doc[key]
                .as_str()
                .or_else(|| doc["credential"][key].as_str())
        })
        .unwrap_or_else(|| panic!("no credential in {doc}"))
        .to_string();
    // Announce over the local endpoint.
    let addr = control_doc(state_dir)["addr"].as_str().unwrap().to_string();
    let body = r#"{"external_id":"nightly-1","labels":{"step":"draft"}}"#;
    let request = format!(
        "POST /v0/runs/announce HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {credential}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let answer = {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    };
    assert!(answer.starts_with("HTTP/1.1 201"), "{answer}");
    let json = &answer[answer.find("\r\n\r\n").unwrap() + 4..];
    let announced: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    let run_id = ["run_id"]
        .iter()
        .find_map(|key| announced["run"][key].as_str().or(announced[key].as_str()))
        .unwrap_or_else(|| panic!("no run id in {announced}"))
        .to_string();
    let show = || -> serde_json::Value {
        let run = hekma(&["run", "show", run_id.as_str(), "--json"]);
        assert!(run.success, "{}", both(&run));
        serde_json::from_str(&run.stdout).unwrap()
    };
    let before = show();
    assert!(before["run"]["lease_expires_at"].is_string(), "{before}");

    helpers::kill_host(state_dir);
    // The next write starts a host over the same state directory.
    let restarted = hekma(&["label", "set", run_id.as_str(), "after=crash"]);
    assert!(restarted.success, "{}", both(&restarted));
    let after = show();
    for field in [
        "run_id",
        "status",
        "lease_expires_at",
        "control_depth",
        "anomalies",
    ] {
        assert_eq!(
            after["run"][field], before["run"][field],
            "{field}: {after}"
        );
    }
    let actions = hekma_conformance::sm7::audit_state_dir(state_dir);
    assert!(
        hekma_conformance::sm7::unauthorized(&actions).is_empty(),
        "{actions:?}"
    );
}
