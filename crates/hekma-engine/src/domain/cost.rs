//! Dollar cost derivation + the dollar Cost Cap (spine AD-7/AD-8, story 3-3) —
//! the DOLLAR half of the one metering guardrail, built on 3-1's token ledger and
//! REUSING 3-2's evaluator/breach/lifecycle path.
//!
//! Story 3-1 delivered `capture → per-direction token ledger`; 3-2 delivered
//! TOKEN enforcement (the pure [`super::budget::BudgetEvaluator`] inside the ledger
//! commit path → [`super::budget::BreachDecision`] → the supervisor executes the
//! Breach Action). 3-3 supplies the money: a per-direction [`Rate`] turns metered
//! tokens into a billing-safe integer-micro-dollar cost ([`Micros`], via the pure
//! [`cost_micros`] derivation), a [`CostCap`] bounds it, and a thin [`CostEvaluator`]
//! returns 3-2's EXACT [`super::budget::BreachDecision`] so the supervisor's ONE
//! enforcement site fires the SAME `pause`/`stop`/`warn` action for a dollar breach
//! as for a token breach.
//!
//! ## The two disciplines this module exists to enforce
//!
//! 1. **Money is integer micro-dollars, NEVER `f64`** (AD-8 + the 3-1 `u64→i64`
//!    money lesson). A cost cap is a financial guardrail: enforcing it on a lossy
//!    `f64` (which cannot represent `$0.01` and drifts over a long accrual) is
//!    dishonest, and a `$/1M`-token rate yields sub-cent per-token costs (`$3/1M`
//!    = 3 micros/token) that cents would truncate to `0`. So all money is
//!    [`Micros`] (`i64`, 1e6/dollar); the derivation uses a `u128` intermediate
//!    that SATURATES (a runaway token count must never wrap the cost and thereby
//!    un-breach) and round-half-up on the divide. The RENDER layer
//!    ([`render_dollars`]) is the ONLY place micros become a `$X.XX` string.
//! 2. **Exactly one module formats currency, and its input carries an
//!    [`EstimateLabel`]** (AD-8, grep-lint-enforced). [`render_dollars`] is the SOLE
//!    code that turns [`Micros`] into a human `$` string, and it ALWAYS appends the
//!    label (`estimated` v1). Every other module passes `Micros` + `EstimateLabel`
//!    as DATA; the wire (Fleet cost view, breach payload, `--json`) carries integer
//!    micros + the label, NEVER a pre-formatted `$` string (so a Host formats its
//!    own currency — AD-14).
//!
//! ## Boundary (what this is NOT)
//!
//! PURE + I/O-free (mirroring [`super::budget`]): no ledger, no lifecycle, no
//! config read. The derivation + evaluator + parse + render are `std`-only, so
//! their unit tests are instant and identical on every OS (retro AI-37: the bulk
//! of the billing coverage lives in this file's pure tests). The no-retroactive-
//! repricing PERSISTENCE (a per-event effective Rate) is an engine-side ledger
//! concern in the store; the LIVE Rate/cap resolve is in [`super::config`]; the
//! enforcement hook is in [`super::supervisor`]. `reconciled` labeling of actuals
//! is a forward seam (AD-8) — v1 every rendered dollar is `estimated`.

use serde::{Deserialize, Serialize};

use super::budget::{BreachAction, BreachDecision, BreachScope};

/// Integer micro-dollars — the billing-safe money representation (AD-8; the 3-1
/// `u64→i64` money lesson). `1 dollar = 1_000_000 micros`, so `$0.000001` is the
/// quantum — fine enough for `$/1M`-token pricing where a single token costs
/// sub-cent fractions.
///
/// A newtype over `i64` (NOT `f64`): a cost is never negative, but `i64` lets a
/// remaining/headroom saturate honestly to `0` (never wrap to a huge positive) and
/// leaves room for a future reconciliation credit (AD-8's `reconciled` seam). The
/// LEDGER, the cap, and the wire keep micros; ONLY [`render_dollars`] turns micros
/// into a `$X.XX` string. `Serialize`/`Deserialize` as a bare integer (serde
/// `transparent`) so it rides the Fleet cost view + the breach payload as a NUMBER,
/// never a float, never a `$` string. `Ord` so the `>=` cap compare is a plain
/// integer comparison.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Micros(pub i64);

/// The number of micro-dollars in one dollar (`1e6`). The scale factor the
/// derivation divides by and the render layer divides by to reach cents/dollars.
pub const MICROS_PER_DOLLAR: i64 = 1_000_000;

