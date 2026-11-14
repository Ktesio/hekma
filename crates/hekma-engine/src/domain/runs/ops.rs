//! Run registry operations, run inside store-writer jobs (story 21-2).
//!
//! Each operation reads and writes through the writer's store inside the
//! job's SAVEPOINT and returns a [`RunChanges`]: the [`RunEvent`]s it caused
//! and the snapshot entries it changed. [`commit`] appends those events to
//! the outbox in the same transaction and registers the snapshot publish as
//! a post-commit hook, so a Run change is durable, exportable and visible to
//! the data path together — or not at all.

use std::sync::Arc;

use super::labels::{self, LabelRefusal};
use super::snapshot::{RunEntry, SnapshotPublisher};
use super::{
    policy, Anomaly, ChildRunRequest, ControlDepth, Label, LabelChange, LabelOutcome, RunError,
    RunEvent, RunEventKind, RunParent, RunRecord, RunStatus, RunUsage, RunView, Selector,
    HOST_IDENTITY_BINDING, MAX_RUN_NESTING, MAX_SELECTED_RUNS, RUN_EVENT_SCHEMA_VERSION,
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
#[derive(Clone, Debug, Default)]
pub(crate) struct RunChanges {
    pub events: Vec<RunEvent>,
    pub entries: Vec<RunEntry>,
    /// Diagnostics the operation surfaced (a fail-closed resolution note):
    /// the caller emits them on the engine's diagnostic surface.
    pub notes: Vec<String>,
    /// Story 22-1: the credential events the operation caused (one per
    /// token per change), appended to the outbox after the Run events.
    pub credentials: Vec<crate::domain::credential::CredentialEvent>,
    /// Story 22-1: the key-index changes, published with the Run entries.
    pub keys: Vec<super::snapshot::KeyChange>,
    /// Test-only: the Runs whose Policy hook ran in this operation.
    #[cfg(test)]
    pub policy_resolved: Vec<RunId>,
}

impl RunChanges {
    pub(crate) fn extend(&mut self, other: RunChanges) {
        self.events.extend(other.events);
        self.entries.extend(other.entries);
        self.notes.extend(other.notes);
        self.credentials.extend(other.credentials);
        self.keys.extend(other.keys);
        #[cfg(test)]
        self.policy_resolved.extend(other.policy_resolved);
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

/// A Run-registry event (a Run event, or since story 22-1 a credential
/// event) + its outbox position, as committed.
pub(crate) type CommittedRunEvent = (EngineEvent, OutboxAppend);

/// Append `changes`' events to the outbox and register the snapshot
/// publish after commit. Returns the events in commit order for the caller
/// to publish on the bus after its own events.
pub(crate) fn commit(
    ctx: &mut WriterCtx<'_>,
    publisher: &Arc<SnapshotPublisher>,
    changes: RunChanges,
) -> Result<Vec<CommittedRunEvent>, StoreError> {
    let mut committed = Vec::with_capacity(changes.events.len() + changes.credentials.len());
    let events = changes.events.into_iter().map(EngineEvent::Run).chain(
        changes
            .credentials
            .into_iter()
            .map(|event| EngineEvent::Credential(Box::new(event))),
    );
    for event in events {
        let append = ctx.store().append_outbox(&event, OUTBOX_MAX_BYTES)?;
        committed.push((event, append));
    }
    if !changes.entries.is_empty() || !changes.keys.is_empty() {
        let publisher = Arc::clone(publisher);
        let entries = changes.entries;
        let keys = changes.keys;
        ctx.after_commit(move || publisher.publish_changes(entries, keys));
    }
    Ok(committed)
}

/// Record one change of `record` (already saved) as an event plus its
/// snapshot entry. The event names the Policies that supplied the Run's
/// limits and the entry carries its stored resolution (AD-14, story 21-3).
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
    let resolution = store.run_policy(&record.run_id)?.map(|row| row.resolution);
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
        labels: bound_only(&labels),
        prior_labels: prior.labels,
        limits: prior
            .limits
            .map(|_| Box::new(prior.after_limits.unwrap_or(limits))),
        prior_limits: prior.limits.map(Box::new),
        limits_layer: prior.layer.map(str::to_string),
        cause,
        policies: resolution
            .as_ref()
            .map(|resolution| resolution.sources.policy_names())
            .unwrap_or_default(),
        at: record.updated_at.clone(),
    });
    changes.entries.push(super::snapshot::entry_of(
        record,
        &labels,
        limits,
        resolution.as_ref(),
        store.entry_extras(&record.run_id)?,
    ));
    Ok(())
}

