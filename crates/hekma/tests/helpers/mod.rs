use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

pub struct TestContext {
    _temp_dir: TempDir,
    pub project_dir: PathBuf,
}

#[allow(dead_code)]
impl TestContext {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let project_dir = temp_dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("Failed to create project directory");

        Self {
            _temp_dir: temp_dir,
            project_dir,
        }
    }
}

/// Full result of a `hekma` invocation, including the exit-success flag and the
/// NUMERIC exit code.
///
/// This never collapses a non-zero exit into an `Err` — agent tests need to
/// assert exit codes AND inspect stderr on the failure paths (duplicate name,
/// running-without-force).
#[allow(dead_code)]
#[derive(Debug)]
pub struct KtRun {
    pub success: bool,
    /// The numeric process exit code (story 4-3, DC-5/DC-6) — the documented,
    /// stable `hekma` contract (`0` success · `1` general · `2` usage · `3` not-found
    /// · `4` invalid-state · `5` unsupported-capability · `6` timed-out). `None`
    /// only when the process was killed by a signal without producing a code,
    /// which no `hekma` test path expects.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `hekma` with `KTESIO_STATE_DIR` pinned to `state_dir` so the engine never
/// touches the real user data dir. Also sets `KTESIO_NO_UPDATE_CHECK=1`.
///
/// Returns the full [`KtRun`] regardless of exit status.
#[allow(dead_code)]
pub fn run_hekma_agent(args: &[&str], working_dir: &Path, state_dir: &Path) -> KtRun {
    run_hekma_agent_with_env(args, working_dir, state_dir, &[])
}

/// Like [`run_hekma_agent`], but with EXTRA environment variables layered on (e.g.
/// `COLUMNS` to force a narrow terminal so the table renderer truncates cells — the
/// FR-23 `list`-surface truncation test needs a deterministic width, independent of
/// the runner's real terminal size).
#[allow(dead_code)]
pub fn run_hekma_agent_with_env(
    args: &[&str],
    working_dir: &Path,
    state_dir: &Path,
    extra_env: &[(&str, &str)],
) -> KtRun {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hekma"));
    command
        .args(args)
        .current_dir(working_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().expect("Failed to execute hekma");

    KtRun {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// The pid of the Service Host serving `state_dir` (from its `control.json`),
/// when one is recorded (story 21-4).
#[allow(dead_code)]
pub fn host_pid(state_dir: &Path) -> Option<u32> {
    hekma_host::read_discovery(state_dir).ok().map(|d| d.pid)
}

/// The pid of the Service Host serving `state_dir`, or a panic naming `what`
/// with the host's `serve.log` (an auto-started host's whole output), so a
/// CI failure shows why no host was recorded.
#[allow(dead_code)]
pub fn expect_host_pid(state_dir: &Path, what: &str) -> u32 {
    host_pid(state_dir).unwrap_or_else(|| {
        let log = std::fs::read_to_string(hekma_host::serve_log_file(state_dir))
            .unwrap_or_else(|err| format!("<no serve.log: {err}>"));
        panic!(
            "{what}: no control.json in {}; serve.log:\n{log}",
            state_dir.display()
        )
    })
}

/// Whether a process with `pid` is still alive (`kill -0` on Unix,
/// `tasklist` on Windows — data-driven, no OS cfg).
#[allow(dead_code)]
pub fn process_alive(pid: u32) -> bool {
    if hekma_conformance::test_support::current_os_key() == "windows" {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Story 21-4: stop the state root's auto-started Service Host — SIGTERM (its
/// normal drain) on Unix, `taskkill` on Windows — and wait for it to exit.
/// The NEXT CLI write then starts a fresh host, whose engine ADOPTS whatever a
/// helper engine started in between (a host only supervises what it started
/// or adopted when it opened). A no-op when no host is recorded.
#[allow(dead_code)]
pub fn stop_host(state_dir: &Path) {
    let Some(pid) = host_pid(state_dir) else {
        return;
    };
    if hekma_conformance::test_support::current_os_key() == "windows" {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    } else {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the Service Host (pid {pid}) did not exit after the stop request"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Story 21-4: KILL the state root's Service Host abruptly (SIGKILL on Unix,
/// `taskkill /F` on Windows) and wait for it to die — the crashed-host shape:
/// a DETACHED agent survives (its handle was disarmed), nothing drains, and
/// the stale `control.json` stays behind (the next CLI write finds it dead
/// and starts a fresh host, which adopts the survivor). This is how a test
/// reaches the "between hosts" window a detached agent runs in.
#[allow(dead_code)]
pub fn kill_host(state_dir: &Path) {
    let Some(pid) = host_pid(state_dir) else {
        return;
    };
    if hekma_conformance::test_support::current_os_key() == "windows" {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    } else {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while process_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the Service Host (pid {pid}) did not die after SIGKILL"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
