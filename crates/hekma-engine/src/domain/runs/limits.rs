//! Run-level limits and the within-Run re-arm (story 21-6, PRD FR-50 /
//! FR-60 / FR-43, spine AD-7 / AD-25).
//!
//! * [`re_arm`] — the ONE re-arm pass, promoted from the Policy change job
//!   (story 21-3) unchanged in behavior. Every raise trigger calls it: a
//!   Policy change, a Run-level limit set through `hekma budget`, and an
//!   instance `budget.*` / `cost.rate.*` configuration write. A ceiling
//!   raised above the totals (or cleared) re-arms its latch and lifts the
//!   matching `block` fence reason PER REASON; a ceiling lowered to at or
//!   below the totals breaches AT ONCE through the same pipeline
//!   evaluation a usage commit runs. The rule itself is
//!   [`crate::domain::governance::re_armed_keys`], pinned by the vectors'
//!   `rearm` cases.
//! * the budget job ([`plan_limits`], [`apply_limits`]) — writes a Run's
//!   `run_limits` as one actor ([`LimitActor`]): validates scope and action
//!   per depth, checks the authority rule (only an Operator or Host may
//!   raise a ceiling or loosen the action; a Caller may only tighten),
//!   re-resolves, stores the resolution, commits one `limits-changed` Run
//!   event and runs the re-arm pass — one writer job per Run.
//!
//! A limit set here BELONGS TO THE RUN, like a verb label: the rows end
//! with the Run, and a fresh start mints a new Run without them.

use serde::{Deserialize, Serialize};

use super::ops::{self, RunChanges};
use super::policy::{self, ResolveInputs};
use super::{LimitSource, PolicyResolution, RunError, RunLimits, RunRecord};
use crate::domain::budget::BreachAction;
use crate::domain::cost::Micros;
use crate::domain::governance::{self, GovernanceDecision, LatchKey, LevelArms, LevelTotals};
use crate::domain::name::InstanceName;
use crate::domain::pipeline::{self, CommittedBreach, EvalInput};
use crate::metering::WireFence;
use crate::ports::{PolicyRecords, RunRecords, RunStore};
use crate::time::now_rfc3339;

/// The live per-instance context the re-arm pass's money half needs (built
/// by the caller — engine or supervisor — which holds the locks): the
/// instance's metering wire fence, its metering source for the breach
/// records, and its fresh explicit inputs. Whether the engine supervises
/// the instance is the CALLER's own fact when it applies the returned
/// process actions (D2) — an unsupervised instance's breaches still
/// record and latch here.
#[derive(Clone, Default)]
pub(crate) struct InstanceContext {
    /// The instance's live wire fence (an engine-observed instance's
    /// listener fence; `None` when it has no listener).
    pub fence: Option<WireFence>,
    /// The instance's metering source (the breach records name it).
    pub metering_source: String,
    /// Whether THIS engine supervises the instance right now: only a
    /// supervised instance's money half runs (the breach-at-once latch
    /// must be executable by the engine that records it); an unsupervised
    /// instance still gets FRESH inputs for its stored resolution.
    pub supervised: bool,
    /// The instance's explicit limits and model, read fresh from its
    /// configuration by the caller; `Err` names why they could not be read
    /// (the Run is then skipped, never resolved as if nothing were set).
    pub inputs: Option<Result<ResolveInputs, String>>,
}

/// What one re-arm pass did to its Run (the budget job reports it per Run;
/// the Policy job ignores it).
#[derive(Clone, Debug, Default)]
pub(crate) struct ReArmReport {
    /// The latch rows released, with the action each latched under.
    pub unlatched: Vec<(LatchKey, BreachAction)>,
    /// The `block` fence reasons lifted (per reason).
    pub fence_cleared: Vec<LatchKey>,
    /// The breaches the pass fired at once on THIS Run.
    pub fired: Vec<GovernanceDecision>,
}

/// The ceilings `run`'s own level enforces under `resolution` (the re-arm
/// half's arms). A CHILD level enforces only its Run- and Policy-sourced
/// PER-RUN arms (the instance's ceilings arm the process level alone — see
/// `pipeline::levels_for`), so a child's latch compares against the ceiling
/// its OWN level would enforce today. An instance-sourced ceiling that the
/// child level never arms must re-arm the latch it once held (a Policy
/// armed the child at 50, latched at 60, and the Policy's deletion left an
/// instance-sourced 40: the latch once stuck forever, because `40 > 60` is
/// false — the 21-3 review round 2 finding N14).
pub(crate) fn level_arms(run: &RunRecord, resolution: &PolicyResolution) -> LevelArms {
    let level_sourced = |source: &LimitSource| !matches!(source, LimitSource::Instance);
    let micros = |value: Micros| value.get().max(0) as u64;
    if run.process_owner {
        LevelArms {
            tokens_per_run: resolution.tokens.per_run,
            tokens_cumulative: resolution.tokens.cumulative,
            dollars_per_run_micros: resolution.dollars.per_run.map(micros),
            dollars_cumulative_micros: resolution.dollars.cumulative.map(micros),
        }
    } else {
        LevelArms {
            tokens_per_run: level_sourced(&resolution.sources.tokens_per_run)
                .then_some(resolution.tokens.per_run)
                .flatten(),
            tokens_cumulative: None,
            dollars_per_run_micros: level_sourced(&resolution.sources.dollars_per_run)
                .then(|| resolution.dollars.per_run.map(micros))
                .flatten(),
            dollars_cumulative_micros: None,
        }
    }
}

