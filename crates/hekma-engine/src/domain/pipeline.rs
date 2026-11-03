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

use std::sync::Arc;

use super::budget::{BreachAction, TokenBudget};
use super::bus::EngineEvent;
use super::cost::{CostCap, EstimateLabel, Micros, Rate};
use super::event::{BreachDimension, BudgetBreachEvent};
use super::governance::{self, GovernanceDecision, LatchKey};
use super::name::InstanceName;
use super::outbox::OUTBOX_MAX_BYTES;
use super::runs::RunLimits;
use super::runs::{self, ops::CommittedRunEvent, ops::RunChanges, snapshot::SnapshotPublisher};
use super::usage::{RecordOutcome, RunId, UsageEvent, UsageUpdateEvent};
use crate::metering::WireFence;
use crate::ports::{OutboxAppend, Rollup, RunRecords, RunStore, StoreError};
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
    /// The Run snapshot publisher (story 21-2): a breach raises its Run's
    /// `breach` Anomaly, which republishes the Run. `None` only in writer
    /// tests that build an input by hand.
    pub runs: Option<Arc<SnapshotPublisher>>,
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
    /// Whether the breach is the process-owning Run's (story 21-2): only
    /// those execute a process Breach Action; a child Run's breach is a
    /// `warn`.
    pub process_level: bool,
}

/// What [`evaluate`] committed, plus the totals reads that degraded to zero
/// (phrased for the caller's diagnostic surface — never silent, AI-18).
#[derive(Clone, Debug, Default)]
pub(crate) struct Evaluation {
    pub breaches: Vec<CommittedBreach>,
    pub degraded_reads: Vec<String>,
    /// The Run events the breaches caused (the `breach` Anomaly), committed
    /// after the breach rows (story 21-2).
    pub run_events: Vec<CommittedRunEvent>,
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

/// Evaluate the budgets over the attributed Run's ancestor chain and commit
/// every breach that fires (see the module docs).
///
/// Story 21-2 (ancestor ceilings, AD-25): the levels run from the attributed
/// Run up to the process-owning Run. Each level decides on its OWN subtree
/// totals (the rollups fold every row into every ancestor) with its OWN
/// latches; the process-owning level carries the instance's configured
/// ceilings and the cumulative scope, a child level the ceilings set on it
/// (per-run only). [`governance::decide_chain`] is the pure core, deepest
/// level first, token before dollar within a level. A breach records on its
/// level's Run under its level's action and raises that Run's `breach`
/// Anomaly; only a process-level breach executes a process action.
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
    let store = ctx.store();
    let mut levels = levels_for(store, input)?;
    levels.retain(Level::is_armed);
    if levels.is_empty() {
        return Ok(evaluation);
    }
    let mut inputs = Vec::with_capacity(levels.len());
    for level in &levels {
        inputs.push(level_input(
            store,
            input,
            level,
            &mut evaluation.degraded_reads,
        )?);
    }
    let outcomes = governance::decide_chain(&inputs);
    let now = now_rfc3339();
    let mut run_changes = RunChanges::default();
    for (level, outcome) in levels.iter().zip(outcomes) {
        for decision in outcome.decisions {
            let event = breach_event(input, &level.run_id, &decision);
            let key = LatchKey::new(decision.dimension, decision.scope);
            store.append_breach(&event)?;
            store.latch(&level.run_id, key, decision.action, &event.at)?;
            let outbox =
                store.append_outbox(&EngineEvent::BudgetBreach(event.clone()), OUTBOX_MAX_BYTES)?;
            if level.acts && decision.action == BreachAction::Block {
                if let Some(fence) = input.fence.clone() {
                    ctx.after_commit(move || fence.raise(key));
                }
            }
            evaluation.breaches.push(CommittedBreach {
                event,
                decision,
                outbox,
                process_level: level.acts,
            });
        }
        if evaluation
            .breaches
            .iter()
            .any(|breach| breach.event.run_id == level.run_id.as_str())
        {
            let changes = runs::ops::mark_breach(store, &level.run_id, &now)?;
            run_changes.events.extend(changes.events);
            run_changes.entries.extend(changes.entries);
        }
    }
    if let Some(publisher) = &input.runs {
        evaluation.run_events = runs::ops::commit(ctx, publisher, run_changes)?;
    }
    Ok(evaluation)
}

/// One level of the ancestor chain.
struct Level {
    run_id: RunId,
    /// The process-owning Run (instance ceilings + the cumulative scope).
    process: bool,
    /// Whether a breach at this level may act on the process: only the
    /// instance's open process-owning Run's may. An ended owner (a Run a
    /// fresh start replaced) records its breach under its action but acts
    /// on nothing — never on the process that replaced it.
    acts: bool,
    /// A child level's own ceilings.
    limits: RunLimits,
    /// A child level's action (`warn` when its own cannot execute).
    child_action: BreachAction,
}

impl Level {
    fn is_armed(&self) -> bool {
        self.process || self.limits.is_armed()
    }
}

