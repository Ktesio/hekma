//! Policies and their resolution (story 21-3, spine AD-25, PRD FR-60).
//!
//! A **Policy** is standing limits that apply to every Run its Selector
//! matches, now and later. Resolution is ONE PURE FUNCTION in the domain
//! core ([`resolve`]) that every surface calls — the pipeline's live
//! enforcement read, the stored per-Run resolution, the CLI and the Control
//! API — none re-implements it:
//!
//! 1. a `breach_action` value that is present but malformed anywhere fails
//!    CLOSED to `pause` with a surfaced note (AD-7, the chain's rule 1 —
//!    before the Run's own action), never to the new default;
//! 2. explicit limits on the Run (`run_limits`, operator/Host);
//! 3. explicit limits on the Agent Instance (any FR-11 layer, merged —
//!    including the FR-21 upgrade write);
//!    * an instance kept on `pause` by the FR-21 upgrade (its marker row)
//!      resolves `pause` at this level until an explicit action is set;
//! 4. per limit, the matching Policy with the MOST SPECIFIC Selector
//!    (a strict term-set superset); Selectors that cannot be compared tie,
//!    and a tie resolves to the tightest value;
//! 5. the stored, editable `default` Policy fills whatever is still unset.
//!
//! Policy Selectors match BOUND labels only ([`SelectorScope::BoundOnly`]),
//! so a Caller can never choose its way into a looser Policy. Caller limits
//! (none today — no Caller surface exists yet) apply only while tighter.
//!
//! Rates resolve through the same chain: the instance's `cost.rate.*`, then
//! a matching Policy's flat Rate, then the default Policy's model-keyed
//! Rate table (exact model match) — so Runs Hekma only sees on the wire
//! still have a price once the shared proxy exists (AD-26).
//!
//! Two conditions are made loud rather than silent (FR-60):
//! * a Breach Action the Run's Control Depth cannot execute falls back to
//!   `warn` and raises the `depth-fallback` Anomaly (21-2's kind — one
//!   condition, however the action was sourced; the resolution names the
//!   Policy);
//! * a Cost Cap with no resolvable Rate raises the `unpriced-model`
//!   Anomaly instead of staying silently inert (money code fails loud).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::verbs::action_executable;
use super::{Anomaly, ControlDepth, RunLimits, RunRecord};
use crate::domain::budget::{BreachAction, TokenBudget};
use crate::domain::cost::CostCap;
use crate::domain::cost::Rate;
use crate::domain::runs::ops::RunChanges;
use crate::domain::runs::selector::Selector;
use crate::domain::usage::RunId;
use crate::ports::{RunRecords, StoreError};

/// The event families' schema versions (AD-14).
pub const POLICY_EVENT_SCHEMA_VERSION: u32 = 1;

/// The reserved default Policy's name.
pub const DEFAULT_POLICY_NAME: &str = "default";

/// How Hekma treats a call without a valid Run Key (PRD FR-53). The modes
/// exist on every Policy row from story 21-3 on, but only `off` is accepted
/// at create/change until Scent-Check ships with its own story (22.3, PRD
/// §13 Q9's interim): an accepted-but-inert `enforce` would be the silent
/// lie the house rules forbid. The default Policy is seeded `off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScentCheckMode {
    /// Check nothing.
    Off,
    /// Serve an unscented call and record it as an Anomaly (22.3).
    Shadow,
    /// Refuse an unscented call with a typed response (22.3).
    Enforce,
}

impl ScentCheckMode {
    /// Every mode, loosest to tightest (the FR-60 tie order).
    pub const ALL: [ScentCheckMode; 3] = [
        ScentCheckMode::Off,
        ScentCheckMode::Shadow,
        ScentCheckMode::Enforce,
    ];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ScentCheckMode::Off => "off",
            ScentCheckMode::Shadow => "shadow",
            ScentCheckMode::Enforce => "enforce",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == raw)
    }
}

impl std::fmt::Display for ScentCheckMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The limits one Policy (or an explicit config read) can carry. Every field
/// is optional: a Policy that does not set a limit does not constrain it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyLimits {
    /// A per-run token ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_run: Option<u64>,
    /// A cumulative token ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_cumulative: Option<u64>,
    /// A per-run Cost Cap, micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollars_per_run_micros: Option<u64>,
    /// A cumulative Cost Cap, micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollars_cumulative_micros: Option<u64>,
    /// The Breach Action for these ceilings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breach_action: Option<BreachAction>,
    /// The Scent-Check Mode (`off` only, until 22.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scent_check: Option<ScentCheckMode>,
    /// A flat Rate for matched Runs (named Policies; the default Policy
    /// prices through its model-keyed table instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
}

impl PolicyLimits {
    /// Whether any field is set.
    pub fn is_set(&self) -> bool {
        self.tokens_per_run.is_some()
            || self.tokens_cumulative.is_some()
            || self.dollars_per_run_micros.is_some()
            || self.dollars_cumulative_micros.is_some()
            || self.breach_action.is_some()
            || self.scent_check.is_some()
            || self.rate.is_some()
    }

    /// The ceiling for `limit`, if this set carries it.
    pub fn ceiling(&self, limit: LimitKind) -> Option<u64> {
        match limit {
            LimitKind::TokensPerRun => self.tokens_per_run,
            LimitKind::TokensCumulative => self.tokens_cumulative,
            LimitKind::DollarsPerRun => self.dollars_per_run_micros,
            LimitKind::DollarsCumulative => self.dollars_cumulative_micros,
        }
    }
}

/// The per-limit kinds the resolver resolves independently (FR-60: "each
/// limit is resolved on its own").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitKind {
    TokensPerRun,
    TokensCumulative,
    DollarsPerRun,
    DollarsCumulative,
}

/// One stored Policy (schema v15). The `default` Policy carries no Selector
/// and is the fallback layer; every other Policy requires a non-empty
/// Selector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    /// The Policy's name (`^[a-z0-9][a-z0-9_-]*$`; `default` is reserved).
    pub name: String,
    /// Its Selector; `None` only on `default`.
    pub selector: Option<Selector>,
    /// The limits it sets.
    pub limits: PolicyLimits,
    /// The model-keyed Rate table (`default` only today; exact model match).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rates: BTreeMap<String, Rate>,
}

impl PolicyRecord {
    /// The seeded `default` Policy: no Selector, no ceilings, Breach Action
    /// `warn`, Scent-Check `off` (PRD §13 Q9's interim), an empty Rate
    /// table (no invented prices in money code). The Operator can edit it
    /// like any other Policy.
    pub fn default_policy() -> Self {
        Self {
            name: DEFAULT_POLICY_NAME.to_string(),
            selector: None,
            limits: PolicyLimits {
                breach_action: Some(BreachAction::Warn),
                scent_check: Some(ScentCheckMode::Off),
                ..PolicyLimits::default()
            },
            rates: BTreeMap::new(),
        }
    }

    /// Whether this is the `default` Policy.
    pub fn is_default(&self) -> bool {
        self.name == DEFAULT_POLICY_NAME
    }
}

