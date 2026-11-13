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
//!   A `block` decision's latch republishes the Run's snapshot entry, whose
//!   disposition the shared proxy reads before the caller is acked (AD-7's
//!   "`block` before the shared proxy", story 23-1).
//!
//! The evaluator's inputs (resolved budget, Rate, cap, actions)
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
use crate::ports::{OutboxAppend, Rollup, RunRecords, RunStore, StoreError};
use crate::store::WriterCtx;
use crate::time::now_rfc3339;

/// Everything [`evaluate`] needs, resolved by the caller under its locks.
#[derive(Clone, Debug)]
pub(crate) struct EvalInput {
    /// The hosting Agent Instance; `None` for a Run with no instance (a
    /// wire Run of the shared proxy, story 23-1), which has no cumulative
    /// scope and no process level.
    pub name: Option<InstanceName>,
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
    /// The Run CHANGES the breaches caused (events + snapshot entries),
    /// returned whether or not they were committed here: a caller that
    /// folds them into its own job (the policy-change job, which passes
    /// `runs: None`) takes them through [`Evaluation::take_run_changes`]
    /// so ONE commit publishes the freshest entries in order.
    pub run_changes: RunChanges,
}

impl Evaluation {
    /// Take the run changes (clearing them here) — for the caller that
    /// commits them in its own job.
    pub(crate) fn take_run_changes(&mut self) -> RunChanges {
        std::mem::take(&mut self.run_changes)
    }
}

/// One usage event for [`commit_usage`].
#[derive(Clone, Debug)]
pub(crate) struct UsageJob {
    pub event: UsageEvent,
    pub generation: u64,
    pub eval: EvalInput,
    /// The Rate the ROW prices at when it differs from the evaluator's
    /// (story 21-3, review round 2): usage attributed to a CHILD Run
    /// prices at the child's own resolved Rate (its Policy's flat Rate),
    /// while the evaluator's Rate stays the process-owning Run's. `None`
    /// prices at `eval.rate` (the owner's own usage).
    pub row_rate: Option<Rate>,
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
    let row_rate = job.row_rate.or(job.eval.rate);
    let outcome = ctx
        .store()
        .insert_usage(&job.event, row_rate, job.generation)?;
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
/// Story 21-3 (review round 1): a child level's own arms come from its own
/// stored Policy resolution (`run_policy`) when one exists — a Policy whose
/// Selector matches the child Run arms ITS level, not only the process
/// level — falling back to its `run_limits` when no resolution is stored
/// yet. The resolution's per-run ceilings, Rate and (for a Policy-sourced
/// action) effective action after the depth fallback ride the level.
///
/// The latch is keyed by action (story 21-1): a `(dimension, scope)` is
/// spent for the Run only under the action that recorded it, so a breach
/// re-fires once when the configured action changes — live and after an
/// adoption alike.
///
/// The Run changes the breaches cause are returned in
/// [`Evaluation::run_changes`] AND committed here (outbox + snapshot) when
/// `input.runs` is set; a caller that folds them into its OWN job's changes
/// passes `runs: None` and commits them itself — one commit, one publish
/// order (the policy-change job's pattern).
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
            // Story 23-1: the entry carries the Run's disposition (its
            // `block` and latched reasons) — republish it even when the
            // `breach` Anomaly was already set, so the proxy refuses the
            // next call before this batch acks.
            if let Some(entry) = runs::ops::refreshed_entry(store, &level.run_id)? {
                run_changes.entries.push(entry);
            }
        }
    }
    evaluation.run_changes = run_changes;
    if let Some(publisher) = &input.runs {
        evaluation.run_events = runs::ops::commit(ctx, publisher, evaluation.take_run_changes())?;
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
    /// A child level's own resolution-derived per-run ceilings (story
    /// 21-3): the Policy resolution's per-run arms when a `run_policy` row
    /// exists, `None` per field otherwise.
    resolved: Option<ResolvedArms>,
    /// A child level's action (`warn` when its own cannot execute).
    child_action: BreachAction,
}