/// Half of [`MICROS_PER_DOLLAR`] as a `u128` — the round-half-up bias added before
/// the integer divide in [`cost_micros`] (so a fractional micro-dollar rounds to
/// the nearest micro, ties going up). Named so the rounding policy is explicit and
/// auditable.
const ROUND_HALF_UP_BIAS: u128 = (MICROS_PER_DOLLAR as u128) / 2;

impl Micros {
    /// Zero micro-dollars (`$0.00`) — the honest cost of zero tokens / a zero rate.
    pub const ZERO: Micros = Micros(0);

    /// The raw signed micro-dollar count.
    pub fn get(self) -> i64 {
        self.0
    }

    /// Saturating add (a cost accumulation never wraps — the billing discipline).
    pub fn saturating_add(self, other: Micros) -> Micros {
        Micros(self.0.saturating_add(other.0))
    }

    /// Saturating remaining/headroom: `self − spent`, floored at `0` (a breached
    /// cap reports `0` remaining, never a negative — the [`super::fleet`] discipline
    /// mirrored in money). Because [`Micros`] is `i64`, a `spent > self` would go
    /// negative without the floor; we clamp so headroom is honestly `0`.
    pub fn saturating_remaining(self, spent: Micros) -> Micros {
        Micros(self.0.saturating_sub(spent.0).max(0))
    }

    /// The micro-dollar count as the `u64` 3-2's [`BreachDecision`] carries in its
    /// unit-agnostic `limit`/`observed` fields. A cost/cap is non-negative, so a
    /// negative (never produced by the derivation, defensive only) clamps to `0`.
    /// This is the ONE reconciliation between [`Micros`] (`i64`) and the decision's
    /// `u64` numeric fields (recorded in Key design decision 3).
    fn as_decision_u64(self) -> u64 {
        u64::try_from(self.0).unwrap_or(0)
    }
}

/// A per-instance Rate (spine FR-20, `[ASSUMPTION: split rates]`) — separate input
/// and output prices, each in micro-dollars per 1M tokens.
///
/// PER-DIRECTION because the 3-1 ledger already splits `input_tokens`/`output_tokens`
/// and real models price output several× input; a single blended rate would misprice
/// every run. The stored unit is micro-dollars-per-1M-tokens (an integer, matching
/// [`Micros`]) — e.g. `$3.00/1M` is `3_000_000`. Either direction MAY be `0` (a free
/// direction), but a Rate is only "supplied" when BOTH directions are configured
/// (see [`super::config::resolve_cost`]): a half-configured Rate is treated as
/// no-Rate-yet (inert, AC-B), avoiding a silently-half-priced ledger. `Serialize`/
/// `Deserialize` (snake_case) so it can be persisted per-event (the no-retro-repricing
/// column) without a float ever touching the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    /// The input (prompt) price in micro-dollars per 1M tokens.
    pub input_micros_per_1m: u64,
    /// The output (completion) price in micro-dollars per 1M tokens.
    pub output_micros_per_1m: u64,
}

impl Rate {
    /// Build a Rate from the two per-direction micro-dollar-per-1M-token prices.
    pub const fn new(input_micros_per_1m: u64, output_micros_per_1m: u64) -> Self {
        Self {
            input_micros_per_1m,
            output_micros_per_1m,
        }
    }

    /// The dollar cost of `(input_tokens, output_tokens)` at this Rate (a method
    /// form of [`cost_micros`], for the ledger read path).
    pub fn cost_of(&self, input_tokens: u64, output_tokens: u64) -> Micros {
        cost_micros(input_tokens, output_tokens, self)
    }
}