/// Note a just-inserted Run's `created` event and entry (story 23-1: the
/// wire-Run job).
pub(crate) fn note_created<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    cause: Option<String>,
) -> Result<(), StoreError> {
    note(
        store,
        changes,
        record,
        RunEventKind::Created,
        Prior::default(),
        cause,
    )
}

/// A fresh snapshot entry for `run_id` with no event (story 23-1: a job
/// that changed only a latch — a `block` breach on a Run already carrying
/// its `breach` Anomaly, a re-arm — republishes the disposition). `None`
/// for a Run with no row.
pub(crate) fn refreshed_entry<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
) -> Result<Option<RunEntry>, StoreError> {
    let Some(record) = store.run_record(run_id)? else {
        return Ok(None);
    };
    let labels = store.run_labels(run_id)?;
    let limits = store.run_limits(run_id)?;
    let resolution = store.run_policy(run_id)?.map(|row| row.resolution);
    Ok(Some(super::snapshot::entry_of(
        &record,
        &labels,
        limits,
        resolution.as_ref(),
        store.entry_extras(run_id)?,
    )))
}

#[derive(Default)]
struct Prior {
    status: Option<RunStatus>,
    depth: Option<ControlDepth>,
    wire_path: Option<bool>,
    labels: Option<Vec<Label>>,
    /// The Run's own limits before, on `limits-changed` (the event then
    /// carries the limits after too).
    limits: Option<super::RunLimits>,
    /// The changed layer after, on `limits-changed` (else the Run's
    /// Operator/Host layer as read back).
    after_limits: Option<super::RunLimits>,
    /// Which layer the limits describe, on `limits-changed` (story 22-1).
    layer: Option<&'static str>,
}

/// The bound subset of `labels` (what a Run event carries).
fn bound_only(labels: &[Label]) -> Vec<Label> {
    labels
        .iter()
        .filter(|label| label.is_bound())
        .cloned()
        .collect()
}

