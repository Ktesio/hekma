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