/// The committed totals `run`'s re-arm compares against: the RUN's subtree
/// totals for the per-run scopes, the INSTANCE's lifetime totals for the
/// cumulative ones (the same read the pipeline's cumulative arm uses — a
/// multi-Run instance over its cumulative cap must not unlatch on a per-Run
/// comparison).
pub(crate) fn level_totals<S: RunStore + ?Sized>(
    store: &S,
    instance: &InstanceName,
    run: &RunRecord,
) -> Result<LevelTotals, RunError> {
    let usage = store
        .run_rollup(instance, &run.run_id)
        .map_err(RunError::Store)?;
    let instance_usage = store.instance_rollup(instance).map_err(RunError::Store)?;
    Ok(LevelTotals {
        run_tokens: usage.totals.total_tokens(),
        cumulative_tokens: instance_usage.totals.total_tokens(),
        run_cost_micros: usage.cost.get().max(0) as u64,
        cumulative_cost_micros: instance_usage.cost.get().max(0) as u64,
    })
}

/// The re-arm pass over one Run (story 21-3, promoted by 21-6 — the ONE
/// implementation every raise trigger runs).
///
/// * RAISED above the totals (or UNSET — a cleared or deleted ceiling is
///   limit-free, so its latch always re-arms): the latch rows for the
///   re-armed `(dimension, scope)` clear, the `breach` Anomaly clears when
///   no latch remains (with its `anomalies-changed` event and snapshot
///   entry), and the matching `block` fence reason clears PER REASON
///   (amendment 2: only the reasons whose new ceiling now sits above the
///   totals; a fence holding any other latched `block` reason stays up).
///   The pairs come from [`governance::re_armed_keys`] (the vectors' rule).
/// * THEN lowered-to-at-or-below: `pipeline::evaluate` commits the breach
///   NOW (breach row, latch, outbox, `breach` Anomaly; the decision's
///   process action returns to the caller to apply), ONLY on an instance
///   this engine supervises (the latch must be executable by the engine
///   that records it).
///
/// The evaluation's Run changes fold into `changes` (the caller's ONE
/// commit publishes the post-breach entries last — the pre-breach entry
/// must not overwrite them), and its degraded reads surface as notes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn re_arm(
    store: &crate::store::SqliteStore,
    instance: &InstanceName,
    run: &RunRecord,
    resolution: &PolicyResolution,
    context: &InstanceContext,
    supervised: bool,
    ctx: &mut crate::store::WriterCtx<'_>,
    breaches: &mut Vec<CommittedBreach>,
    changes: &mut RunChanges,
    notes: &mut Vec<String>,
) -> Result<ReArmReport, RunError> {
    let mut report = ReArmReport::default();
    let totals = level_totals(store, instance, run)?;

    // ---- the re-arm half (raise above the totals, or unset).
    let re_armed = governance::re_armed_keys(&level_arms(run, resolution), &totals);
    let mut any_latch_left = false;
    if !re_armed.is_empty() {
        for (key, action) in store.latches(&run.run_id).map_err(RunError::Store)? {
            if re_armed.contains(&key) {
                store
                    .unlatch(&run.run_id, key, action)
                    .map_err(RunError::Store)?;
                report.unlatched.push((key, action));
                // Per-reason fence clear: only the raised reasons clear; the
                // fence stays up while any other latched `block` reason
                // holds (amendment 2).
                if action == BreachAction::Block {
                    if !report.fence_cleared.contains(&key) {
                        report.fence_cleared.push(key);
                    }
                    if let Some(fence) = &context.fence {
                        let fence = fence.clone();
                        ctx.after_commit(move || fence.clear(key));
                    }
                }
            } else {
                any_latch_left = true;
            }
        }
        if !any_latch_left {
            changes.extend(
                ops::clear_breach_anomaly(store, &run.run_id, &now_rfc3339())
                    .map_err(RunError::Store)?,
            );
        }
        // Story 23-1: the snapshot entry's disposition follows the latch
        // rows — a lifted `block` reason stops refusing at the proxy in
        // the same commit, per reason.
        if !report.unlatched.is_empty() {
            if let Some(entry) =
                ops::refreshed_entry(store, &run.run_id).map_err(RunError::Store)?
            {
                changes.entries.push(entry);
            }
        }
    }

    // ---- the breach-at-once half (lowered to at-or-below the usage),
    // ONLY on an instance this engine supervises (the latch must be
    // executable by the engine that records it):
    // the same pipeline evaluation a usage commit runs, over the NEW arms.
    // The process level's arms are the instance's (the process-owning
    // Run's resolution — the matched Run's own when it IS the owner; the
    // owner's STORED resolution when the match is a child, whose own arms
    // ride its own level inside the evaluation).
    if !supervised {
        return Ok(report);
    }
    let owner_arms = if run.process_owner {
        resolution.clone()
    } else {
        let owner_row = match store.open_process_run(instance).map_err(RunError::Store)? {
            Some(owner) => owner,
            None => {
                notes.push(format!(
                    "Run `{}` (agent `{}`) was not breach-checked: its agent has no open \
                     process-owning Run to arm the process level",
                    run.run_id.as_str(),
                    instance.as_str(),
                ));
                return Ok(report);
            }
        };
        match store
            .run_policy(&owner_row.run_id)
            .map_err(RunError::Store)?
        {
            Some(row) => row.resolution,
            None => {
                notes.push(format!(
                    "Run `{}` (agent `{}`) was not breach-checked: its agent's process-owning \
                     Run has no stored resolution to arm the process level",
                    run.run_id.as_str(),
                    instance.as_str(),
                ));
                return Ok(report);
            }
        }
    };
    let armed = supervised
        && (owner_arms.tokens.is_set()
            || (owner_arms.rate.is_some() && owner_arms.dollars.is_set())
            || resolution.tokens.per_run.is_some()
            || (resolution.rate.is_some() && resolution.dollars.per_run.is_some()));
    if armed {
        let input = EvalInput {
            name: instance.clone(),
            run_id: run.run_id.clone(),
            // A missing context (the instance not supervised here) is the
            // honest `unknown`, never a fabricated empty source.
            metering_source: if context.metering_source.is_empty() {
                "unknown".to_string()
            } else {
                context.metering_source.clone()
            },
            token_budget: owner_arms.tokens,
            token_action: owner_arms.effective_action,
            rate: owner_arms.rate,
            cost_cap: owner_arms.dollars,
            cost_action: owner_arms.effective_action,
            supervised: true,
            fence: context.fence.clone(),
            runs: None,
            #[cfg(test)]
            totals_fault: false,
        };
        let mut evaluation = pipeline::evaluate(ctx, &input).map_err(RunError::Store)?;
        report.fired.extend(
            evaluation
                .breaches
                .iter()
                .filter(|breach| breach.event.run_id == run.run_id.as_str())
                .map(|breach| breach.decision),
        );
        breaches.extend(std::mem::take(&mut evaluation.breaches));
        notes.extend(std::mem::take(&mut evaluation.degraded_reads));
        changes.extend(evaluation.take_run_changes());
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// The budget job (story 21-6): Run-level limits as one actor.
// ---------------------------------------------------------------------------

/// Who sets a Run-level limit (PRD FR-60 / FR-53). An Operator or Host may
/// raise a ceiling or loosen the Breach Action; a Caller may only TIGHTEN
/// (a change that loosens any effective limit is refused,
/// `raise-forbidden`). Every Control API credential maps to `Operator`
/// today; the Caller credential classes wire in with their own stories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitActor {
    /// The Operator (the CLI, the Control API's operator bearer).
    Operator,
    /// A Host acting on its own Runs.
    Host,
    /// The workload itself.
    Caller,
}

impl LimitActor {
    /// The stable wire form (also the `run_limits.source` and the
    /// `limits-changed` event's cause).
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitActor::Operator => "operator",
            LimitActor::Host => "host",
            LimitActor::Caller => "caller",
        }
    }
}