/// The PURE cost derivation (spine AD-5, AC-A/AD-8) — `tokens × price / 1e6` per
/// direction, summed, in billing-safe integer micro-dollars.
///
/// `cost = round(input_tokens × rate.input / 1e6) + round(output_tokens × rate.output / 1e6)`,
/// each direction priced INDEPENDENTLY (AC6). Two disciplines make this billing-safe:
///
/// * **Overflow SATURATES, never wraps** — the `tokens × price` multiply is done in
///   `u128` (a `u64 × u64` product can exceed `u64`), and the final micro-dollar
///   result saturates into `i64` ([`i64::MAX`]) rather than wrapping. A runaway
///   `u64::MAX` tokens × a large rate must NOT wrap the cost to a small number and
///   silently un-breach — it pins at the max instead (still `>=` any real cap).
/// * **Round-half-up** — the divide by `1e6` truncates in integer math, so we add
///   [`ROUND_HALF_UP_BIAS`] (`500_000`) before the divide: a fractional micro-dollar
///   rounds to the nearest micro, a tie (exactly half) going UP. Deterministic and
///   testable (the money-correctness crux).
///
/// NO I/O, NO ledger, NO lifecycle — pure, so the boundary/precision/overflow tests
/// are cheap and cross-OS by construction.
pub fn cost_micros(input_tokens: u64, output_tokens: u64, rate: &Rate) -> Micros {
    let per_direction = |tokens: u64, price_per_1m: u64| -> i64 {
        // u128 intermediate: a u64 token count × a u64 price cannot overflow u128.
        let product = (tokens as u128).saturating_mul(price_per_1m as u128);
        // Round-half-up on the divide by 1e6 (the per-1M denominator): add half the
        // divisor before the integer division. Saturating so the +bias cannot wrap.
        let rounded = product
            .saturating_add(ROUND_HALF_UP_BIAS)
            .saturating_div(MICROS_PER_DOLLAR as u128);
        // Saturate the u128 micro-dollars into i64 (the Micros domain): a runaway
        // value pins at i64::MAX rather than wrapping negative (the un-breach guard).
        i64::try_from(rounded).unwrap_or(i64::MAX)
    };
    let input_cost = per_direction(input_tokens, rate.input_micros_per_1m);
    let output_cost = per_direction(output_tokens, rate.output_micros_per_1m);
    Micros(input_cost.saturating_add(output_cost))
}

/// A per-instance dollar Cost Cap (spine FR-21) — per-run and cumulative dollar
/// ceilings, mirroring 3-2's [`super::budget::TokenBudget`] but bounding MONEY, not
/// tokens.
///
/// Both scopes are optional ([`Micros`]): an UNSET scope (`None`) NEVER breaches.
/// The per-run ceiling bounds a single Run's derived cost; the cumulative ceiling
/// bounds the instance's lifetime derived cost (the SAME two FR-21 scopes, reusing
/// [`BreachScope`] verbatim). A `CostCap` set with NO Rate is inert (AC-B — the
/// resolve returns no Rate and enforcement is skipped). NO token field (that is
/// `TokenBudget`). `Serialize`/`Deserialize` (snake_case) so it rides the Fleet cost
/// view + the breach payload as integer micros.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostCap {
    /// The per-run dollar ceiling (bounds one Run's derived cost). `None` = unset.
    pub per_run: Option<Micros>,
    /// The cumulative dollar ceiling (bounds the lifetime derived cost). `None` =
    /// unset.
    pub cumulative: Option<Micros>,
}

impl CostCap {
    /// A cap with neither scope set (the honest "no cost cap configured" value — the
    /// evaluator returns [`BreachDecision::WithinBudget`] for it always).
    pub const fn none() -> Self {
        Self {
            per_run: None,
            cumulative: None,
        }
    }

    /// Whether at least one scope is configured (a real cap exists). An all-`None`
    /// cap is surfaced as an honest ABSENT cap in Fleet detail (never a fabricated
    /// `$0.00` ceiling).
    pub fn is_set(&self) -> bool {
        self.per_run.is_some() || self.cumulative.is_some()
    }
}

/// The pure DOLLAR evaluator (spine AD-7 — the enforcement stage's DECIDE half for
/// money), a thin sibling of [`super::budget::BudgetEvaluator`].
///
/// It REUSES 3-2's [`BreachDecision`]/[`BreachScope`]/[`BreachAction`] VERBATIM (the
/// decision's `limit`/`observed` are already unit-agnostic `u64` — they carry
/// micro-dollars here instead of tokens), the SAME `>=` threshold, and the SAME
/// per-run-before-cumulative precedence. Keeping it a separate function (over
/// generalizing the token evaluator) leaves 3-2's token tests untouched and the two
/// evaluators independently readable (Key design decision 3). PURE: no I/O, no
/// lifecycle, no ledger — trivially unit-testable and identical on every OS.
pub struct CostEvaluator;

