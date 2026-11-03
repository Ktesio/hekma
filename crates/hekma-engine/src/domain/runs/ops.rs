//! Run registry operations, run inside store-writer jobs (story 21-2).
//!
//! Each operation reads and writes through the writer's store inside the
//! job's SAVEPOINT and returns a [`RunChanges`]: the [`RunEvent`]s it caused
//! and the snapshot entries it changed. [`commit`] appends those events to
//! the outbox in the same transaction and registers the snapshot publish as
//! a post-commit hook, so a Run change is durable, exportable and visible to
//! the data path together — or not at all.

use std::sync::Arc;

use super::snapshot::{RunEntry, SnapshotPublisher};
use super::{
    Anomaly, ChildRunRequest, ControlDepth, Label, LabelSource, RunError, RunEvent, RunEventKind,
    RunParent, RunRecord, RunStatus, RunUsage, RunView, HOST_IDENTITY_BINDING, MAX_RUN_NESTING,
    RUN_EVENT_SCHEMA_VERSION,
};
use crate::domain::budget::BreachAction;
use crate::domain::bus::EngineEvent;
use crate::domain::cost::EstimateLabel;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::name::InstanceName;
use crate::domain::outbox::OUTBOX_MAX_BYTES;
use crate::domain::usage::RunId;
use crate::ports::{NewRun, OutboxAppend, RunRecords, RunStore, StoreError};
use crate::store::WriterCtx;

/// What one operation changed.
#[derive(Debug, Default)]
pub(crate) struct RunChanges {
    pub events: Vec<RunEvent>,
    pub entries: Vec<RunEntry>,
}

impl RunChanges {
    fn extend(&mut self, other: RunChanges) {
        self.events.extend(other.events);
        self.entries.extend(other.entries);
    }
}

/// How a `→ failed` transition leaves the process-owning Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunFate {
    /// The Restart Policy relaunches: the Run stays `active` with the
    /// `failed` Anomaly.
    Relaunching,
    /// No relaunch: the Run ends with the `failed` Anomaly.
    Terminal,
}

/// A Run event + its outbox position, as committed.
pub(crate) type CommittedRunEvent = (RunEvent, OutboxAppend);

/// Append `changes`' events to the outbox and register the snapshot
/// publish after commit. Returns the events in commit order for the caller
/// to publish on the bus after its own events.
pub(crate) fn commit(
    ctx: &mut WriterCtx<'_>,
    publisher: &Arc<SnapshotPublisher>,
    changes: RunChanges,
) -> Result<Vec<CommittedRunEvent>, StoreError> {
    let mut committed = Vec::with_capacity(changes.events.len());
    for event in changes.events {
        let append = ctx
            .store()
            .append_outbox(&EngineEvent::Run(event.clone()), OUTBOX_MAX_BYTES)?;
        committed.push((event, append));
    }
    if !changes.entries.is_empty() {
        let publisher = Arc::clone(publisher);
        let entries = changes.entries;
        ctx.after_commit(move || publisher.publish(entries));
    }
    Ok(committed)
}

/// Record one change of `record` (already saved) as an event plus its
/// snapshot entry.
fn note<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    kind: RunEventKind,
    prior: Prior,
    cause: Option<String>,
) -> Result<(), StoreError> {
    let labels = store.run_labels(&record.run_id)?;
    let limits = store.run_limits(&record.run_id)?;
    changes.events.push(RunEvent {
        schema_version: RUN_EVENT_SCHEMA_VERSION,
        change: kind,
        run_id: record.run_id.clone(),
        parent_run_id: record.parent_run_id.clone(),
        instance: record.instance.clone(),
        control_depth: record.control_depth,
        wire_path: record.wire_path,
        status: record.status,
        prior_status: prior.status,
        prior_control_depth: prior.depth,
        prior_wire_path: prior.wire_path,
        anomalies: record.anomalies.clone(),
        labels: labels
            .iter()
            .filter(|label| label.source == LabelSource::Bound)
            .cloned()
            .collect(),
        cause,
        at: record.updated_at.clone(),
    });
    changes.entries.push(RunEntry {
        record: record.clone(),
        labels,
        limits,
    });
    Ok(())
}

#[derive(Default)]
struct Prior {
    status: Option<RunStatus>,
    depth: Option<ControlDepth>,
    wire_path: Option<bool>,
}