/// One Run-level limit a change can clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunLimitField {
    /// The per-run token ceiling.
    TokensPerRun,
    /// The cumulative token ceiling.
    TokensCumulative,
    /// The per-run dollar cap.
    DollarsPerRun,
    /// The cumulative dollar cap.
    DollarsCumulative,
    /// The Run's own Breach Action.
    BreachAction,
}

impl RunLimitField {
    /// Every field, in display order.
    pub const ALL: [RunLimitField; 5] = [
        RunLimitField::TokensPerRun,
        RunLimitField::TokensCumulative,
        RunLimitField::DollarsPerRun,
        RunLimitField::DollarsCumulative,
        RunLimitField::BreachAction,
    ];

    /// The stable wire form (`tokens-per-run`, …).
    pub fn as_str(&self) -> &'static str {
        match self {
            RunLimitField::TokensPerRun => "tokens-per-run",
            RunLimitField::TokensCumulative => "tokens-cumulative",
            RunLimitField::DollarsPerRun => "dollars-per-run",
            RunLimitField::DollarsCumulative => "dollars-cumulative",
            RunLimitField::BreachAction => "breach-action",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == raw)
    }

    fn is_set_in(&self, limits: &RunLimits) -> bool {
        match self {
            RunLimitField::TokensPerRun => limits.token_per_run.is_some(),
            RunLimitField::TokensCumulative => limits.token_cumulative.is_some(),
            RunLimitField::DollarsPerRun => limits.dollar_per_run_micros.is_some(),
            RunLimitField::DollarsCumulative => limits.dollar_cumulative_micros.is_some(),
            RunLimitField::BreachAction => limits.breach_action.is_some(),
        }
    }

    fn clear_in(&self, limits: &mut RunLimits) {
        match self {
            RunLimitField::TokensPerRun => limits.token_per_run = None,
            RunLimitField::TokensCumulative => limits.token_cumulative = None,
            RunLimitField::DollarsPerRun => limits.dollar_per_run_micros = None,
            RunLimitField::DollarsCumulative => limits.dollar_cumulative_micros = None,
            RunLimitField::BreachAction => limits.breach_action = None,
        }
    }
}

/// A change to one Run's own limits (`hekma budget set`,
/// `PATCH /v0/runs/{id}/limits`): the fields to set (a set field replaces
/// the Run's value) and the fields to clear. Merged over the Run's current
/// `run_limits`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimitChange {
    /// The limits to set.
    #[serde(default)]
    pub set: RunLimits,
    /// The limits to clear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<RunLimitField>,
}

