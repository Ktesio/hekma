//! Per-OS process backends (spine AD-4) — THE cfg boundary.
//!
//! This module and its `unix/` / `windows/` children are the SOLE location
//! where OS-conditional compilation (`#[cfg(unix)]` / `#[cfg(windows)]` /
//! `target_os`) is allowed — the OS-cfg CI gate allowlists exactly
//! `^crates/hekma-engine/src/backends/`. Everywhere else (the supervisor,
//! ports, domain, `kt`) names the [`ProcessBackend`](crate::ports::ProcessBackend)
//! trait and the cfg-selected [`Backend`] / [`Handle`] aliases below — never a
//! concrete backend, never an OS type, never a `#[cfg]`.
//!
//! ## The selection
//!
//! [`Backend`] resolves to the Unix backend on any Unix target and the Windows
//! backend on Windows. [`current`] constructs the one for the running target.
//! The supervisor stores running processes as `HashMap<_, `[`Handle`]`>` and
//! calls the port methods; the concrete OS resources (process groups, Job
//! Objects) are hidden inside the selected backend.

use crate::ports::ProcessBackend;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

// ---- The cfg-selected concrete backend + its handle (named via aliases) ----

/// The concrete [`ProcessBackend`] for the current target (Unix flavor).
#[cfg(unix)]
pub type Backend = unix::UnixBackend;

/// The concrete [`ProcessBackend`] for the current target (Windows flavor).
#[cfg(windows)]
pub type Backend = windows::WindowsBackend;

/// The running-process handle type for the current target's [`Backend`].
///
/// The supervisor stores these without ever naming an OS type — this alias IS
/// the seam.
pub type Handle = <Backend as ProcessBackend>::Handle;

/// Construct the process backend for the running operating system (AD-4).
///
/// The only per-OS selection point. Callers get a value typed as [`Backend`]
/// and drive it through the [`ProcessBackend`] trait.
pub fn current() -> Backend {
    Backend::new()
}

/// Check the engine secrets file's permissions for the running OS (story 2-4 AC6,
/// spine AD-10). The OS-specific permission INSPECTION — Unix refuses a
/// group/other-accessible (non-`0600`) file with a `chmod 600` remediation; Windows
/// is a documented portable skip relying on default per-user profile ACLs (see the
/// per-backend docs). This is the ONLY seam that reaches the per-OS check; the
/// OS-agnostic file resolver ([`crate::ports::FileSecretResolver`]) calls THIS, so
/// no `#[cfg]` leaks out of `backends/`. Re-exported cfg-selected exactly like
/// [`Backend`].
/// Story 24-1: listen for an engine on the Shim socket at `path` (its
/// directory is created owner-only). Unix only for now; the Windows named
/// pipe arrives with the Shim's wiring.
pub fn shim_listen(
    path: &std::path::Path,
) -> std::io::Result<Box<dyn crate::shim::server::ShimListener>> {
    #[cfg(unix)]
    {
        Ok(Box::new(unix::shim::listen(path)?))
    }
    #[cfg(windows)]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the Shim's Windows transport is not built yet",
        ))
    }
}

/// Story 24-1: connect to the Shim socket at `path`.
pub fn shim_connect(
    path: &std::path::Path,
) -> std::io::Result<Box<dyn crate::shim::server::ShimStream>> {
    #[cfg(unix)]
    {
        unix::shim::connect(path)
    }
    #[cfg(windows)]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the Shim's Windows transport is not built yet",
        ))
    }
}

/// Story 22-5: how many OTHER accounts could log in on this machine, read
/// from the OS's account list; `None` when this cannot be told (Windows,
/// or an account list that does not name the current user, as on macOS or
/// a directory-service host). Drives the metering proxy's port-squatting
/// warning.
pub fn interactive_users_hint() -> Option<usize> {
    #[cfg(unix)]
    {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        let me = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()?;
        other_interactive_users(&passwd, &me)
    }
    #[cfg(windows)]
    {
        None
    }
}

/// The OS-agnostic rule over a `passwd`-format account list: an account
/// can log in when its uid is at least 1000 (not `nobody`, 65534) and its
/// shell is not a no-login stub. `None` when `me` is not among them (the
/// list cannot speak for this machine).
pub(crate) fn other_interactive_users(passwd: &str, me: &str) -> Option<usize> {
    let humans: Vec<&str> = passwd
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let (name, uid, shell) = (fields.first()?, fields.get(2)?, fields.get(6)?);
            let uid: u32 = uid.parse().ok()?;
            let shell = shell.trim();
            let no_login = shell.is_empty()
                || ["nologin", "false", "sync", "shutdown", "halt"]
                    .iter()
                    .any(|stub| shell.ends_with(stub));
            (uid >= 1000 && uid != 65534 && !no_login).then_some(*name)
        })
        .collect();
    humans
        .contains(&me)
        .then(|| humans.iter().filter(|name| **name != me).count())
}