/// The caller-resolved resolution inputs that do NOT live in the store: the
/// instance's explicit config limits (all FR-11 layers merged) and its
/// resolved `model` key. Stored beside each Run's resolution so a
/// re-resolution inside a writer job (a label change) reuses exactly what
/// the last caller-side read produced — a label change cannot change them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveInputs {
    /// The instance's explicit limits (unset fields absent).
    pub explicit: PolicyLimits,
    /// A `breach_action` that is present but malformed at any layer: the
    /// value, surfaced as a fail-closed note (AD-7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub malformed_action: Option<String>,
    /// The instance's resolved `model` key (the Rate table's lookup key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Everything the pure resolver needs. Run-shaped facts come from the
/// store; the caller supplies the instance-shaped ones.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionInput {
    /// The Run's Control Depth.
    pub depth: ControlDepth,
    /// Whether the Run's model traffic crosses the Metering Proxy.
    pub wire_path: bool,
    /// The Run's BOUND labels (`key`, `value`) — the only labels that
    /// select Policies.
    pub bound_labels: Vec<(String, String)>,
    /// The limits set directly on the Run.
    pub run_limits: RunLimits,
    /// The caller-resolved instance facts.
    pub inputs: ResolveInputs,
    /// Caller-supplied limits (FR-53/FR-60: a Caller may only TIGHTEN —
    /// they apply per limit only where tighter than the resolved value).
    /// No surface supplies them yet; the field is threaded so 22.x wires
    /// in without touching the function's shape.
    #[serde(default, skip_serializing_if = "PolicyLimits::is_set")]
    pub caller_limits: PolicyLimits,
    /// The instance is kept on `pause` by the FR-21 upgrade (its marker
    /// row): the action resolves `pause` at the explicit level until a
    /// Breach Action is set explicitly.
    #[serde(default)]
    pub kept_on_pause: bool,
    /// The whole policy set, `default` included.
    pub policies: Vec<PolicyRecord>,
}

/// Where one resolved limit came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitSource {
    /// Set on the Run.
    Run,
    /// Set explicitly on the Agent Instance (config, or the upgrade
    /// marker's fail-closed `pause`).
    Instance,
    /// Supplied by the named Policy.
    Policy(String),
    /// Supplied by the `default` Policy.
    Default,
    /// Not set anywhere.
    Unset,
}

/// The per-limit sources (AD-14: surfaces name the Policies that supplied a
/// Run's limits).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitSources {
    pub tokens_per_run: LimitSource,
    pub tokens_cumulative: LimitSource,
    pub dollars_per_run: LimitSource,
    pub dollars_cumulative: LimitSource,
    pub breach_action: LimitSource,
    pub scent_check: LimitSource,
    pub rate: LimitSource,
}

impl LimitSources {
    /// The names of every Policy that supplied a limit (the `default`
    /// Policy counts).
    pub fn policy_names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            &self.tokens_per_run,
            &self.tokens_cumulative,
            &self.dollars_per_run,
            &self.dollars_cumulative,
            &self.breach_action,
            &self.scent_check,
            &self.rate,
        ]
        .into_iter()
        .filter_map(|source| match source {
            LimitSource::Policy(name) => Some(name.clone()),
            LimitSource::Default => Some(DEFAULT_POLICY_NAME.to_string()),
            LimitSource::Run | LimitSource::Instance | LimitSource::Unset => None,
        })
        .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The same, over the limits a BUDGET view shows (the ceilings, the
    /// action, the Rate — everything but Scent-Check, which the budget
    /// surfaces do not carry): naming `default` there for a Scent-Check the
    /// view never shows would be noise, not provenance.
    pub fn budget_policy_names(&self) -> Vec<String> {
        let mut names: Vec<String> = [
            &self.tokens_per_run,
            &self.tokens_cumulative,
            &self.dollars_per_run,
            &self.dollars_cumulative,
            &self.breach_action,
            &self.rate,
        ]
        .into_iter()
        .filter_map(|source| match source {
            LimitSource::Policy(name) => Some(name.clone()),
            LimitSource::Default => Some(DEFAULT_POLICY_NAME.to_string()),
            LimitSource::Run | LimitSource::Instance | LimitSource::Unset => None,
        })
        .collect();
        names.sort();
        names.dedup();
        names
    }
}

/// A matched Policy's Breach Action the Run's depth cannot execute: the
/// effective action is `warn` and the `depth-fallback` Anomaly is raised
/// (FR-60). The action takes effect if the Run's depth later allows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthFallback {
    /// The Policy whose action cannot execute.
    pub policy: String,
    /// The action that cannot execute.
    pub action: BreachAction,
}

/// A resolved Cost Cap with no resolvable Rate: the cap cannot fire and the
/// `unpriced-model` Anomaly is raised instead of a silent no-op (money code
/// fails loud).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnpricedCap {
    /// The model looked up (or "no model configured").
    pub model: String,
    /// The Policy that supplied the cap (an explicit-instance cap keeps its
    /// documented inert-with-notice, FR-20).
    pub policy: String,
}

/// The pure resolver's output: the effective limits, where each came from,
/// the effective Breach Action after the depth fallback, and the two loud
/// conditions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyResolution {
    /// The effective Token Budget (per-run and cumulative ceilings).
    pub tokens: TokenBudget,
    /// The effective Cost Cap.
    pub dollars: CostCap,
    /// The resolved Breach Action (before the depth fallback).
    pub action: BreachAction,
    /// The effective Breach Action after the depth fallback.
    pub effective_action: BreachAction,
    /// The resolved Scent-Check Mode.
    pub scent_check: ScentCheckMode,
    /// The resolved Rate, if any.
    pub rate: Option<Rate>,
    /// The per-limit sources.
    pub sources: LimitSources,
    /// The depth fallback, when the action cannot execute at this depth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<DepthFallback>,
    /// The unpriced-cap condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpriced: Option<UnpricedCap>,
    /// A malformed explicit `breach_action` fail-closed note (AD-7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_closed: Option<String>,
}

impl PolicyResolution {
    /// The Anomalies this resolution raises on its Run.
    pub fn raised_anomalies(&self) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();
        if self.fallback.is_some() {
            anomalies.push(Anomaly::DepthFallback);
        }
        if self.unpriced.is_some() {
            anomalies.push(Anomaly::UnpricedModel);
        }
        anomalies
    }
}

/// The Breach Action tightness order (FR-60's tie rule):
/// `warn` < `pause` < `stop` < `block`.
pub fn action_tightness(action: BreachAction) -> u8 {
    match action {
        BreachAction::Warn => 0,
        BreachAction::Pause => 1,
        BreachAction::Stop => 2,
        BreachAction::Block => 3,
    }
}

/// The Scent-Check Mode tightness order (FR-60's tie rule).
fn scent_tightness(mode: ScentCheckMode) -> u8 {
    match mode {
        ScentCheckMode::Off => 0,
        ScentCheckMode::Shadow => 1,
        ScentCheckMode::Enforce => 2,
    }
}

/// Whether Selector `a` is MORE SPECIFIC than `b`: `a`'s term set is a
/// strict superset of `b`'s (FR-60, ruled 2026-10-30). Term count alone
/// never decides — `{a=1,b=2}` and `{a=1,c=3}` cannot be compared and tie.
pub fn dominates(a: &Selector, b: &Selector) -> bool {
    let a_terms = a.terms();
    let b_terms = b.terms();
    a_terms.len() > b_terms.len()
        && b_terms
            .iter()
            .all(|term| a_terms.iter().any(|candidate| candidate == term))
}