impl CostEvaluator {
    /// Evaluate the just-derived per-run + cumulative COST (micro-dollars) against
    /// the resolved [`CostCap`].
    ///
    /// Threshold semantics are **`>=`** (AC-C "reaches = at-or-over", in micros): a
    /// cost landing EXACTLY on the cap breaches. An UNSET scope (`None`) is skipped.
    /// PRECEDENCE: the per-run scope is checked FIRST (the action is identical
    /// regardless of scope, so precedence only affects the reported label). The
    /// returned [`BreachDecision::Breached`] carries the scope, the resolved
    /// [`BreachAction`], and the cap `limit` + `observed` cost as `u64` micros — the
    /// [`Micros`]→`u64` reconciliation happens HERE at the decision boundary (a
    /// non-negative cost/cap, so nothing is lost).
    pub fn evaluate(
        run_cost: Micros,
        cumulative_cost: Micros,
        cap: &CostCap,
        action: BreachAction,
    ) -> BreachDecision {
        // Per-run first (the tighter, run-local bound) — matching 3-2's precedence.
        if let Some(limit) = cap.per_run {
            if run_cost >= limit {
                return BreachDecision::Breached {
                    scope: BreachScope::PerRun,
                    action,
                    limit: limit.as_decision_u64(),
                    observed: run_cost.as_decision_u64(),
                };
            }
        }
        // Then cumulative (the lifetime bound).
        if let Some(limit) = cap.cumulative {
            if cumulative_cost >= limit {
                return BreachDecision::Breached {
                    scope: BreachScope::Cumulative,
                    action,
                    limit: limit.as_decision_u64(),
                    observed: cumulative_cost.as_decision_u64(),
                };
            }
        }
        BreachDecision::WithinBudget
    }
}

/// The estimate/reconciled label every rendered dollar figure carries (spine AD-8,
/// FR-23 — "every dollar figure the Engine renders is labeled").
///
/// v1 the label is ALWAYS [`EstimateLabel::Estimated`] (the metering is
/// self-reported — the engine derives dollars from what the agent reports, not
/// provider-confirmed actuals). [`EstimateLabel::Reconciled`] is a FORWARD SEAM that
/// AD-8 already names: it exists in the type so the wire + render carry it from the
/// start, but the reconciliation-with-provider-actuals INGESTION is deferred (no
/// code path produces `Reconciled` in v1). Kebab-case on the wire (`"estimated"` /
/// `"reconciled"`), matching the [`super::budget::BreachScope`] convention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EstimateLabel {
    /// A derived-from-self-reported-tokens figure (the v1 default; honestly an
    /// estimate, never presented as an actual — SM-C3).
    #[default]
    Estimated,
    /// A provider-confirmed actual (the forward seam; no v1 code path produces it).
    Reconciled,
}

impl EstimateLabel {
    /// The stable wire/label form (`"estimated"` / `"reconciled"`), matching the
    /// serde kebab-case rename so a wire string and this label never diverge.
    pub fn as_str(&self) -> &'static str {
        match self {
            EstimateLabel::Estimated => "estimated",
            EstimateLabel::Reconciled => "reconciled",
        }
    }
}

impl std::fmt::Display for EstimateLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// THE single currency-rendering module (spine AD-8, FR-23 — grep-lint-enforced).
///
/// This is the SOLE code in the entire engine + `kt` that turns [`Micros`] into a
/// human `$X.XX` string, and it ALWAYS appends the [`EstimateLabel`] (so no
/// unlabeled dollar figure ever reaches a human — FR-23's rendering-layer rule). A
/// currency grep-lint CI gate proves no other module builds a `$`-formatted money
/// string. Format: `"$0.30 (estimated)"` — the dollar amount rounded to cents (2
/// decimal places, round-half-up at the cent quantum) followed by the label in
/// parentheses.
///
/// Every OTHER module passes `Micros` + `EstimateLabel` as DATA; `--json` emits the
/// integer micros + the label (never this string) so a Host formats its own
/// currency (AD-14). A NEGATIVE micro value (never produced by the derivation —
/// defensive) renders with a leading `-` before the `$`.
pub fn render_dollars(micros: Micros, label: EstimateLabel) -> String {
    format!("{} ({label})", render_dollars_bare(micros))
}

