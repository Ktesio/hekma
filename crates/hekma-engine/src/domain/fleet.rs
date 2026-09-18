//! The serializable Fleet view (spine AD-14 seed) — the `kt --json` shape.
//!
//! [`FleetEntry`] is the per-instance row `kt agent list`/`show` renders and, in
//! `--json` mode, serializes. It COMPOSES the already-`Serialize` domain types —
//! [`InstanceName`](crate::InstanceName) (a plain string on the wire),
//! [`LifecycleState`] (snake_case), and [`RestartPolicy`] (kebab-case) — rather
//! than hand-rolling field serialization, so `kt --json` and the future 7-2 Host
//! event stream share ONE schema (AD-14: "one event schema, two consumers").
//!
//! ## Metering fields — usage (3-1), budget (3-2), DOLLARS (3-3), Fleet totals (3-5)
//!
//! Metering is Epic 3 (AD-7). Story 3-1 makes [`FleetEntry::usage`] REAL: a
//! [`UsageView`] carrying the instance's cumulative (and current-Run) TOKEN totals
//! from the Usage Ledger. The active [`FleetEntry::metering_source`] rides alongside
//! it (AC-C — read from the persisted adapter snapshot). Story 3-2 makes
//! [`FleetEntry::budget`] REAL for TOKENS: a [`BudgetView`] carrying the configured
//! per-run + cumulative ceilings, the Breach Action, and the REMAINING tokens per
//! scope. Story 3-3 makes the DOLLAR dimension real WHEN a Rate is configured: the
//! [`UsageView`] gains the DERIVED cost + [`EstimateLabel`], and the [`BudgetView`]
//! gains the dollar Cost Cap + dollars-remaining per scope — all as INTEGER MICROS +
//! the label (NEVER a `$` string on the wire — AD-14; the human render is `kt`'s,
//! through the one currency module). The dollar fields stay ABSENT for a no-Rate
//! instance (AC-B: no Rate ⇒ no dollar figure, never a fabricated `$0.00`), and
//! `budget` stays `Option`-typed (`None` when NEITHER a token budget nor an
//! enforceable dollar cap exists — a truthful ABSENCE). Populating these fields is a
//! backward-additive change.
//!
//! Story 3-5 adds the Fleet-WIDE read: [`FleetTotals`] aggregates the per-instance
//! rows into total tokens (input/output over every instance, every Run) + a total
//! derived dollar figure, computed by the PURE [`FleetTotals::from_entries`] over the
//! already-composed [`FleetEntry`] rows (no second ledger query — AD-2/AD-6). It rides
//! on the [`FleetListing`] document (`totals`), and [`FleetListing::new`] computes it
//! internally so the document is always self-consistent (the total is DERIVED from the
//! rows it carries). Because the document GAINS a first-class aggregate that `--json`
//! consumers + the future 7-2 Host stream negotiate on, [`crate::FLEET_SCHEMA_VERSION`]
//! bumps 1 → 2 (ADDITIVE: a new reader parses every old v1 document, no field is
//! renamed/removed; a v1 consumer that ignores `totals` still parses `instances`).
//!
//! ## Boundary (what this is NOT)
//!
//! This is a READ/VIEW type: it holds no logic, spawns nothing, and builds no
//! paths. [`crate::Engine::fleet`] composes it from the existing `list()` +
//! `instance_status()` reads (one SQLite read pass). It is the AD-14 SEED, not
//! the 7-2 subscription bus.

use serde::{Deserialize, Serialize};

use super::budget::{BreachAction, TokenBudget};
use super::cost::{CostCap, EstimateLabel, Micros};
use super::event::FLEET_SCHEMA_VERSION;
use super::lifecycle::LifecycleState;
use super::name::InstanceName;
use super::restart::RestartPolicy;
use super::usage::UsageTotals;

/// The per-instance budget/status surfaced in Fleet detail — TOKEN ceilings
/// (story 3-2, AC9) PLUS the DOLLAR Cost Cap + dollars-remaining when a Rate is
/// configured (story 3-3, AD-8; absent otherwise).
///
/// Present iff a budget is CONFIGURED (at least one scope set); an un-budgeted
/// instance carries `None` in [`FleetEntry::budget`] (a truthful absence, never a
/// fabricated `0` ceiling). Carries the configured per-run + cumulative ceilings,
/// the Breach Action, and the REMAINING tokens per scope (ceiling − current total,
/// saturating at 0 — a breached scope reports `0` remaining, never a negative).
/// The remaining values are computed from the SAME ledger totals `usage` reports,
/// so they equal the Usage Ledger exactly (the FR-22 discipline). snake_case on
/// the wire (AD-14); `Deserialize` so `--json` round-trips.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetView {
    /// The configured per-run token ceiling (`null` when the per-run scope is
    /// unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_run_limit: Option<u64>,
    /// Remaining tokens in the per-run scope for the CURRENT Run (ceiling − current
    /// Run total, saturating at 0). `null` when the per-run scope is unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_run_remaining: Option<u64>,
    /// The configured cumulative token ceiling (`null` when the cumulative scope is
    /// unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_limit: Option<u64>,
    /// Remaining tokens in the cumulative scope (ceiling − cumulative total,
    /// saturating at 0). `null` when the cumulative scope is unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_remaining: Option<u64>,
    /// The configured per-run DOLLAR Cost Cap in micro-dollars (story 3-3) —
    /// present ONLY when a Rate is configured AND the per-run dollar scope is set;
    /// `null`/absent otherwise (AC-B: a cap with no Rate is inert → no dollar
    /// figure). NEVER a `$` string (AD-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_run_cost_cap: Option<Micros>,
    /// Remaining dollars in the per-run scope (cap − current-Run derived cost,
    /// saturating at 0 — a breached scope reports `$0`, never negative) in
    /// micro-dollars (story 3-3). Present under the same condition as
    /// [`per_run_cost_cap`](Self::per_run_cost_cap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_run_dollars_remaining: Option<Micros>,
    /// The configured cumulative DOLLAR Cost Cap in micro-dollars (story 3-3) —
    /// present ONLY when a Rate is configured AND the cumulative dollar scope is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_cost_cap: Option<Micros>,
    /// Remaining dollars in the cumulative scope (cap − cumulative derived cost,
    /// saturating at 0) in micro-dollars (story 3-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_dollars_remaining: Option<Micros>,
    /// The estimate label on the dollar cap/remaining figures (story 3-3, AD-8) —
    /// present ONLY when a Rate is configured; v1 always `estimated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_label: Option<EstimateLabel>,
    /// The configured Breach Action (`pause` / `stop` / `warn`). Governs BOTH the
    /// token and the dollar breach (story 3-3: one action).
    pub breach_action: BreachAction,
}