/// The tightest ceiling among tied maximal candidates: the MINIMUM (a
/// higher ceiling would loosen; FR-60's tie rule).
fn tightest_ceiling(values: &[u64]) -> Option<u64> {
    // No candidate sets the limit: nothing to pick (never a panic on the
    // enforcement path).
    values.iter().min().copied()
}

/// The flat-Rate tie rule's tightness: the `input+output` micros sum — a
/// HIGHER sum is tighter (the conservative, overstate-cost direction; a
/// lower rate would understate cost in money code).
fn rate_tightness(rate: &Rate) -> u128 {
    u128::from(rate.input_micros_per_1m) + u128::from(rate.output_micros_per_1m)
}

/// Resolve one Run's effective limits (AD-25's one pure function). Total
/// and deterministic: the same input always yields the same output, on
/// every OS. See the module docs for the precedence chain.
pub fn resolve(input: &ResolutionInput) -> PolicyResolution {
    let mut fail_closed = None;
    let default_policy = input
        .policies
        .iter()
        .find(|policy| policy.is_default())
        .cloned()
        .unwrap_or_else(PolicyRecord::default_policy);
    let named: Vec<&PolicyRecord> = input
        .policies
        .iter()
        .filter(|policy| !policy.is_default())
        .filter(|policy| {
            policy
                .selector
                .as_ref()
                .is_some_and(|selector| selector.matches_pairs(&input.bound_labels))
        })
        .collect();
    // FR-60's per-limit rule: "each limit is resolved on its own" — the
    // most-specific comparison runs among the matching Policies that SET
    // the limit, so a more-specific Policy that leaves it unset does not
    // suppress a less-specific one that sets it.
    let maximal_setting = |sets: &dyn Fn(&PolicyLimits) -> bool| -> Vec<&PolicyRecord> {
        let setting: Vec<&PolicyRecord> = named
            .iter()
            .copied()
            .filter(|policy| sets(&policy.limits))
            .collect();
        maximal_candidates(&setting)
    };

    let mut sources = LimitSources {
        tokens_per_run: LimitSource::Unset,
        tokens_cumulative: LimitSource::Unset,
        dollars_per_run: LimitSource::Unset,
        dollars_cumulative: LimitSource::Unset,
        breach_action: LimitSource::Unset,
        scent_check: LimitSource::Unset,
        rate: LimitSource::Unset,
    };
    let mut tokens = TokenBudget::none();
    let mut dollars = CostCap::none();
    // The dollar scopes resolve as raw micros first (the per-limit loop is
    // unit-agnostic), then fold into the `CostCap`'s `Micros`.
    let mut dollar_scope: [Option<u64>; 2] = [None, None];

    // ---- Ceilings, per limit kind: Run > instance > the most specific
    // matching Policy > the default Policy. Ties among the maximal
    // candidates take the tightest value (the minimum).
    for kind in [
        LimitKind::TokensPerRun,
        LimitKind::TokensCumulative,
        LimitKind::DollarsPerRun,
        LimitKind::DollarsCumulative,
    ] {
        let slot = match kind {
            LimitKind::TokensPerRun => &mut tokens.per_run,
            LimitKind::TokensCumulative => &mut tokens.cumulative,
            LimitKind::DollarsPerRun => &mut dollar_scope[0],
            LimitKind::DollarsCumulative => &mut dollar_scope[1],
        };
        let source = match kind {
            LimitKind::TokensPerRun => &mut sources.tokens_per_run,
            LimitKind::TokensCumulative => &mut sources.tokens_cumulative,
            LimitKind::DollarsPerRun => &mut sources.dollars_per_run,
            LimitKind::DollarsCumulative => &mut sources.dollars_cumulative,
        };
        if let Some(value) = run_ceiling(&input.run_limits, kind) {
            *slot = Some(value);
            *source = LimitSource::Run;
            continue;
        }
        if let Some(value) = input.inputs.explicit.ceiling(kind) {
            *slot = Some(value);
            *source = LimitSource::Instance;
            continue;
        }
        let candidates: Vec<&PolicyRecord> =
            maximal_setting(&|limits| limits.ceiling(kind).is_some());
        if let Some((value, name)) = policy_ceiling(&candidates, kind) {
            *slot = Some(value);
            *source = LimitSource::Policy(name);
        } else if let Some(value) = default_policy.limits.ceiling(kind) {
            *slot = Some(value);
            *source = LimitSource::Default;
        }
    }
    dollars.per_run = dollar_scope[0].map(|micros| crate::domain::cost::Micros(micros as i64));
    dollars.cumulative = dollar_scope[1].map(|micros| crate::domain::cost::Micros(micros as i64));

    // ---- The Breach Action: a malformed explicit value anywhere fails
    // CLOSED to `pause` with a note (AD-7) BEFORE every other source,
    // the Run's own action included (the precedence chain's rule 1) >
    // explicit Run > instance > the upgrade marker's `pause` > the most
    // specific matching Policy > the default Policy (`warn`, so the
    // action always resolves).
    let action = if let Some(value) = input.inputs.malformed_action.clone() {
        fail_closed = Some(format!(
            "the configured breach action `{value}` is not one of pause, stop, warn or block, so \
             it fails closed to pause until it is fixed"
        ));
        sources.breach_action = LimitSource::Instance;
        BreachAction::Pause
    } else if let Some(action) = input.run_limits.breach_action {
        sources.breach_action = LimitSource::Run;
        action
    } else if let Some(action) = input.inputs.explicit.breach_action {
        sources.breach_action = LimitSource::Instance;
        action
    } else if input.kept_on_pause {
        sources.breach_action = LimitSource::Instance;
        BreachAction::Pause
    } else if let Some((action, name)) =
        policy_action(&maximal_setting(&|limits| limits.breach_action.is_some()))
    {
        sources.breach_action = LimitSource::Policy(name);
        action
    } else {
        sources.breach_action = LimitSource::Default;
        default_policy
            .limits
            .breach_action
            .unwrap_or(BreachAction::Warn)
    };

    // ---- The Scent-Check Mode: instance > the most specific matching
    // Policy (ties to the tightest mode) > the default Policy (`off`).
    let scent_check = if let Some(mode) = input.inputs.explicit.scent_check {
        sources.scent_check = LimitSource::Instance;
        mode
    } else if let Some((mode, name)) =
        policy_scent(&maximal_setting(&|limits| limits.scent_check.is_some()))
    {
        sources.scent_check = LimitSource::Policy(name);
        mode
    } else {
        sources.scent_check = LimitSource::Default;
        default_policy
            .limits
            .scent_check
            .unwrap_or(ScentCheckMode::Off)
    };

    // ---- The Rate: the instance's `cost.rate.*` (both directions or
    // absent — a half-configured Rate falls through, FR-20's documented
    // "no Rate yet") > a matching Policy's flat Rate (most specific; a tie
    // takes the higher input+output sum, then the alphabetically-first
    // name) > the default Policy's table entry for the resolved `model`
    // (exact, case-sensitive).
    let rate = if input.inputs.explicit.rate.is_some() {
        sources.rate = LimitSource::Instance;
        input.inputs.explicit.rate
    } else if let Some(policy) = policy_rate(&maximal_setting(&|limits| limits.rate.is_some())) {
        sources.rate = LimitSource::Policy(policy.name.clone());
        policy.limits.rate
    } else {
        let entry = input
            .inputs
            .model
            .as_deref()
            .and_then(|model| default_policy.rates.get(model).copied());
        if entry.is_some() {
            sources.rate = LimitSource::Default;
        }
        entry
    };

    // ---- The depth fallback: a POLICY-sourced action the Run's (depth,
    // wire path) cannot execute acts as `warn` with the `depth-fallback`
    // Anomaly (FR-60); it takes effect when the depth allows it again. An
    // action set DIRECTLY (Run limits or instance config) keeps the story
    // 19-2 contract: the decision carries the configured action and the
    // executor degrades loudly (a `block` with no listener records `block`,
    // fences nothing, and says so).
    let action_source = match &sources.breach_action {
        LimitSource::Policy(name) => Some(name.clone()),
        LimitSource::Default => Some(DEFAULT_POLICY_NAME.to_string()),
        LimitSource::Run | LimitSource::Instance | LimitSource::Unset => None,
    };
    let fallback = if action != BreachAction::Warn
        && action_source.is_some()
        && !action_executable(input.depth, input.wire_path, action)
    {
        Some(DepthFallback {
            policy: action_source.clone().unwrap_or_default(),
            action,
        })
    } else {
        None
    };
    let effective_action = if fallback.is_some() {
        BreachAction::Warn
    } else {
        action
    };

    // ---- The unpriced cap: a resolved Cost Cap with no Rate whose ceiling
    // source is a Policy fails loud; an explicit-instance cap keeps its
    // documented inert-with-notice (FR-20).
    let cap_policy = [&sources.dollars_per_run, &sources.dollars_cumulative]
        .into_iter()
        .find_map(|source| match source {
            LimitSource::Policy(name) => Some(name.clone()),
            LimitSource::Default if dollars.is_set() => Some(DEFAULT_POLICY_NAME.to_string()),
            _ => None,
        });
    let unpriced = if dollars.is_set() && rate.is_none() {
        cap_policy.map(|policy| UnpricedCap {
            model: input
                .inputs
                .model
                .clone()
                .unwrap_or_else(|| "no model configured".to_string()),
            policy,
        })
    } else {
        None
    };

    PolicyResolution {
        tokens,
        dollars,
        action,
        effective_action,
        scent_check,
        rate,
        sources,
        fallback,
        unpriced,
        fail_closed,
    }
}

