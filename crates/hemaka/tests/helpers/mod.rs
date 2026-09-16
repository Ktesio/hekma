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

/// Full result of a `hemaka` invocation, including the exit-success flag and the
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
    /// stable `hemaka` contract (`0` success · `1` general · `2` usage · `3` not-found
    /// · `4` invalid-state · `5` unsupported-capability · `6` timed-out). `None`
    /// only when the process was killed by a signal without producing a code,
    /// which no `hemaka` test path expects.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `hemaka` with `KTESIO_STATE_DIR` pinned to `state_dir` so the engine never
/// touches the real user data dir. Also sets `KTESIO_NO_UPDATE_CHECK=1`.
///
/// Returns the full [`KtRun`] regardless of exit status.
#[allow(dead_code)]
pub fn run_hemaka_agent(args: &[&str], working_dir: &Path, state_dir: &Path) -> KtRun {
    run_hemaka_agent_with_env(args, working_dir, state_dir, &[])
}

/// Like [`run_hemaka_agent`], but with EXTRA environment variables layered on (e.g.
/// `COLUMNS` to force a narrow terminal so the table renderer truncates cells — the
/// FR-23 `list`-surface truncation test needs a deterministic width, independent of
/// the runner's real terminal size).
#[allow(dead_code)]
pub fn run_hemaka_agent_with_env(
    args: &[&str],
    working_dir: &Path,
    state_dir: &Path,
    extra_env: &[(&str, &str)],
) -> KtRun {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hemaka"));
    command
        .args(args)
        .current_dir(working_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().expect("Failed to execute hemaka");

    KtRun {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}