/// Move `record` to `status` with `anomalies`, saving it and noting the
/// event (`ended`, `status-changed` or `anomalies-changed`). Nothing when
/// neither changed. Story 22-1: a move to `ended` revokes the Run's
/// remaining active credentials in the same job (`run-ended`).
pub(crate) fn move_to<S: RunRecords + ?Sized>(
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
    if kind == RunEventKind::Ended {
        super::keys::on_run_ended(store, changes, &record.run_id, at)?;
    }
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
    // Story 22-1: the credentials an exit takes out of service are revoked
    // by [`revoke_on_exit`], in the transition's own transaction — never in
    // this projection, which a caller may run in a savepoint that absorbs
    // failures.
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

/// The credentials a lifecycle transition takes out of service (story 22-1,
/// P1): `failed` and `stopped` record a confirmed exit, so the instance's
/// open process-owning Run loses every launch-issued credential
/// (`process-exited`) — before any relaunch spawns its next generation —
/// and, when the edge ends the Run (`stopped`, or a terminal `failed`),
/// every remaining one (`run-ended`), with those of the in-process
/// children the edge's cascade ends. The caller commits these in the
/// transition's OWN transaction, outside any failure-absorbing savepoint:
/// a revocation that cannot be written fails the transition, so an exit is
/// never recorded with its key still honoured.
pub(crate) fn revoke_on_exit<S: RunRecords + ?Sized>(
    store: &S,
    instance: &InstanceName,
    new_state: LifecycleState,
    fate: RunFate,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    if !matches!(new_state, LifecycleState::Failed | LifecycleState::Stopped) {
        return Ok(changes);
    }
    let Some(run) = store.open_process_run(instance)? else {
        return Ok(changes);
    };
    super::keys::on_process_exited(store, &mut changes, &run.run_id, at)?;
    let ends = new_state == LifecycleState::Stopped || fate == RunFate::Terminal;
    if ends {
        super::keys::on_run_ended(store, &mut changes, &run.run_id, at)?;
        // The in-process children the projection's cascade ends with the
        // Run lose their credentials here too: they were handed to the
        // process that just exited, so they never outlive a projection that
        // could not be written.
        for child in store.open_in_process_descendants(&run.run_id)? {
            super::keys::on_run_ended(store, &mut changes, &child.run_id, at)?;
        }
    }
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
    /// The caller-resolved instance facts (story 21-3): the created or
    /// kept Run resolves against the Policies in the SAME job (the 21-9
    /// deferral, filled). `None` reuses the Run's stored inputs (a kept
    /// Run); a fresh Run with `None` resolves with no explicit instance
    /// limits.
    pub inputs: Option<super::policy::ResolveInputs>,
    /// Story 23-1: the instance's resolved upstream base URL, recorded on
    /// the Run (the shared proxy's Run route forwards there); `None` keeps
    /// what is recorded (an adoption) or records none (self-reported).
    pub upstream: Option<String>,
    /// Story 22-2: the custody row this launch freezes on the Run. `None`
    /// (an adoption) keeps what is recorded; the row's `origin` is frozen
    /// for the Run's life — a relaunch after a configuration edit changes
    /// nothing.
    pub custody: Option<crate::ports::RunCustodyRow>,
}

/// A supervised launch's credentials (story 22-1), value-free: the Run Key
/// and, when the Adapter maps one, its refresh credential.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LaunchCredentials {
    pub key: crate::domain::credential::TokenStub,
    pub refresh: Option<crate::domain::credential::TokenStub>,
}