/// The Run-level ceiling for `kind` (`run_limits`), if set. Only per-run
/// scopes exist at the Run level today (21-2).
fn run_ceiling(limits: &RunLimits, kind: LimitKind) -> Option<u64> {
    match kind {
        LimitKind::TokensPerRun => limits.token_per_run,
        LimitKind::DollarsPerRun => limits
            .dollar_per_run_micros
            .map(|micros| micros.get() as u64),
        LimitKind::TokensCumulative | LimitKind::DollarsCumulative => None,
    }
}

/// The winning ceiling among the maximal candidate Policies that set
/// `kind`: the tightest value (the minimum), an equal value naming the
/// alphabetically-first Policy. Returns `(value, name)`.
fn policy_ceiling(candidates: &[&PolicyRecord], kind: LimitKind) -> Option<(u64, String)> {
    let values: Vec<u64> = candidates
        .iter()
        .map(|policy| policy.limits.ceiling(kind))
        .collect::<Option<_>>()?;
    let value = tightest_ceiling(&values)?;
    let name = candidates
        .iter()
        .filter(|policy| policy.limits.ceiling(kind) == Some(value))
        .map(|policy| policy.name.clone())
        .min()?;
    Some((value, name))
}

/// The winning action among the maximal candidates that set one: the
/// TIGHTEST action (`warn` < `pause` < `stop` < `block`, FR-60's tie
/// order), an equal value naming the alphabetically-first Policy.
fn policy_action(candidates: &[&PolicyRecord]) -> Option<(BreachAction, String)> {
    let winner = candidates
        .iter()
        .copied()
        .filter(|policy| policy.limits.breach_action.is_some())
        .max_by(|a, b| {
            action_tightness(a.limits.breach_action.unwrap_or(BreachAction::Warn))
                .cmp(&action_tightness(
                    b.limits.breach_action.unwrap_or(BreachAction::Warn),
                ))
                .then_with(|| b.name.cmp(&a.name))
        })?;
    Some((winner.limits.breach_action?, winner.name.clone()))
}

/// The winning Scent-Check Mode among the maximal candidates that set one:
/// the TIGHTEST mode (`off` < `shadow` < `enforce`, FR-60's tie order), an
/// equal value naming the alphabetically-first Policy.
fn policy_scent(candidates: &[&PolicyRecord]) -> Option<(ScentCheckMode, String)> {
    let winner = candidates
        .iter()
        .copied()
        .filter(|policy| policy.limits.scent_check.is_some())
        .max_by(|a, b| {
            scent_tightness(a.limits.scent_check.unwrap_or(ScentCheckMode::Off))
                .cmp(&scent_tightness(
                    b.limits.scent_check.unwrap_or(ScentCheckMode::Off),
                ))
                .then_with(|| b.name.cmp(&a.name))
        })?;
    Some((winner.limits.scent_check?, winner.name.clone()))
}

/// The winning flat-Rate candidate among the maximal candidates that set
/// one: the higher input+output sum (the conservative direction), an equal
/// sum naming the alphabetically-first Policy.
fn policy_rate<'a>(candidates: &[&'a PolicyRecord]) -> Option<&'a PolicyRecord> {
    candidates
        .iter()
        .copied()
        .filter(|policy| policy.limits.rate.is_some())
        .min_by(|a, b| {
            let sum_a = rate_tightness(a.limits.rate.as_ref().expect("filtered"));
            let sum_b = rate_tightness(b.limits.rate.as_ref().expect("filtered"));
            sum_b.cmp(&sum_a).then_with(|| a.name.cmp(&b.name))
        })
}

/// The maximal (not strictly dominated) candidates among the matching
/// Policies: every candidate no other candidate's Selector strictly
/// dominates. The remainder tie (FR-60: "Selectors that cannot be compared
/// tie").
fn maximal_candidates<'a>(candidates: &[&'a PolicyRecord]) -> Vec<&'a PolicyRecord> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other.name != candidate.name
                    && other
                        .selector
                        .as_ref()
                        .zip(candidate.selector.as_ref())
                        .is_some_and(|(other_selector, candidate_selector)| {
                            dominates(other_selector, candidate_selector)
                        })
            })
        })
        .collect()
}