impl RunLimitChange {
    /// Refuse a malformed change before any Run is read: an empty change,
    /// or one field both set and cleared.
    pub fn validate(&self) -> Result<(), RunError> {
        let invalid = |reason: String| RunError::Invalid {
            field: "limits".to_string(),
            reason,
        };
        if !self.set.is_set() && self.clear.is_empty() {
            return Err(invalid(
                "name at least one limit to set or clear".to_string(),
            ));
        }
        for field in &self.clear {
            if field.is_set_in(&self.set) {
                return Err(invalid(format!(
                    "`{}` is both set and cleared",
                    field.as_str()
                )));
            }
        }
        // The store keeps a ceiling as a signed 64-bit integer: a larger
        // one would be stored clamped and read back different from what
        // was planned, so it is refused instead (no money value is ever
        // silently changed).
        for (field, value) in [
            ("tokens-per-run", self.set.token_per_run),
            ("tokens-cumulative", self.set.token_cumulative),
        ] {
            if value.is_some_and(|value| value > i64::MAX as u64) {
                return Err(invalid(format!("`{field}` must be at most {}", i64::MAX)));
            }
        }
        // A dollar cap below zero is no cap anyone meant: refused here,
        // through every surface, rather than stored as a $0 cap that fires
        // at once.
        for (field, value) in [
            ("dollars-per-run", self.set.dollar_per_run_micros),
            ("dollars-cumulative", self.set.dollar_cumulative_micros),
        ] {
            if value.is_some_and(|value| value.get() < 0) {
                return Err(invalid(format!("`{field}` must not be negative")));
            }
        }
        Ok(())
    }

    /// The change merged over `current`.
    pub fn merged_over(&self, current: &RunLimits) -> RunLimits {
        let mut next = *current;
        if self.set.token_per_run.is_some() {
            next.token_per_run = self.set.token_per_run;
        }
        if self.set.dollar_per_run_micros.is_some() {
            next.dollar_per_run_micros = self.set.dollar_per_run_micros;
        }
        if self.set.token_cumulative.is_some() {
            next.token_cumulative = self.set.token_cumulative;
        }
        if self.set.dollar_cumulative_micros.is_some() {
            next.dollar_cumulative_micros = self.set.dollar_cumulative_micros;
        }
        if self.set.breach_action.is_some() {
            next.breach_action = self.set.breach_action;
        }
        for field in &self.clear {
            field.clear_in(&mut next);
        }
        next
    }

    fn sets_cumulative(&self) -> Option<RunLimitField> {
        if self.set.token_cumulative.is_some() {
            Some(RunLimitField::TokensCumulative)
        } else if self.set.dollar_cumulative_micros.is_some() {
            Some(RunLimitField::DollarsCumulative)
        } else {
            None
        }
    }

    fn only_clears(&self) -> bool {
        !self.set.is_set() && !self.clear.is_empty()
    }
}

/// How a change moved the Run's EFFECTIVE limits (the resolved ceilings
/// and action, not just its own rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitsDirection {
    /// At least one effective limit loosened and none tightened: latches
    /// above the totals re-arm and their `block` fence reasons lift.
    Raised,
    /// At least one effective limit tightened and none loosened: a ceiling
    /// at or below the usage breaches at once.
    Lowered,
    /// The change only cleared Run-level limits (the next layer applies
    /// again — it may raise or lower the effective ceiling).
    Cleared,
    /// The Run's limits changed, mixed or with no effective movement.
    Set,
    /// The Run's limits are already what the change asks for.
    Unchanged,
}

impl LimitsDirection {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitsDirection::Raised => "raised",
            LimitsDirection::Lowered => "lowered",
            LimitsDirection::Cleared => "cleared",
            LimitsDirection::Set => "set",
            LimitsDirection::Unchanged => "unchanged",
        }
    }
}

/// What the budget job did (or, on a dry run, would do) to one Run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimitsOutcome {
    /// The Run.
    pub run_id: crate::domain::usage::RunId,
    /// How the effective limits moved.
    pub direction: LimitsDirection,
    /// The Run's own limits before.
    pub prior_limits: RunLimits,
    /// The Run's own limits after.
    pub limits: RunLimits,
    /// The resolved limits after (each with its source; a Run-level limit
    /// carries source `run`).
    pub resolution: PolicyResolution,
    /// The breaches the change fired at once (planned, on a dry run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fired: Vec<GovernanceDecision>,
    /// The latches the change re-armed (planned, on a dry run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub re_armed: Vec<LatchKey>,
    /// The `block` fence reasons the change lifted (planned, on a dry run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fence_cleared: Vec<LatchKey>,
    /// Notes (a Run this engine does not supervise, a degraded read).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Whether nothing was written.
    pub dry_run: bool,
    /// Always `true`: a limit set this way belongs to the Run — it ends with
    /// the Run, and a fresh start mints a new Run without it. Limits every
    /// future Run should carry belong in the agent's `budget.*`
    /// configuration or a Policy.
    pub run_scoped: bool,
}

/// The budget job's answer per Run, in request order: each Run's outcome or
/// its own refusal.
pub type RunLimitsResults = Vec<(
    crate::domain::usage::RunId,
    Result<RunLimitsOutcome, RunError>,
)>;