/// Issue a supervised launch's credentials on its Run (story 22-1, P1): in
/// the launch-record job, with the launch's generation, custody-eligible,
/// issued by the engine. A token exists in state.db only if its launch was
/// recorded.
pub(crate) fn issue_launch<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    generation: u64,
    credentials: LaunchCredentials,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    // One live generation at a time, by construction: whatever an earlier
    // launch of this Run still holds — used refresh credentials included —
    // is revoked before the new generation
    // is issued (its exit should already have revoked it — this holds even
    // when that exit was never recorded).
    super::keys::revoke_run_tokens(
        store,
        &mut changes,
        run_id,
        |row| {
            row.launch_generation
                .is_some_and(|older| older < generation)
        },
        super::keys::cause::PROCESS_EXITED,
        at,
    )?;
    super::keys::revoke_used_where(
        store,
        &mut changes,
        run_id,
        |row| {
            row.launch_generation
                .is_some_and(|older| older < generation)
        },
        super::keys::cause::PROCESS_EXITED,
        at,
    )?;
    match credentials.refresh {
        Some(refresh) => super::keys::issue_pair(
            store,
            &mut changes,
            &record,
            credentials.key,
            refresh,
            Some(generation),
            None,
            "engine",
            true,
            None,
            at,
        )?,
        None => super::keys::issue(
            store,
            &mut changes,
            &record,
            super::keys::Issue {
                token: credentials.key,
                launch_generation: Some(generation),
                paired: None,
                issued_by_hash: None,
                issuer: "engine".to_string(),
                custody_eligible: true,
                prior_token_id: None,
            },
            at,
        )?,
    }
    Ok(changes)
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
            let label_changes =
                sync_bound_labels(store, &run.run_id, &run.labels, run.inputs.clone(), at)?;
            let labels_changed = !label_changes.entries.is_empty();
            changes.extend(label_changes);
            // Story 21-3: the kept Run's label sync resolves with the
            // caller's FRESH inputs when it has them (an adoption re-syncs
            // labels against a config it just read — the stored inputs
            // would be one read stale); the stored inputs otherwise. A
            // label change already resolved inside its own hook — never
            // resolve twice for one job.
            if !labels_changed {
                changes.extend(policy::resolve_run(
                    store,
                    &run.run_id,
                    run.inputs.clone(),
                    at,
                )?);
            }
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
            // Story 21-3: the Run resolves at creation — the 21-9 deferral
            // (a Policy created while this engine was off, a hand-edited
            // config) cannot leave a fresh Run unpriced/unresolved until
            // its first label change.
            changes.extend(policy::resolve_run(
                store,
                &run.run_id,
                run.inputs.clone(),
                at,
            )?);
        }
    }
    // Story 23-1: the instance's upstream rides the Run (the shared proxy's
    // Run route forwards there); the freshest entry publishes last.
    if let Some(upstream) = &run.upstream {
        store.set_run_upstream(&run.run_id, Some(upstream))?;
        if let Some(entry) = refreshed_entry(store, &run.run_id)? {
            changes.entries.push(entry);
        }
    }
    // Story 22-2: the custody row freezes on the Run with the same job
    // (the origin for the Run's life; the reference, never a value).
    if let Some(custody) = &run.custody {
        store.set_run_custody(&run.run_id, Some(custody.clone()))?;
        if let Some(entry) = refreshed_entry(store, &run.run_id)? {
            changes.entries.push(entry);
        }
    }
    Ok(changes)
}

/// Re-derive a Run's depth and wire path; a change keeps the id and emits
/// `depth-changed` (FR-49). Latches are keyed by run id, so they survive.
/// The Run's Policies re-resolve in the same job (a depth change moves the
/// fallback rule, so a Policy-sourced `depth-fallback` Anomaly sets or
/// clears here); a directly set non-`warn` Breach Action the new depth
/// cannot execute raises `depth-fallback` after it (the 21-2 loud signal
/// for directly set actions, which the resolution deliberately leaves to
/// the executor).
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
        ..Prior::default()
    };
    record.control_depth = depth;
    record.wire_path = wire_path;
    record.updated_at = at.to_string();
    store.save_run(&record)?;
    note(
        store,
        &mut changes,
        &record,
        RunEventKind::DepthChanged,
        prior,
        None,
    )?;
    // The resolution's own `store_changes` raises the `depth-fallback`
    // Anomaly for BOTH sources — a POLICY-sourced action per the
    // resolver, a directly set one per the 21-2 rule (round 3: the
    // duplicate re-raise that used to live here was dead code once
    // store_changes owned the directly set arm; removed, not tested
    // around).
    changes.extend(policy::resolve_run(store, &record.run_id, None, at)?);
    Ok(changes)
}

/// Re-sync a Run's configuration-bound labels to `config` (the instance's
/// `labels.*` now) — a config write, a relaunch or an adoption. Only
/// `config`-origin rows are replaced; a `label-verb` key is kept and a
/// configured key replaces a Caller row ([`labels::plan_config_sync`]). One
/// `labels-changed` event (cause `config`) and one Policy re-resolution when
/// the set changed; nothing at all when it did not or the Run has no row.
pub(crate) fn sync_bound_labels<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    config: &[Label],
    inputs: Option<super::policy::ResolveInputs>,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    let current = store.run_labels(run_id)?;
    let next = labels::plan_config_sync(&current, config);
    if next == current {
        return Ok(changes);
    }
    write_labels(
        store,
        &mut changes,
        record,
        &current,
        &next,
        "config",
        inputs,
        at,
    )?;
    Ok(changes)
}