impl BudgetView {
    /// Build a [`BudgetView`] from the resolved [`TokenBudget`] + [`BreachAction`]
    /// and the CURRENT per-run + cumulative token totals (from the Usage Ledger),
    /// or `None` when no budget is configured (an honest absent budget — AC9).
    ///
    /// Remaining is `ceiling.saturating_sub(total)` per scope, so a scope that has
    /// reached/exceeded its ceiling reports `0` remaining (never a negative). Only
    /// a SET scope contributes a limit/remaining; an unset scope stays `None`.
    pub fn from_budget(
        budget: &TokenBudget,
        action: BreachAction,
        current_run_total: u64,
        cumulative_total: u64,
    ) -> Option<Self> {
        if !budget.is_set() {
            return None;
        }
        Some(Self {
            per_run_limit: budget.per_run,
            per_run_remaining: budget.per_run.map(|c| c.saturating_sub(current_run_total)),
            cumulative_limit: budget.cumulative,
            cumulative_remaining: budget
                .cumulative
                .map(|c| c.saturating_sub(cumulative_total)),
            per_run_cost_cap: None,
            per_run_dollars_remaining: None,
            cumulative_cost_cap: None,
            cumulative_dollars_remaining: None,
            estimate_label: None,
            breach_action: action,
        })
    }

    /// Build a combined token + DOLLAR [`BudgetView`] (story 3-3, AC10) — present
    /// iff a token budget OR a dollar Cost Cap is configured. The token fields come
    /// from `budget` + the token totals (as [`Self::from_budget`]); the DOLLAR
    /// fields come from `cost_cap` + the DERIVED costs, present ONLY when
    /// `has_rate` (a cap with no Rate is inert — AC-B: no dollar figure surfaced).
    /// Remaining is `cap.saturating_remaining(cost)` per scope (a breached scope →
    /// `$0`, never negative). Returns `None` when NEITHER a token budget nor an
    /// enforceable dollar cap exists (an honest absent budget — never a fabricated
    /// ceiling).
    #[allow(clippy::too_many_arguments)]
    pub fn from_budget_and_cost(
        budget: &TokenBudget,
        cost_cap: &CostCap,
        action: BreachAction,
        current_run_total: u64,
        cumulative_total: u64,
        has_rate: bool,
        current_run_cost: Micros,
        cumulative_cost: Micros,
        label: EstimateLabel,
    ) -> Option<Self> {
        // The dollar dimension is surfaced ONLY when a Rate exists AND a cap scope is
        // set (AC-B inert otherwise: no Rate ⇒ no dollar figure at all).
        let dollar_active = has_rate && cost_cap.is_set();
        // Present iff SOMETHING is configured (a token budget OR an enforceable cap).
        if !budget.is_set() && !dollar_active {
            return None;
        }
        Some(Self {
            per_run_limit: budget.per_run,
            per_run_remaining: budget.per_run.map(|c| c.saturating_sub(current_run_total)),
            cumulative_limit: budget.cumulative,
            cumulative_remaining: budget
                .cumulative
                .map(|c| c.saturating_sub(cumulative_total)),
            per_run_cost_cap: dollar_active.then_some(cost_cap.per_run).flatten(),
            per_run_dollars_remaining: dollar_active
                .then(|| {
                    cost_cap
                        .per_run
                        .map(|c| c.saturating_remaining(current_run_cost))
                })
                .flatten(),
            cumulative_cost_cap: dollar_active.then_some(cost_cap.cumulative).flatten(),
            cumulative_dollars_remaining: dollar_active
                .then(|| {
                    cost_cap
                        .cumulative
                        .map(|c| c.saturating_remaining(cumulative_cost))
                })
                .flatten(),
            // The label rides only when the dollar dimension is active.
            estimate_label: dollar_active.then_some(label),
            breach_action: action,
        })
    }
}

/// The per-instance Usage Ledger totals surfaced in Fleet detail (story 3-1
/// tokens + story 3-3 dollars, AC-C/AC11/AC10).
///
/// Carries the CUMULATIVE token totals (summed over every Run) and the
/// CURRENT-RUN totals (the active `starting`→terminal span, or zero when the
/// instance is not currently running). Story 3-3 adds the DERIVED DOLLAR cost
/// (cumulative + current-run, integer micros) + the [`EstimateLabel`] — present
/// ONLY when a Rate is configured, `None`/absent otherwise (AC-B honest absence:
/// no Rate ⇒ NO dollar figure, never a fabricated `$0.00`). The dollar cost equals
/// the Usage-Ledger-derived cost exactly (each row priced at its own persisted
/// Rate — FR-22). The wire carries INTEGER MICROS + the label, NEVER a `$` string
/// (AD-14). snake_case on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageView {
    /// Cumulative input (prompt) tokens over all of the instance's Runs.
    pub cumulative_input_tokens: u64,
    /// Cumulative output (completion) tokens over all of the instance's Runs.
    pub cumulative_output_tokens: u64,
    /// Input tokens for the CURRENT Run (0 when the instance is not running).
    pub current_run_input_tokens: u64,
    /// Output tokens for the CURRENT Run (0 when the instance is not running).
    pub current_run_output_tokens: u64,
    /// The DERIVED cumulative dollar cost in micro-dollars (story 3-3) — present
    /// ONLY when a Rate is configured; `null`/absent otherwise (AC-B: no Rate ⇒ no
    /// dollar figure). Equals the ledger-derived cost exactly (FR-22).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_dollars: Option<Micros>,
    /// The DERIVED current-Run dollar cost in micro-dollars (story 3-3) — present
    /// ONLY when a Rate is configured; `null`/absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run_dollars: Option<Micros>,
    /// The estimate label on the dollar figures (story 3-3, AD-8) — present ONLY
    /// when a Rate is configured; v1 always `estimated`. NEVER a `$` string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_label: Option<EstimateLabel>,
}

impl UsageView {
    /// Build a TOKENS-ONLY [`UsageView`] from the cumulative + current-run
    /// [`UsageTotals`] the Fleet read summed from the ledger (the dollar fields
    /// absent — the no-Rate case, AC-B). Story 3-3's [`Self::with_dollars`] adds the
    /// derived cost when a Rate is present.
    pub fn new(cumulative: UsageTotals, current_run: UsageTotals) -> Self {
        Self {
            cumulative_input_tokens: cumulative.input_tokens,
            cumulative_output_tokens: cumulative.output_tokens,
            current_run_input_tokens: current_run.input_tokens,
            current_run_output_tokens: current_run.output_tokens,
            cumulative_dollars: None,
            current_run_dollars: None,
            estimate_label: None,
        }
    }

    /// Attach the DERIVED dollar cost + label to a [`UsageView`] (story 3-3) — the
    /// Rate-present case. The costs are the ledger-derived micros (each row priced
    /// at its own persisted Rate — no retro-repricing); the label is v1 `estimated`.
    pub fn with_dollars(
        mut self,
        cumulative_dollars: Micros,
        current_run_dollars: Micros,
        label: EstimateLabel,
    ) -> Self {
        self.cumulative_dollars = Some(cumulative_dollars);
        self.current_run_dollars = Some(current_run_dollars);
        self.estimate_label = Some(label);
        self
    }

    /// The cumulative total tokens (input + output over all Runs), saturating.
    pub fn cumulative_total_tokens(&self) -> u64 {
        self.cumulative_input_tokens
            .saturating_add(self.cumulative_output_tokens)
    }
}

