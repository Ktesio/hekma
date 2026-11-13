//! Wire dispositions (story 23-4, FR-56, spine AD-25 / AD-26 / AD-27).
//!
//! An Operator or Host holds, releases, reroutes or rejects a Run's model
//! calls at the shared Metering Proxy. Each verb sets or clears exactly its
//! own reason in `run_dispositions`; the snapshot folds the reasons over a
//! Run's ancestor chain and the proxy acts on the strongest (reject >
//! hold > reroute > pass). Breach reasons stay latch-derived and clear only
//! through a re-arm.
//!
//! A verb needs a wire path: an `observed` Run fails fast naming its depth,
//! a supervised Run whose calls skip the proxy names its Metering Source,
//! and an ended Run is refused.

use super::ops::{self, RunChanges};
use super::{RunError, RunId, RunRecord, RunStatus};
use crate::ports::RunRecords as _;

use crate::store::SqliteStore;

/// One disposition verb.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispositionVerb {
    /// Park the Run's calls until released.
    Hold,
    /// Clear the hold.
    Release,
    /// Refuse the Run's calls (cleared only by the Run ending).
    Reject,
    /// Send the Run's calls to `binding/route`, or back (`None`).
    Reroute(Option<String>),
}

impl DispositionVerb {
    /// The verb's name (`hold`, `release`, `reject`, `reroute`).
    pub fn as_str(&self) -> &'static str {
        match self {
            DispositionVerb::Hold => "hold",
            DispositionVerb::Release => "release",
            DispositionVerb::Reject => "reject",
            DispositionVerb::Reroute(_) => "reroute",
        }
    }
}

/// What a disposition verb did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispositionOutcome {
    /// The Run.
    pub run_id: RunId,
    /// Whether anything changed (setting a set reason, or clearing an
    /// absent one, changes nothing).
    pub changed: bool,
    /// The verb reasons standing afterwards (`hold`, `reject`, `reroute`).
    pub reasons: Vec<String>,
}

/// The verb × depth gate: a Run the proxy can act on.
fn gate(record: &RunRecord, verb: &DispositionVerb) -> Result<(), RunError> {
    use super::ControlDepth;
    if record.is_ended() {
        return Err(RunError::RunEnded {
            run_id: record.run_id.as_str().to_string(),
        });
    }
    let reason = if record.control_depth == ControlDepth::Observed {
        "an observed Run is known only from its telemetry; its calls do not pass through \
         the metering proxy, so they cannot be held, rerouted or rejected"
    } else if !record.wire_path {
        "this Run's model calls skip the metering proxy (its adapter reports usage itself), \
         so they cannot be held, rerouted or rejected"
    } else {
        return Ok(());
    };
    Err(RunError::VerbUnavailable {
        run_id: record.run_id.as_str().to_string(),
        verb: verb.as_str().to_string(),
        depth: record.control_depth.display_with_wire(record.wire_path),
        reason: reason.to_string(),
    })
}

/// The reroute key rule (T3.10, R2.6): a route with its own custodied key
/// uses it; otherwise a Run under custody may be rerouted only to the
/// origin its key is pinned to — a custodied key never leaves its origin.
fn check_route_key(
    store: &SqliteStore,
    record: &RunRecord,
    target: &super::snapshot::RerouteTarget,
) -> Result<(), RunError> {
    if target.custodied_key.is_some() {
        return Ok(());
    }
    let mut custody = None;
    for run in store.run_chain(&record.run_id)? {
        if let Some(row) = store.run_custody(&run.run_id)?.filter(|row| row.state == "on") {
            custody = row.origin;
            break;
        }
    }
    let Some(origin) = custody else {
        return Ok(());
    };
    if crate::domain::custody::origins_equal(&target.upstream, &origin) {
        return Ok(());
    }
    Err(RunError::Invalid {
        field: "route".to_string(),
        reason: format!(
            "this Run's provider key is held for {origin}, and route `{}/{}` goes elsewhere; \
             give the route its own key (`hekma binding route add … --custodied-key \
             secret:NAME`) or pick a route on the same origin",
            target.binding, target.route
        ),
    })
}

/// The writer job for one verb on one Run (see the module docs). Holding
/// a Run that owns no process flips it to `held`; releasing it flips it
/// back to `active`.
pub(crate) fn apply(
    store: &SqliteStore,
    run_id: &RunId,
    verb: &DispositionVerb,
    set_by: &str,
    at: &str,
) -> Result<(DispositionOutcome, RunChanges), RunError> {
    let mut changes = RunChanges::default();
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    gate(&record, verb)?;
    let before = store.run_dispositions(run_id)?;
    let has = |reason: &str| before.iter().any(|row| row.reason == reason);
    let (changed, cause) = match verb {
        DispositionVerb::Hold => {
            store.put_disposition(run_id, "hold", None, set_by, at)?;
            (!has("hold"), "hold".to_string())
        }
        DispositionVerb::Release => (store.delete_disposition(run_id, "hold")?, "release".to_string()),
        DispositionVerb::Reject => {
            store.put_disposition(run_id, "reject", None, set_by, at)?;
            (!has("reject"), "reject".to_string())
        }
        DispositionVerb::Reroute(Some(route)) => {
            let target = store.reroute_target(route)?.ok_or_else(|| RunError::Invalid {
                field: "route".to_string(),
                reason: format!(
                    "no route `{route}`: name one as <binding>/<route> (`hekma binding route add`)"
                ),
            })?;
            check_route_key(store, &record, &target)?;
            let same = before
                .iter()
                .any(|row| row.reason == "reroute" && row.route.as_deref() == Some(route));
            store.put_disposition(run_id, "reroute", Some(route), set_by, at)?;
            (!same, format!("reroute {route}"))
        }
        DispositionVerb::Reroute(None) => (
            store.delete_disposition(run_id, "reroute")?,
            "reroute off".to_string(),
        ),
    };
    if changed {
        let mut current = store.run_record(run_id)?.expect("read above");
        // A Run that owns no process takes `held` from the hold reason; a
        // process-owning Run's status is its lifecycle's.
        let next = match verb {
            DispositionVerb::Hold if !current.process_owner => match current.status {
                RunStatus::Active | RunStatus::Silent => Some(RunStatus::Held),
                _ => None,
            },
            DispositionVerb::Release if !current.process_owner => {
                (current.status == RunStatus::Held).then_some(RunStatus::Active)
            }
            _ => None,
        };
        if let Some(status) = next {
            let anomalies = current.anomalies.clone();
            if let Some(moved) = ops::move_to(
                store,
                &mut changes,
                current.clone(),
                status,
                anomalies,
                Some(format!("{cause} by {set_by}")),
                at,
            )? {
                current = moved;
            }
        }
        current.updated_at = at.to_string();
        store.save_run(&current)?;
        ops::note_disposition(store, &mut changes, &current, format!("{cause} by {set_by}"))?;
    }
    let reasons = store
        .run_dispositions(run_id)?
        .into_iter()
        .map(|row| match row.route {
            Some(route) => format!("{} {route}", row.reason),
            None => row.reason,
        })
        .collect();
    Ok((
        DispositionOutcome {
            run_id: run_id.clone(),
            changed,
            reasons,
        },
        changes,
    ))
}

#[cfg(test)]
#[path = "dispositions_tests.rs"]
mod tests;