/// Re-sync a binding Run's `binding`-origin labels to `binding` (story
/// 23-1: a binding label change re-resolves its Runs in the same job).
pub(crate) fn sync_binding_labels<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    binding: &[(String, String)],
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    let current = store.run_labels(run_id)?;
    let next = labels::plan_binding_sync(&current, binding);
    if next == current {
        return Ok(changes);
    }
    write_labels(
        store,
        &mut changes,
        record,
        &current,
        &next,
        "binding",
        None,
        at,
    )?;
    Ok(changes)
}

/// Write a Run's next label set (`next`, from a planner), then run the
/// Policy hook inside the same job, then note the `labels-changed` event
/// (bound labels before and after) and the snapshot entry — so a Policy
/// re-resolution commits and publishes with the label change (AD-25).
#[allow(clippy::too_many_arguments)]
fn write_labels<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    mut record: RunRecord,
    current: &[Label],
    next: &[Label],
    cause: &str,
    inputs: Option<super::policy::ResolveInputs>,
    at: &str,
) -> Result<(), StoreError> {
    store.set_run_labels(&record.run_id, next)?;
    record.updated_at = at.to_string();
    store.save_run(&record)?;
    let resolved = policy::labels_changed(store, &record.run_id, inputs, at)?;
    // Both planners only ever change bound rows (a verb write, a config
    // row, a bound key replacing a Caller's), so a changed set is a changed
    // bound set.
    note(
        store,
        changes,
        &record,
        RunEventKind::LabelsChanged,
        Prior {
            labels: Some(bound_only(current)),
            ..Prior::default()
        },
        Some(cause.to_string()),
    )?;
    changes.extend(resolved);
    Ok(())
}

/// Note a disposition change on `record` (story 23-4): one
/// `disposition-changed` event whose cause names the reason and the change.
pub(crate) fn note_disposition<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    cause: String,
) -> Result<(), StoreError> {
    note(
        store,
        changes,
        record,
        RunEventKind::DispositionChanged,
        Prior::default(),
        Some(cause),
    )
}

/// Note a Caller-label change on `record` (already saved; story 23-2: a
/// re-announce upserts its Caller labels): one `labels-changed` event with
/// `cause`. Caller labels never select a Policy (FR-60), so nothing
/// re-resolves.
pub(crate) fn note_caller_labels<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    cause: &str,
) -> Result<(), StoreError> {
    let labels = store.run_labels(&record.run_id)?;
    note(
        store,
        changes,
        record,
        RunEventKind::LabelsChanged,
        Prior {
            labels: Some(bound_only(&labels)),
            ..Prior::default()
        },
        Some(cause.to_string()),
    )
}

/// Note a `limits-changed` event for `record` (story 21-6): the changed
/// layer of the Run's own limits before (`prior`) and after (`after`), with
/// the actor as the cause. The change is the Run's latest update: the row
/// is saved here with `updated_at = at`, and the event carries that `at`
/// (the audit time of a money change, not the Run's last earlier write).
#[allow(clippy::too_many_arguments)]
pub(crate) fn note_limits_changed<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    prior: super::RunLimits,
    after: super::RunLimits,
    actor: &str,
    layer: &'static str,
    at: &str,
) -> Result<(), StoreError> {
    let mut record = record.clone();
    record.updated_at = at.to_string();
    store.save_run(&record)?;
    note(
        store,
        changes,
        &record,
        RunEventKind::LimitsChanged,
        Prior {
            limits: Some(prior),
            after_limits: Some(after),
            layer: Some(layer),
            ..Prior::default()
        },
        Some(actor.to_string()),
    )
}