/// The budget-show read of one Run (`hekma budget show`,
/// `GET /v0/runs/{id}/limits`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimitsView {
    /// The Run's row.
    pub run: RunRecord,
    /// The limits set on the Run itself by an Operator or a Host.
    pub limits: RunLimits,
    /// The limits a Caller set on the Run (they apply only where tighter
    /// than every other layer; the resolution names them `caller`).
    #[serde(default, skip_serializing_if = "unset_limits")]
    pub caller_limits: RunLimits,
    /// The resolved limits, each with its source (`run`, `caller`,
    /// `instance`, `policy`, `default`, `unset`).
    pub resolution: PolicyResolution,
    /// The Run's subtree usage.
    pub usage: super::RunUsage,
    /// The hosting instance's lifetime usage (the cumulative scope's
    /// total), when the Run has an instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<super::RunUsage>,
    /// The headroom per scope (ceiling minus total, saturating at zero),
    /// when anything is armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<crate::domain::BudgetView>,
    /// The latched `(dimension, scope)` pairs, with the action each
    /// latched under.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latched: Vec<governance::LatchedReason>,
    /// Notes the read surfaces (an unpriced dollar cap that cannot fire).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Serde's skip test for an unset layer.
fn unset_limits(limits: &RunLimits) -> bool {
    !limits.is_set()
}

/// The planned change on one Run — the shared half of the dry run and the
/// write (one computation, so a dry run plans exactly what the write does
/// before it executes).
pub(crate) struct LimitsPlan {
    pub record: RunRecord,
    /// The layer the actor writes (a Caller's own; else the Run's).
    pub slot: crate::ports::LimitSlot,
    /// The Caller's layer after an Operator/Host clear reached it (story
    /// 22-1: the Operator outranks); `None` when the Caller layer is kept.
    pub caller_cleared: Option<RunLimits>,
    /// That layer before and after.
    pub prior_limits: RunLimits,
    pub limits: RunLimits,
    pub resolution: PolicyResolution,
    pub inputs: ResolveInputs,
    pub direction: LimitsDirection,
    /// The pure re-arm over the Run's own level (the dry run's report).
    pub planned: governance::RearmOutcome,
}

/// The effective per-limit values a direction compares (ceilings with
/// `None` = unlimited, and the action's tightness).
fn effective(resolution: &PolicyResolution) -> ([Option<u64>; 4], u8) {
    let micros = |value: Micros| value.get().max(0) as u64;
    (
        [
            resolution.tokens.per_run,
            resolution.tokens.cumulative,
            resolution.dollars.per_run.map(micros),
            resolution.dollars.cumulative.map(micros),
        ],
        policy::action_tightness(resolution.effective_action),
    )
}

const EFFECTIVE_NAMES: [&str; 4] = [
    "tokens-per-run",
    "tokens-cumulative",
    "dollars-per-run",
    "dollars-cumulative",
];

/// The limits a change loosens and tightens (names), effective before →
/// after. `None` is unlimited: unset after a set ceiling loosens.
fn movement(
    before: &PolicyResolution,
    after: &PolicyResolution,
) -> (Vec<&'static str>, Vec<&'static str>) {
    let (b, b_action) = effective(before);
    let (a, a_action) = effective(after);
    let mut looser = Vec::new();
    let mut tighter = Vec::new();
    for index in 0..4 {
        match (b[index], a[index]) {
            (Some(_), None) => looser.push(EFFECTIVE_NAMES[index]),
            (None, Some(_)) => tighter.push(EFFECTIVE_NAMES[index]),
            (Some(x), Some(y)) if y > x => looser.push(EFFECTIVE_NAMES[index]),
            (Some(x), Some(y)) if y < x => tighter.push(EFFECTIVE_NAMES[index]),
            _ => {}
        }
    }
    if a_action < b_action {
        looser.push("breach-action");
    } else if a_action > b_action {
        tighter.push("breach-action");
    }
    (looser, tighter)
}

/// The first limit a Caller's `change` asks to loosen against what applies
/// now (`before`, the effective resolution): a ceiling set above the
/// effective one, or an action looser than the effective one. A Caller's
/// limit lives in its own layer, so the effective movement alone cannot see
/// a request that the other layers happen to outrank. A CLEAR is judged by
/// the effective movement alone (story 22-1, the 21-6 review's N5): a
/// Caller may clear its own dormant row — one the other layers outrank, so
/// clearing it loosens nothing — and is refused only when the clear would
/// loosen what applies.
fn caller_loosens(change: &RunLimitChange, before: &PolicyResolution) -> Option<&'static str> {
    let (ceilings, action) = effective(before);
    let micros = |value: Micros| value.get().max(0) as u64;
    let asked = [
        change.set.token_per_run,
        change.set.token_cumulative,
        change.set.dollar_per_run_micros.map(micros),
        change.set.dollar_cumulative_micros.map(micros),
    ];
    for index in 0..4 {
        if let Some(value) = asked[index] {
            if ceilings[index].is_some_and(|applies| value > applies) {
                return Some(EFFECTIVE_NAMES[index]);
            }
        }
    }
    if change
        .set
        .breach_action
        .is_some_and(|asked| policy::action_tightness(asked) < action)
    {
        return Some("breach-action");
    }
    // Story 22-1 (the 21-6 review's L2): a Caller may not trade `pause` or
    // `stop` for the cooperative `block`.
    if change.set.breach_action == Some(BreachAction::Block)
        && !policy::caller_action_tightens(BreachAction::Block, before.effective_action)
        && before.effective_action != BreachAction::Block
    {
        return Some("breach-action");
    }
    None
}

