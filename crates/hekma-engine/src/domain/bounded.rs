//! AD-18 Step 2: the engine's named lock scopes.
//!
//! `engine.rs` takes its two big locks only through these:
//!
//! - [`RegistryCell::critical`]: the registry, the outer lock.
//! - [`SupervisorCell::after`]: the supervisor, reachable only by showing a
//!   held registry guard. The lock order (registry, then supervisor) is
//!   therefore a compile-time fact, not a convention.
//! - There is no other way to the supervisor. The lock-debt ratchet
//!   (`scripts/check_ad17_debt.py`, `.ad17-debt.txt`) fails CI on any bare
//!   `.lock()` that comes back in `engine.rs`.
//! - [`cell_scope`]: a small engine cell (the proxy slot, the recovery
//!   record), locked for one closure; its guard never escapes.
//!
//! All of them keep `#[track_caller]`, so `TimedMutex`'s per-site hold
//! stats still name the engine.rs line that took the lock.

use std::sync::Mutex;

use super::timed_mutex::{TimedGuard, TimedMutex};
use super::{Registry, Supervisor};

/// A held registry guard.
pub(crate) type RegistryGuard<'a> = TimedGuard<'a, Registry>;

/// The registry's lock: the outer one.
pub(crate) struct RegistryCell(TimedMutex<Registry>);

impl RegistryCell {
    pub(crate) fn new(mutex: TimedMutex<Registry>) -> Self {
        RegistryCell(mutex)
    }

    /// Take the registry for a critical section. Panics on a poisoned
    /// lock, as every engine site always has.
    #[track_caller]
    pub(crate) fn critical(&self) -> RegistryGuard<'_> {
        self.0.lock().expect("registry mutex poisoned")
    }

    /// Take the registry even if a panic poisoned it — only for best-effort
    /// teardown (a panic in `Drop` would abort the process).
    #[track_caller]
    pub(crate) fn critical_recovering(&self) -> RegistryGuard<'_> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The supervisor's lock: the inner one.
pub(crate) struct SupervisorCell(TimedMutex<Supervisor>);

impl SupervisorCell {
    pub(crate) fn new(mutex: TimedMutex<Supervisor>) -> Self {
        SupervisorCell(mutex)
    }

    /// Take the supervisor while `registry` is held: the only ordered way
    /// in.
    #[track_caller]
    pub(crate) fn after<'a>(&'a self, registry: &RegistryGuard<'_>) -> TimedGuard<'a, Supervisor> {
        let _ = registry;
        self.0.lock().expect("supervisor mutex poisoned")
    }

    /// [`SupervisorCell::after`], recovering a poisoned lock — only for
    /// best-effort teardown.
    #[track_caller]
    pub(crate) fn after_recovering<'a>(
        &'a self,
        registry: &RegistryGuard<'_>,
    ) -> TimedGuard<'a, Supervisor> {
        let _ = registry;
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Lock a small engine cell for one closure. Panics on a poisoned lock,
/// naming `what`.
pub(crate) fn cell_scope<T, R>(cell: &Mutex<T>, what: &str, f: impl FnOnce(&mut T) -> R) -> R {
    let mut guard = cell
        .lock()
        .unwrap_or_else(|_| panic!("{what} mutex poisoned"));
    f(&mut guard)
}

/// [`cell_scope`] for a cell whose readers tolerate a poisoned lock (the
/// credential reader: a read-only connection a panic cannot corrupt).
pub(crate) fn cell_scope_recovering<T, R>(cell: &Mutex<T>, f: impl FnOnce(&mut T) -> R) -> R {
    let mut guard = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_scope_runs_its_closure_under_the_lock_and_returns_its_value() {
        let cell = Mutex::new(1);
        assert_eq!(
            cell_scope(&cell, "test", |v| {
                *v += 1;
                *v * 10
            }),
            20
        );
        assert_eq!(*cell.lock().unwrap(), 2);
    }

    #[test]
    #[should_panic(expected = "probe mutex poisoned")]
    fn a_poisoned_cell_names_itself() {
        let cell = std::sync::Arc::new(Mutex::new(0));
        let poisoner = std::sync::Arc::clone(&cell);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        cell_scope(&cell, "probe", |_| ());
    }
}