/// The cached policy set (amendment 3, orchestrator review 2026-11-09):
/// loaded once at writer open, swapped ONLY by a policy-change job's
/// post-commit hook in the same writer job that commits the row — the
/// writer is the only Policy writer, so a reader either sees the
/// pre-change set with pre-change rows or the post-change set with
/// post-change rows; staleness is impossible by construction (a crash
/// between commit and swap dies with the process; the next open loads the
/// committed set). [`PolicyCache::current`] is one `Arc` clone — the
/// per-usage-event resolution reads no SQL.
///
/// **Degraded reads fail closed WITHOUT disarming (21-3 review rounds 1
/// and 2):** a load that failed — or a set whose `default` row went
/// missing — marks the cache DEGRADED and keeps the LAST GOOD set. A
/// resolution through a degraded cache then resolves against that last
/// good set with the action FORCED to `pause` and a note naming why: the
/// ceilings stay enforced (the last thing known beats nothing, which once
/// turned every budget in the fleet off), while the action never falls
/// back to `warn` by accident (AD-7).
#[derive(Debug, Default)]
pub(crate) struct PolicyCache {
    current: std::sync::RwLock<std::sync::Arc<Vec<PolicyRecord>>>,
    degraded: std::sync::RwLock<Option<String>>,
    loaded_fingerprint: std::sync::RwLock<Option<String>>,
}

impl PolicyCache {
    /// The current policy set (a cheap `Arc` clone). `Err` carries the
    /// reason the set cannot be trusted; the caller fails closed WITH the
    /// last good set ([`Self::current_or_last_good`]).
    pub(crate) fn current(
        &self,
    ) -> Result<std::sync::Arc<Vec<PolicyRecord>>, PolicyCacheUnavailable> {
        if let Ok(guard) = self.degraded.read() {
            if let Some(reason) = guard.as_ref() {
                return Err(PolicyCacheUnavailable {
                    reason: reason.clone(),
                });
            }
        }
        match self.current.read() {
            Ok(guard) => Ok(std::sync::Arc::clone(&guard)),
            Err(poisoned) => Ok(std::sync::Arc::clone(&poisoned.into_inner())),
        }
    }

    /// The set a resolution should use: the current one when healthy, the
    /// LAST GOOD one when degraded — plus the degradation reason when it
    /// applies (the caller forces the action to `pause` and notes it).
    /// Never answers a set it never successfully loaded (an empty vec
    /// before the first load; the caller treats that as fail-closed too).
    pub(crate) fn current_or_last_good(
        &self,
    ) -> (std::sync::Arc<Vec<PolicyRecord>>, Option<String>) {
        let reason = match self.degraded.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let set = match self.current.read() {
            Ok(guard) => std::sync::Arc::clone(&guard),
            Err(poisoned) => std::sync::Arc::clone(&poisoned.into_inner()),
        };
        (set, reason)
    }

    /// Publish the next policy set, clear the degraded marker and stamp
    /// the fingerprint it was read at (the cross-process staleness check's
    /// baseline). Called only from a store-writer post-commit hook or the
    /// open/tick load, so swaps follow commit order.
    pub(crate) fn publish(&self, policies: Vec<PolicyRecord>) {
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = std::sync::Arc::new(policies);
        drop(guard);
        if let Ok(mut degraded) = self.degraded.write() {
            *degraded = None;
        }
    }

    /// Mark the cache untrustworthy (a failed load): the LAST good set
    /// stays for enforcement (the action forced to `pause`) until a
    /// successful publish clears it.
    pub(crate) fn degrade(&self, reason: String) {
        let mut degraded = match self.degraded.write() {
            Ok(degraded) => degraded,
            Err(poisoned) => poisoned.into_inner(),
        };
        *degraded = Some(reason);
    }

    /// Whether the policy table's fingerprint moved since the last load
    /// (the caller just read `now`). The fingerprint covers every row's
    /// name and update time, so ANY committed change — this engine's own
    /// (the writer is a different connection, so its commits move the
    /// read-only connection's change counters too) or another engine's —
    /// forces one reload; an unchanged table costs one tiny aggregate read
    /// per tick and nothing else.
    pub(crate) fn stale_at(&self, now: &str) -> bool {
        match self.loaded_fingerprint.read() {
            Ok(guard) => guard.as_deref() != Some(now),
            Err(poisoned) => poisoned.into_inner().as_deref() != Some(now),
        }
    }

    /// Stamp the fingerprint the current set was read at.
    pub(crate) fn stamp(&self, version: String) {
        let mut guard = match self.loaded_fingerprint.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(version);
    }
}

/// Why the cached policy set cannot be trusted (the fail-closed reason).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PolicyCacheUnavailable {
    pub reason: String,
}

impl std::fmt::Display for PolicyCacheUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// The stored per-Run resolution (schema v15) and the re-resolution paths.
/// The stored-resolution row (the resolution plus the inputs that produced
/// it, so a re-resolution inside a writer job reuses the caller's last
/// config read).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPolicy {
    /// The resolution.
    pub resolution: PolicyResolution,
    /// The inputs that produced it.
    pub inputs: ResolveInputs,
    /// When it was written (RFC 3339).
    pub updated_at: String,
}

/// Resolve `run`'s Policies against the store, write its `run_policy` row,
/// set/clear the resolution's Anomalies on the Run, and return the changes
/// (an `anomalies-changed` event and the snapshot entries when something
/// changed). `inputs` is the caller-resolved instance facts; the previous
/// row's stored inputs are reused when `None` (a label change cannot change
/// them). A Run with no row resolves nothing.
pub(crate) fn resolve_run<S: RunRecords + ?Sized>(
    store: &S,
    run: &RunId,
    inputs: Option<ResolveInputs>,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let Some(record) = store.run_record(run)? else {
        return Ok(RunChanges::default());
    };
    let prior = store.run_policy(run)?;
    let inputs = inputs
        .or_else(|| prior.as_ref().map(|row| row.inputs.clone()))
        .unwrap_or_default();
    let resolution = resolve_for(store, &record, &inputs)?;
    store_changes(store, record, &resolution, &inputs, at)
}

/// Store an ALREADY-COMPUTED resolution for `record` (the policy-change
/// job's path: it computed the resolution for the money half and stores the
/// same value — one resolution, one stored row).
pub(crate) fn resolve_and_store<S: RunRecords + ?Sized>(
    store: &S,
    record: &RunRecord,
    resolution: &PolicyResolution,
    inputs: &ResolveInputs,
    at: &str,
) -> Result<RunChanges, StoreError> {
    store_changes(store, record.clone(), resolution, inputs, at)
}

/// Compute `record`'s resolution against the store (the pure call over the
/// store's own facts).
fn resolve_for<S: RunRecords + ?Sized>(
    store: &S,
    record: &RunRecord,
    inputs: &ResolveInputs,
) -> Result<PolicyResolution, StoreError> {
    let labels = store.run_labels(&record.run_id)?;
    let run_limits = store.run_limits(&record.run_id)?;
    let kept = record
        .instance
        .as_deref()
        .map(|name| store.breach_default_kept(name))
        .transpose()?
        .unwrap_or(false);
    Ok(resolve(&ResolutionInput {
        depth: record.control_depth,
        wire_path: record.wire_path,
        bound_labels: labels
            .iter()
            .filter(|label| label.is_bound())
            .map(|label| (label.key.clone(), label.value.clone()))
            .collect(),
        run_limits,
        inputs: inputs.clone(),
        caller_limits: PolicyLimits::default(),
        kept_on_pause: kept,
        policies: store.policies()?,
    }))
}