/// Note an `anomalies-changed` event for `record` (already saved with its
/// new Anomalies) plus its snapshot entry. Story 21-3's public entry point
/// for the resolution-owned Anomalies (`depth-fallback`, `unpriced-model`).
pub(crate) fn note_anomalies_changed<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    record: &RunRecord,
    _at: &str,
) -> Result<(), StoreError> {
    note(
        store,
        changes,
        record,
        RunEventKind::AnomaliesChanged,
        Prior::default(),
        None,
    )
}

/// The label verb (story 21-9, FR-49): bind and unbind labels on one
/// non-ended Run as the Operator. `config` is what the instance
/// configuration binds the Run today — the caller resolves it for a
/// process-owning Run before the job (empty otherwise), so removing a verb
/// label restores a value it shadowed. All-or-nothing; a dry run plans and
/// writes nothing; an unchanged set writes and emits nothing.
pub(crate) fn label<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    change: &LabelChange,
    config: &[Label],
    dry_run: bool,
    at: &str,
) -> Result<(LabelOutcome, RunChanges), RunError> {
    change.validate()?;
    let mut changes = RunChanges::default();
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    if record.is_ended() {
        return Err(RunError::RunEnded {
            run_id: run_id.as_str().to_string(),
        });
    }
    let config: &[Label] = if record.process_owner { config } else { &[] };
    let current = store.run_labels(run_id)?;
    let next =
        labels::plan_label_change(&current, change, config).map_err(|refusal| match refusal {
            LabelRefusal::FromConfig(key) => RunError::LabelFromConfig {
                run_id: run_id.as_str().to_string(),
                key,
                instance: record.instance.clone().unwrap_or_default(),
            },
            LabelRefusal::NotBound(key) => RunError::LabelNotBound {
                run_id: run_id.as_str().to_string(),
                key,
            },
        })?;
    let changed = next != current;
    if changed && !dry_run {
        write_labels(
            store,
            &mut changes,
            record,
            &current,
            &next,
            "label-verb",
            None,
            at,
        )?;
    }
    Ok((
        LabelOutcome {
            run_id: run_id.clone(),
            labels: next,
            prior_labels: current,
            changed,
            dry_run,
        },
        changes,
    ))
}