/// One Agent Instance as the Fleet listing / `--json` document sees it (story
/// 1-7, FR-4).
///
/// Composes the registry identity (`name`, `kind`, `agent_home`), the live
/// runtime status (`state`, `restart_count`, `restart_policy`), and the REAL
/// metering view: `usage` (token totals real since story 3-1, plus the derived
/// dollar cost when a Rate exists since 3-3) and `budget` (the configured TOKEN
/// ceilings + Breach Action real since 3-2, plus the dollar Cost Cap + headroom
/// when a Rate exists since 3-3; `null` only when NO budget is configured). The
/// dollar fields stay a typed absence when no Rate exists (AC-B). Field names are
/// snake_case so the `--json` document is stable and re-parseable. The Fleet-WIDE
/// sum across these rows is [`FleetTotals`] (story 3-5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetEntry {
    /// Fleet-unique instance name (serializes as a plain string).
    pub name: InstanceName,
    /// Agent kind (the adapter identity).
    pub kind: String,
    /// Current Lifecycle State, read live from the store (snake_case on the wire).
    pub state: LifecycleState,
    /// Consecutive-failure restart count (story 1-6). `0` when never restarted.
    pub restart_count: u32,
    /// The effective per-instance Restart Policy (story 1-6; kebab-case on the
    /// wire).
    pub restart_policy: RestartPolicy,
    /// The last-known failed cause, present for a `failed` instance (story 1-6).
    /// `None` (JSON `null`) otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_cause: Option<String>,
    /// Token budget / status (story 3-2, AC9) — the configured per-run + cumulative
    /// ceilings, the Breach Action, and the REMAINING tokens per scope, PLUS the
    /// dollar Cost Cap + dollars-remaining when a Rate is configured (story 3-3,
    /// AD-8; absent otherwise). `None` (JSON `null`) for an
    /// instance with NO budget configured — a truthful absence, never a fabricated
    /// `0` ceiling. The remaining values equal the Usage Ledger exactly (FR-22).
    /// ALWAYS PRESENT on the wire (rendered as `null` when absent), preserving the
    /// 3-1 `budget` key so `--json` consumers see a stable shape.
    pub budget: Option<BudgetView>,
    /// Usage Ledger token totals (story 3-1, AC-C/AC11) — cumulative + current-Run,
    /// TOKENS ONLY (AD-8). ALWAYS PRESENT (a concrete object, never `null`): a
    /// never-metered instance shows an honest all-ZERO [`UsageView`] (a truthful
    /// zero — the ledger genuinely holds zero tokens for it), distinct from the
    /// Epic-1 `budget` `null` "does not exist yet". The totals equal the ledger
    /// exactly (the FR-22 discipline). The derived dollar cost rides here when a Rate
    /// is configured (story 3-3), absent otherwise (AC-B).
    pub usage: UsageView,
    /// The active Metering Source wire string (`self-reported` / `engine-observed`),
    /// visible in Fleet detail (AC-C). Read from the persisted adapter snapshot.
    pub metering_source: String,
    /// Absolute Agent Home path (engine-computed; the path authority).
    pub agent_home: String,
}

impl FleetEntry {
    /// The human-readable ABSENCE token rendered in a Fleet cell that has no value
    /// (story 1-7). A single `—` — used for the `budget` cell of an UN-budgeted
    /// instance (no budget configured — story 3-2 keeps this honest absence) and
    /// wherever else a truthful "nothing here" is shown. Kept here so `list` and
    /// `show` render the SAME token.
    pub const METERING_SEED_CELL: &'static str = "—";
}

/// The Fleet-WIDE usage + cost aggregate (story 3-5, FR-22 — the greenfield).
///
/// Sums the already-composed per-instance [`FleetEntry`] rows into a single honest
/// total: cumulative input/output tokens across EVERY instance (every Run), and the
/// summed DERIVED dollar cost across only the instances that HAVE a Rate. It is
/// computed PURELY by [`FleetTotals::from_entries`] over the `Vec<FleetEntry>` the
/// Fleet read already built (no second ledger query — AD-2/AD-6), using the SAME
/// saturating integer discipline the per-instance totals use ([`u64`] saturating add
/// for tokens; [`Micros::saturating_add`] for dollars), so no aggregate can wrap or
/// touch a float.
///
/// ## The honesty rules (the crux — AD-8 / FR-23 / SM-C3)
///
/// A naive `sum()` would be dishonest three ways this type prevents:
///
/// 1. **Label the estimate.** v1 EVERY derived dollar is `estimated` (3-3), so any
///    non-empty dollar total carries [`EstimateLabel::Estimated`]. There is NO path
///    to a `reconciled` Fleet total in v1 (the arm is an unreachable forward seam
///    until reconciliation ingestion ships). Mixing metering sources
///    (`self-reported` + `engine-observed`) does not change this — both are
///    estimates; the per-instance `metering_source` stays visible in the rows the
///    total summarizes, so a reader sees the mix (AC7) without the aggregate
///    enumerating per-source subtotals.
/// 2. **Zero-not-absent — count the honest zero, never fabricate/omit.** EVERY entry
///    contributes its real token total (a never-metered instance's genuine `0` is
///    COUNTED — the ledger truly holds zero for it, 3-1's truthful zero — never
///    skipped); a no-Rate instance contributes `0` dollars but is NOT claimed to have
///    cost `$0.00` (its dollar cost is UNKNOWN, not zero).
/// 3. **Say so when partial.** If a metered instance has NO Rate, its real token
///    consumption has an unknown dollar cost the aggregate CANNOT include — so a
///    non-`None` [`total_dollars`](Self::total_dollars) is then a LOWER BOUND, flagged
///    [`dollars_partial`](Self::dollars_partial). It is NEVER presented as the exact
///    Fleet cost (SM-C3 — honesty outranks precision).
///
/// The dollar shape encodes all three: `total_dollars` is `None` when NO instance has
/// a Rate (nothing to estimate); `Some(sum)` with `estimate_label = Some(Estimated)`
/// when at least one does; and `dollars_partial = true` additionally when SOME but not
/// all metered instances have Rates. Integer micros + the label on the wire (snake_case,
/// NO `f64`, NO `$` string — AD-14); the human render routes through the ONE currency
/// module ([`render_dollars`](super::cost::render_dollars)/[`render_dollars_bare`](super::cost::render_dollars_bare)).
///
/// The Fleet total is CUMULATIVE only — there is no cross-instance per-Run scope (a
/// "per-run" total is meaningful per-instance, where each instance is in its own Run,
/// not across instances that are each in different Runs). Recorded decision (AC3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetTotals {
    /// Cumulative input (prompt) tokens summed over EVERY instance, every Run
    /// (saturating). Always present — a never-metered instance contributes a
    /// truthful `0` (zero-not-absent, AC4).
    pub total_input_tokens: u64,
    /// Cumulative output (completion) tokens summed over EVERY instance, every Run
    /// (saturating). Always present (zero-not-absent, AC4).
    pub total_output_tokens: u64,
    /// The summed DERIVED dollar cost across only the instances that HAVE a Rate, in
    /// micro-dollars (saturating). `None` when NO instance has a Rate (nothing to
    /// estimate — an honest absence, never a fabricated `$0.00`); `Some(sum)` when at
    /// least one does. When `Some` and [`dollars_partial`](Self::dollars_partial) is
    /// `true`, this is a LOWER BOUND (some metered instances are unpriced). NEVER a
    /// `$` string on the wire (AD-14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_dollars: Option<Micros>,
    /// `true` when the dollar total OMITS a metered-but-unpriced instance's real (but
    /// un-derivable) consumption — i.e. SOME but not all metered instances have Rates.
    /// Signals that [`total_dollars`](Self::total_dollars) is a lower bound, not the
    /// exact Fleet cost (AC5, SM-C3). `false` when the total is complete (all metered
    /// instances have Rates) OR when there is nothing to estimate (`total_dollars` is
    /// `None`).
    pub dollars_partial: bool,
    /// How many instances are metered-but-unpriced — the ones with real usage tokens
    /// (`> 0`) but NO derived dollar cost (no Rate), i.e. exactly the instances whose
    /// missing cost makes [`dollars_partial`](Self::dollars_partial) `true`. Lets the
    /// human footer name the count ("N instances unpriced") so the reader knows the
    /// basis of the lower bound (AC7); `0` whenever `dollars_partial` is `false`. An
    /// ADDITIVE v2 field (serializes as a plain integer, never a `$` string — AD-14);
    /// a v1/older consumer that ignores it still parses the rest.
    #[serde(default)]
    pub unpriced_count: usize,
    /// The estimate label on [`total_dollars`](Self::total_dollars) (AD-8) — present
    /// (`Some`) iff a dollar was summed; v1 always [`EstimateLabel::Estimated`].
    /// `None` when `total_dollars` is `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate_label: Option<EstimateLabel>,
}

