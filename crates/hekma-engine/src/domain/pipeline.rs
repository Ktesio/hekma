//! The AD-7 pipeline as store-writer jobs (story 21-1, spine AD-27).
//!
//! AD-7 says one pipeline commits usage, evaluates budgets in the same commit
//! path and records breaches; AD-27 moves it into the store writer. These are
//! the jobs the writer runs for it. Each runs inside ONE batch SAVEPOINT, so
//! everything it writes commits together or not at all:
//!
//! * [`commit_usage`] — the usage row (dedup, launch generation), both
//!   rollups, the usage outbox row, then [`evaluate`].
//! * [`evaluate`] — read the just-committed totals from the rollups and the
//!   Run's latches, run the pure [`governance::decide`], and for each
//!   decision write the breach row, the latch row and the breach outbox row.
//!   A `block` decision registers a post-commit hook that raises the
//!   instance's wire fence before the caller is acked (the writer sets the
//!   flag, AD-7's "`block` before the shared proxy").
//!
//! The evaluator's inputs (resolved budget, Rate, cap, actions, the fence)
//! travel in the job (D2: the writer reads no engine state). The caller gets
//! the committed events back in commit order and executes pause / stop through
//! the normal lifecycle before releasing its locks.

use super::budget::{BreachAction, TokenBudget};
use super::bus::EngineEvent;
use super::cost::{CostCap, EstimateLabel, Micros, Rate};
use super::event::{BreachDimension, BudgetBreachEvent};
use super::governance::{self, GovernanceDecision, LatchKey};
use super::name::InstanceName;
use super::outbox::OUTBOX_MAX_BYTES;
use super::usage::{RecordOutcome, RunId, UsageEvent, UsageUpdateEvent};
use crate::metering::WireFence;
use crate::ports::{OutboxAppend, Rollup, RunStore, StoreError};
use crate::store::WriterCtx;
use crate::time::now_rfc3339;

/// Everything [`evaluate`] needs, resolved by the caller under its locks.
#[derive(Clone, Debug)]
pub(crate) struct EvalInput {
    pub name: InstanceName,
    pub run_id: RunId,
    pub metering_source: String,
    pub token_budget: TokenBudget,
    pub token_action: BreachAction,
    pub rate: Option<Rate>,
    pub cost_cap: CostCap,
    pub cost_action: BreachAction,
    /// Whether the instance is currently supervised. An unsupervised
    /// instance commits its usage but fires nothing (the pre-21-1
    /// `apply_breach` declined the same way).
    pub supervised: bool,
    /// The instance's wire fence, when it has a metering listener.
    pub fence: Option<WireFence>,
    /// cfg(test) seam (19-1 review round 1): fail every totals read so the
    /// degrade-to-zero path is reachable.
    #[cfg(test)]
    pub totals_fault: bool,
}

/// One breach the job committed, with the decision that fired it.
#[derive(Clone, Debug)]
pub(crate) struct CommittedBreach {
    pub event: BudgetBreachEvent,
    pub decision: GovernanceDecision,
    pub outbox: OutboxAppend,
}

/// What [`evaluate`] committed, plus the totals reads that degraded to zero
/// (phrased for the caller's diagnostic surface — never silent, AI-18).
#[derive(Clone, Debug, Default)]
pub(crate) struct Evaluation {
    pub breaches: Vec<CommittedBreach>,
    pub degraded_reads: Vec<String>,
}

/// One usage event for [`commit_usage`].
#[derive(Clone, Debug)]
pub(crate) struct UsageJob {
    pub event: UsageEvent,
    pub generation: u64,
    pub eval: EvalInput,
}

/// What [`commit_usage`] committed.
#[derive(Clone, Debug)]
pub(crate) struct UsageCommit {
    /// The usage event and its outbox append, for a fresh row.
    pub update: Option<(UsageUpdateEvent, OutboxAppend)>,
    pub evaluation: Evaluation,
}

/// Commit one usage event and evaluate the budgets on the new totals.
pub(crate) fn commit_usage(
    ctx: &mut WriterCtx<'_>,
    job: UsageJob,
) -> Result<UsageCommit, StoreError> {
    let outcome = ctx
        .store()
        .insert_usage(&job.event, job.eval.rate, job.generation)?;
    if outcome == RecordOutcome::DuplicateReplay {
        // Nothing new committed: no event, no evaluation (AC-A).
        return Ok(UsageCommit {
            update: None,
            evaluation: Evaluation::default(),
        });
    }
    let update = UsageUpdateEvent::new(job.event);
    let append = ctx
        .store()
        .append_outbox(&EngineEvent::UsageUpdate(update.clone()), OUTBOX_MAX_BYTES)?;
    let evaluation = evaluate(ctx, &job.eval)?;
    Ok(UsageCommit {
        update: Some((update, append)),
        evaluation,
    })
}

