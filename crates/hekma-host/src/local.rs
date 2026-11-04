//! The local endpoint's discovery file and the detached auto-start (story
//! 21-4, AD-20's one data-path owner).
//!
//! Every `hekma serve` binds a loopback Control API listener and records it
//! in `<state-root>/control.json` — `{schema_version, pid, addr, token,
//! hekma_version, autostarted}` — so the local `hekma` CLI can find the
//! state root's owner. The file is created OWNER-ONLY (`0600` on Unix; on
//! Windows it inherits the state directory's ACL), written to a temp file and
//! renamed into place AFTER the bind, and removed after the API stops. The
//! token exists in plaintext only here and in the CLI's request (the host's
//! resolver keeps its SHA-256) — the same trust boundary as `secrets.toml`.
//!
//! [`spawn_detached`] is the CLI's auto-start: `<exe> serve --autostart …`
//! detached, stdin null, stdout/stderr appended to `<state-root>/serve.log`
//! — the per-OS spawn is the engine's ([`hekma_engine::spawn_detached_process`]).
//!
//! This file joins `signals.rs` on the OS-cfg allowlist: the owner-only file
//! mode is a per-OS std API (there is no data-driven form for a Unix mode).

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The discovery file's name, inside the state root.
pub const CONTROL_FILE: &str = "control.json";

/// The auto-started host's output log, inside the state root.
pub const SERVE_LOG_FILE: &str = "serve.log";

/// The discovery file's schema version.
pub const DISCOVERY_SCHEMA_VERSION: u32 = 1;

/// The `control.json` document: where the state root's Service Host serves
/// its local endpoint, and the operator token it accepts there.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Discovery {
    /// [`DISCOVERY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The host's process id.
    pub pid: u32,
    /// The loopback address of the local endpoint.
    pub addr: SocketAddr,
    /// The local operator token (64 hex digits; 256 bits from the OS CSPRNG).
    pub token: String,
    /// The build that wrote the file (the CLI notes a different one).
    pub hekma_version: String,
    /// Whether the host was auto-started by a CLI command.
    pub autostarted: bool,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a credential: never printed.
        f.debug_struct("Discovery")
            .field("schema_version", &self.schema_version)
            .field("pid", &self.pid)
            .field("addr", &self.addr)
            .field("token", &"<redacted>")
            .field("hekma_version", &self.hekma_version)
            .field("autostarted", &self.autostarted)
            .finish()
    }
}

/// The discovery file's path under a state root.
pub fn control_file(state_base: &Path) -> PathBuf {
    state_base.join(CONTROL_FILE)
}

/// The auto-started host's log path under a state root.
pub fn serve_log_file(state_base: &Path) -> PathBuf {
    state_base.join(SERVE_LOG_FILE)
}

/// Mint a 256-bit operator token from the OS CSPRNG, as 64 hex digits.
pub(crate) fn mint_token() -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the OS random source failed; no local token was minted".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Write the discovery file owner-only: a temp file in the same directory,
/// then a rename over `control.json` (a reader never sees a torn file).
pub fn write_discovery(state_base: &Path, discovery: &Discovery) -> std::io::Result<()> {
    let target = control_file(state_base);
    let temp = state_base.join(format!(".{CONTROL_FILE}.{}.tmp", discovery.pid));
    let text = serde_json::to_string_pretty(discovery).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut file = create_owner_only(&temp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        rename_with_retry(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// How many times [`write_discovery`] tries the rename into place.
const RENAME_ATTEMPTS: u32 = 20;

/// The pause between rename attempts.
const RENAME_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(25);

/// Rename `temp` over `target`, retrying a failure for up to ~0.5 s. On
/// Windows a rename over a file another process holds open fails with a
/// sharing violation — and a CLI command waiting for an auto-started host
/// reads `control.json` every 50 ms, so the host's first write can collide
/// with that read. Elsewhere the first attempt succeeds.
fn rename_with_retry(temp: &Path, target: &Path) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match std::fs::rename(temp, target) {
            Ok(()) => return Ok(()),
            Err(err) if attempt >= RENAME_ATTEMPTS => return Err(err),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(RENAME_RETRY_PAUSE);
            }
        }
    }
}

