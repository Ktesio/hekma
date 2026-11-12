//! The Registry's Run-limit surface (story 21-6): the budget job and the
//! config-write re-arm as store-writer jobs, and the budget read.
//!
//! Every job here runs in ONE writer job per Run and returns the breaches
//! it fired at once; their process actions are the CALLER's to apply (D2,
//! as the Policy change job does).

use std::sync::Arc;

use crate::domain::name::InstanceName;
use crate::domain::pipeline::CommittedBreach;
use crate::domain::runs::limits::{self, InstanceContext};
use crate::domain::runs::ops::{self, CommittedRunEvent, RunChanges};
use crate::domain::runs::{
    LimitActor, ResolveInputs, RunError, RunLimitChange, RunLimitsOutcome, RunLimitsView,
};
use crate::domain::usage::RunId;
use crate::domain::Registry;
use crate::ports::RunRecords;
use crate::time::now_rfc3339;

/// What one committed budget job (or config-write re-arm) produced.
#[derive(Debug, Default)]
pub(crate) struct LimitsApplied {
    /// The Run's outcome (`None` for a config write with no open Run).
    pub outcome: Option<RunLimitsOutcome>,
    /// Breaches fired at once; the caller applies their process actions.
    pub breaches: Vec<CommittedBreach>,
    /// Run events the job committed (publish on the bus, after the
    /// breaches — the outbox order).
    pub run_events: Vec<CommittedRunEvent>,
    /// Notes for the diagnostic surface.
    pub notes: Vec<String>,
}

impl Registry {
    /// Plan a Run-level limit change (the dry run): every refusal and the
    /// planned outcome, read-only (works on a read-only engine).
    pub(crate) fn plan_run_limits(
        &self,
        run_id: &RunId,
        change: &RunLimitChange,
        actor: LimitActor,
        context: Option<&InstanceContext>,
    ) -> Result<RunLimitsOutcome, RunError> {
        let plan = self
            .store
            .snapshot(|store| Ok(limits::plan_limits(store, run_id, change, actor, context)))??;
        Ok(plan.dry_outcome())
    }

    /// Commit a Run-level limit change (one writer job): the budget job of
    /// [`limits::apply_limits`], its Run events committed in the same job.
    pub(crate) fn apply_run_limits(
        &self,
        run_id: &RunId,
        change: &RunLimitChange,
        actor: LimitActor,
        context: Option<InstanceContext>,
    ) -> Result<LimitsApplied, RunError> {
        let publisher = Arc::clone(&self.run_snapshot);
        let run_id = run_id.clone();
        let change = change.clone();
        let result = self.writer.call(move |ctx| {
            let store = ctx.store();
            let at = now_rfc3339();
            match limits::apply_limits(store, ctx, &run_id, &change, actor, context.as_ref(), &at) {
                Ok((outcome, changes, breaches)) => {
                    let notes = outcome.notes.clone();
                    let mut notes = notes;
                    notes.extend(changes.notes.iter().cloned());
                    let run_events = ops::commit(ctx, &publisher, changes)?;
                    Ok(Ok(LimitsApplied {
                        outcome: Some(outcome),
                        breaches,
                        run_events,
                        notes,
                    }))
                }
                Err(RunError::Store(err)) => Err(err),
                Err(refusal) => Ok(Err(refusal)),
            }
        });
        match result {
            Ok(inner) => inner,
            Err(err) => Err(RunError::Store(err)),
        }
    }

    /// The config-write re-arm (story 21-6, the 19-2 live-un-fence
    /// trigger): after a `budget.*` or `cost.rate.*` write, re-resolve the
    /// instance's open process-owning Run with the FRESH inputs and run the
    /// SAME re-arm pass a Policy change and `hekma budget` run — a raise
    /// above the totals re-arms and lifts the matching fence reasons per
    /// reason; a lowering at or below the usage breaches at once. An
    /// instance with no open Run resolves nothing (the next start resolves
    /// at creation).
    pub(crate) fn rearm_after_config_write(
        &self,
        name: &InstanceName,
        inputs: ResolveInputs,
        context: InstanceContext,
    ) -> Result<LimitsApplied, RunError> {
        let publisher = Arc::clone(&self.run_snapshot);
        let instance = name.clone();
        let result = self.writer.call(move |ctx| {
            let body = |ctx: &mut crate::store::WriterCtx<'_>| -> Result<LimitsApplied, RunError> {
                let store = ctx.store();
                let Some(open) = store.open_process_run(&instance)? else {
                    return Ok(LimitsApplied::default());
                };
                let at = now_rfc3339();
                let run_limits = store.run_limits(&open.run_id)?;
                let resolution = limits::resolve_with(store, &open, &run_limits, &inputs)?;
                let mut changes = RunChanges::default();
                changes.extend(crate::domain::runs::policy::resolve_and_store(
                    store,
                    &open,
                    &resolution,
                    &inputs,
                    &at,
                )?);
                let mut breaches = Vec::new();
                let mut notes = Vec::new();
                let supervised = context.supervised;
                limits::re_arm(
                    store,
                    &instance,
                    &open,
                    &resolution,
                    &context,
                    supervised,
                    ctx,
                    &mut breaches,
                    &mut changes,
                    &mut notes,
                )?;
                notes.extend(changes.notes.iter().cloned());
                let run_events = ops::commit(ctx, &publisher, changes)?;
                Ok(LimitsApplied {
                    outcome: None,
                    breaches,
                    run_events,
                    notes,
                })
            };
            match body(ctx) {
                Ok(applied) => Ok(Ok(applied)),
                Err(RunError::Store(err)) => Err(err),
                Err(refusal) => Ok(Err(refusal)),
            }
        });
        match result {
            Ok(inner) => inner,
            Err(err) => Err(RunError::Store(err)),
        }
    }

    /// The budget read of one Run (read-only connection).
    pub(crate) fn run_limits_view(&self, run_id: &RunId) -> Result<RunLimitsView, RunError> {
        let instance = self
            .store
            .snapshot(|store| store.run_record(run_id))?
            .and_then(|record| record.instance);
        let inputs = instance
            .as_deref()
            .map(|instance| self.try_resolve_inputs_for(instance));
        self.store
            .snapshot(|store| Ok(limits::limits_view(store, run_id, inputs)))?
    }
}