/// Evaluate an instance's budgets for one Run and commit every breach that
/// fires (see the module docs).
///
/// The latch is keyed by action (story 21-1): a `(dimension, scope)` is
/// spent for the Run only under the action that recorded it, so a breach
/// re-fires once when the configured action changes — live and after an
/// adoption alike.
pub(crate) fn evaluate(
    ctx: &mut WriterCtx<'_>,
    input: &EvalInput,
) -> Result<Evaluation, StoreError> {
    let mut evaluation = Evaluation::default();
    if !input.supervised {
        return Ok(evaluation);
    }
    let token_armed = input.token_budget.is_set();
    let dollar_armed = input.rate.is_some() && input.cost_cap.is_set();
    if !token_armed && !dollar_armed {
        return Ok(evaluation);
    }
    let store = ctx.store();
    let (run, cumulative) = {
        let mut read = |scope: &str, result: Result<Rollup, StoreError>| match result {
            Ok(rollup) => rollup,
            Err(err) => {
                evaluation.degraded_reads.push(format!(
                    "the {scope} totals read failed — enforcement degrades it to zero and \
                     decides on the degraded total (a zero ceiling still fires on it): {err}"
                ));
                Rollup::zero()
            }
        };
        #[cfg(test)]
        let (run_read, cumulative_read) = if input.totals_fault {
            let fault = || {
                Err(StoreError::Backend(
                    "injected totals-read fault (test seam)".to_string(),
                ))
            };
            (fault(), fault())
        } else {
            (
                store.run_rollup(&input.name, &input.run_id),
                store.instance_rollup(&input.name),
            )
        };
        #[cfg(not(test))]
        let (run_read, cumulative_read) = (
            store.run_rollup(&input.name, &input.run_id),
            store.instance_rollup(&input.name),
        );
        (
            read("per-run", run_read),
            read("cumulative", cumulative_read),
        )
    };
    let token = token_armed.then_some(governance::TokenArm {
        run_total: run.totals.total_tokens(),
        cumulative_total: cumulative.totals.total_tokens(),
        budget: input.token_budget,
        action: input.token_action,
    });
    let dollar = dollar_armed.then_some(governance::DollarArm {
        run_cost: run.cost,
        cumulative_cost: cumulative.cost,
        cap: input.cost_cap,
        action: input.cost_action,
    });
    let latched: Vec<LatchKey> = store
        .latches(&input.run_id)?
        .into_iter()
        .filter(|(key, action)| *action == configured_action(input, key.dimension))
        .map(|(key, _action)| key)
        .collect();
    let outcome = governance::decide(&governance::GovernanceInput {
        token,
        dollar,
        latched,
    });
    for decision in outcome.decisions {
        let event = breach_event(input, &decision);
        let key = LatchKey::new(decision.dimension, decision.scope);
        store.append_breach(&event)?;
        store.latch(&input.run_id, key, decision.action, &event.at)?;
        let outbox =
            store.append_outbox(&EngineEvent::BudgetBreach(event.clone()), OUTBOX_MAX_BYTES)?;
        if decision.action == BreachAction::Block {
            if let Some(fence) = input.fence.clone() {
                ctx.after_commit(move || fence.raise(key));
            }
        }
        evaluation.breaches.push(CommittedBreach {
            event,
            decision,
            outbox,
        });
    }
    Ok(evaluation)
}

/// The Breach Action configured for a dimension.
fn configured_action(input: &EvalInput, dimension: BreachDimension) -> BreachAction {
    match dimension {
        BreachDimension::Tokens => input.token_action,
        BreachDimension::Dollars => input.cost_action,
    }
}

/// Build the breach record for one decision (token counts, or integer
/// micro-dollars plus the estimate label — never a `$` string, AD-14).
fn breach_event(input: &EvalInput, decision: &GovernanceDecision) -> BudgetBreachEvent {
    match decision.dimension {
        BreachDimension::Tokens => BudgetBreachEvent::new(
            input.name.as_str(),
            input.run_id.as_str(),
            decision.scope,
            decision.limit,
            decision.observed,
            decision.action,
            &input.metering_source,
            now_rfc3339(),
        ),
        BreachDimension::Dollars => BudgetBreachEvent::new_cost(
            input.name.as_str(),
            input.run_id.as_str(),
            decision.scope,
            Micros(decision.limit as i64),
            Micros(decision.observed as i64),
            EstimateLabel::Estimated,
            decision.action,
            &input.metering_source,
            now_rfc3339(),
        ),
    }
}

#[cfg(test)]
/// Record one already-built breach event the way [`evaluate`] does — the
/// breach row, the latch under its action and the outbox row — for the
/// paths that record a breach outside a usage commit (tests seed history
/// through it).
pub(crate) fn record_breach(
    ctx: &mut WriterCtx<'_>,
    event: &BudgetBreachEvent,
) -> Result<OutboxAppend, StoreError> {
    let store = ctx.store();
    store.append_breach(event)?;
    store.latch(
        &RunId::from_wire(event.run_id.clone()),
        LatchKey::new(event.dimension, event.scope),
        event.action,
        &event.at,
    )?;
    store.append_outbox(&EngineEvent::BudgetBreach(event.clone()), OUTBOX_MAX_BYTES)
}