/// A child level's resolution-derived arms (story 21-3).
struct ResolvedArms {
    token_per_run: Option<u64>,
    dollar_per_run_micros: Option<u64>,
    /// The child's own Rate (a Policy flat Rate), when its resolution
    /// carries one.
    rate: Option<Rate>,
    /// The effective action for a POLICY-sourced action (the depth
    /// fallback already applied); `None` when the action is set directly
    /// (the 21-2 loud-degrade contract keeps the configured action).
    policy_action: Option<BreachAction>,
    /// A Caller's tighter action (story 21-6): set directly, so it keeps
    /// the depth check below like the Run's own action.
    caller_action: Option<BreachAction>,
}

impl Level {
    fn is_armed(&self) -> bool {
        self.process
            || self.limits.is_armed()
            || self.resolved.as_ref().is_some_and(|arms| {
                arms.token_per_run.is_some() || arms.dollar_per_run_micros.is_some()
            })
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
        // placeholder row gives its latches their reference. An instance-
        // less id with no row governs nothing.
        let Some(name) = &input.name else {
            return Ok(Vec::new());
        };
        store.ensure_run_row(&input.run_id, name)?;
        return Ok(vec![Level {
            run_id: input.run_id.clone(),
            process: true,
            acts: true,
            limits: RunLimits::default(),
            resolved: None,
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
                resolved: None,
                child_action: BreachAction::Warn,
            });
            // The process-owning Run is the top of what this process
            // governs; nothing above it applies here.
            break;
        }
        let limits = store.run_limits(&record.run_id)?;
        // Story 21-3: the child's own stored resolution arms its level —
        // a Policy matching the child Run supplies ITS ceilings, Rate and
        // (policy-sourced) effective action. Only RUN- and POLICY-sourced
        // arms ride the child level: the INSTANCE's explicit ceilings arm
        // the process level alone (the owner's resolution, in the
        // EvalInput) — a child level enforcing them too would fire the
        // instance ceiling once per level.
        // A Caller's tighter limit (story 21-6) also arms from the stored
        // resolution: it is not in the Run's own (Operator/Host) row.
        let policy_sourced = |source: &crate::domain::runs::LimitSource| {
            matches!(
                source,
                crate::domain::runs::LimitSource::Policy(_)
                    | crate::domain::runs::LimitSource::Default
                    | crate::domain::runs::LimitSource::Caller
            )
        };
        let resolution = store.run_policy(&record.run_id)?.map(|row| {
            let token_per_run = policy_sourced(&row.resolution.sources.tokens_per_run)
                .then_some(row.resolution.tokens.per_run)
                .flatten();
            let dollar_per_run_micros = policy_sourced(&row.resolution.sources.dollars_per_run)
                .then(|| {
                    row.resolution
                        .dollars
                        .per_run
                        .map(|micros| micros.get() as u64)
                })
                .flatten();
            let rate = policy_sourced(&row.resolution.sources.rate)
                .then_some(row.resolution.rate)
                .flatten();
            let action_source = &row.resolution.sources.breach_action;
            let caller_action = (*action_source == crate::domain::runs::LimitSource::Caller)
                .then_some(row.resolution.action);
            let policy_action = (caller_action.is_none() && policy_sourced(action_source))
                .then_some(row.resolution.effective_action);
            ResolvedArms {
                token_per_run,
                dollar_per_run_micros,
                rate,
                policy_action,
                caller_action,
            }
        });
        let action = resolution
            .as_ref()
            .and_then(|arms| arms.policy_action.or(arms.caller_action))
            .unwrap_or_else(|| limits.action());
        let child_action = if resolution
            .as_ref()
            .and_then(|arms| arms.policy_action)
            .is_some()
        {
            // A policy-sourced action already fell back at resolution.
            action
        } else if runs::ops::action_refusal(action, record.control_depth, record.wire_path)
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
            resolved: resolution,
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
        let arms = level.resolved.as_ref();
        (
            TokenBudget {
                per_run: arms
                    .and_then(|arms| arms.token_per_run)
                    .or(level.limits.token_per_run),
                cumulative: None,
            },
            level.child_action,
            CostCap {
                per_run: arms
                    .and_then(|arms| arms.dollar_per_run_micros)
                    .map(|micros| Micros(micros as i64))
                    .or(level.limits.dollar_per_run_micros),
                cumulative: None,
            },
            level.child_action,
        )
    };
    // A child level's dollar arm prices through its OWN Rate when its
    // resolution carries one (a Policy flat Rate), else the instance-level
    // Rate that prices its rows.
    let rate = level
        .resolved
        .as_ref()
        .and_then(|arms| arms.rate)
        .or(input.rate);
    let token_armed = token_budget.is_set();
    let dollar_armed = rate.is_some() && cost_cap.is_set();
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
    // Story 23-1 (v18): the Run rollup is keyed by run id alone; the
    // cumulative scope binds an Agent Instance, so an instance-less chain
    // reads none.
    let instance_read = || match &input.name {
        Some(name) => store.instance_rollup(name),
        None => Ok(Rollup::zero()),
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
        (store.run_rollup_by_id(&level.run_id), instance_read())
    };
    #[cfg(not(test))]
    let (run_read, cumulative_read) = (store.run_rollup_by_id(&level.run_id), instance_read());
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
    let instance = input
        .name
        .as_ref()
        .map(|name| name.as_str())
        .unwrap_or_default();
    let event = match decision.dimension {
        BreachDimension::Tokens => BudgetBreachEvent::new(
            instance,
            run_id.as_str(),
            decision.scope,
            decision.limit,
            decision.observed,
            decision.action,
            &input.metering_source,
            now_rfc3339(),
        ),
        BreachDimension::Dollars => BudgetBreachEvent::new_cost(
            instance,
            run_id.as_str(),
            decision.scope,
            Micros(decision.limit as i64),
            Micros(decision.observed as i64),
            EstimateLabel::Estimated,
            decision.action,
            &input.metering_source,
            now_rfc3339(),
        ),
    };
    if input.name.is_some() {
        event
    } else {
        event.without_instance()
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
    if let Some(Ok(instance)) = event.instance.as_deref().map(InstanceName::new) {
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

// ---------------------------------------------------------------------------
// Story 23-1: the shared Metering Proxy's per-call jobs.
// ---------------------------------------------------------------------------

/// The usage a proxied call's response reported (the OpenAI / Anthropic
/// JSON and SSE parse seams).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProxiedUsage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

/// A proxied call's close: its record and, when it reported one, its usage.
#[derive(Clone, Debug)]
pub(crate) struct ProxiedClose {
    pub record: crate::domain::calls::CallRecord,
    pub usage: Option<ProxiedUsage>,
    /// Story 22-3: the call was attributed without a credential Hekma
    /// issued while its Run's chain resolved `shadow` — the close raises
    /// the `unscented` Anomaly on the attributed Run (set once).
    pub unscented: bool,
}

/// What one committed proxied close hands the Supervisor (through the
/// evaluation inbox, drained on the reaper tick): the usage event to
/// publish, then the evaluation exactly as `apply_evaluation` takes it.
#[derive(Debug)]
pub(crate) struct ProxiedCommit {
    pub instance: Option<InstanceName>,
    pub update: (UsageUpdateEvent, OutboxAppend),
    pub evaluation: Evaluation,
}

/// The Supervisor's evaluation inbox (story 23-1): a writer post-commit
/// hook pushes each committed proxied evaluation (cheap, no engine lock);
/// the reaper tick drains it in commit order and publishes and acts under
/// its locks.
#[derive(Debug)]
pub(crate) struct EvaluationInbox {
    tx: std::sync::Mutex<std::sync::mpsc::Sender<InboxItem>>,
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<InboxItem>>,
}

/// One inbox item, in commit order.
#[derive(Debug)]
pub(crate) enum InboxItem {
    /// A committed proxied close's usage and evaluation.
    Proxied(Box<ProxiedCommit>),
    /// Run events a proxy job committed (a wire Run's `created`).
    RunEvents(Vec<runs::ops::CommittedRunEvent>),
}

impl Default for EvaluationInbox {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx: std::sync::Mutex::new(tx),
            rx: std::sync::Mutex::new(rx),
        }
    }
}

impl EvaluationInbox {
    /// Push one committed evaluation (a post-commit hook).
    pub(crate) fn push(&self, commit: ProxiedCommit) {
        let tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        let _ = tx.send(InboxItem::Proxied(Box::new(commit)));
    }

