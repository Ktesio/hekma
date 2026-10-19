//! The single-writer host lock (story 17-2): one `hekma serve` process per
//! state directory, enforced by an ADVISORY OS file lock on
//! `<state-root>/host.lock` (`hekma_engine::paths::HOST_LOCK_FILE` — the
//! engine's path authority names it; this module only ever joins it onto the
//! engine-resolved state root).
//!
//! Why advisory-`File::try_lock` and nothing fancier: the lock must be held
//! BEFORE any engine open (a second host must be refused without touching
//! the state at all), it must survive with no engine running (the host may
//! be down while the DB exists), and it must self-heal across a crashed
//! host (the OS releases the lock when the holder dies; the residue file
//! content is stale but the LOCK is gone — re-acquisition succeeds). A DB
//! lock-table row could not satisfy the first two (no engine open yet / no
//! engine at all); a lockfile with a pid-only sanity check could not
//! satisfy self-healing robustly (pid reuse). `File::try_lock` (stable
//! 1.89; MSRV 1.96.1 — no new dependency) gives all three.
//!
//! Holder record: the acquirer writes `pid=<n>` into the file so a refused
//! second host can NAME the live holder. The write is best-effort sugar —
//! the LOCK is the safety property; a failed record write leaves the lock
//! held with an unreadable pid (the refusal then says so honestly rather
//! than fabricating a pid).

use std::io::Write;
use std::path::{Path, PathBuf};

/// An acquired host lock. Holding it IS the exclusive right to serve the
/// state directory; dropping it releases the lock (the OS releases on file
/// close — including across a crashed host). The residue `pid=<n>` content
/// is intentionally left behind: a stale file is re-acquired, never a
/// refusal (the LOCK decides, never the content).
#[derive(Debug)]
pub struct HostLock {
    /// The locked file handle. Kept open for the host's whole life; the
    /// advisory lock dies with this handle (drop = release).
    _file: std::fs::File,
    /// The lock file's path (carried for diagnostics).
    #[allow(dead_code)]
    path: PathBuf,
}

/// Why a lock could not be acquired.
#[derive(Debug)]
pub enum LockRefusal {
    /// The lock is HELD by another live process (`holder`: its PID when the
    /// `pid=<n>` record was readable, `None` when not — the lock is real
    /// either way; `WouldBlock` proved it).
    Held {
        /// The holder's PID, when readable from the lock file's record.
        holder: Option<u32>,
    },
    /// The lock file could not be opened/locked (an OS I/O error — state dir
    /// permissions, disk failure). Distinct from `Held`: nothing holds the
    /// directory; the host simply could not reach the lock.
    Io(std::io::Error),
}

impl HostLock {
    /// Acquire the host lock on `path` (creating the file if absent — the
    /// caller has already created the state directory). On success the lock
    /// is held until the returned [`HostLock`] drops; on
    /// [`LockRefusal::Held`] nothing was modified except (possibly) the
    /// file's creation.
    pub fn acquire(path: &Path) -> Result<HostLock, LockRefusal> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockRefusal::Io)?;
        match file.try_lock() {
            Ok(()) => {
                // Best-effort holder record (see module docs): the lock is
                // the property; the pid line is diagnosability sugar. A
                // failed write keeps the lock held — a refuser reads
                // `holder: None` and says so, never a fabricated pid.
                let mut guard = &file;
                let _ = guard.set_len(0);
                let _ = writeln!(guard, "pid={}", std::process::id());
                let _ = guard.flush();
                Ok(HostLock {
                    _file: file,
                    path: path.to_path_buf(),
                })
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(LockRefusal::Held {
                holder: read_holder_pid(path),
            }),
            Err(std::fs::TryLockError::Error(err)) => Err(LockRefusal::Io(err)),
        }
    }
}

/// Read the `pid=<n>` holder record from a lock file — `Some(pid)` when the
/// file exists and its (whitespace-trimmed) content parses; `None` for an
/// absent file, an unreadable file, or ANY other content (a refuser must
/// never guess a pid from garbage). Public for the refusal tests.
pub fn read_holder_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim();
    let pid = line.strip_prefix("pid=")?;
    pid.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh per-test state directory + the lock path inside it.
    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("host.lock")
    }

    #[test]
    fn a_second_owner_is_refused_naming_the_holder_pid() {
        // The AC: with one host holding the lock, a second acquire is
        // `Held` naming the LIVE holder's pid (this test process's).
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);
        let first = HostLock::acquire(&path).expect("first host acquires");
        let second = HostLock::acquire(&path).expect_err("second host is refused");
        match second {
            LockRefusal::Held { holder } => {
                assert_eq!(
                    holder,
                    Some(std::process::id()),
                    "the refusal must name the live holder's pid"
                );
            }
            LockRefusal::Io(err) => panic!("a held lock must not surface as I/O: {err}"),
        }
        // The first host's lock is UNAFFECTED by the refused attempt.
        drop(first);
    }

    #[test]
    fn the_lock_releases_on_a_clean_drop_and_reacquires() {
        // Release-on-clean-exit: dropping the holder releases the advisory
        // lock; the next acquire succeeds (and rewrites the pid record).
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);
        let first = HostLock::acquire(&path).expect("first acquire");
        let pid_text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            pid_text.trim(),
            format!("pid={}", std::process::id()),
            "the holder record names the acquirer"
        );
        drop(first);
        let second = HostLock::acquire(&path).expect("re-acquire after release");
        drop(second);
    }

    #[test]
    fn a_stale_or_garbage_lock_file_does_not_refuse() {
        // Stale-free across killed hosts: residue content — a dead pid, an
        // empty file, or garbage — is NEVER a refusal; the LOCK decides.
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);
        for residue in ["pid=99999999\n", "", "not-a-pid-record\n", "pid=\n"] {
            std::fs::write(&path, residue).unwrap();
            let lock = HostLock::acquire(&path)
                .unwrap_or_else(|err| panic!("residue {residue:?} must re-acquire: {err:?}"));
            drop(lock);
        }
    }

    #[test]
    fn an_unreadable_holder_record_refuses_with_no_pid_not_a_guess() {
        // AI-18 honesty: when the record is unreadable the refusal carries
        // `None` (the wording then says "its pid record could not be read"),
        // never a fabricated or zero pid.
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);
        std::fs::write(&path, "garbage\n").unwrap();
        assert_eq!(read_holder_pid(&path), None, "garbage parses to no pid");
        assert_eq!(read_holder_pid(&dir.path().join("absent.lock")), None);
    }
}