/// The dollar VALUE ONLY (`$X.XX`) — NO [`EstimateLabel`] suffix (spine AD-8).
///
/// The bare-value companion of [`render_dollars`], and the OTHER half of "exactly
/// one module formats currency": both live HERE, so the `$X.XX` digits still
/// originate in this ONE module even when a caller must carry the estimate
/// qualifier elsewhere (e.g. a narrow table COLUMN HEADER, where a per-cell label
/// could be truncated away — FR-23 requires the label to survive, so on such a
/// surface it belongs in the header, not the truncatable cell). This lets a caller
/// compose a compact `remaining/cap` pair (the label appended once, by the caller)
/// WITHOUT string-surgery on a labeled string. Same cent rounding (round-half-up)
/// and the same defensive leading `-` for a negative as [`render_dollars`].
///
/// A caller using this is responsible for ensuring the estimate qualifier is
/// present SOMEWHERE stable on its surface (AD-8/FR-23 — no unlabeled dollar
/// reaches a human); it MUST NOT be used to drop the label from a lone dollar
/// figure.
pub fn render_dollars_bare(micros: Micros) -> String {
    let raw = micros.get();
    let sign = if raw < 0 { "-" } else { "" };
    // Work in the absolute value so the cent rounding is symmetric; the sign is
    // reattached. abs() on i64::MIN would overflow — use the widening to i128.
    let abs = (raw as i128).unsigned_abs();
    let micros_per_dollar = MICROS_PER_DOLLAR as u128;
    // Round-half-up to the CENT quantum: cents = round(abs / 10_000). One cent is
    // 10_000 micros; half a cent (5_000) rounds up.
    let micros_per_cent = micros_per_dollar / 100; // 10_000
    let total_cents = (abs + micros_per_cent / 2) / micros_per_cent;
    let dollars = total_cents / 100;
    let cents = total_cents % 100;
    format!("{sign}${dollars}.{cents:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Micros newtype ----

    #[test]
    fn micros_zero_and_get() {
        assert_eq!(Micros::ZERO, Micros(0));
        assert_eq!(Micros(1_500_000).get(), 1_500_000);
        assert_eq!(Micros::default(), Micros::ZERO);
    }

    #[test]
    fn micros_serializes_as_a_bare_integer_never_a_float_or_string() {
        // AD-8/AD-14: the wire carries integer micros, NEVER a float and NEVER a
        // `$` string (a Host formats its own currency).
        let value: serde_json::Value = serde_json::to_value(Micros(300_000)).unwrap();
        assert_eq!(value, serde_json::json!(300_000));
        assert!(
            value.is_i64() || value.is_u64(),
            "must be an integer: {value}"
        );
        let back: Micros = serde_json::from_value(value).unwrap();
        assert_eq!(back, Micros(300_000));
    }

    #[test]
    fn micros_saturating_add_never_wraps() {
        assert_eq!(Micros(5).saturating_add(Micros(7)), Micros(12));
        assert_eq!(
            Micros(i64::MAX).saturating_add(Micros(1)),
            Micros(i64::MAX),
            "saturates, never wraps to negative"
        );
    }

    #[test]
    fn micros_saturating_remaining_floors_at_zero() {
        // A cap of $1 with $0.30 spent leaves $0.70.
        assert_eq!(
            Micros(1_000_000).saturating_remaining(Micros(300_000)),
            Micros(700_000)
        );
        // Spending past the cap floors at 0, never negative (the headroom discipline).
        assert_eq!(
            Micros(1_000_000).saturating_remaining(Micros(1_500_000)),
            Micros(0)
        );
        assert_eq!(
            Micros(1_000_000).saturating_remaining(Micros(1_000_000)),
            Micros(0)
        );
    }

    // ---- cost_micros: the billing derivation (the bulk of coverage) ----

    #[test]
    fn zero_tokens_cost_zero() {
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(cost_micros(0, 0, &rate), Micros::ZERO);
    }

    #[test]
    fn zero_rate_costs_zero_even_with_tokens() {
        let rate = Rate::new(0, 0);
        assert_eq!(cost_micros(1_000_000, 5_000_000, &rate), Micros::ZERO);
    }

    #[test]
    fn input_only_prices_only_the_input_direction() {
        // 1M input tokens at $3/1M = $3.00 = 3_000_000 micros; output rate ignored
        // because there are 0 output tokens.
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(cost_micros(1_000_000, 0, &rate), Micros(3_000_000));
    }

    #[test]
    fn output_only_prices_only_the_output_direction() {
        // 1M output tokens at $15/1M = $15.00; 0 input tokens.
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(cost_micros(0, 1_000_000, &rate), Micros(15_000_000));
    }

    #[test]
    fn input_and_output_are_priced_independently_then_summed() {
        // The common real case: output priced several× input. 1M in @ $3/1M +
        // 1M out @ $15/1M = $3 + $15 = $18 = 18_000_000 micros.
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(cost_micros(1_000_000, 1_000_000, &rate), Micros(18_000_000));
        // A distinct-count case: 500k in @ $2/1M ($1.00) + 250k out @ $8/1M ($2.00)
        // = $3.00.
        let rate2 = Rate::new(2_000_000, 8_000_000);
        assert_eq!(cost_micros(500_000, 250_000, &rate2), Micros(3_000_000));
    }

    #[test]
    fn a_single_token_costs_the_sub_cent_micro_fraction() {
        // $3/1M = 3 micros per input token; $15/1M = 15 micros per output token.
        // This is WHY micro-dollars: cents would truncate both to 0.
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(cost_micros(1, 0, &rate), Micros(3));
        assert_eq!(cost_micros(0, 1, &rate), Micros(15));
        assert_eq!(cost_micros(1, 1, &rate), Micros(18));
    }

    #[test]
    fn the_divide_rounds_half_up_deterministically() {
        // Construct a token×price product landing exactly on a half-micro so the
        // round-half-up rule is observable. price = 1 micro/1M, tokens = 1_500_000:
        // product = 1_500_000; /1e6 = 1.5 micros → rounds UP to 2.
        let rate = Rate::new(1, 0);
        assert_eq!(
            cost_micros(1_500_000, 0, &rate),
            Micros(2),
            "1.5 micros rounds half-up to 2"
        );
        // Just below the half (product = 1_499_999 → 1.499999 → rounds to 1).
        assert_eq!(cost_micros(1_499_999, 0, &rate), Micros(1));
        // Just at the next half up (product = 2_500_000 → 2.5 → rounds to 3).
        assert_eq!(cost_micros(2_500_000, 0, &rate), Micros(3));
        // A whole micro (product = 3_000_000 → exactly 3, no rounding).
        assert_eq!(cost_micros(3_000_000, 0, &rate), Micros(3));
    }

    #[test]
    fn a_huge_token_count_saturates_and_never_wraps() {
        // THE un-breach guard: u64::MAX tokens × a large rate must NOT wrap the cost
        // to a small (or negative) number — it saturates at i64::MAX (still >= any
        // real cap, so the breach still fires). Without the u128 intermediate +
        // saturation this would wrap and silently un-breach.
        let rate = Rate::new(u64::MAX, u64::MAX);
        let cost = cost_micros(u64::MAX, u64::MAX, &rate);
        assert_eq!(cost, Micros(i64::MAX), "saturates at i64::MAX, never wraps");
        assert!(cost.get() > 0, "a runaway cost stays positive (no wrap)");
        // Even one direction runaway saturates that direction (then the sum saturates).
        let one_dir = cost_micros(u64::MAX, 0, &Rate::new(u64::MAX, 0));
        assert_eq!(one_dir, Micros(i64::MAX));
    }

    #[test]
    fn cost_of_method_matches_the_free_function() {
        let rate = Rate::new(3_000_000, 15_000_000);
        assert_eq!(
            rate.cost_of(1_000_000, 2_000_000),
            cost_micros(1_000_000, 2_000_000, &rate)
        );
    }

    #[test]
    fn rate_round_trips_snake_case_as_integers() {
        let rate = Rate::new(3_000_000, 15_000_000);
        let value: serde_json::Value = serde_json::to_value(rate).unwrap();
        assert_eq!(value["input_micros_per_1m"], serde_json::json!(3_000_000));
        assert_eq!(value["output_micros_per_1m"], serde_json::json!(15_000_000));
        let back: Rate = serde_json::from_value(value).unwrap();
        assert_eq!(back, rate);
    }

    // ---- CostCap shape ----

    #[test]
    fn cost_cap_none_is_unset_and_not_set() {
        let c = CostCap::none();
        assert_eq!(c.per_run, None);
        assert_eq!(c.cumulative, None);
        assert!(!c.is_set());
        assert_eq!(CostCap::default(), CostCap::none());
    }

    #[test]
    fn cost_cap_is_set_when_any_scope_present() {
        assert!(CostCap {
            per_run: Some(Micros(1)),
            cumulative: None
        }
        .is_set());
        assert!(CostCap {
            per_run: None,
            cumulative: Some(Micros(1))
        }
        .is_set());
    }

    #[test]
    fn cost_cap_round_trips_as_integer_micros() {
        let cap = CostCap {
            per_run: Some(Micros(5_000_000)),
            cumulative: Some(Micros(50_000_000)),
        };
        let value: serde_json::Value = serde_json::to_value(cap).unwrap();
        // Integer micros on the wire — no float, no `$` string.
        assert_eq!(value["per_run"], serde_json::json!(5_000_000));
        assert_eq!(value["cumulative"], serde_json::json!(50_000_000));
        let back: CostCap = serde_json::from_value(value).unwrap();
        assert_eq!(back, cap);
    }

    // ---- CostEvaluator: the pure dollar decision (reusing 3-2's BreachDecision) ----

    #[test]
    fn no_cap_is_always_within_budget() {
        let d = CostEvaluator::evaluate(
            Micros(i64::MAX),
            Micros(i64::MAX),
            &CostCap::none(),
            BreachAction::Pause,
        );
        assert_eq!(d, BreachDecision::WithinBudget);
    }

    #[test]
    fn below_cap_is_within_budget() {
        let cap = CostCap {
            per_run: Some(Micros(1_000_000)),
            cumulative: Some(Micros(10_000_000)),
        };
        // $0.99 < $1 and $9.99 < $10 → within.
        let d =
            CostEvaluator::evaluate(Micros(990_000), Micros(9_990_000), &cap, BreachAction::Warn);
        assert_eq!(d, BreachDecision::WithinBudget);
    }

    #[test]
    fn cost_exactly_at_the_cap_breaches_the_ge_boundary() {
        // AC-C: reaches = `>=` in micros. A cost landing EXACTLY on the cap breaches.
        let cap = CostCap {
            per_run: None,
            cumulative: Some(Micros(5_000_000)),
        };
        let d = CostEvaluator::evaluate(Micros(0), Micros(5_000_000), &cap, BreachAction::Pause);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::Cumulative,
                action: BreachAction::Pause,
                limit: 5_000_000,
                observed: 5_000_000,
            }
        );
    }

    #[test]
    fn one_micro_below_the_cap_does_not_breach() {
        // The exact companion of the boundary test: one micro-dollar under is within.
        let cap = CostCap {
            per_run: None,
            cumulative: Some(Micros(5_000_000)),
        };
        assert_eq!(
            CostEvaluator::evaluate(Micros(0), Micros(4_999_999), &cap, BreachAction::Pause),
            BreachDecision::WithinBudget
        );
    }

    #[test]
    fn over_cap_breaches_and_reports_observed_micros() {
        let cap = CostCap {
            per_run: None,
            cumulative: Some(Micros(5_000_000)),
        };
        let d = CostEvaluator::evaluate(Micros(0), Micros(12_500_000), &cap, BreachAction::Stop);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::Cumulative,
                action: BreachAction::Stop,
                limit: 5_000_000,
                observed: 12_500_000,
            }
        );
    }

    #[test]
    fn per_run_cost_cap_breaches_independently_of_cumulative() {
        let cap = CostCap {
            per_run: Some(Micros(2_000_000)),
            cumulative: None,
        };
        let d = CostEvaluator::evaluate(
            Micros(2_000_000),
            Micros(i64::MAX),
            &cap,
            BreachAction::Pause,
        );
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 2_000_000,
                observed: 2_000_000,
            }
        );
    }

    #[test]
    fn an_unset_cost_scope_never_breaches() {
        let cap = CostCap {
            per_run: None,
            cumulative: Some(Micros(1_000_000)),
        };
        // per_run unset: a massive run cost does NOT breach; cumulative not reached.
        assert_eq!(
            CostEvaluator::evaluate(Micros(i64::MAX), Micros(10), &cap, BreachAction::Pause),
            BreachDecision::WithinBudget
        );
    }

    #[test]
    fn both_set_and_both_would_trip_reports_per_run_first() {
        // PRECEDENCE: per-run reported first when both would trip (the action is
        // identical regardless of scope).
        let cap = CostCap {
            per_run: Some(Micros(1_000_000)),
            cumulative: Some(Micros(2_000_000)),
        };
        let d = CostEvaluator::evaluate(
            Micros(1_500_000),
            Micros(3_000_000),
            &cap,
            BreachAction::Pause,
        );
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 1_000_000,
                observed: 1_500_000,
            }
        );
    }

    #[test]
    fn a_zero_cost_cap_breaches_on_any_cost() {
        // A cap of $0 means "no dollars to spend": any cost (even 0) is `>= 0`, an
        // immediate breach. Recorded so the boundary is unambiguous.
        let cap = CostCap {
            per_run: Some(Micros(0)),
            cumulative: None,
        };
        let d = CostEvaluator::evaluate(Micros(0), Micros(0), &cap, BreachAction::Pause);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 0,
                observed: 0,
            }
        );
    }

    // ---- EstimateLabel ----

    #[test]
    fn estimate_label_default_is_estimated_v1() {
        assert_eq!(EstimateLabel::default(), EstimateLabel::Estimated);
    }

    #[test]
    fn estimate_label_wire_form_round_trips_kebab_case() {
        for (label, wire) in [
            (EstimateLabel::Estimated, "estimated"),
            (EstimateLabel::Reconciled, "reconciled"),
        ] {
            assert_eq!(label.as_str(), wire);
            assert_eq!(label.to_string(), wire);
            let json = serde_json::to_string(&label).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: EstimateLabel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, label);
        }
    }

    // ---- render_dollars: THE single currency formatter ----

    #[test]
    fn render_zero_is_a_labeled_zero_dollars() {
        assert_eq!(
            render_dollars(Micros(0), EstimateLabel::Estimated),
            "$0.00 (estimated)"
        );
    }

    #[test]
    fn render_rounds_micros_to_cents_half_up() {
        // $0.30 exactly (300_000 micros).
        assert_eq!(
            render_dollars(Micros(300_000), EstimateLabel::Estimated),
            "$0.30 (estimated)"
        );
        // Sub-cent rounds to the rendered cent precision, half-up: 305_000 micros =
        // $0.305 → $0.31 (half-cent rounds up).
        assert_eq!(
            render_dollars(Micros(305_000), EstimateLabel::Estimated),
            "$0.31 (estimated)"
        );
        // 304_999 micros = $0.304999 → $0.30 (below the half-cent).
        assert_eq!(
            render_dollars(Micros(304_999), EstimateLabel::Estimated),
            "$0.30 (estimated)"
        );
        // A dollar-and-cents value: $12.34 = 12_340_000 micros.
        assert_eq!(
            render_dollars(Micros(12_340_000), EstimateLabel::Estimated),
            "$12.34 (estimated)"
        );
    }

    #[test]
    fn render_always_appends_the_label() {
        // FR-23: no unlabeled dollar figure. Both labels appear in the output.
        assert!(render_dollars(Micros(1_000_000), EstimateLabel::Estimated).contains("(estimated)"));
        assert!(
            render_dollars(Micros(1_000_000), EstimateLabel::Reconciled).contains("(reconciled)")
        );
    }

    #[test]
    fn render_handles_a_large_value() {
        // A big cumulative cost renders correctly (no overflow, no truncation): $9.20 M.
        assert_eq!(
            render_dollars(Micros(9_200_000_000_000), EstimateLabel::Estimated),
            "$9200000.00 (estimated)"
        );
    }

    #[test]
    fn render_handles_a_defensive_negative_with_a_leading_sign() {
        // The derivation never produces a negative, but the formatter is total: a
        // negative renders with a leading `-` (defensive; e.g. a future credit).
        assert_eq!(
            render_dollars(Micros(-500_000), EstimateLabel::Estimated),
            "-$0.50 (estimated)"
        );
    }

    // ---- render_dollars_bare: the value-only companion (still the ONE module) ----

    #[test]
    fn render_dollars_bare_is_the_value_with_no_label() {
        // The bare form is the `$X.XX` digits ONLY — no `(estimated)`/`(reconciled)`
        // suffix — so a caller can place the qualifier elsewhere (e.g. a column
        // header) without doing string-surgery on a labeled string (primary L1).
        assert_eq!(render_dollars_bare(Micros(0)), "$0.00");
        assert_eq!(render_dollars_bare(Micros(300_000)), "$0.30");
        assert_eq!(render_dollars_bare(Micros(12_340_000)), "$12.34");
    }

    #[test]
    fn render_dollars_bare_rounds_cents_half_up_like_the_labeled_form() {
        // Identical cent rounding to render_dollars: $0.305 → $0.31 (half-cent up),
        // $0.304999 → $0.30 (below the half-cent).
        assert_eq!(render_dollars_bare(Micros(305_000)), "$0.31");
        assert_eq!(render_dollars_bare(Micros(304_999)), "$0.30");
    }

    #[test]
    fn render_dollars_bare_keeps_the_defensive_leading_sign() {
        // A defensive negative still renders with a leading `-` before the `$`.
        assert_eq!(render_dollars_bare(Micros(-500_000)), "-$0.50");
    }

    #[test]
    fn render_dollars_is_exactly_bare_plus_the_label() {
        // The labeled form is DEFINED as the bare value + " (label)", so the two can
        // never diverge — the digits originate in ONE place (render_dollars_bare).
        for micros in [
            Micros(0),
            Micros(300_000),
            Micros(-500_000),
            Micros(i64::MAX),
        ] {
            for label in [EstimateLabel::Estimated, EstimateLabel::Reconciled] {
                assert_eq!(
                    render_dollars(micros, label),
                    format!("{} ({label})", render_dollars_bare(micros)),
                );
            }
        }
    }
}