impl FleetTotals {
    /// The PURE Fleet-wide aggregate (story 3-5, AC-A/AC3/AC4/AC5) — sum the
    /// already-composed per-instance rows with the honesty rules. NO engine, NO I/O:
    /// unit-testable in isolation and cross-OS by construction.
    ///
    /// Tokens: sum `usage.cumulative_input_tokens`/`cumulative_output_tokens`
    /// (saturating) over ALL entries — every entry counts, including a never-metered
    /// instance's honest `0` (zero-not-absent, AC4). Dollars: sum
    /// `usage.cumulative_dollars` (via [`Micros::saturating_add`]) over ONLY the
    /// entries that carry one (a Rate is configured); an entry with no Rate contributes
    /// `0` dollars but marks the total PARTIAL rather than being counted as `$0.00`
    /// (AC5). The label is `Some(Estimated)` iff any dollar was summed (v1 — AC5);
    /// `total_dollars` is `None` when NO instance has a Rate (nothing to estimate).
    pub fn from_entries(entries: &[FleetEntry]) -> Self {
        let mut total_input_tokens: u64 = 0;
        let mut total_output_tokens: u64 = 0;
        // The running dollar sum + whether ANY instance contributed a dollar figure
        // (had a Rate) + how many METERED instances lacked one (each → partial, and the
        // count is what the human footer names).
        let mut dollar_sum = Micros::ZERO;
        let mut any_rate = false;
        let mut unpriced_count: usize = 0;

        for entry in entries {
            let usage = &entry.usage;
            // Tokens ALWAYS count (zero-not-absent — a never-metered instance's real 0
            // is in the sum, never skipped).
            total_input_tokens = total_input_tokens.saturating_add(usage.cumulative_input_tokens);
            total_output_tokens =
                total_output_tokens.saturating_add(usage.cumulative_output_tokens);
            // Dollars: sum ONLY where a derived cost exists (a Rate is configured).
            match usage.cumulative_dollars {
                Some(cost) => {
                    any_rate = true;
                    dollar_sum = dollar_sum.saturating_add(cost);
                }
                None => {
                    // A metered instance with NO Rate has real tokens but an UNKNOWN
                    // dollar cost the aggregate cannot include — its presence makes the
                    // dollar total a lower bound (partial), NOT a fabricated $0.00. An
                    // instance with zero tokens either way contributes nothing to hide,
                    // so only a metered-but-unpriced instance is counted toward the
                    // partial-ness (and the footer's "N unpriced" note).
                    if usage.cumulative_total_tokens() > 0 {
                        unpriced_count += 1;
                    }
                }
            }
        }

        // The dollar total is present iff SOMETHING was priced (a Rate existed). With
        // nothing priced there is nothing to estimate → an honest absence (`None`),
        // never a $0.00 that would imply zero cost.
        let (total_dollars, estimate_label) = if any_rate {
            (Some(dollar_sum), Some(EstimateLabel::Estimated))
        } else {
            (None, None)
        };
        // Partial ONLY when we DID price some dollars but a metered instance was left
        // unpriced (a lower bound that must say so). With no dollar total at all there
        // is no lower bound to qualify — so the reported count stays `0` in lockstep
        // with `dollars_partial == false` (no lower bound ⇒ nothing to name).
        let dollars_partial = any_rate && unpriced_count > 0;
        let unpriced_count = if dollars_partial { unpriced_count } else { 0 };

        Self {
            total_input_tokens,
            total_output_tokens,
            total_dollars,
            dollars_partial,
            unpriced_count,
            estimate_label,
        }
    }

    /// The combined total tokens (input + output across the whole Fleet), saturating.
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens
            .saturating_add(self.total_output_tokens)
    }
}

/// The `kt agent list --json` document (story 1-7, AD-14; the Fleet-WIDE aggregate
/// story 3-5).
///
/// A versioned wrapper carrying [`FLEET_SCHEMA_VERSION`], the per-instance
/// [`FleetEntry`] rows, and the Fleet-wide [`FleetTotals`] aggregate, so `--json`
/// consumers (and the future 7-2 Host stream) negotiate on the version and never see
/// an unversioned document. An empty Fleet serializes with an empty `instances` array
/// + an all-zero / `None`-dollars `totals` (valid JSON — AC9).
///
/// The `totals` is DERIVED from `instances` inside [`FleetListing::new`], so the
/// document is always self-consistent (the aggregate equals
/// [`FleetTotals::from_entries`] over the rows it carries — never passed
/// independently, never able to drift from the rows).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetListing {
    /// The Fleet document schema version ([`FLEET_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The Fleet-WIDE usage + cost aggregate over `instances` (story 3-5), computed
    /// in [`FleetListing::new`] so it stays consistent with the rows.
    pub totals: FleetTotals,
    /// Every Agent Instance in the Fleet, ordered by name.
    pub instances: Vec<FleetEntry>,
}