#[cfg(test)]
mod interactive_users_tests {
    use super::other_interactive_users;

    #[test]
    fn other_login_capable_accounts_are_counted_and_an_unlisted_self_cannot_tell() {
        let passwd = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1::/usr/sbin:/usr/sbin/nologin
nobody:x:65534:65534::/nonexistent:/bin/sh
ana:x:1000:1000::/home/ana:/bin/bash
bo:x:1001:1001::/home/bo:/bin/zsh
svc:x:1002:1002::/srv:/usr/sbin/nologin
# a comment
";
        assert_eq!(other_interactive_users(passwd, "ana"), Some(1));
        assert_eq!(
            other_interactive_users("ana:x:1000:1000::/h:/bin/sh", "ana"),
            Some(0)
        );
        assert_eq!(other_interactive_users(passwd, "svc"), None);
        assert_eq!(other_interactive_users(passwd, "root"), None);
        assert_eq!(other_interactive_users("broken line", "ana"), None);
    }
}

#[cfg(unix)]
pub use unix::check_secrets_file_permissions;
#[cfg(windows)]
pub use windows::check_secrets_file_permissions;

// ---- Story 22-2: home isolation's per-OS variable list ----

/// The home-pointing environment variables an isolated Agent Home rewrites
/// (story 22-2, `provider.isolate_home = "on"`): Unix keeps `HOME` plus the
/// XDG base directories; Windows rewrites the profile variables (tools also
/// reach the known-folder APIs, which the docs name as the residual limit).
/// Overriding any of these in the spawn environment replaces the inherited
/// value — the OS floor re-applies `HOME`/`USERPROFILE` first, then the
/// mapped environment wins.
#[cfg(unix)]
pub use unix::isolated_home_env_vars;
#[cfg(windows)]
pub use windows::isolated_home_env_vars;

/// Story 22-2 (P10): the Service Host's memory hardening — non-dumpable
/// and `RLIMIT_CORE` 0 on Linux, nothing elsewhere.
#[cfg(unix)]
pub use unix::harden_host_memory;
#[cfg(windows)]
pub use windows::harden_host_memory;

/// Owner-only file permissions (`0600` on Unix; a fresh profile file
/// already is on Windows) — the secrets-file posture.
#[cfg(unix)]
pub use unix::restrict_to_owner;
#[cfg(windows)]
pub use windows::restrict_to_owner;

// ---- Story 11-2 review-1: atomic-write filesystem glue (the cfg home) ----
// The per-OS bits of `paths::write_atomically` (permission preservation and
// the rename with the Windows sharing-retry), cfg-selected exactly like the
// secrets permission check so `paths.rs` stays cfg-free.

/// Preserve an existing target's permissions onto the atomic-write temp before
/// the rename (Unix mode-bit copy; a portable no-op on Windows — see the
/// per-backend docs).
#[cfg(unix)]
pub use unix::preserve_target_mode;
#[cfg(windows)]
pub use windows::preserve_target_mode;

/// Rename the temp over the atomic-write target (immediate on Unix; one
/// backoff-and-retry for a transient Windows sharing violation).
#[cfg(unix)]
pub use unix::rename_over_target;
#[cfg(windows)]
pub use windows::rename_over_target;

// ---- Story 21-4: the detached service spawn (the CLI's host auto-start) ----

// The per-OS detached spawn of a prepared command (Unix: its own process
// group; Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP |
// CREATE_NO_WINDOW`, with the spawner's stdio kept out of the inheritance).
#[cfg(unix)]
use unix::spawn_detached_command;
#[cfg(windows)]
use windows::spawn_detached_command;

/// Spawn `exe args…` DETACHED from the calling process and its terminal,
/// stdin null, stdout and stderr appended to `log` (created if absent);
/// returns the child's pid. The child is never waited on — it outlives the
/// caller by design (story 21-4: `hekma` auto-starting its Service Host).
///
/// On Windows the spawner's own stdin/stdout/stderr are kept out of the
/// child's handle inheritance (the story 15-4 guard): without it a script
/// capturing the `hekma` command's output would wait on the pipe until the
/// detached child exits.
pub fn spawn_detached_process(
    exe: &std::path::Path,
    args: &[std::ffi::OsString],
    log: &std::path::Path,
) -> std::io::Result<u32> {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    let child = spawn_detached_command(&mut command)?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_backend_constructs() {
        // On any host CI runs, `current()` must build the target's backend.
        // (Behavioral spawn/stop coverage lives in the supervisor + conformance
        // integration tests; this only proves the selection compiles + builds.)
        let _backend = current();
    }
}
