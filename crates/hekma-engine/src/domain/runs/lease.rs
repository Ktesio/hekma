//! Leases and the sweeper (story 23-2, FR-52, AD-25).
//!
//! An announced Run holds a **Lease**, renewed by a heartbeat or by any
//! call the proxy attributes to it. A wire Run (a binding's Run, a split, a
//! hint child, the unattributed Run) holds none: it goes `silent` after an
//! inactivity window. The sweeper marks expired or idle Runs `silent` and
//! ends a Run that stayed `silent` for the silent window (cause
//! `silent-window`; its tokens revoke through the run-ended path).
//!
//! `silent` is a status, never an action: nothing is stopped, paused,
//! blocked or fenced by silence (NFR-9), and a renewed Run returns to
//! `active`. A process-owning Run is never swept — its process is its
//! heartbeat.
//!
//! Every time column here is second-precision RFC 3339 (the `updated_at`
//! format), so string order is time order.

use super::ops::{move_to, RunChanges};
use super::{RunId, RunStatus};
use crate::ports::{RunRecords, StoreError};

/// The Lease and window knobs (`EngineOptions::leases`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseOptions {
    /// How long a heartbeat or an identified call extends a Lease.
    pub lease_secs: u64,
    /// How long a wire Run (no Lease) may go without traffic before it is
    /// marked `silent`.
    pub wire_inactivity_secs: u64,
    /// How long a Run may stay `silent` before it ends (`silent-window`).
    pub silent_window_secs: u64,
    /// How often the sweeper runs.
    pub sweep_interval_secs: u64,
}

/// The default Lease, in seconds.
pub const LEASE_SECS: u64 = 60;
/// The default inactivity window for a wire Run, in seconds.
pub const WIRE_INACTIVITY_SECS: u64 = 900;
/// The default silent window, in seconds.
pub const SILENT_WINDOW_SECS: u64 = 3_600;
/// The default sweep interval, in seconds.
pub const SWEEP_INTERVAL_SECS: u64 = 5;

/// The most Runs one sweep pass changes (the rest wait for the next tick).
pub(crate) const SWEEP_BATCH: usize = 500;

impl Default for LeaseOptions {
    fn default() -> Self {
        LeaseOptions {
            lease_secs: LEASE_SECS,
            wire_inactivity_secs: WIRE_INACTIVITY_SECS,
            silent_window_secs: SILENT_WINDOW_SECS,
            sweep_interval_secs: SWEEP_INTERVAL_SECS,
        }
    }
}

/// `at` plus `secs`, in the stored format.
pub(crate) fn plus(at_unix: u64, secs: u64) -> String {
    crate::time::format_rfc3339(at_unix.saturating_add(secs))
}

/// `at` minus `secs`, in the stored format.
fn minus(at_unix: u64, secs: u64) -> String {
    crate::time::format_rfc3339(at_unix.saturating_sub(secs))
}

/// Activity on a Run (story 23-2): an identified proxied call or a
/// heartbeat. Records `last_activity_at`, extends the Lease when the Run
/// holds one (or when `grant` asks for one — a heartbeat, an announce), and
/// returns a `silent` Run to `active` (one `StatusChanged`). An ended Run
/// is left alone.
pub(crate) fn note_activity<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    options: &LeaseOptions,
    now_unix: u64,
    grant: bool,
) -> Result<(), StoreError> {
    let Some(record) = store.run_record(run_id)? else {
        return Ok(());
    };
    if record.is_ended() {
        return Ok(());
    }
    let at = crate::time::format_rfc3339(now_unix);
    let (lease, _) = store.run_lease(run_id)?;
    if record.process_owner {
        // Supervision owns a process-owning Run's aliveness: activity only.
        return store.touch_run_activity(run_id, &at);
    }
    if lease.is_some() || grant {
        store.set_run_lease(run_id, Some(&plus(now_unix, options.lease_secs)), Some(&at))?;
    } else {
        store.touch_run_activity(run_id, &at)?;
    }
    if record.status == RunStatus::Silent {
        let anomalies = record.anomalies.clone();
        move_to(
            store,
            changes,
            record,
            RunStatus::Active,
            anomalies,
            Some("renewed".to_string()),
            &at,
        )?;
    }
    Ok(())
}

/// A proxied call on a Run that announced without traffic (story 23-2):
/// its first identified call gives it a wire path, so it moves `observed →
/// announced` (one `depth-changed`, id kept). Only a Run that holds a Lease
/// at `observed` — an endpoint announce — moves; a process-owning Run's
/// depth is supervision's.
pub(crate) fn note_wire_call<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    now_unix: u64,
) -> Result<(), StoreError> {
    let Some(record) = store.run_record(run_id)? else {
        return Ok(());
    };
    if record.is_ended()
        || record.process_owner
        || record.control_depth != super::ControlDepth::Observed
        || store.run_lease(run_id)?.0.is_none()
    {
        return Ok(());
    }
    let at = crate::time::format_rfc3339(now_unix);
    changes.extend(super::ops::sync_depth(
        store,
        record,
        super::ControlDepth::Announced,
        true,
        &at,
    )?);
    Ok(())
}

/// One sweep (story 23-2): Runs whose Lease expired, or that hold none and
/// were idle past the inactivity window, go `silent`; Runs `silent` past
/// the silent window end (`silent-window`). Process-owning Runs are never
/// read (the predicates exclude them).
pub(crate) fn sweep<S: RunRecords + ?Sized>(
    store: &S,
    options: &LeaseOptions,
    now_unix: u64,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let at = crate::time::format_rfc3339(now_unix);
    let silent_before = minus(now_unix, options.silent_window_secs);
    for run_id in store.silent_runs_past(&silent_before, SWEEP_BATCH)? {
        let Some(record) = store.run_record(&run_id)? else {
            continue;
        };
        if record.process_owner || record.status != RunStatus::Silent {
            continue;
        }
        let anomalies = record.anomalies.clone();
        move_to(
            store,
            &mut changes,
            record,
            RunStatus::Ended,
            anomalies,
            Some("silent-window".to_string()),
            &at,
        )?;
        store.set_run_lease(&run_id, None, None)?;
    }
    let inactive_before = minus(now_unix, options.wire_inactivity_secs);
    for run_id in store.runs_going_silent(&at, &inactive_before, SWEEP_BATCH)? {
        let Some(record) = store.run_record(&run_id)? else {
            continue;
        };
        if record.process_owner || record.status != RunStatus::Active {
            continue;
        }
        let anomalies = record.anomalies.clone();
        move_to(
            store,
            &mut changes,
            record,
            RunStatus::Silent,
            anomalies,
            Some("lease-expired".to_string()),
            &at,
        )?;
    }
    Ok(changes)
}