/// Write the resolution row, set/clear its Anomalies, and note the changes.
fn store_changes<S: RunRecords + ?Sized>(
    store: &S,
    mut record: RunRecord,
    resolution: &PolicyResolution,
    inputs: &ResolveInputs,
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    if let Some(note) = &resolution.fail_closed {
        changes.notes.push(note.clone());
    }
    #[cfg(test)]
    changes.policy_resolved.push(record.run_id.clone());
    store.set_run_policy(&record.run_id, resolution, inputs, at)?;
    let labels = store.run_labels(&record.run_id)?;
    let run_limits = store.run_limits(&record.run_id)?;
    changes.entries.push(super::snapshot::entry_of(
        &record,
        &labels,
        run_limits,
        Some(resolution),
    ));
    let raised = resolution.raised_anomalies();
    let owned = [Anomaly::DepthFallback, Anomaly::UnpricedModel];
    let mut anomalies: Vec<Anomaly> = record
        .anomalies
        .iter()
        .copied()
        .filter(|anomaly| !owned.contains(anomaly))
        .collect();
    anomalies.extend(raised);
    anomalies.sort();
    anomalies.dedup();
    if anomalies != record.anomalies {
        record.anomalies = anomalies;
        record.updated_at = at.to_string();
        store.save_run(&record)?;
        super::ops::note_anomalies_changed(store, &mut changes, &record, at)?;
    }
    Ok(changes)
}

/// Re-resolve `run`'s Policies after its bound labels changed (AD-25): the
/// 21-9 hook, filled. Runs inside the label write's own writer job, after
/// the label write and before the snapshot entry is built, so the
/// re-resolution commits and publishes together with the label change that
/// caused it. The stored inputs are reused (a label change cannot change
/// the instance's explicit limits or model).
pub(crate) fn labels_changed<S: RunRecords + ?Sized>(
    store: &S,
    run: &RunId,
    at: &str,
) -> Result<RunChanges, StoreError> {
    resolve_run(store, run, None, at)
}

// ---------------------------------------------------------------------------
// Events (AD-14: new families for Policies and the FR-21 upgrade).
// ---------------------------------------------------------------------------

/// A Policy change event: created, changed or deleted. Committed with the
/// outbox row in the change's own writer job. The payload's own change
/// field serializes as `change` (the [`super::RunEvent`] precedent): the
/// bus envelope's tag is `kind`, and a payload field of the same name
/// would serialize twice and make the stored row unreadable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyEvent {
    /// The event family's schema version.
    pub schema_version: u32,
    /// Which change this is.
    #[serde(rename = "change")]
    pub kind: PolicyEventKind,
    /// The Policy's name.
    pub name: String,
    /// Its Selector, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
    /// The limits after the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<PolicyLimits>,
    /// The limits before a change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_limits: Option<PolicyLimits>,
    /// The model-keyed Rate table after the change (`default`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rates: BTreeMap<String, Rate>,
    /// How many non-ended Runs the (new) Selector matches.
    pub matched: usize,
    /// How many matched Runs fall back to `warn` at their depth.
    pub fallbacks: usize,
    /// When this happened (RFC 3339).
    pub at: String,
}

/// Which Policy change happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyEventKind {
    Created,
    Changed,
    Deleted,
}

impl PolicyEventKind {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            PolicyEventKind::Created => "created",
            PolicyEventKind::Changed => "changed",
            PolicyEventKind::Deleted => "deleted",
        }
    }
}

/// The one event the FR-21 upgrade emits: which instances were kept on
/// `pause`, and whether each one's `config.toml` was written (an unwritable
/// config keeps `pause` through its marker, with a diagnostic).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreachDefaultUpgradedEvent {
    /// The event family's schema version.
    pub schema_version: u32,
    /// The kept instances.
    pub instances: Vec<UpgradedInstance>,
    /// When the upgrade finished (RFC 3339).
    pub at: String,
}

