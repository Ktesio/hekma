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