/// The Run's resolution with `limits` as its own (Operator/Host) limits and
/// `caller` as its Caller layer (the pure resolver over the store's facts
/// and the caller's inputs).
pub(crate) fn resolve_with<S: RunRecords + ?Sized>(
    store: &S,
    record: &RunRecord,
    limits: &RunLimits,
    caller: &RunLimits,
    inputs: &ResolveInputs,
) -> Result<PolicyResolution, RunError> {
    let labels = store.run_labels(&record.run_id)?;
    let kept = record
        .instance
        .as_deref()
        .map(|name| store.breach_default_kept(name))
        .transpose()?
        .unwrap_or(false);
    Ok(policy::resolve(&policy::ResolutionInput {
        depth: record.control_depth,
        wire_path: record.wire_path,
        bound_labels: labels
            .iter()
            .filter(|label| label.is_bound())
            .map(|label| (label.key.clone(), label.value.clone()))
            .collect(),
        run_limits: *limits,
        inputs: inputs.clone(),
        caller_limits: policy::caller_policy_limits(caller),
        kept_on_pause: kept,
        policies: store.policies()?,
    }))
}

/// The Run's caller-side inputs: fresh from the context when the caller
/// read them, else the Run's stored inputs, else none (a Run with no
/// instance). An unreadable configuration REFUSES the change — resolving
/// it as "nothing set" would let a Policy fill (and breach on) a limit the
/// instance set explicitly (AD-7: fail closed).
fn inputs_for<S: RunRecords + ?Sized>(
    store: &S,
    record: &RunRecord,
    context: Option<&InstanceContext>,
) -> Result<ResolveInputs, RunError> {
    match context.and_then(|context| context.inputs.clone()) {
        Some(Ok(inputs)) => Ok(inputs),
        Some(Err(reason)) => Err(RunError::Invalid {
            field: "config".to_string(),
            reason: format!(
                "the configuration of agent `{}` could not be read ({reason}), so Run `{}`'s \
                 limits cannot be resolved — fix the configuration and run the command again",
                record.instance.as_deref().unwrap_or_default(),
                record.run_id.as_str(),
            ),
        }),
        None => Ok(store
            .run_policy(&record.run_id)?
            .map(|row| row.inputs)
            .unwrap_or_default()),
    }
}

/// The action a Run's OWN level executes under `resolution` (the
/// pipeline's rule): the process-owning Run's effective action; a child's
/// Policy-sourced action after its depth fallback, else its own action —
/// `warn` when its depth cannot execute it.
fn level_action(
    record: &RunRecord,
    resolution: &PolicyResolution,
    limits: &RunLimits,
) -> BreachAction {
    if record.process_owner {
        return resolution.effective_action;
    }
    if matches!(
        resolution.sources.breach_action,
        LimitSource::Policy(_) | LimitSource::Default
    ) {
        return resolution.effective_action;
    }
    let action = if resolution.sources.breach_action == LimitSource::Caller {
        resolution.action
    } else {
        limits.action()
    };
    if ops::action_refusal(action, record.control_depth, record.wire_path).is_some() {
        BreachAction::Warn
    } else {
        action
    }
}