    /// Push the Run events a proxy job committed.
    pub(crate) fn push_run_events(&self, events: Vec<runs::ops::CommittedRunEvent>) {
        if events.is_empty() {
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
        let _ = tx.send(InboxItem::RunEvents(events));
    }

    /// Take everything queued, in push (commit) order.
    pub(crate) fn drain(&self) -> Vec<InboxItem> {
        let rx = self.rx.lock().unwrap_or_else(|p| p.into_inner());
        rx.try_iter().collect()
    }
}

/// The call-start job (unacked): the `open_calls` row.
pub(crate) fn record_call_start(
    ctx: &mut WriterCtx<'_>,
    start: &crate::domain::calls::CallStart,
) -> Result<(), StoreError> {
    ctx.store().insert_open_call(start)
}

/// The Rate a proxied row prices at (story 23-1, D2 and "pricing wire
/// Runs"): a Run under a supervised process keeps its process-owning Run's
/// instance-resolved Rate (the instance's configured model); a Run with no
/// process owner prices at its own resolution's Rate (a Policy flat Rate)
/// if one resolves, else the default Policy's model-keyed entry for the
/// call's model; neither → unpriced.
fn proxied_row_rate(
    store: &crate::store::SqliteStore,
    own: Option<&crate::domain::PolicyResolution>,
    owner: Option<&crate::domain::PolicyResolution>,
    model: Option<&str>,
) -> Result<Option<Rate>, StoreError> {
    use crate::ports::PolicyRecords as _;
    if let Some(owner) = owner {
        // A child of a supervised Run prices at its own Policy Rate when
        // one resolves, as `commit_usage`'s `row_rate` does.
        return Ok(own.and_then(|own| own.rate).or(owner.rate));
    }
    if let Some(rate) = own.and_then(|own| own.rate) {
        return Ok(Some(rate));
    }
    let Some(model) = model else {
        return Ok(None);
    };
    Ok(store
        .policy(super::runs::DEFAULT_POLICY_NAME)?
        .and_then(|policy| policy.rates.get(model).copied()))
}

/// The proxied close job (unacked): the calls-store record and the
/// `open_calls` delete, then — when the call reported usage — the usage row
/// keyed (run, call id), both rollups, the outbox row and the evaluation
/// over the Run's ancestor chain, all in this job's SAVEPOINT. The
/// evaluator's inputs come from state the writer owns (D2): the process-
/// owning ancestor's stored resolution for the process level, each level's
/// own stored resolution for its own. A replayed close changes nothing: the
/// usage row's (run, call id) key dedups it. An existing calls-store record
/// alone does not prove the usage landed, because the calls store is its
/// own file and a commit is atomic per file, so the job goes on to the
/// usage row either way. The post-commit hooks republish the changed
/// entries (the `block` reasons ride them) and push the committed
/// evaluation into the Supervisor's inbox.
pub(crate) fn commit_proxied_call(
    ctx: &mut WriterCtx<'_>,
    close: ProxiedClose,
    runs: &Arc<SnapshotPublisher>,
    inbox: &Arc<EvaluationInbox>,
    leases: &runs::lease::LeaseOptions,
) -> Result<(), StoreError> {
    use crate::ports::PolicyRecords as _;
    let store = ctx.store();
    store.insert_call_record(&close.record)?;
    // Story 23-2: every identified call is activity on its Run — a refused
    // one too (a client still trying is alive): `last_activity_at` moves,
    // a held Lease extends, a `silent` Run returns to `active`.
    {
        let mut changes = RunChanges::default();
        let now = crate::time::now_unix_secs();
        runs::lease::note_wire_call(store, &mut changes, &close.record.run_id, now)?;
        runs::lease::note_activity(
            store,
            &mut changes,
            &close.record.run_id,
            leases,
            now,
            false,
        )?;
        if !changes.events.is_empty() || !changes.entries.is_empty() {
            let events = runs::ops::commit(ctx, runs, changes)?;
            let inbox = Arc::clone(inbox);
            ctx.after_commit(move || inbox.push_run_events(events));
        }
    }
    let store = ctx.store();
    // Story 22-3: the first unscented call under `shadow` raises the
    // `unscented` Anomaly on the attributed Run, in this transaction (one
    // `anomalies-changed` event; further calls are no-ops).
    if close.unscented {
        let mut changes = RunChanges::default();
        if let Some(record) = store.run_chain(&close.record.run_id)?.first().cloned() {
            if !record.is_ended() && !record.anomalies.contains(&runs::Anomaly::UnscentedCall) {
                let mut anomalies = record.anomalies.clone();
                anomalies.push(runs::Anomaly::UnscentedCall);
                let status = record.status;
                runs::ops::move_to(
                    store,
                    &mut changes,
                    record,
                    status,
                    anomalies,
                    None,
                    &crate::time::now_rfc3339(),
                )?;
            }
        }
        if !changes.events.is_empty() || !changes.entries.is_empty() {
            let events = runs::ops::commit(ctx, runs, changes)?;
            let inbox = Arc::clone(inbox);
            ctx.after_commit(move || inbox.push_run_events(events));
        }
    }
    let store = ctx.store();
    let Some(usage) = close.usage else {
        return Ok(());
    };
    let run_id = close.record.run_id.clone();
    let chain = store.run_chain(&run_id)?;
    let Some(record) = chain.first().cloned() else {
        // The Run's row is not committed yet (a call on a pending Run route
        // that beat its launch job): fail the job so the record pump
        // re-queues it — the record and the usage commit together, later.
        return Err(StoreError::Backend(format!(
            "the Run {} of call {} is not recorded yet",
            run_id.as_str(),
            close.record.call_id
        )));
    };
    let instance = record
        .instance
        .as_deref()
        .and_then(|name| InstanceName::new(name).ok());
    let owner = chain.iter().find(|run| run.process_owner);
    let owner_resolution = match owner {
        Some(owner) => store.run_policy(&owner.run_id)?.map(|row| row.resolution),
        None => None,
    };
    let own_resolution = store.run_policy(&run_id)?.map(|row| row.resolution);
    let rate = proxied_row_rate(
        store,
        own_resolution.as_ref(),
        owner_resolution.as_ref().filter(|_| owner.is_some()),
        close.record.model.as_deref(),
    )?;
    let generation = match &instance {
        Some(name) => store.launch_generation(name).unwrap_or(0),
        None => 0,
    };
    let event = UsageEvent {
        instance: instance.as_ref().map(|name| name.as_str().to_string()),
        run_id: run_id.clone(),
        call_id: Some(close.record.call_id.clone()),
        input_tokens: usage.input,
        output_tokens: usage.output,
        cached_tokens: Some(usage.cached.min(usage.input)),
        metering_source: "engine-observed".to_string(),
        sequence: 0,
        occurred_at: close.record.ended_at.clone(),
    };
    if store.insert_usage(&event, rate, generation)? == RecordOutcome::DuplicateReplay {
        return Ok(());
    }
    let update = UsageUpdateEvent::new(event);
    let append =
        store.append_outbox(&EngineEvent::UsageUpdate(update.clone()), OUTBOX_MAX_BYTES)?;
    // The process level's arms: the process-owning ancestor's stored
    // resolution (none for an instance-less chain — its levels arm from
    // their own resolutions).
    let (token_budget, cost_cap, action) = match &owner_resolution {
        Some(arms) => (arms.tokens, arms.dollars, arms.effective_action),
        None => (
            TokenBudget::default(),
            CostCap::default(),
            BreachAction::Warn,
        ),
    };
    let input = EvalInput {
        name: owner.and(instance.clone()),
        run_id: run_id.clone(),
        metering_source: "engine-observed".to_string(),
        token_budget,
        token_action: action,
        rate: owner_resolution.as_ref().and_then(|r| r.rate),
        cost_cap,
        cost_action: action,
        supervised: true,
        runs: Some(Arc::clone(runs)),
        #[cfg(test)]
        totals_fault: false,
    };
    let evaluation = evaluate(ctx, &input)?;
    let inbox = Arc::clone(inbox);
    let commit = ProxiedCommit {
        instance,
        update: (update, append),
        evaluation,
    };
    ctx.after_commit(move || inbox.push(commit));
    Ok(())
}