/// Move `record` to `status` with `anomalies`, saving it and noting the
/// event (`ended`, `status-changed` or `anomalies-changed`). Nothing when
/// neither changed.
fn move_to<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    mut record: RunRecord,
    status: RunStatus,
    anomalies: Vec<Anomaly>,
    cause: Option<String>,
    at: &str,
) -> Result<Option<RunRecord>, StoreError> {
    let mut anomalies = anomalies;
    anomalies.sort();
    anomalies.dedup();
    if record.status == status && record.anomalies == anomalies {
        return Ok(None);
    }
    let prior = record.status;
    let kind = if status == RunStatus::Ended && prior != RunStatus::Ended {
        record.end_cause = cause.clone();
        record.ended_at = Some(at.to_string());
        RunEventKind::Ended
    } else if status != prior {
        RunEventKind::StatusChanged
    } else {
        RunEventKind::AnomaliesChanged
    };
    record.status = status;
    record.anomalies = anomalies;
    record.updated_at = at.to_string();
    store.save_run(&record)?;
    let prior = Prior {
        status: (kind != RunEventKind::AnomaliesChanged).then_some(prior),
        ..Prior::default()
    };
    note(store, changes, &record, kind, prior, cause)?;
    Ok(Some(record))
}

/// Cascade a process-owning Run's status change to its in-process children
/// (PRD FR-49): `held` → children held (`parent-paused`), back to `active` →
/// held children active (`parent-resumed`; a child never holds on its own
/// before wire hold exists), `ended` → children ended (`parent-stopped` on a
/// stop, `parent-ended` otherwise).
fn cascade<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    owner: &RunId,
    from: RunStatus,
    to: RunStatus,
    stopped: bool,
    at: &str,
) -> Result<(), StoreError> {
    if from == to {
        return Ok(());
    }
    for child in store.open_in_process_descendants(owner)? {
        let anomalies = child.anomalies.clone();
        match to {
            RunStatus::Held if child.status == RunStatus::Active => {
                move_to(
                    store,
                    changes,
                    child,
                    RunStatus::Held,
                    anomalies,
                    Some("parent-paused".to_string()),
                    at,
                )?;
            }
            RunStatus::Active if child.status == RunStatus::Held => {
                move_to(
                    store,
                    changes,
                    child,
                    RunStatus::Active,
                    anomalies,
                    Some("parent-resumed".to_string()),
                    at,
                )?;
            }
            RunStatus::Ended => {
                let cause = if stopped {
                    "parent-stopped"
                } else {
                    "parent-ended"
                };
                move_to(
                    store,
                    changes,
                    child,
                    RunStatus::Ended,
                    anomalies,
                    Some(cause.to_string()),
                    at,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Project a lifecycle transition onto the instance's open process-owning
/// Run (spine AD-15 amendment; the table in the story spec) and cascade to
/// its in-process children. An instance with no open Run (a launch that
/// failed before its spawn record) changes nothing.
pub(crate) fn project_transition<S: RunRecords + ?Sized>(
    store: &S,
    instance: &InstanceName,
    new_state: LifecycleState,
    fate: RunFate,
    cause: &str,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(run) = store.open_process_run(instance)? else {
        return Ok(changes);
    };
    let mut anomalies = run.anomalies.clone();
    let (status, end_cause) = match new_state {
        LifecycleState::Running => {
            anomalies.retain(|a| *a != Anomaly::Failed);
            (RunStatus::Active, None)
        }
        // A crash the Restart Policy relaunches only raises the Anomaly: the
        // Run keeps its status (a crash out of `paused` is not a resume).
        LifecycleState::Failed => {
            anomalies.push(Anomaly::Failed);
            match fate {
                RunFate::Relaunching => (run.status, None),
                RunFate::Terminal => (RunStatus::Ended, Some(cause.to_string())),
            }
        }
        LifecycleState::Stopped => (RunStatus::Ended, Some(cause.to_string())),
        LifecycleState::Paused => (RunStatus::Held, None),
        // `starting` and `stopping` keep the Run's status: a fresh Run is
        // created `active` at its spawn record, and stopping a paused agent
        // is not a resume (no false `parent-resumed` cascade).
        LifecycleState::Starting | LifecycleState::Stopping | LifecycleState::Registered => {
            return Ok(changes)
        }
    };
    let from = run.status;
    let owner = run.run_id.clone();
    move_to(store, &mut changes, run, status, anomalies, end_cause, at)?;
    cascade(
        store,
        &mut changes,
        &owner,
        from,
        status,
        new_state == LifecycleState::Stopped,
        at,
    )?;
    Ok(changes)
}

/// What a supervised spawn or adoption asks of the Run registry.
#[derive(Clone, Debug)]
pub(crate) struct SupervisedRun {
    pub instance: InstanceName,
    pub run_id: RunId,
    pub wire_path: bool,
    pub labels: Vec<Label>,
    /// A fresh operator `start` (any other open process-owning Run of the
    /// instance ends as `replaced`); `false` for a relaunch or adoption,
    /// which keep the Run.
    pub fresh: bool,
}

/// Create the supervised Run (fresh start), or keep it (relaunch, adoption):
/// a kept Run re-syncs its depth / wire path and its bound labels, and a
/// kept id with no row (a pre-Run record) is created.
pub(crate) fn ensure_supervised<S: RunRecords + ?Sized>(
    store: &S,
    run: &SupervisedRun,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    if let Some(open) = store.open_process_run(&run.instance)? {
        if open.run_id != run.run_id {
            let anomalies = open.anomalies.clone();
            let owner = open.run_id.clone();
            let from = open.status;
            move_to(
                store,
                &mut changes,
                open,
                RunStatus::Ended,
                anomalies,
                Some("replaced".to_string()),
                at,
            )?;
            cascade(
                store,
                &mut changes,
                &owner,
                from,
                RunStatus::Ended,
                false,
                at,
            )?;
        }
    }
    match store.run_record(&run.run_id)? {
        Some(existing) if !run.fresh => {
            // A kept Run that already ended (a relaunch after a failure the
            // projection treated as terminal, or an adoption of a Run the
            // backfill recorded as ended) is running again: reopen it.
            let existing = if existing.is_ended() {
                let mut reopened = existing;
                reopened.status = RunStatus::Active;
                reopened.end_cause = None;
                reopened.ended_at = None;
                reopened.updated_at = at.to_string();
                store.save_run(&reopened)?;
                note(
                    store,
                    &mut changes,
                    &reopened,
                    RunEventKind::StatusChanged,
                    Prior {
                        status: Some(RunStatus::Ended),
                        ..Prior::default()
                    },
                    Some("relaunched".to_string()),
                )?;
                reopened
            } else {
                existing
            };
            changes.extend(sync_depth(
                store,
                existing,
                ControlDepth::Supervised,
                run.wire_path,
                at,
            )?);
            changes.extend(sync_bound_labels(store, &run.run_id, &run.labels, at)?);
        }
        _ => {
            let record = RunRecord {
                run_id: run.run_id.clone(),
                control_depth: ControlDepth::Supervised,
                wire_path: run.wire_path,
                status: RunStatus::Active,
                anomalies: Vec::new(),
                parent_run_id: None,
                instance: Some(run.instance.as_str().to_string()),
                process_owner: true,
                end_cause: None,
                created_at: at.to_string(),
                updated_at: at.to_string(),
                ended_at: None,
            };
            store.insert_run(&NewRun {
                record: record.clone(),
                labels: run.labels.clone(),
                limits: Default::default(),
                identity: None,
            })?;
            note(
                store,
                &mut changes,
                &record,
                RunEventKind::Created,
                Prior::default(),
                None,
            )?;
        }
    }
    Ok(changes)
}

/// Re-derive a Run's depth and wire path; a change keeps the id and emits
/// `depth-changed` (FR-49). Latches are keyed by run id, so they survive.
/// A directly set non-`warn` Breach Action the new depth cannot execute
/// raises `depth-fallback`; one it can execute clears it.
pub(crate) fn sync_depth<S: RunRecords + ?Sized>(
    store: &S,
    mut record: RunRecord,
    depth: ControlDepth,
    wire_path: bool,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    if record.control_depth == depth && record.wire_path == wire_path {
        return Ok(changes);
    }
    let prior = Prior {
        depth: Some(record.control_depth),
        wire_path: Some(record.wire_path),
        status: None,
    };
    record.control_depth = depth;
    record.wire_path = wire_path;
    record.updated_at = at.to_string();
    let limits = store.run_limits(&record.run_id)?;
    if limits.is_armed() && limits.action() != BreachAction::Warn && !record.process_owner {
        let unexecutable = action_refusal(limits.action(), depth, wire_path).is_some();
        record.anomalies.retain(|a| *a != Anomaly::DepthFallback);
        if unexecutable {
            record.anomalies.push(Anomaly::DepthFallback);
            record.anomalies.sort();
        }
    }
    store.save_run(&record)?;
    note(
        store,
        &mut changes,
        &record,
        RunEventKind::DepthChanged,
        prior,
        None,
    )?;
    Ok(changes)
}

/// Replace a Run's bound labels; one `labels-changed` event when the set
/// changed.
pub(crate) fn sync_bound_labels<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    labels: &[Label],
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let current: Vec<Label> = store
        .run_labels(run_id)?
        .into_iter()
        .filter(|label| label.source == LabelSource::Bound)
        .collect();
    let mut wanted = labels.to_vec();
    wanted.sort();
    if current == wanted {
        return Ok(changes);
    }
    let Some(mut record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    store.replace_labels(run_id, LabelSource::Bound, &wanted)?;
    record.updated_at = at.to_string();
    store.save_run(&record)?;
    note(
        store,
        &mut changes,
        &record,
        RunEventKind::LabelsChanged,
        Prior::default(),
        None,
    )?;
    Ok(changes)
}

/// Raise the `breach` Anomaly on a Run that recorded a breach (FR-21's loud
/// signal, any action). Nothing when it is already raised or the Run has no
/// row (a legacy id with no Run).
pub(crate) fn mark_breach<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    if record.anomalies.contains(&Anomaly::Breach) {
        return Ok(changes);
    }
    let mut anomalies = record.anomalies.clone();
    anomalies.push(Anomaly::Breach);
    let status = record.status;
    move_to(store, &mut changes, record, status, anomalies, None, at)?;
    Ok(changes)
}

/// Why a directly set Breach Action cannot execute on a child Run at this
/// depth, if it cannot (the verb × depth table's Breach Action row).
pub(crate) fn action_refusal(
    action: BreachAction,
    depth: ControlDepth,
    wire_path: bool,
) -> Option<String> {
    if action == BreachAction::Warn {
        return None;
    }
    let cell = super::verb_cell(super::RunVerb::BreachActionPauseStopBlock, depth, wire_path);
    if !wire_path {
        Some(
            "the hosting process's model traffic does not pass through the Metering Proxy, so \
             a child Run cannot be paused, stopped or blocked on its own"
                .to_string(),
        )
    } else if !cell.available {
        Some(format!(
            "holding and rejecting a {} Run's calls is not available yet — use `warn`, or set \
             the ceiling on the process-owning Run",
            depth.as_str()
        ))
    } else {
        None
    }
}

/// Open a child Run (FR-43, FR-49) under an existing, non-ended parent.
/// A request whose external id already maps to a Run returns that Run with
/// no change.
pub(crate) fn open_child<S: RunRecords + RunStore + ?Sized>(
    store: &S,
    request: &ChildRunRequest,
    run_id: RunId,
    at: &str,
) -> Result<(RunId, RunChanges), RunError> {
    let mut changes = RunChanges::default();
    if let Some(external) = &request.external_id {
        if let Some(existing) = store.identity_run(HOST_IDENTITY_BINDING, external)? {
            return Ok((existing, changes));
        }
    }
    let parent = match &request.parent {
        RunParent::Run(id) => store.run_record(id)?.ok_or_else(|| RunError::NotFound {
            run_id: id.as_str().to_string(),
        })?,
        RunParent::Instance(name) => {
            let iname = InstanceName::new(name).map_err(|e| RunError::Invalid {
                field: "parent".to_string(),
                reason: e.to_string(),
            })?;
            store
                .open_process_run(&iname)?
                .ok_or_else(|| RunError::NoOpenRun {
                    instance: name.clone(),
                })?
        }
    };
    if parent.is_ended() {
        return Err(RunError::ParentEnded {
            run_id: parent.run_id.as_str().to_string(),
        });
    }
    let level = store.run_chain(&parent.run_id)?.len() + 1;
    if level > MAX_RUN_NESTING {
        return Err(RunError::NestingTooDeep {
            parent: parent.run_id.as_str().to_string(),
            depth: level,
            max: MAX_RUN_NESTING,
        });
    }
    let depth = ControlDepth::Announced;
    let wire_path = parent.wire_path;
    if let Some(action) = request.limits.breach_action {
        if let Some(reason) = action_refusal(action, depth, wire_path) {
            return Err(RunError::ActionUnavailable {
                action: action.as_str().to_string(),
                depth: depth.as_str().to_string(),
                metering_source: if wire_path {
                    "engine-observed"
                } else {
                    "self-reported"
                }
                .to_string(),
                reason,
            });
        }
    }
    let mut labels = Vec::with_capacity(request.labels.len());
    for (key, value) in &request.labels {
        super::validate_label_key(key)
            .and_then(|()| super::validate_label_value(value))
            .map_err(|reason| RunError::Invalid {
                field: "label".to_string(),
                reason,
            })?;
        if labels.iter().any(|label: &Label| &label.key == key) {
            return Err(RunError::Invalid {
                field: "label".to_string(),
                reason: format!("label key `{key}` is given twice"),
            });
        }
        labels.push(Label::caller(key.clone(), value.clone()));
    }
    labels.sort();
    // A child opened under a held parent starts held, as the cascade would
    // have left it.
    let held = parent.status == RunStatus::Held;
    let record = RunRecord {
        run_id: run_id.clone(),
        control_depth: depth,
        wire_path,
        status: if held {
            RunStatus::Held
        } else {
            RunStatus::Active
        },
        anomalies: Vec::new(),
        parent_run_id: Some(parent.run_id.clone()),
        instance: parent.instance.clone(),
        process_owner: false,
        end_cause: None,
        created_at: at.to_string(),
        updated_at: at.to_string(),
        ended_at: None,
    };
    store.insert_run(&NewRun {
        record: record.clone(),
        labels,
        limits: request.limits,
        identity: request
            .external_id
            .clone()
            .map(|external| (HOST_IDENTITY_BINDING.to_string(), external)),
    })?;
    note(
        store,
        &mut changes,
        &record,
        RunEventKind::Created,
        Prior::default(),
        held.then(|| "parent-paused".to_string()),
    )?;
    Ok((run_id, changes))
}

/// Close a child Run (idempotent on an ended one). A process-owning Run is
/// stopped, never closed. Its own children keep running (only the children
/// inside a process end with the Run that owns it, FR-49).
pub(crate) fn close<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    at: &str,
) -> Result<RunChanges, RunError> {
    let mut changes = RunChanges::default();
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    if record.process_owner && !record.is_ended() {
        return Err(RunError::OwnsProcess {
            run_id: run_id.as_str().to_string(),
            instance: record.instance.clone().unwrap_or_default(),
        });
    }
    if record.is_ended() {
        return Ok(changes);
    }
    let anomalies = record.anomalies.clone();
    move_to(
        store,
        &mut changes,
        record,
        RunStatus::Ended,
        anomalies,
        Some("closed".to_string()),
        at,
    )?;
    Ok(changes)
}

/// Read one Run for the facade: its row, labels, limits, subtree usage and
/// open children.
pub(crate) fn view<S: RunRecords + RunStore + ?Sized>(
    store: &S,
    run_id: &RunId,
) -> Result<RunView, RunError> {
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    let usage = match &record.instance {
        Some(instance) => {
            let name = InstanceName::new(instance).map_err(|e| {
                RunError::Store(StoreError::CorruptRow {
                    name: instance.clone(),
                    detail: e.to_string(),
                })
            })?;
            let rollup = store.run_rollup(&name, run_id)?;
            RunUsage {
                input_tokens: rollup.totals.input_tokens,
                output_tokens: rollup.totals.output_tokens,
                cached_tokens: rollup.totals.cached_tokens,
                cost_micros: rollup.cost,
                estimate: EstimateLabel::Estimated,
            }
        }
        None => RunUsage::default(),
    };
    Ok(RunView {
        labels: store.run_labels(run_id)?,
        limits: store.run_limits(run_id)?,
        open_children: store.open_children(run_id)?,
        usage,
        record,
    })
}

/// Every non-ended Run as snapshot entries (the writer's initial publish).
pub(crate) fn open_entries<S: RunRecords + ?Sized>(store: &S) -> Result<Vec<RunEntry>, StoreError> {
    store
        .open_runs()?
        .into_iter()
        .map(|record| {
            Ok(RunEntry {
                labels: store.run_labels(&record.run_id)?,
                limits: store.run_limits(&record.run_id)?,
                record,
            })
        })
        .collect()
}

/// End every open process-owning Run whose instance holds no process
/// (state `failed`, `stopped` or `registered`): a relaunch that was planned
/// but never ran (its engine exited during the backoff) leaves the Run
/// `active`, and nothing else would end it. Ends with cause
/// `relaunch-not-run`, keeping its Anomalies; a relaunch that does run later
/// reopens it (`ensure_supervised`).
pub(crate) fn end_runs_without_a_process(
    store: &crate::store::SqliteStore,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    for record in store.open_owners_without_a_process()? {
        let owner = record.run_id.clone();
        let from = record.status;
        let anomalies = record.anomalies.clone();
        move_to(
            store,
            &mut changes,
            record,
            RunStatus::Ended,
            anomalies,
            Some("relaunch-not-run".to_string()),
            at,
        )?;
        cascade(
            store,
            &mut changes,
            &owner,
            from,
            RunStatus::Ended,
            false,
            at,
        )?;
    }
    Ok(changes)
}