/// Read the discovery file. `Err` when it is absent, unreadable or not a
/// discovery document this build understands.
pub fn read_discovery(state_base: &Path) -> std::io::Result<Discovery> {
    let text = std::fs::read_to_string(control_file(state_base))?;
    let discovery: Discovery = serde_json::from_str(&text)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if discovery.schema_version != DISCOVERY_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "control.json schema v{} (this build reads v{DISCOVERY_SCHEMA_VERSION})",
                discovery.schema_version
            ),
        ));
    }
    Ok(discovery)
}

/// Remove the discovery file IF it is still this process's (a successor host
/// may already have replaced it). Best-effort.
pub(crate) fn remove_discovery(state_base: &Path, pid: u32) {
    if let Ok(current) = read_discovery(state_base) {
        if current.pid == pid {
            let _ = std::fs::remove_file(control_file(state_base));
        }
    }
}

#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Fresh and owner-only: a leftover file (or a planted symlink) is
    // removed first and the open is `create_new`, so the token never lands
    // in a file whose permissions this process did not set.
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    // Windows: the file inherits the state directory's ACL (the same trust
    // boundary as `secrets.toml`).
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Spawn `exe args…` DETACHED from the calling terminal, stdin null,
/// stdout and stderr appended to `log`; returns the child's pid. The engine
/// owns the per-OS spawn ([`hekma_engine::spawn_detached_process`]: its own
/// process group on Unix; a detached new process group without a console on
/// Windows, with the caller's own stdio kept out of the child's handle
/// inheritance so a script capturing `hekma` never waits on the host).
pub fn spawn_detached(exe: &Path, args: &[std::ffi::OsString], log: &Path) -> std::io::Result<u32> {
    hekma_engine::spawn_detached_process(exe, args, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32) -> Discovery {
        Discovery {
            schema_version: DISCOVERY_SCHEMA_VERSION,
            pid,
            addr: "127.0.0.1:4242".parse().unwrap(),
            token: "ab".repeat(32),
            hekma_version: "0.0.0-test".into(),
            autostarted: true,
        }
    }

    #[test]
    fn the_discovery_file_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let written = sample(7);
        write_discovery(dir.path(), &written).unwrap();
        assert_eq!(read_discovery(dir.path()).unwrap(), written);
        // No temp file is left behind.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![CONTROL_FILE.to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(control_file(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "control.json is owner-only");
        }
    }

    #[test]
    fn a_rename_that_keeps_failing_gives_up_with_the_error() {
        let dir = tempfile::tempdir().unwrap();
        // A missing source fails every attempt: the last error comes back.
        let err =
            rename_with_retry(&dir.path().join("absent"), &dir.path().join("target")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn removal_only_takes_this_process_file() {
        let dir = tempfile::tempdir().unwrap();
        write_discovery(dir.path(), &sample(7)).unwrap();
        remove_discovery(dir.path(), 8);
        assert!(
            control_file(dir.path()).exists(),
            "a successor's file stays"
        );
        remove_discovery(dir.path(), 7);
        assert!(!control_file(dir.path()).exists());
        // Removing an absent file is a no-op.
        remove_discovery(dir.path(), 7);
    }

    #[test]
    fn a_foreign_or_broken_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_discovery(dir.path()).is_err(), "absent");
        std::fs::write(control_file(dir.path()), "{not json").unwrap();
        assert!(read_discovery(dir.path()).is_err(), "broken");
        let mut future = serde_json::to_value(sample(1)).unwrap();
        future["schema_version"] = serde_json::json!(2);
        std::fs::write(control_file(dir.path()), future.to_string()).unwrap();
        let err = read_discovery(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema v2"), "{err}");
    }

    #[test]
    fn the_token_is_256_bits_of_hex_and_the_debug_form_hides_it() {
        let one = mint_token().unwrap();
        let two = mint_token().unwrap();
        assert_eq!(one.len(), 64);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(one, two);
        let shown = format!("{:?}", sample(1));
        assert!(!shown.contains(&"ab".repeat(32)), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn a_detached_spawn_runs_and_appends_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = serve_log_file(dir.path());
        std::fs::write(&log, "earlier\n").unwrap();
        // The test harness itself, listing its tests, is a portable child.
        let exe = std::env::current_exe().unwrap();
        let pid = spawn_detached(&exe, &["--list".into()], &log).unwrap();
        assert!(pid > 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let text = std::fs::read_to_string(&log).unwrap();
            if text.starts_with("earlier\n") && text.contains(": test") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never wrote to the log: {text}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}