/// Plan a change on one Run: every refusal (not found, ended, scope,
/// action, authority) and the planned outcome, from reads only. The write
/// path runs this same plan inside its writer job.
pub(crate) fn plan_limits(
    store: &crate::store::SqliteStore,
    run_id: &crate::domain::usage::RunId,
    change: &RunLimitChange,
    actor: LimitActor,
    context: Option<&InstanceContext>,
) -> Result<LimitsPlan, RunError> {
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
    // Scope: the cumulative scope is the hosting instance's lifetime
    // ledger, so only its process-owning Run carries one (21-2's ruling).
    if !record.process_owner {
        if let Some(field) = change.sets_cumulative() {
            return Err(RunError::ScopeUnavailable {
                run_id: run_id.as_str().to_string(),
                limit: field.as_str().to_string(),
            });
        }
    }
    // The Breach Action through the verb × depth table's data (21-2's
    // `action_refusal`): a child's `pause`/`stop`/`block` until wire hold
    // exists; `block` anywhere with no wire path, naming the Metering
    // Source.
    if let Some(action) = change.set.breach_action {
        let metering_source = context
            .map(|context| context.metering_source.clone())
            .filter(|source| !source.is_empty())
            .unwrap_or_else(|| {
                if record.wire_path {
                    "engine-observed"
                } else {
                    "self-reported"
                }
                .to_string()
            });
        let refusal = if record.process_owner {
            (action == BreachAction::Block && !record.wire_path).then(|| {
                "`block` fences the agent's model calls at the Metering Proxy, and this Run's \
                 model traffic does not pass through it — use pause, stop or warn"
                    .to_string()
            })
        } else {
            ops::action_refusal(action, record.control_depth, record.wire_path)
        };
        if let Some(reason) = refusal {
            return Err(RunError::ActionUnavailable {
                action: action.as_str().to_string(),
                depth: record.control_depth.display_with_wire(record.wire_path),
                metering_source,
                reason,
            });
        }
    }
    // A Caller writes its OWN layer (applied only where tighter than every
    // other layer, so a later Operator, Host or Policy tightening wins); an
    // Operator or Host writes the Run's layer. The other layer is read, so
    // both resolutions see the whole Run.
    let slot = if actor == LimitActor::Caller {
        crate::ports::LimitSlot::Caller
    } else {
        crate::ports::LimitSlot::Run
    };
    let run_layer = store.run_limits(run_id)?;
    let caller_layer = store.run_caller_limits(run_id)?;
    let prior_limits = match slot {
        crate::ports::LimitSlot::Run => run_layer,
        crate::ports::LimitSlot::Caller => caller_layer,
    };
    let limits = change.merged_over(&prior_limits);
    // The Operator outranks (story 22-1, the 21-6 open question): an
    // Operator or Host CLEAR of a limit clears it in the Caller's layer
    // too — otherwise a Caller's self-imposed row could never be lifted.
    // A set never touches the Caller's layer (the Caller's row still
    // applies only where it is tighter).
    let (run_after, caller_after) = match slot {
        crate::ports::LimitSlot::Run => (
            limits,
            RunLimitChange {
                set: RunLimits::default(),
                clear: change.clear.clone(),
            }
            .merged_over(&caller_layer),
        ),
        crate::ports::LimitSlot::Caller => (run_layer, limits),
    };
    let caller_cleared = (slot == crate::ports::LimitSlot::Run && caller_after != caller_layer)
        .then_some(caller_after);
    let inputs = inputs_for(store, &record, context)?;
    let prior_resolution = resolve_with(store, &record, &run_layer, &caller_layer, &inputs)?;
    let resolution = resolve_with(store, &record, &run_after, &caller_after, &inputs)?;
    let (looser, tighter) = movement(&prior_resolution, &resolution);
    // The authority rule (FR-60 / FR-53): a Caller may only tighten — no
    // value above what applies now, no looser action, no clear of a limit
    // that applies, and no loosening of the effective limits.
    if actor == LimitActor::Caller {
        if let Some(limit) = caller_loosens(change, &prior_resolution).or(looser.first().copied()) {
            return Err(RunError::RaiseForbidden {
                run_id: run_id.as_str().to_string(),
                limit: limit.to_string(),
            });
        }
    }
    let direction = if limits == prior_limits && caller_cleared.is_none() {
        LimitsDirection::Unchanged
    } else if change.only_clears() {
        LimitsDirection::Cleared
    } else if !looser.is_empty() && tighter.is_empty() {
        LimitsDirection::Raised
    } else if !tighter.is_empty() && looser.is_empty() {
        LimitsDirection::Lowered
    } else {
        LimitsDirection::Set
    };
    // The planned re-arm over the Run's own level (pure, the vectors'
    // rule): what a dry run reports, subject to the act.
    let planned = match record.instance.as_deref().map(InstanceName::new) {
        Some(Ok(name)) => {
            let totals = level_totals(store, &name, &record)?;
            let latched = store
                .latches(run_id)?
                .into_iter()
                .map(|(reason, action)| governance::LatchedReason { reason, action })
                .collect();
            governance::rearm(&governance::RearmInput {
                arms: level_arms(&record, &resolution),
                action: level_action(&record, &resolution, &run_after),
                priced: resolution.rate.is_some(),
                totals,
                latched,
            })
        }
        _ => governance::RearmOutcome::default(),
    };
    Ok(LimitsPlan {
        record,
        slot,
        caller_cleared,
        prior_limits,
        limits,
        resolution,
        inputs,
        direction,
        planned,
    })
}

impl LimitsPlan {
    /// The dry run's outcome (nothing written).
    pub(crate) fn dry_outcome(self) -> RunLimitsOutcome {
        let unchanged = self.direction == LimitsDirection::Unchanged;
        RunLimitsOutcome {
            run_id: self.record.run_id.clone(),
            direction: self.direction,
            prior_limits: self.prior_limits,
            limits: self.limits,
            resolution: self.resolution,
            fired: if unchanged {
                Vec::new()
            } else {
                self.planned.fired
            },
            re_armed: if unchanged {
                Vec::new()
            } else {
                self.planned
                    .unlatched
                    .iter()
                    .map(|latched| latched.reason)
                    .fold(Vec::new(), |mut keys, key| {
                        if !keys.contains(&key) {
                            keys.push(key);
                        }
                        keys
                    })
            },
            fence_cleared: if unchanged {
                Vec::new()
            } else {
                self.planned.fence_cleared
            },
            notes: Vec::new(),
            dry_run: true,
            run_scoped: true,
        }
    }
}