impl FleetListing {
    /// Build a listing document from the composed entries, stamping the current
    /// [`FLEET_SCHEMA_VERSION`] and computing the Fleet-wide [`FleetTotals`] PURELY
    /// from those entries (self-consistent — the total is derived from the rows, one
    /// read pass, no second ledger query — AD-2/AD-6).
    pub fn new(instances: Vec<FleetEntry>) -> Self {
        let totals = FleetTotals::from_entries(&instances);
        Self {
            schema_version: FLEET_SCHEMA_VERSION,
            totals,
            instances,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str) -> FleetEntry {
        FleetEntry {
            name: InstanceName::new(name).unwrap(),
            kind: "mock".to_string(),
            state: LifecycleState::Registered,
            restart_count: 0,
            restart_policy: RestartPolicy::OnFailure,
            failed_cause: None,
            budget: None,
            usage: UsageView::new(UsageTotals::zero(), UsageTotals::zero()),
            metering_source: "self-reported".to_string(),
            agent_home: format!("/x/agents/{name}"),
        }
    }

    #[test]
    fn entry_serializes_budget_as_null_when_unbudgeted_and_usage_as_real_zero_tokens() {
        // Story 3-2: an UN-budgeted instance's `budget` is an honest `null` (a
        // truthful absence, never a fabricated `0`), while `usage` is a REAL
        // UsageView — a concrete all-zero object for a never-metered instance.
        let entry = sample_entry("demo");
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        // budget: the honest absence (present as null, key preserved for consumers).
        assert_eq!(value["budget"], serde_json::Value::Null, "{value}");
        // usage: a real object with zero token totals (NOT null, NOT a number).
        assert!(
            value["usage"].is_object(),
            "usage must be an object: {value}"
        );
        assert_eq!(
            value["usage"]["cumulative_input_tokens"],
            serde_json::json!(0)
        );
        assert_eq!(
            value["usage"]["cumulative_output_tokens"],
            serde_json::json!(0)
        );
        // The active Metering Source is surfaced (AC-C).
        assert_eq!(value["metering_source"], serde_json::json!("self-reported"));
    }

    #[test]
    fn entry_carries_real_usage_totals_when_metered() {
        // A metered instance surfaces real cumulative + current-run token totals,
        // snake_case on the wire (AC11).
        let mut entry = sample_entry("demo");
        entry.usage = UsageView::new(
            UsageTotals {
                input_tokens: 100,
                output_tokens: 250,
            },
            UsageTotals {
                input_tokens: 40,
                output_tokens: 60,
            },
        );
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(
            value["usage"]["cumulative_input_tokens"],
            serde_json::json!(100)
        );
        assert_eq!(
            value["usage"]["cumulative_output_tokens"],
            serde_json::json!(250)
        );
        assert_eq!(
            value["usage"]["current_run_input_tokens"],
            serde_json::json!(40)
        );
        assert_eq!(entry.usage.cumulative_total_tokens(), 350);
        // Still tokens only — no dollars leaked into the view.
        assert!(value["usage"].get("cost").is_none());
        assert!(value["usage"].get("dollars").is_none());
    }

    // ---- Story 3-2: BudgetView (AC9) ----

    #[test]
    fn budget_view_is_none_when_no_scope_configured() {
        // An un-budgeted instance yields None (an honest absent budget).
        let v = BudgetView::from_budget(&TokenBudget::none(), BreachAction::Pause, 100, 200);
        assert!(v.is_none());
    }

    #[test]
    fn budget_view_carries_limits_remaining_and_action() {
        // A budgeted instance: limits + remaining (ceiling − total, per scope) +
        // the action. Tokens only — no dollar cap/headroom.
        let budget = TokenBudget {
            per_run: Some(100),
            cumulative: Some(1000),
        };
        let v = BudgetView::from_budget(&budget, BreachAction::Stop, 30, 400).unwrap();
        assert_eq!(v.per_run_limit, Some(100));
        assert_eq!(v.per_run_remaining, Some(70)); // 100 − 30
        assert_eq!(v.cumulative_limit, Some(1000));
        assert_eq!(v.cumulative_remaining, Some(600)); // 1000 − 400
        assert_eq!(v.breach_action, BreachAction::Stop);

        let value: serde_json::Value = serde_json::to_value(v).unwrap();
        assert_eq!(value["per_run_limit"], serde_json::json!(100));
        assert_eq!(value["per_run_remaining"], serde_json::json!(70));
        assert_eq!(value["cumulative_remaining"], serde_json::json!(600));
        assert_eq!(value["breach_action"], serde_json::json!("stop"));
        // Tokens only — no dollar cap/headroom.
        assert!(value.get("cost_cap").is_none());
        assert!(value.get("dollars_remaining").is_none());
        // Round-trips.
        let back: BudgetView = serde_json::from_value(value).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn budget_view_remaining_saturates_at_zero_when_breached() {
        // A scope at/over its ceiling reports 0 remaining, never a negative.
        let budget = TokenBudget {
            per_run: None,
            cumulative: Some(100),
        };
        let v = BudgetView::from_budget(&budget, BreachAction::Pause, 0, 250).unwrap();
        assert_eq!(v.cumulative_remaining, Some(0), "saturates, never negative");
        // The unset per-run scope stays absent in the view.
        assert_eq!(v.per_run_limit, None);
        let value: serde_json::Value = serde_json::to_value(v).unwrap();
        assert!(
            value.get("per_run_limit").is_none(),
            "an unset scope is omitted: {value}"
        );
    }

    #[test]
    fn entry_with_a_budget_serializes_the_view() {
        // A budgeted entry surfaces the real BudgetView in `--json`.
        let mut entry = sample_entry("web-1");
        entry.budget = BudgetView::from_budget(
            &TokenBudget {
                per_run: None,
                cumulative: Some(500),
            },
            BreachAction::Pause,
            0,
            120,
        );
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["budget"]["cumulative_limit"], serde_json::json!(500));
        assert_eq!(
            value["budget"]["cumulative_remaining"],
            serde_json::json!(380)
        );
        assert_eq!(value["budget"]["breach_action"], serde_json::json!("pause"));
        // Round-trips.
        let json = serde_json::to_string(&entry).unwrap();
        let back: FleetEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn entry_uses_plain_string_and_wire_forms_for_composed_types() {
        // The composed domain types keep their wire forms: name is a plain
        // string, state is snake_case, policy is kebab-case (AD-14: reuse the
        // already-Serialize types, do not re-derive).
        let mut entry = sample_entry("web-1");
        entry.state = LifecycleState::Running;
        entry.restart_policy = RestartPolicy::Never;
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["name"], serde_json::json!("web-1"));
        assert_eq!(value["state"], serde_json::json!("running"));
        assert_eq!(value["restart_policy"], serde_json::json!("never"));
    }

    #[test]
    fn entry_round_trips_through_json() {
        // The --json document must be stable + re-parseable (AC5).
        let entry = sample_entry("demo");
        let json = serde_json::to_string(&entry).unwrap();
        let back: FleetEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn failed_cause_is_omitted_when_absent_and_present_when_set() {
        // A non-failed entry omits `failed_cause` (skip_serializing_if); a failed
        // one carries the cause string.
        let entry = sample_entry("demo");
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert!(value.get("failed_cause").is_none(), "{value}");

        let mut failed = sample_entry("boom");
        failed.state = LifecycleState::Failed;
        failed.failed_cause = Some("crashed with code 1".to_string());
        let value: serde_json::Value = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            value["failed_cause"],
            serde_json::json!("crashed with code 1")
        );
    }

    #[test]
    fn listing_carries_schema_version_and_instances() {
        let listing = FleetListing::new(vec![sample_entry("a"), sample_entry("b")]);
        assert_eq!(listing.schema_version, FLEET_SCHEMA_VERSION);
        let value: serde_json::Value = serde_json::to_value(&listing).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(FLEET_SCHEMA_VERSION)
        );
        assert_eq!(value["instances"].as_array().unwrap().len(), 2);
        // Story 3-5: the document also carries a top-level `totals` object.
        assert!(value["totals"].is_object(), "totals present: {value}");
    }