/// The levels from the attributed Run up to its process-owning Run. A run id
/// with no Run row (a legacy id) is one process level, as before Runs.
fn levels_for<S: RunRecords + ?Sized>(
    store: &S,
    input: &EvalInput,
) -> Result<Vec<Level>, StoreError> {
    let chain = store.run_chain(&input.run_id)?;
    if chain.is_empty() {
        // A run id with no Run row: one process level, as before Runs; the
        // placeholder row gives its latches their reference.
        store.ensure_run_row(&input.run_id, &input.name)?;
        return Ok(vec![Level {
            run_id: input.run_id.clone(),
            process: true,
            acts: true,
            limits: RunLimits::default(),
            child_action: BreachAction::Warn,
        }]);
    }
    let mut levels = Vec::with_capacity(chain.len());
    for record in chain {
        if record.process_owner {
            levels.push(Level {
                acts: !record.is_ended(),
                run_id: record.run_id,
                process: true,
                limits: RunLimits::default(),
                child_action: BreachAction::Warn,
            });
            // The process-owning Run is the top of what this process
            // governs; nothing above it applies here.
            break;
        }
        let limits = store.run_limits(&record.run_id)?;
        let action = limits.action();
        let child_action = if runs::ops::action_refusal(
            action,
            record.control_depth,
            record.wire_path,
        )
        .is_some()
        {
            BreachAction::Warn
        } else {
            action
        };
        levels.push(Level {
            run_id: record.run_id,
            process: false,
            acts: false,
            limits,
            child_action,
        });
    }
    Ok(levels)
}

/// Build one level's decision input: its arms over its subtree totals and
/// its latches under its configured actions.
fn level_input(
    store: &crate::store::SqliteStore,
    input: &EvalInput,
    level: &Level,
    degraded: &mut Vec<String>,
) -> Result<governance::GovernanceInput, StoreError> {
    let (token_budget, token_action, cost_cap, cost_action) = if level.process {
        (
            input.token_budget,
            input.token_action,
            input.cost_cap,
            input.cost_action,
        )
    } else {
        (
            TokenBudget {
                per_run: level.limits.token_per_run,
                cumulative: None,
            },
            level.child_action,
            CostCap {
                per_run: level.limits.dollar_per_run_micros,
                cumulative: None,
            },
            level.child_action,
        )
    };
    let token_armed = token_budget.is_set();
    let dollar_armed = input.rate.is_some() && cost_cap.is_set();
    let mut read = |scope: &str, result: Result<Rollup, StoreError>| match result {
        Ok(rollup) => rollup,
        Err(err) => {
            degraded.push(format!(
                "the {scope} totals read failed — enforcement degrades it to zero and decides \
                 on the degraded total (a zero ceiling still fires on it): {err}"
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
            store.run_rollup(&input.name, &level.run_id),
            store.instance_rollup(&input.name),
        )
    };
    #[cfg(not(test))]
    let (run_read, cumulative_read) = (
        store.run_rollup(&input.name, &level.run_id),
        store.instance_rollup(&input.name),
    );
    let run = read("per-run", run_read);
    let cumulative = if level.process {
        read("cumulative", cumulative_read)
    } else {
        Rollup::zero()
    };
    let token = token_armed.then_some(governance::TokenArm {
        run_total: run.totals.total_tokens(),
        cumulative_total: cumulative.totals.total_tokens(),
        budget: token_budget,
        action: token_action,
    });
    let dollar = dollar_armed.then_some(governance::DollarArm {
        run_cost: run.cost,
        cumulative_cost: cumulative.cost,
        cap: cost_cap,
        action: cost_action,
    });
    let latched: Vec<LatchKey> = store
        .latches(&level.run_id)?
        .into_iter()
        .filter(|(key, action)| {
            *action
                == match key.dimension {
                    BreachDimension::Tokens => token_action,
                    BreachDimension::Dollars => cost_action,
                }
        })
        .map(|(key, _action)| key)
        .collect();
    Ok(governance::GovernanceInput {
        token,
        dollar,
        latched,
    })
}

/// Build the breach record for one decision (token counts, or integer
/// micro-dollars plus the estimate label — never a `$` string, AD-14).
fn breach_event(
    input: &EvalInput,
    run_id: &RunId,
    decision: &GovernanceDecision,
) -> BudgetBreachEvent {
    match decision.dimension {
        BreachDimension::Tokens => BudgetBreachEvent::new(
            input.name.as_str(),
            run_id.as_str(),
            decision.scope,
            decision.limit,
            decision.observed,
            decision.action,
            &input.metering_source,
            now_rfc3339(),
        ),
        BreachDimension::Dollars => BudgetBreachEvent::new_cost(
            input.name.as_str(),
            run_id.as_str(),
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
    if let Ok(instance) = InstanceName::new(&event.instance) {
        store.ensure_run_row(&RunId::from_wire(event.run_id.clone()), &instance)?;
    }
    store.latch(
        &RunId::from_wire(event.run_id.clone()),
        LatchKey::new(event.dimension, event.scope),
        event.action,
        &event.at,
    )?;
    store.append_outbox(&EngineEvent::BudgetBreach(event.clone()), OUTBOX_MAX_BYTES)
}
