//! The Policy re-resolution hook (story 21-9, spine AD-25).
//!
//! "A label change is an event and re-resolves the Run's Policies" (AD-25):
//! every committed bound-label change — the label verb, a configuration
//! write, a relaunch or an adoption re-sync — calls [`labels_changed`] inside
//! the same writer job, after the label write and before the snapshot entry
//! is built, so a Policy re-resolution commits and publishes together with the
//! label change that caused it. Policies do not exist yet (story 21.3), so the
//! hook changes nothing today; 21.3 fills it in.

use super::ops::RunChanges;
use crate::domain::usage::RunId;
use crate::ports::{RunRecords, StoreError};

/// Re-resolve `run`'s Policies after its bound labels changed. Returns the
/// further Run changes the re-resolution made (none until Policies exist;
/// under test, the call itself is noted on the returned changes so tests can
/// pin that every bound-label path calls the hook inside its writer job).
pub(crate) fn labels_changed<S: RunRecords + ?Sized>(
    _store: &S,
    run: &RunId,
    _at: &str,
) -> Result<RunChanges, StoreError> {
    #[cfg(test)]
    let changes = RunChanges {
        policy_resolved: vec![run.clone()],
        ..RunChanges::default()
    };
    #[cfg(not(test))]
    let changes = {
        let _ = run;
        RunChanges::default()
    };
    Ok(changes)
}