/// One kept instance in a [`BreachDefaultUpgradedEvent`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradedInstance {
    /// The Agent Instance's name.
    pub name: String,
    /// Whether `pause` was written into its `config.toml` (`false` = kept
    /// through the marker, with a diagnostic).
    pub config_written: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runs::Label;

    fn policy(name: &str, selector: &str, limits: PolicyLimits) -> PolicyRecord {
        PolicyRecord {
            name: name.to_string(),
            selector: Some(selector.parse().unwrap()),
            limits,
            rates: BTreeMap::new(),
        }
    }

    fn input(labels: &[(&str, &str)]) -> ResolutionInput {
        ResolutionInput {
            depth: ControlDepth::Supervised,
            wire_path: true,
            bound_labels: labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            ..ResolutionInput::default()
        }
    }

    fn limits_of(resolution: &PolicyResolution) -> Option<u64> {
        resolution.tokens.per_run
    }

    #[test]
    fn the_action_always_resolves_to_the_default_policy_warn() {
        let mut input = input(&[]);
        input.policies = vec![PolicyRecord::default_policy()];
        let resolution = resolve(&input);
        assert_eq!(resolution.action, BreachAction::Warn);
        assert_eq!(resolution.effective_action, BreachAction::Warn);
        assert_eq!(resolution.scent_check, ScentCheckMode::Off);
        assert!(resolution.fail_closed.is_none());
        assert!(resolution.fallback.is_none());
    }

    #[test]
    fn a_missing_default_row_still_resolves_warn() {
        // The pure function never depends on the caller passing the default
        // row: it seeds the documented default itself (a store read that
        // FAILS fails closed before it gets here).
        let resolution = resolve(&input(&[]));
        assert_eq!(resolution.action, BreachAction::Warn);
    }

    #[test]
    fn explicit_outranks_every_policy_and_the_run_outranks_the_instance() {
        let mut input = input(&[("team", "core")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    tokens_per_run: Some(1_000),
                    breach_action: Some(BreachAction::Stop),
                    ..PolicyLimits::default()
                },
            ),
        ];
        input.inputs.explicit.tokens_per_run = Some(5_000);
        input.inputs.explicit.breach_action = Some(BreachAction::Pause);
        let resolution = resolve(&input);
        assert_eq!(limits_of(&resolution), Some(5_000));
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Instance
        ));
        assert_eq!(resolution.action, BreachAction::Pause);

        let mut with_run = input.clone();
        with_run.run_limits = RunLimits {
            token_per_run: Some(100),
            dollar_per_run_micros: None,
            breach_action: Some(BreachAction::Warn),
        };
        let resolution = resolve(&with_run);
        assert_eq!(limits_of(&resolution), Some(100));
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Run
        ));
        assert_eq!(resolution.action, BreachAction::Warn);
    }

    #[test]
    fn a_malformed_action_fails_closed_to_pause_with_a_note() {
        let mut input = input(&[]);
        input.policies = vec![PolicyRecord::default_policy()];
        input.inputs.malformed_action = Some("throttle".to_string());
        let resolution = resolve(&input);
        assert_eq!(resolution.action, BreachAction::Pause);
        let note = resolution.fail_closed.expect("fail-closed note");
        assert!(note.contains("throttle"), "{note}");
        assert!(note.contains("pause"), "{note}");
    }

    #[test]
    fn a_malformed_instance_action_outranks_the_run_level_action() {
        // The precedence chain's rule 1: a malformed explicit value fails
        // closed BEFORE every other source — the Run's own action included
        // (the pre-fix chain let the Run's `warn` win and never paused).
        let mut input = input(&[]);
        input.policies = vec![PolicyRecord::default_policy()];
        input.inputs.malformed_action = Some("1".to_string());
        input.run_limits = RunLimits {
            breach_action: Some(BreachAction::Warn),
            ..RunLimits::default()
        };
        let resolution = resolve(&input);
        assert_eq!(resolution.action, BreachAction::Pause);
        assert!(resolution.fail_closed.is_some());
    }

    #[test]
    fn the_upgrade_marker_holds_pause_above_the_policies() {
        let mut input = input(&[("team", "core")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    breach_action: Some(BreachAction::Stop),
                    ..PolicyLimits::default()
                },
            ),
        ];
        input.kept_on_pause = true;
        let resolution = resolve(&input);
        // FR-21: an upgraded instance kept on pause still pauses when a
        // Policy says otherwise — the marker sits at the explicit level.
        assert_eq!(resolution.action, BreachAction::Pause);
        assert!(matches!(
            resolution.sources.breach_action,
            LimitSource::Instance
        ));
    }

    #[test]
    fn the_most_specific_selector_wins_per_limit_whatever_the_value() {
        let mut input = input(&[("team", "core"), ("env", "prod")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "loose",
                "team=core",
                PolicyLimits {
                    tokens_per_run: Some(1_000),
                    breach_action: Some(BreachAction::Pause),
                    ..PolicyLimits::default()
                },
            ),
            policy(
                "tight",
                "team=core,env=prod",
                PolicyLimits {
                    tokens_per_run: Some(2_000),
                    breach_action: Some(BreachAction::Warn),
                    ..PolicyLimits::default()
                },
            ),
        ];
        let resolution = resolve(&input);
        // The strict superset wins per limit, even where its value is
        // LOOSER (FR-60: specificity decides, ties take the tightest).
        assert_eq!(limits_of(&resolution), Some(2_000));
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Policy(ref name) if name == "tight"
        ));
        assert_eq!(resolution.action, BreachAction::Warn);
        // A limit only the loose Policy sets still applies.
        let mut partial = input.clone();
        partial.policies[2].limits.tokens_per_run = None;
        let resolution = resolve(&partial);
        assert_eq!(limits_of(&resolution), Some(1_000));
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Policy(ref name) if name == "loose"
        ));
    }

    #[test]
    fn incomparable_selectors_tie_to_the_tightest_value() {
        let mut input = input(&[("a", "1"), ("b", "2"), ("c", "3")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "left",
                "a=1,b=2",
                PolicyLimits {
                    breach_action: Some(BreachAction::Warn),
                    tokens_per_run: Some(5_000),
                    ..PolicyLimits::default()
                },
            ),
            policy(
                "right",
                "a=1,c=3",
                PolicyLimits {
                    breach_action: Some(BreachAction::Pause),
                    tokens_per_run: Some(3_000),
                    ..PolicyLimits::default()
                },
            ),
        ];
        let resolution = resolve(&input);
        // Neither Selector is a superset of the other: tie → tightest
        // (pause over warn, the lower ceiling).
        assert_eq!(resolution.action, BreachAction::Pause);
        assert_eq!(limits_of(&resolution), Some(3_000));
        // An equal value names the alphabetically-first Policy.
        input.policies[2].limits.tokens_per_run = Some(5_000);
        let resolution = resolve(&input);
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Policy(ref name) if name == "left"
        ));
    }

    #[test]
    fn the_default_policy_fills_ceilings_too_and_every_wire_form_renders() {
        let mut input = input(&[]);
        let mut default = PolicyRecord::default_policy();
        default.limits.tokens_per_run = Some(9_000);
        input.policies = vec![default];
        let resolution = resolve(&input);
        assert_eq!(limits_of(&resolution), Some(9_000));
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Default
        ));
        // The Scent-Check modes' wire forms and Display (created before
        // Scent-Check ships; the resolve-side tie order below covers the
        // arms the create/change refusal leaves cold).
        for (mode, wire) in [
            (ScentCheckMode::Off, "off"),
            (ScentCheckMode::Shadow, "shadow"),
            (ScentCheckMode::Enforce, "enforce"),
        ] {
            assert_eq!(mode.as_str(), wire);
            assert_eq!(ScentCheckMode::from_wire(wire), Some(mode));
            assert_eq!(mode.to_string(), wire);
        }
        assert!(ScentCheckMode::from_wire("nope").is_none());
        for (kind, wire) in [
            (PolicyEventKind::Created, "created"),
            (PolicyEventKind::Changed, "changed"),
            (PolicyEventKind::Deleted, "deleted"),
        ] {
            assert_eq!(kind.as_str(), wire);
        }
        for (action, tightness) in [
            (BreachAction::Warn, 0),
            (BreachAction::Pause, 1),
            (BreachAction::Stop, 2),
            (BreachAction::Block, 3),
        ] {
            assert_eq!(action_tightness(action), tightness);
        }
    }

    #[test]
    fn the_scent_tie_order_runs_off_shadow_enforce() {
        // The resolve-side rule is total even though create/change refuse
        // `shadow`/`enforce` until their own story: the TIGHTEST mode wins
        // a tie, an equal value names the alphabetically-first Policy.
        let mut input = input(&[("a", "1")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "aaa",
                "a=1",
                PolicyLimits {
                    scent_check: Some(ScentCheckMode::Shadow),
                    ..PolicyLimits::default()
                },
            ),
            policy(
                "zzz",
                "a=1",
                PolicyLimits {
                    scent_check: Some(ScentCheckMode::Enforce),
                    ..PolicyLimits::default()
                },
            ),
        ];
        let resolution = resolve(&input);
        assert_eq!(resolution.scent_check, ScentCheckMode::Enforce);
        assert!(matches!(
            resolution.sources.scent_check,
            LimitSource::Policy(ref name) if name == "zzz"
        ));
        // An instance-set mode outranks every Policy.
        let mut with_instance = input.clone();
        with_instance.inputs.explicit.scent_check = Some(ScentCheckMode::Off);
        assert_eq!(resolve(&with_instance).scent_check, ScentCheckMode::Off);
    }

    #[test]
    fn a_poisoned_cache_still_answers_and_stays_stamped() {
        // The poisoned-lock arms: a poisoned RwLock never wedges the cache
        // (the guard is recovered) — fail-closed resolution must never
        // depend on lock hygiene.
        let cache = PolicyCache::default();
        cache.publish(vec![PolicyRecord::default_policy()]);
        {
            // Poison the lock: a panic while the write guard is LIVE (the
            // unwind drops the guard, which poisons the RwLock).
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let _ = std::panic::catch_unwind(|| {
                let _guard = cache.current.write().unwrap();
                panic!("poison the cache lock");
            });
            std::panic::set_hook(previous);
        }
        assert!(cache.current().is_ok(), "a poisoned lock still answers");
        cache.stamp("v1".to_string());
        assert!(!cache.stale_at("v1"), "the stamped fingerprint is current");
        assert!(cache.stale_at("v2"), "a moved fingerprint is stale");
        cache.publish(Vec::new());
        assert!(cache.current().is_ok());
    }

    #[test]
    fn caller_labels_never_select_a_policy() {
        let mut input = input(&[]);
        input.bound_labels.clear();
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    tokens_per_run: Some(1_000),
                    ..PolicyLimits::default()
                },
            ),
        ];
        // The Run's only `team=core` label is a Caller label: the Selector
        // grammar's bound-only scope never matches it.
        let labels = vec![Label::caller("team", "core")];
        assert!(!"team=core"
            .parse::<Selector>()
            .unwrap()
            .matches(&labels, crate::domain::SelectorScope::BoundOnly));
        let resolution = resolve(&input);
        assert_eq!(limits_of(&resolution), None);
        assert!(matches!(
            resolution.sources.tokens_per_run,
            LimitSource::Unset
        ));
    }

    #[test]
    fn the_rate_chain_is_instance_then_policy_then_the_default_table() {
        let mut input = input(&[("team", "core")]);
        let mut default = PolicyRecord::default_policy();
        default
            .rates
            .insert("gpt-demo".to_string(), Rate::new(1_000_000, 2_000_000));
        input.policies = vec![
            default,
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    rate: Some(Rate::new(3_000_000, 4_000_000)),
                    ..PolicyLimits::default()
                },
            ),
        ];
        input.inputs.model = Some("gpt-demo".to_string());

        // 1. A matching Policy's flat Rate beats the table.
        let resolution = resolve(&input);
        assert_eq!(resolution.rate, Some(Rate::new(3_000_000, 4_000_000)));
        assert!(matches!(
            resolution.sources.rate,
            LimitSource::Policy(ref name) if name == "p"
        ));

        // 2. The instance's explicit Rate beats both.
        input.inputs.explicit.rate = Some(Rate::new(9_000_000, 9_000_000));
        let resolution = resolve(&input);
        assert_eq!(resolution.rate, Some(Rate::new(9_000_000, 9_000_000)));
        assert!(matches!(resolution.sources.rate, LimitSource::Instance));

        // 3. The table entry when nothing else supplies a Rate.
        input.inputs.explicit.rate = None;
        input.policies[1].limits.rate = None;
        let resolution = resolve(&input);
        assert_eq!(resolution.rate, Some(Rate::new(1_000_000, 2_000_000)));
        assert!(matches!(resolution.sources.rate, LimitSource::Default));

        // The model match is exact and case-sensitive.
        input.inputs.model = Some("GPT-DEMO".to_string());
        assert_eq!(resolve(&input).rate, None);
    }

    #[test]
    fn a_flat_rate_tie_takes_the_higher_sum_then_the_first_name() {
        let mut input = input(&[("a", "1")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "aaa",
                "a=1",
                PolicyLimits {
                    rate: Some(Rate::new(1_000_000, 1_000_000)),
                    ..PolicyLimits::default()
                },
            ),
            policy(
                "zzz",
                "a=1",
                PolicyLimits {
                    rate: Some(Rate::new(2_000_000, 2_000_000)),
                    ..PolicyLimits::default()
                },
            ),
        ];
        let resolution = resolve(&input);
        // Higher sum (the conservative direction), not the name order.
        assert_eq!(resolution.rate, Some(Rate::new(2_000_000, 2_000_000)));
        assert!(matches!(
            resolution.sources.rate,
            LimitSource::Policy(ref name) if name == "zzz"
        ));
        // An equal sum names the alphabetically-first Policy.
        input.policies[2].limits.rate = Some(Rate::new(1_000_000, 1_000_000));
        let resolution = resolve(&input);
        assert!(matches!(
            resolution.sources.rate,
            LimitSource::Policy(ref name) if name == "aaa"
        ));
    }

    #[test]
    fn an_action_the_depth_cannot_execute_falls_back_to_warn() {
        let mut input = input(&[("team", "core")]);
        input.depth = ControlDepth::Announced;
        input.wire_path = true;
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    breach_action: Some(BreachAction::Pause),
                    ..PolicyLimits::default()
                },
            ),
        ];
        let resolution = resolve(&input);
        assert_eq!(resolution.action, BreachAction::Pause);
        assert_eq!(resolution.effective_action, BreachAction::Warn);
        let fallback = resolution.fallback.clone().expect("fallback");
        assert_eq!(fallback.policy, "p");
        assert_eq!(fallback.action, BreachAction::Pause);
        assert_eq!(resolution.raised_anomalies(), vec![Anomaly::DepthFallback]);

        // The same action executes at the supervised depth, and `block`
        // needs a wire path even there.
        input.depth = ControlDepth::Supervised;
        input.wire_path = false;
        input.policies[1].limits.breach_action = Some(BreachAction::Block);
        let resolution = resolve(&input);
        assert_eq!(resolution.effective_action, BreachAction::Warn);
        input.wire_path = true;
        let resolution = resolve(&input);
        assert_eq!(resolution.effective_action, BreachAction::Block);
    }

    #[test]
    fn a_policy_cap_without_a_rate_raises_the_unpriced_anomaly() {
        let mut input = input(&[("team", "core")]);
        input.policies = vec![
            PolicyRecord::default_policy(),
            policy(
                "p",
                "team=core",
                PolicyLimits {
                    dollars_per_run_micros: Some(5_000_000),
                    ..PolicyLimits::default()
                },
            ),
        ];
        input.inputs.model = Some("gpt-demo".to_string());
        let resolution = resolve(&input);
        let unpriced = resolution.unpriced.clone().expect("unpriced");
        assert_eq!(unpriced.model, "gpt-demo");
        assert_eq!(unpriced.policy, "p");
        assert_eq!(resolution.raised_anomalies(), vec![Anomaly::UnpricedModel]);
        // An explicit-instance cap with no Rate keeps the documented
        // inert-with-notice (no Anomaly — FR-20).
        input.policies[1].limits.dollars_per_run_micros = None;
        input.inputs.explicit.dollars_per_run_micros = Some(5_000_000);
        assert!(resolve(&input).unpriced.is_none());
        // A Rate clears it.
        input.policies[1].limits.dollars_per_run_micros = Some(5_000_000);
        input.inputs.explicit.rate = Some(Rate::new(1, 1));
        assert!(resolve(&input).unpriced.is_none());
        // No model configured names it (the explicit cap stays cleared).
        input.inputs.explicit.rate = None;
        input.inputs.explicit.dollars_per_run_micros = None;
        input.inputs.model = None;
        assert_eq!(
            resolve(&input).unpriced.expect("unpriced").model,
            "no model configured"
        );
    }

    #[test]
    fn dominance_is_a_strict_term_set_superset() {
        let a: Selector = "a=1".parse().unwrap();
        let ab: Selector = "a=1,b=2".parse().unwrap();
        let ac: Selector = "a=1,c=3".parse().unwrap();
        assert!(dominates(&ab, &a));
        assert!(!dominates(&a, &ab));
        assert!(!dominates(&ab, &ac));
        assert!(!dominates(&a, &a));
    }
}