/// Commit a change on one Run (inside ONE writer job): re-plan on the
/// writer's own reads, write `run_limits` (`source` = the actor), store the
/// new resolution (its Anomalies), commit one `limits-changed` Run event
/// (prior and after), then run the [`re_arm`] pass — a raise re-arms and
/// lifts the matching fence reasons per reason, a lowering at or below the
/// usage breaches at once. An unchanged change writes nothing.
pub(crate) fn apply_limits(
    store: &crate::store::SqliteStore,
    ctx: &mut crate::store::WriterCtx<'_>,
    run_id: &crate::domain::usage::RunId,
    change: &RunLimitChange,
    actor: LimitActor,
    context: Option<&InstanceContext>,
    at: &str,
) -> Result<(RunLimitsOutcome, RunChanges, Vec<CommittedBreach>), RunError> {
    let plan = plan_limits(store, run_id, change, actor, context)?;
    let mut changes = RunChanges::default();
    let mut breaches = Vec::new();
    let mut notes = Vec::new();
    let mut outcome = RunLimitsOutcome {
        run_id: run_id.clone(),
        direction: plan.direction,
        prior_limits: plan.prior_limits,
        limits: plan.limits,
        resolution: plan.resolution.clone(),
        fired: Vec::new(),
        re_armed: Vec::new(),
        fence_cleared: Vec::new(),
        notes: Vec::new(),
        dry_run: false,
        run_scoped: true,
    };
    if plan.direction == LimitsDirection::Unchanged {
        return Ok((outcome, changes, breaches));
    }
    store.replace_run_limits(run_id, plan.slot, &plan.limits, actor.as_str(), at)?;
    if let Some(caller_after) = &plan.caller_cleared {
        store.replace_run_limits(
            run_id,
            crate::ports::LimitSlot::Caller,
            caller_after,
            actor.as_str(),
            at,
        )?;
    }
    changes.extend(policy::resolve_and_store(
        store,
        &plan.record,
        &plan.resolution,
        &plan.inputs,
        at,
    )?);
    // The event carries the Run as it stands after the resolution's own
    // Anomaly changes (re-read: the resolution may have saved the row).
    let record = store.run_record(run_id)?.unwrap_or(plan.record);
    ops::note_limits_changed(
        store,
        &mut changes,
        &record,
        plan.prior_limits,
        plan.limits,
        actor.as_str(),
        match plan.slot {
            crate::ports::LimitSlot::Run => "run",
            crate::ports::LimitSlot::Caller => "caller",
        },
        at,
    )?;
    if let Some(instance) = record.instance.as_deref() {
        if let Ok(name) = InstanceName::new(instance) {
            let context = context.cloned().unwrap_or_default();
            let supervised = context.supervised;
            if !supervised {
                notes.push(format!(
                    "Run `{}` (agent `{instance}`) was not breach-checked here: this engine does \
                     not supervise the agent. A raise or clear still re-arms its latch (done \
                     here); a lowering waits for the supervising engine's next evaluation",
                    run_id.as_str(),
                ));
            }
            let report = re_arm(
                store,
                &name,
                &record,
                &plan.resolution,
                &context,
                supervised,
                ctx,
                &mut breaches,
                &mut changes,
                &mut notes,
            )?;
            for (key, _) in &report.unlatched {
                if !outcome.re_armed.contains(key) {
                    outcome.re_armed.push(*key);
                }
            }
            outcome.fence_cleared = report.fence_cleared;
            outcome.fired = report.fired;
        }
    }
    outcome.notes = notes;
    Ok((outcome, changes, breaches))
}

/// The budget-show read of one Run (read-only connection): the Run-level
/// limits, the fresh resolution with its sources, the subtree and
/// cumulative usage, the headroom and the latches.
pub(crate) fn limits_view(
    store: &crate::store::SqliteStore,
    run_id: &crate::domain::usage::RunId,
    inputs: Option<Result<ResolveInputs, String>>,
) -> Result<RunLimitsView, RunError> {
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    let limits = store.run_limits(run_id)?;
    let mut notes = Vec::new();
    let inputs = match inputs {
        Some(Ok(inputs)) => inputs,
        Some(Err(reason)) => {
            notes.push(format!(
                "the agent's configuration could not be read ({reason}); the limits below \
                 resolve from the Run's stored inputs"
            ));
            store
                .run_policy(run_id)?
                .map(|row| row.inputs)
                .unwrap_or_default()
        }
        None => store
            .run_policy(run_id)?
            .map(|row| row.inputs)
            .unwrap_or_default(),
    };
    let caller_limits = store.run_caller_limits(run_id)?;
    let resolution = resolve_with(store, &record, &limits, &caller_limits, &inputs)?;
    let view = ops::view(store, run_id)?;
    let (cumulative_usage, cumulative_tokens, cumulative_cost) =
        match record.instance.as_deref().map(InstanceName::new) {
            Some(Ok(name)) => {
                let rollup = store.instance_rollup(&name)?;
                (
                    Some(super::RunUsage {
                        input_tokens: rollup.totals.input_tokens,
                        output_tokens: rollup.totals.output_tokens,
                        cached_tokens: rollup.totals.cached_tokens,
                        cost_micros: rollup.cost,
                        estimate: crate::domain::EstimateLabel::Estimated,
                    }),
                    rollup.totals.total_tokens(),
                    rollup.cost,
                )
            }
            _ => (None, 0, Micros::ZERO),
        };
    let arms = level_arms(&record, &resolution);
    let budget = crate::domain::BudgetView::from_budget_and_cost(
        &crate::domain::TokenBudget {
            per_run: arms.tokens_per_run,
            cumulative: arms.tokens_cumulative,
        },
        &crate::domain::CostCap {
            per_run: arms.dollars_per_run_micros.map(|v| Micros(v as i64)),
            cumulative: arms.dollars_cumulative_micros.map(|v| Micros(v as i64)),
        },
        level_action(&record, &resolution, &limits),
        view.usage
            .input_tokens
            .saturating_add(view.usage.output_tokens),
        cumulative_tokens,
        resolution.rate.is_some(),
        view.usage.cost_micros,
        cumulative_cost,
        crate::domain::EstimateLabel::Estimated,
        &resolution.sources.budget_policy_names(),
    );
    if resolution.dollars.is_set() && resolution.rate.is_none() {
        notes.push(
            "a dollar cap is set but no Rate resolves for this Run's model, so the cap cannot \
             fire until a Rate is configured"
                .to_string(),
        );
    }
    let latched = store
        .latches(run_id)?
        .into_iter()
        .map(|(reason, action)| governance::LatchedReason { reason, action })
        .collect();
    Ok(RunLimitsView {
        run: record,
        limits,
        caller_limits,
        resolution,
        usage: view.usage,
        cumulative_usage,
        budget,
        latched,
        notes,
    })
}