    #[test]
    fn fleet_schema_version_is_2_after_the_3_5_additive_bump() {
        // Story 3-5 bumped the Fleet document version 1 → 2 (additive: `totals`
        // gained). Guard the value so the bump is deliberate + recorded.
        assert_eq!(FLEET_SCHEMA_VERSION, 2);
    }

    #[test]
    fn empty_listing_serializes_with_an_empty_array_and_zero_totals() {
        // AC9: an empty Fleet is still valid JSON — an empty `instances` array + an
        // all-zero / absent-dollars `totals` (nothing to estimate).
        let listing = FleetListing::new(vec![]);
        let value: serde_json::Value = serde_json::to_value(&listing).unwrap();
        assert_eq!(value["instances"], serde_json::json!([]));
        assert_eq!(value["totals"]["total_input_tokens"], serde_json::json!(0));
        assert_eq!(value["totals"]["total_output_tokens"], serde_json::json!(0));
        // No dollars to estimate → the field is absent (skip_serializing_if), not $0.
        assert!(
            value["totals"].get("total_dollars").is_none(),
            "empty Fleet has no dollar total: {value}"
        );
        assert_eq!(value["totals"]["dollars_partial"], serde_json::json!(false));
        // And it re-parses.
        let json = serde_json::to_string(&listing).unwrap();
        let back: FleetListing = serde_json::from_str(&json).unwrap();
        assert_eq!(back, listing);
    }

    #[test]
    fn listing_totals_equal_from_entries_over_its_rows() {
        // The document is SELF-CONSISTENT: `new(instances)` computes `totals` as
        // exactly `FleetTotals::from_entries(&instances)` — the aggregate is DERIVED
        // from the rows it carries, never passed independently (Key design decision 4).
        let entries = vec![
            metered_entry("a", 100, 250, Some(Micros(300_000))),
            metered_entry("b", 40, 60, None),
        ];
        let listing = FleetListing::new(entries.clone());
        assert_eq!(listing.totals, FleetTotals::from_entries(&entries));
    }

    // ---- Story 3-5: FleetTotals — the pure Fleet-wide aggregate + honesty rules ----

    /// A metered [`FleetEntry`] with the given cumulative token totals and, when
    /// `dollars` is `Some`, a derived cost + `estimated` label (a Rate is configured).
    /// `None` dollars = a no-Rate instance (the dollar figure honestly absent).
    fn metered_entry(name: &str, input: u64, output: u64, dollars: Option<Micros>) -> FleetEntry {
        let mut entry = sample_entry(name);
        let base = UsageView::new(
            UsageTotals {
                input_tokens: input,
                output_tokens: output,
            },
            UsageTotals::zero(),
        );
        entry.usage = match dollars {
            Some(cost) => base.with_dollars(cost, Micros::ZERO, EstimateLabel::Estimated),
            None => base,
        };
        entry
    }

    #[test]
    fn totals_of_an_empty_fleet_are_zero_and_dollars_absent() {
        // Empty Fleet → all-zero tokens, no dollar total (nothing to estimate), no
        // label, not partial.
        let totals = FleetTotals::from_entries(&[]);
        assert_eq!(totals.total_input_tokens, 0);
        assert_eq!(totals.total_output_tokens, 0);
        assert_eq!(totals.total_tokens(), 0);
        assert_eq!(totals.total_dollars, None);
        assert_eq!(totals.estimate_label, None);
        assert!(!totals.dollars_partial);
        assert_eq!(totals, FleetTotals::default());
    }

