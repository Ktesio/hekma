//! Per-OS process backends (spine AD-4) — THE cfg boundary.
//!
//! This module and its `unix/` / `windows/` children are the SOLE location
//! where OS-conditional compilation (`#[cfg(unix)]` / `#[cfg(windows)]` /
//! `target_os`) is allowed — the OS-cfg CI gate allowlists exactly
//! `^crates/ktesio-engine/src/backends/`. Everywhere else (the supervisor,
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
#[cfg(unix)]
pub use unix::check_secrets_file_permissions;
#[cfg(windows)]
pub use windows::check_secrets_file_permissions;

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