/// Resolve a Selector over the non-ended Runs (story 21-9): every Run
/// carrying all its terms, bound or Caller, ordered by run id. More than
/// [`MAX_SELECTED_RUNS`] matches is refused (`too-many-runs`), never cut.
pub(crate) fn select<S: RunRecords + ?Sized>(
    store: &S,
    selector: &Selector,
) -> Result<Vec<RunRecord>, RunError> {
    let runs = store.select_runs(selector, MAX_SELECTED_RUNS + 1)?;
    if runs.len() > MAX_SELECTED_RUNS {
        return Err(RunError::TooManyRuns {
            selector: selector.to_string(),
            max: MAX_SELECTED_RUNS,
        });
    }
    Ok(runs)
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

/// Clear the `breach` Anomaly from a Run whose every latch re-armed
/// (story 21-3's raise path; the Anomaly stays while any latch holds).
/// Nothing when it is not raised or the Run has no row. Returns the
/// `anomalies-changed` event and snapshot entry (committed by the caller's
/// own `commit`, so the clearing publishes — the pre-fix silent save left
/// surfaces stale, 21-3 review round 1).
pub(crate) fn clear_breach_anomaly<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    let Some(record) = store.run_record(run_id)? else {
        return Ok(changes);
    };
    if !record.anomalies.contains(&Anomaly::Breach) {
        return Ok(changes);
    }
    let mut anomalies: Vec<Anomaly> = record
        .anomalies
        .iter()
        .copied()
        .filter(|anomaly| *anomaly != Anomaly::Breach)
        .collect();
    anomalies.sort();
    anomalies.dedup();
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
    // Story 23-1: `block` executes at any depth with a wire path (the shared
    // proxy fences the Run and everything beneath it).
    if action == BreachAction::Block && wire_path {
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
/// no change. The child resolves against the Policies in the same job
/// (story 21-3: creation resolves — `inputs` is the caller-resolved
/// instance facts; `None` resolves with no explicit instance limits).
pub(crate) fn open_child<S: RunRecords + RunStore + ?Sized>(
    store: &S,
    request: &ChildRunRequest,
    run_id: RunId,
    inputs: Option<super::policy::ResolveInputs>,
    at: &str,
) -> Result<(RunId, RunChanges), RunError> {
    open_child_bound(store, request, run_id, inputs, HOST_IDENTITY_BINDING, at)
}

/// [`open_child`] with the external id resolved under `binding` (story
/// 22-1: a Caller's external ids live in its own namespace, so a Run Key can
/// never claim — or probe — an id the Operator or a Host will use).
pub(crate) fn open_child_bound<S: RunRecords + RunStore + ?Sized>(
    store: &S,
    request: &ChildRunRequest,
    run_id: RunId,
    inputs: Option<super::policy::ResolveInputs>,
    binding: &str,
    at: &str,
) -> Result<(RunId, RunChanges), RunError> {
    let mut changes = RunChanges::default();
    if let Some(external) = &request.external_id {
        if let Some(existing) = store.identity_run(binding, external)? {
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
    // The Caller guard (FR-49): every Caller-label write goes through it —
    // vacuous for a fresh Run, which has no bound label yet.
    labels::check_caller_labels(&run_id, &store.run_labels(&run_id)?, &labels)?;
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
            .map(|external| (binding.to_string(), external)),
    })?;
    note(
        store,
        &mut changes,
        &record,
        RunEventKind::Created,
        Prior::default(),
        held.then(|| "parent-paused".to_string()),
    )?;
    // Story 21-3: the child resolves at creation (a bound label it carries
    // can select a Policy whose ceilings arm its own level from the start).
    changes.extend(policy::resolve_run(store, &run_id, inputs, at)?);
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
    // Story 23-1 (v18): the Run rollup is keyed by the run id alone, so a
    // Run without an Agent Instance (a wire Run of the shared proxy) shows
    // its usage too — it used to read as zero.
    let rollup = store.run_rollup_by_id(run_id)?;
    let usage = RunUsage {
        input_tokens: rollup.totals.input_tokens,
        output_tokens: rollup.totals.output_tokens,
        cached_tokens: rollup.totals.cached_tokens,
        cost_micros: rollup.cost,
        estimate: EstimateLabel::Estimated,
    };
    let custody = if record.process_owner {
        Some(super::listing::CustodyView::of(store.run_custody(run_id)?))
    } else {
        None
    };
    let (lease_expires_at, last_activity_at) = store.run_lease(run_id)?;
    let (held_calls, oldest_held_at, last_hold_end) = store.run_held(run_id)?;
    Ok(RunView {
        lease_expires_at,
        last_activity_at,
        dispositions: store.run_disposition_reasons(run_id)?,
        held_calls,
        oldest_held_at,
        last_hold_end,
        labels: store.run_labels(run_id)?,
        limits: store.run_limits(run_id)?,
        policy: store.run_policy(run_id)?.map(|row| row.resolution),
        open_children: store.open_children(run_id)?,
        usage,
        record,
        custody,
    })
}

/// Every non-ended Run as snapshot entries (the writer's initial publish).
pub(crate) fn open_entries<S: RunRecords + ?Sized>(store: &S) -> Result<Vec<RunEntry>, StoreError> {
    store
        .open_runs()?
        .into_iter()
        .map(|record| {
            let labels = store.run_labels(&record.run_id)?;
            let limits = store.run_limits(&record.run_id)?;
            let resolution = store.run_policy(&record.run_id)?.map(|row| row.resolution);
            Ok(super::snapshot::entry_of(
                &record,
                &labels,
                limits,
                resolution.as_ref(),
                store.entry_extras(&record.run_id)?,
            ))
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