    #[test]
    fn totals_of_one_rated_instance_equal_that_instance_labeled_estimated() {
        // Fleet of one (Rate'd) → the aggregate equals that instance's totals, and the
        // dollar total is labeled `estimated`, not partial.
        let entries = vec![metered_entry(
            "solo",
            1_000_000,
            2_000_000,
            Some(Micros(45_000_000)),
        )];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_input_tokens, 1_000_000);
        assert_eq!(t.total_output_tokens, 2_000_000);
        assert_eq!(t.total_dollars, Some(Micros(45_000_000)));
        assert_eq!(t.estimate_label, Some(EstimateLabel::Estimated));
        assert!(!t.dollars_partial, "a single Rate'd instance is complete");
    }

    #[test]
    fn totals_of_one_no_rate_instance_have_tokens_but_absent_dollars() {
        // Fleet of one (no Rate) → tokens equal that instance's tokens; dollars are
        // honestly absent (None) — never a fabricated $0.00 for an unpriced instance.
        let entries = vec![metered_entry("solo", 500, 700, None)];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_input_tokens, 500);
        assert_eq!(t.total_output_tokens, 700);
        assert_eq!(t.total_dollars, None, "no Rate ⇒ nothing to estimate");
        assert_eq!(t.estimate_label, None);
        // A lone unpriced instance is not "partial" — there is no dollar total to
        // qualify as a lower bound (partial only applies once SOMETHING is priced).
        assert!(!t.dollars_partial);
    }

    #[test]
    fn totals_of_a_mixed_fleet_sum_all_tokens_but_only_rated_dollars_and_flag_partial() {
        // THE honesty crux (AC4/AC5): a mixed Fleet — one Rate'd + accrued, one
        // metered-but-no-Rate, one never-metered. Tokens sum ALL THREE (zero-not-
        // absent); dollars sum ONLY the Rate'd one; the total is a labeled LOWER BOUND
        // flagged `dollars_partial` because a metered instance had no Rate.
        let entries = vec![
            metered_entry("rated", 1_000_000, 1_000_000, Some(Micros(18_000_000))),
            metered_entry("unpriced", 500, 250, None), // metered, NO Rate
            metered_entry("idle", 0, 0, None),         // never metered (honest zero)
        ];
        let t = FleetTotals::from_entries(&entries);
        // Tokens sum all three (the idle instance's 0 is COUNTED, the unpriced one's
        // real tokens are COUNTED).
        assert_eq!(t.total_input_tokens, 1_000_000 + 500);
        assert_eq!(t.total_output_tokens, 1_000_000 + 250);
        // Dollars sum ONLY the Rate'd instance.
        assert_eq!(t.total_dollars, Some(Micros(18_000_000)));
        assert_eq!(t.estimate_label, Some(EstimateLabel::Estimated));
        // ...and the total is a LOWER BOUND — a metered instance ("unpriced") was left
        // out of the dollar sum, so it says so (SM-C3), and the count names exactly the
        // one metered-but-unpriced instance (the never-metered "idle" is NOT counted).
        assert!(
            t.dollars_partial,
            "a metered-but-unpriced instance makes the dollar total partial"
        );
        assert_eq!(
            t.unpriced_count, 1,
            "only the metered-but-unpriced instance is counted (not the idle 0)"
        );
    }

    #[test]
    fn unpriced_count_names_every_metered_but_unpriced_instance_and_ignores_the_rest() {
        // The count is EXACTLY the instances that make the total partial: metered
        // (tokens > 0) AND no Rate. A Rate'd instance, a Rate'd-$0 instance, and a
        // never-metered no-Rate instance all contribute 0 to the count; only the two
        // metered-no-Rate instances are named.
        let entries = vec![
            metered_entry("rated", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_entry("rated0", 0, 0, Some(Micros::ZERO)),
            metered_entry("idle", 0, 0, None), // never metered — not counted
            metered_entry("unpriced-a", 500, 0, None),
            metered_entry("unpriced-b", 0, 250, None),
        ];
        let t = FleetTotals::from_entries(&entries);
        assert!(t.dollars_partial);
        assert_eq!(t.unpriced_count, 2, "exactly the two metered-no-Rate rows");
    }

    #[test]
    fn unpriced_count_is_zero_when_the_total_is_not_partial() {
        // No lower bound to qualify ⇒ the count reads 0, in lockstep with
        // `dollars_partial == false`. Covers: all-priced, a lone no-Rate instance (no
        // dollar total at all), and an empty Fleet.
        let all_priced = FleetTotals::from_entries(&[
            metered_entry("a", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_entry("b", 0, 1_000_000, Some(Micros(15_000_000))),
        ]);
        assert!(!all_priced.dollars_partial);
        assert_eq!(all_priced.unpriced_count, 0, "complete total names nothing");

        // A metered no-Rate instance with NO priced instance anywhere: `total_dollars`
        // is None, so there is no lower bound to qualify — the count stays 0 even though
        // an instance is unpriced (nothing was priced to be a lower bound of).
        let no_rate_at_all = FleetTotals::from_entries(&[metered_entry("solo", 500, 700, None)]);
        assert!(!no_rate_at_all.dollars_partial);
        assert_eq!(no_rate_at_all.unpriced_count, 0);

        assert_eq!(FleetTotals::from_entries(&[]).unpriced_count, 0);
    }

    #[test]
    fn totals_of_all_rated_instances_sum_dollars_and_are_not_partial() {
        // All metered instances have Rates → dollars sum all, the total is complete
        // (not partial), labeled `estimated`.
        let entries = vec![
            metered_entry("a", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_entry("b", 0, 1_000_000, Some(Micros(15_000_000))),
        ];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_input_tokens, 1_000_000);
        assert_eq!(t.total_output_tokens, 1_000_000);
        assert_eq!(t.total_dollars, Some(Micros(18_000_000)));
        assert!(
            !t.dollars_partial,
            "all metered instances priced ⇒ complete"
        );
        assert_eq!(t.estimate_label, Some(EstimateLabel::Estimated));
    }

    #[test]
    fn a_rated_zero_usage_instance_is_counted_and_not_partial() {
        // A Rate'd instance with zero usage contributes a genuine $0 to the sum (an
        // honest labeled zero — it HAS a Rate, the cost is truly $0), and does NOT flag
        // partial. Distinct from a no-Rate instance (unknown cost).
        let entries = vec![
            metered_entry("rated0", 0, 0, Some(Micros::ZERO)),
            metered_entry("rated", 1_000_000, 0, Some(Micros(3_000_000))),
        ];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_dollars, Some(Micros(3_000_000)), "0 + 3_000_000");
        assert!(
            !t.dollars_partial,
            "a Rate'd $0 instance is priced, not unpriced — no partial flag"
        );
    }

    #[test]
    fn zero_not_absent_every_instance_is_counted_in_the_token_sum() {
        // Zero-not-absent (AC4): N instances contribute N token contributions — a
        // never-metered instance's `0` is present in the sum, never skipped. Ten
        // idle instances + one metered: the token total is exactly the metered one's,
        // and adding the zeros does not change it (proving they were summed, not
        // dropped, and did not fabricate anything).
        let mut entries: Vec<FleetEntry> = (0..10)
            .map(|i| metered_entry(&format!("idle{i}"), 0, 0, None))
            .collect();
        entries.push(metered_entry("busy", 123, 456, None));
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_input_tokens, 123);
        assert_eq!(t.total_output_tokens, 456);
        // The count of entries and the token total are mutually consistent — no
        // instance was silently dropped (11 summed, all-zero contributions included).
        assert_eq!(entries.len(), 11);
    }

    #[test]
    fn token_sums_saturate_and_never_wrap() {
        // Saturation (AC3): many large instances must SATURATE at u64::MAX, never wrap
        // to a small number. Two instances each near u64::MAX.
        let entries = vec![
            metered_entry("big1", u64::MAX, u64::MAX, None),
            metered_entry("big2", u64::MAX, u64::MAX, None),
        ];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.total_input_tokens, u64::MAX, "saturates, never wraps");
        assert_eq!(t.total_output_tokens, u64::MAX);
        assert_eq!(t.total_tokens(), u64::MAX, "combined also saturates");
    }

    #[test]
    fn dollar_sum_saturates_and_never_wraps() {
        // Saturation (AC3): the Micros dollar sum saturates at i64::MAX rather than
        // wrapping negative (a runaway Fleet cost must not un-breach any reader).
        let entries = vec![
            metered_entry("big1", 1, 0, Some(Micros(i64::MAX))),
            metered_entry("big2", 1, 0, Some(Micros(i64::MAX))),
        ];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(
            t.total_dollars,
            Some(Micros(i64::MAX)),
            "saturates, no wrap"
        );
        assert!(
            t.total_dollars.unwrap().get() > 0,
            "a runaway dollar total stays positive"
        );
    }

    #[test]
    fn the_fleet_dollar_label_is_always_estimated_when_present_never_reconciled() {
        // v1 the Fleet dollar total, when present, is ALWAYS `estimated` — there is no
        // code path to a `reconciled` Fleet total (the arm is an unreachable forward
        // seam). Any Rate'd Fleet proves it.
        let entries = vec![metered_entry("a", 10, 20, Some(Micros(30)))];
        let t = FleetTotals::from_entries(&entries);
        assert_eq!(t.estimate_label, Some(EstimateLabel::Estimated));
        assert_ne!(t.estimate_label, Some(EstimateLabel::Reconciled));
    }

    #[test]
    fn fleet_totals_serialize_as_integer_micros_and_a_label_never_a_dollar_string() {
        // AD-14/AC-B: the aggregate rides the wire as integer micros + a snake/kebab
        // label string, snake_case fields, NEVER a `$` string and NEVER a float.
        let t = FleetTotals::from_entries(&[metered_entry(
            "a",
            1_000_000,
            2_000_000,
            Some(Micros(45_000_000)),
        )]);
        let value: serde_json::Value = serde_json::to_value(t).unwrap();
        assert_eq!(value["total_input_tokens"], serde_json::json!(1_000_000));
        assert_eq!(value["total_output_tokens"], serde_json::json!(2_000_000));
        assert_eq!(value["total_dollars"], serde_json::json!(45_000_000));
        assert!(
            value["total_dollars"].is_i64() || value["total_dollars"].is_u64(),
            "dollars are an integer: {value}"
        );
        assert_eq!(value["estimate_label"], serde_json::json!("estimated"));
        assert_eq!(value["dollars_partial"], serde_json::json!(false));
        // The unpriced count rides as a plain integer (0 for a complete total), NOT a
        // `$` string (AD-14).
        assert_eq!(value["unpriced_count"], serde_json::json!(0));
        assert!(value["unpriced_count"].is_u64(), "an integer: {value}");
        let json = serde_json::to_string(&t).unwrap();
        assert!(!json.contains('$'), "no `$` string on the wire: {json}");
        // Round-trips.
        let back: FleetTotals = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn a_partial_fleet_total_serializes_the_partial_flag_as_data() {
        // AC5: the partial-ness rides as DATA on the wire so a --json consumer knows
        // the dollar total is a lower bound.
        let entries = vec![
            metered_entry("rated", 1_000_000, 0, Some(Micros(3_000_000))),
            metered_entry("unpriced", 500, 0, None),
        ];
        let value: serde_json::Value =
            serde_json::to_value(FleetTotals::from_entries(&entries)).unwrap();
        assert_eq!(value["dollars_partial"], serde_json::json!(true));
        assert_eq!(value["total_dollars"], serde_json::json!(3_000_000));
        assert_eq!(value["estimate_label"], serde_json::json!("estimated"));
        // The count of unpriced instances rides as DATA too (a plain integer), so a
        // --json consumer can render the same "N unpriced" the human footer does.
        assert_eq!(value["unpriced_count"], serde_json::json!(1));
    }

    #[test]
    fn metering_seed_cell_is_the_em_dash_token() {
        // The human cell token is `—` (consistent between list + show).
        assert_eq!(FleetEntry::METERING_SEED_CELL, "—");
    }

    // ---- Story 3-3: dollar fields on the views (AC-B/AC10) ----

    #[test]
    fn usage_view_with_dollars_carries_labeled_integer_micros() {
        // A Rate'd instance: the derived cost rides as integer micros + the label,
        // NEVER a `$` string, NEVER a float.
        let usage = UsageView::new(
            UsageTotals {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
            },
            UsageTotals::zero(),
        )
        .with_dollars(Micros(18_000_000), Micros(0), EstimateLabel::Estimated);
        let value: serde_json::Value = serde_json::to_value(usage).unwrap();
        assert_eq!(value["cumulative_dollars"], serde_json::json!(18_000_000));
        assert_eq!(value["current_run_dollars"], serde_json::json!(0));
        assert_eq!(value["estimate_label"], serde_json::json!("estimated"));
        let json = serde_json::to_string(&usage).unwrap();
        assert!(!json.contains('$'), "no `$` string on the wire: {json}");
        let back: UsageView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn usage_view_without_a_rate_omits_the_dollar_fields() {
        // AC-B: no Rate ⇒ no dollar figure at all (the fields are absent, not `$0`).
        let usage = UsageView::new(UsageTotals::zero(), UsageTotals::zero());
        let value: serde_json::Value = serde_json::to_value(usage).unwrap();
        assert!(value.get("cumulative_dollars").is_none(), "{value}");
        assert!(value.get("current_run_dollars").is_none(), "{value}");
        assert!(value.get("estimate_label").is_none(), "{value}");
    }

    #[test]
    fn budget_view_with_a_rate_carries_the_dollar_cap_and_remaining() {
        // AC10: a Rate'd instance with a dollar cap surfaces the cap + remaining
        // (saturating) in integer micros + the label. Cap $0.50, spent $0.30 → $0.20
        // remaining.
        let v = BudgetView::from_budget_and_cost(
            &TokenBudget::none(),
            &CostCap {
                per_run: None,
                cumulative: Some(Micros(500_000)),
            },
            BreachAction::Pause,
            0,
            0,
            true, // has_rate
            Micros(0),
            Micros(300_000),
            EstimateLabel::Estimated,
        )
        .expect("a dollar cap makes the view present even with no token budget");
        assert_eq!(v.cumulative_cost_cap, Some(Micros(500_000)));
        assert_eq!(v.cumulative_dollars_remaining, Some(Micros(200_000)));
        assert_eq!(v.estimate_label, Some(EstimateLabel::Estimated));
        let value: serde_json::Value = serde_json::to_value(v).unwrap();
        assert_eq!(value["cumulative_cost_cap"], serde_json::json!(500_000));
        assert_eq!(
            value["cumulative_dollars_remaining"],
            serde_json::json!(200_000)
        );
        let json = serde_json::to_string(&v).unwrap();
        assert!(!json.contains('$'), "no `$` string on the wire: {json}");
    }

    #[test]
    fn budget_view_dollar_remaining_saturates_at_zero_when_breached() {
        // A breached dollar scope reports $0 remaining, never negative.
        let v = BudgetView::from_budget_and_cost(
            &TokenBudget::none(),
            &CostCap {
                per_run: None,
                cumulative: Some(Micros(500_000)),
            },
            BreachAction::Pause,
            0,
            0,
            true,
            Micros(0),
            Micros(800_000), // spent past the cap
            EstimateLabel::Estimated,
        )
        .unwrap();
        assert_eq!(v.cumulative_dollars_remaining, Some(Micros(0)));
    }

    #[test]
    fn budget_view_cost_cap_with_no_rate_is_inert_no_dollar_fields() {
        // AC-B: a Cost Cap set WITHOUT a Rate is inert — the view carries NO dollar
        // cap/remaining (has_rate = false), and with no token budget either the view
        // is None (an honest absent budget, never a fabricated dollar ceiling).
        let v = BudgetView::from_budget_and_cost(
            &TokenBudget::none(),
            &CostCap {
                per_run: None,
                cumulative: Some(Micros(500_000)),
            },
            BreachAction::Pause,
            0,
            0,
            false, // NO rate
            Micros(0),
            Micros(0),
            EstimateLabel::Estimated,
        );
        assert!(
            v.is_none(),
            "a cap with no Rate and no token budget is an honest absent budget"
        );

        // With a token budget present but no Rate, the view IS present but its dollar
        // fields stay absent (inert).
        let v = BudgetView::from_budget_and_cost(
            &TokenBudget {
                per_run: None,
                cumulative: Some(100),
            },
            &CostCap {
                per_run: None,
                cumulative: Some(Micros(500_000)),
            },
            BreachAction::Pause,
            0,
            50,
            false, // NO rate
            Micros(0),
            Micros(0),
            EstimateLabel::Estimated,
        )
        .unwrap();
        assert_eq!(v.cumulative_limit, Some(100));
        assert_eq!(v.cumulative_cost_cap, None, "no Rate ⇒ dollar cap inert");
        assert_eq!(v.estimate_label, None);
    }
}
