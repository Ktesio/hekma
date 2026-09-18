//! Token budgets + the pure budget evaluator (spine AD-7, story 3-2) — the
//! ENFORCEMENT stage of the one metering pipeline.
//!
//! Story 3-1 delivered `MeteringSource → UsageEvent → ledger transaction` and
//! STOPPED at a documented evaluator-ready choke point
//! ([`super::supervisor::Supervisor::ingest_usage`]). 3-2 owns the rest of AD-7:
//! `→ BudgetEvaluator (inside the same commit path) → BreachDecision → supervisor
//! executes the Breach Action → emits the breach event`. This module is the pure,
//! I/O-free heart of that: the [`TokenBudget`] domain shape (FR-18's two scopes),
//! the [`BreachAction`] the operator configures (`pause`/`stop`/`warn`, default
//! `pause`), the [`BreachScope`]/[`BreachDecision`] the evaluator returns, and the
//! [`BudgetEvaluator`] itself — a total, pure token comparison that turns
//! just-committed totals into a decision.
//!
//! ## Boundary (what this is — the TOKEN dimension)
//!
//! This module is TOKENS ONLY: a [`TokenBudget`] is token-count ceilings, and the
//! [`BudgetEvaluator`] is the pure token decision. The DOLLAR dimension (story 3-3)
//! now lives in the sibling [`super::cost`] module — `Micros`/`Rate`/`CostCap` + a
//! thin `CostEvaluator` that REUSES THIS module's [`BreachDecision`]/[`BreachScope`]/
//! [`BreachAction`] verbatim (the decision's `limit`/`observed` are unit-agnostic
//! `u64`, carrying micro-dollars for a dollar breach). Both evaluators DECIDE; they
//! never enforce (no lifecycle, no ledger, no I/O) — the supervisor ACTS on the
//! decision in ONE choke point, so each decision stays trivially unit-testable and
//! cross-OS by construction (retro AI-37: the bulk of the boundary/scope coverage
//! lives in the two modules' pure unit tests). `EstimateLabel`, currency rendering,
//! and dollar headroom are `super::cost`'s (AD-8), not here.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A per-instance Token Budget (spine FR-18) — per-run and cumulative TOKEN
/// ceilings. The dollar `CostCap` sibling lives in [`super::cost`] (story 3-3);
/// a [`TokenBudget`] itself carries no dollars/Rate/label (AD-8).
///
/// Both scopes are optional (`u64` to match [`super::usage::UsageTotals`]'s token
/// type): an UNSET scope (`None`) NEVER breaches — only a configured ceiling is
/// enforced. The per-run ceiling bounds a single Run's span
/// (`starting`→terminal); the cumulative ceiling bounds the instance's lifetime
/// across all Runs (spine AD-7's Run definition). `Serialize`/`Deserialize`
/// (snake_case) so it rides the Fleet-detail `budget` view (AC9) and the breach
/// event payload (AC10) without a second dialect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBudget {
    /// The per-run token ceiling (bounds one Run's span). `None` = unset (never
    /// breaches the per-run scope).
    pub per_run: Option<u64>,
    /// The cumulative token ceiling (bounds the instance's lifetime over all
    /// Runs). `None` = unset (never breaches the cumulative scope).
    pub cumulative: Option<u64>,
}

impl TokenBudget {
    /// A budget with neither scope set (the honest "no budget configured" value —
    /// the evaluator returns [`BreachDecision::WithinBudget`] for it always).
    pub const fn none() -> Self {
        Self {
            per_run: None,
            cumulative: None,
        }
    }

    /// Whether at least one scope is configured (a real budget exists). An
    /// all-`None` budget is surfaced as an honest ABSENT budget in Fleet detail
    /// (never a fabricated `0` ceiling — AC9).
    pub fn is_set(&self) -> bool {
        self.per_run.is_some() || self.cumulative.is_some()
    }
}

/// The Breach Action the supervisor executes when a budget is breached (spine
/// FR-21, AC-C) — per-instance configurable, defaulting to [`BreachAction::Pause`]
/// (the ratified shipped default, PRD FR-21 `[FIXED — pause default ratified by
/// Islam 2026-07-02]`).
///
/// The three semantics (executed by the supervisor via Epic-1's EXISTING
/// lifecycle — AD-15, NO new state/edge, only a new CAUSE): **`pause`** drives
/// `running → paused` (honoring the adapter pause Capability Declaration — story
/// 1-5); **`stop`** drives `running → stopping → stopped` (story 1-4);
/// **`warn`** performs NO lifecycle transition (records the breach event only).
/// The wire spelling is kebab-case (`"pause"`/`"stop"`/`"warn"`) — the config
/// value + the breach-event `action` field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreachAction {
    /// Pause the instance (`running → paused`) — the ratified DEFAULT. Honors the
    /// adapter's pause Capability Declaration honestly (guaranteed/best-effort/
    /// unsupported); a best-effort/unsupported pause still records the breach.
    #[default]
    Pause,
    /// Stop the instance to a terminal `stopped` (`running → stopping → stopped`).
    Stop,
    /// Record the breach event only — NO lifecycle transition (the agent keeps
    /// running; the lightest guardrail: visibility without interruption).
    Warn,
}

impl BreachAction {
    /// The stable wire/label form (`"pause"`/`"stop"`/`"warn"`), matching the
    /// serde kebab-case rename so a config string and this label never diverge
    /// (the [`super::restart::RestartPolicy`] convention).
    pub fn as_str(&self) -> &'static str {
        match self {
            BreachAction::Pause => "pause",
            BreachAction::Stop => "stop",
            BreachAction::Warn => "warn",
        }
    }
}

impl std::fmt::Display for BreachAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error a bad `budget.breach_action` config string produces (AC-C: an
/// unknown action is rejected at config-write time with a clear diagnostic, never
/// silently defaulted). Names the offending value + the accepted set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseBreachActionError {
    /// The rejected value.
    pub value: String,
}

impl std::fmt::Display for ParseBreachActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' is not a valid breach action (expected one of: pause, stop, warn)",
            self.value
        )
    }
}

impl std::error::Error for ParseBreachActionError {}

impl FromStr for BreachAction {
    type Err = ParseBreachActionError;

    /// Parse the config string into a [`BreachAction`] (AC-C validation). Matches
    /// the kebab-case wire spelling exactly; anything else is rejected (never
    /// silently defaulted — the honesty rule). Case-sensitive: the config
    /// convention is lower-case tokens.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pause" => Ok(BreachAction::Pause),
            "stop" => Ok(BreachAction::Stop),
            "warn" => Ok(BreachAction::Warn),
            other => Err(ParseBreachActionError {
                value: other.to_string(),
            }),
        }
    }
}

/// Which budget SCOPE tripped (spine FR-18) — reported on a [`BreachDecision::Breached`]
/// so the breach event + the `BudgetExceeded` transition cause are honest about
/// the reason. `PerRun` is checked before `Cumulative` (precedence — the tighter,
/// run-local bound is reported first when both would trip on the same event).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreachScope {
    /// The per-run ceiling tripped (the current Run's total reached the per-run
    /// budget).
    PerRun,
    /// The cumulative ceiling tripped (the lifetime total reached the cumulative
    /// budget).
    Cumulative,
}

impl BreachScope {
    /// The stable wire/label form (`"per-run"`/`"cumulative"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            BreachScope::PerRun => "per-run",
            BreachScope::Cumulative => "cumulative",
        }
    }
}

impl std::fmt::Display for BreachScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The pure evaluator's verdict for one just-committed ledger event (spine AD-7).
///
/// [`BreachDecision::WithinBudget`] = no configured ceiling was reached (or no
/// budget is set). [`BreachDecision::Breached`] carries enough to build the breach
/// event + the honest `BudgetExceeded` cause WITHOUT re-reading anything: the
/// scope that tripped, the action to take (the resolved [`BreachAction`]), the
/// `limit` that was reached, and the `observed` total that reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreachDecision {
    /// No ceiling reached — the agent keeps running, nothing recorded.
    WithinBudget,
    /// A ceiling was reached (`observed >= limit`) — the supervisor records the
    /// breach event ALWAYS and then executes `action`.
    Breached {
        /// Which scope tripped (per-run reported before cumulative).
        scope: BreachScope,
        /// The configured Breach Action to execute.
        action: BreachAction,
        /// The ceiling that was reached.
        limit: u64,
        /// The committed total that reached it (`>= limit`).
        observed: u64,
    },
}

impl BreachDecision {
    /// Whether this decision is a breach (a convenience for the supervisor's
    /// branch + the tests).
    pub fn is_breached(&self) -> bool {
        matches!(self, BreachDecision::Breached { .. })
    }
}

/// The pure budget evaluator (spine AD-7 — the enforcement stage's DECIDE half).
///
/// A zero-sized decision engine: [`BudgetEvaluator::evaluate`] is a total, pure
/// function of the just-committed per-run + cumulative totals, the resolved
/// [`TokenBudget`], and the resolved [`BreachAction`]. It performs NO I/O, NO
/// lifecycle, NO ledger read — so it is trivially unit-testable and identical on
/// every OS. The supervisor calls it INSIDE the ledger-commit choke point
/// ([`super::supervisor::Supervisor::ingest_usage`]) right after a fresh
/// `Inserted`, then ACTS on the returned [`BreachDecision`].
pub struct BudgetEvaluator;

impl BudgetEvaluator {
    /// Evaluate the just-committed totals against the resolved budget.
    ///
    /// Threshold semantics are **`>=`** (AC-A "reaches = at-or-over"): a budget of
    /// exactly `N` tokens breaches when the committed total is `>= N`, NOT strictly
    /// greater — hitting the ceiling IS the breach (the honest guardrail fires AT
    /// the ceiling, not one token past it). An UNSET scope (`None`) is skipped
    /// (never breaches). PRECEDENCE: the per-run scope is checked FIRST; if it
    /// trips, its scope is reported (the action is the same regardless of scope, so
    /// precedence only affects the reported label). Otherwise the cumulative scope
    /// is checked. If neither trips, [`BreachDecision::WithinBudget`].
    pub fn evaluate(
        run_total: u64,
        cumulative_total: u64,
        budget: &TokenBudget,
        action: BreachAction,
    ) -> BreachDecision {
        // Per-run first (the tighter, run-local bound) — AC precedence.
        if let Some(limit) = budget.per_run {
            if run_total >= limit {
                return BreachDecision::Breached {
                    scope: BreachScope::PerRun,
                    action,
                    limit,
                    observed: run_total,
                };
            }
        }
        // Then cumulative (the lifetime bound).
        if let Some(limit) = budget.cumulative {
            if cumulative_total >= limit {
                return BreachDecision::Breached {
                    scope: BreachScope::Cumulative,
                    action,
                    limit,
                    observed: cumulative_total,
                };
            }
        }
        BreachDecision::WithinBudget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TokenBudget shape ----

    #[test]
    fn token_budget_none_is_unset_and_not_set() {
        let b = TokenBudget::none();
        assert_eq!(b.per_run, None);
        assert_eq!(b.cumulative, None);
        assert!(!b.is_set());
        assert_eq!(TokenBudget::default(), TokenBudget::none());
    }

    #[test]
    fn token_budget_is_set_when_any_scope_present() {
        assert!(TokenBudget {
            per_run: Some(1),
            cumulative: None
        }
        .is_set());
        assert!(TokenBudget {
            per_run: None,
            cumulative: Some(1)
        }
        .is_set());
        assert!(TokenBudget {
            per_run: Some(1),
            cumulative: Some(2)
        }
        .is_set());
    }

    #[test]
    fn token_budget_is_tokens_only_and_round_trips_snake_case() {
        // AC4: the two FR-18 scopes, snake_case on the wire, NO dollar/rate/label.
        let b = TokenBudget {
            per_run: Some(100),
            cumulative: Some(500),
        };
        let value: serde_json::Value = serde_json::to_value(b).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["cumulative", "per_run"]);
        assert!(obj.get("cost").is_none());
        assert!(obj.get("rate").is_none());
        assert!(obj.get("estimate_label").is_none());
        let back: TokenBudget = serde_json::from_value(value).unwrap();
        assert_eq!(back, b);
    }

    // ---- BreachAction parse + wire form ----

    #[test]
    fn breach_action_default_is_pause() {
        // The ratified shipped default (PRD FR-21).
        assert_eq!(BreachAction::default(), BreachAction::Pause);
    }

    #[test]
    fn breach_action_parses_the_three_known_tokens() {
        assert_eq!(
            "pause".parse::<BreachAction>().unwrap(),
            BreachAction::Pause
        );
        assert_eq!("stop".parse::<BreachAction>().unwrap(), BreachAction::Stop);
        assert_eq!("warn".parse::<BreachAction>().unwrap(), BreachAction::Warn);
    }

    #[test]
    fn breach_action_rejects_unknown_and_names_it_never_defaults() {
        // AC-C: an unknown action is rejected (never silently defaulted).
        let err = "throttle".parse::<BreachAction>().unwrap_err();
        assert_eq!(err.value, "throttle");
        let msg = err.to_string();
        assert!(msg.contains("throttle"), "{msg}");
        assert!(msg.contains("pause"), "{msg}");
        assert!(msg.contains("stop"), "{msg}");
        assert!(msg.contains("warn"), "{msg}");
        // Case-sensitive: the config convention is lower-case.
        assert!("Pause".parse::<BreachAction>().is_err());
        assert!("".parse::<BreachAction>().is_err());
    }

    #[test]
    fn breach_action_wire_form_round_trips() {
        for (action, wire) in [
            (BreachAction::Pause, "pause"),
            (BreachAction::Stop, "stop"),
            (BreachAction::Warn, "warn"),
        ] {
            assert_eq!(action.as_str(), wire);
            assert_eq!(action.to_string(), wire);
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            let back: BreachAction = serde_json::from_str(&json).unwrap();
            assert_eq!(back, action);
            // as_str() agrees with the parse round-trip.
            assert_eq!(wire.parse::<BreachAction>().unwrap(), action);
        }
    }

    #[test]
    fn breach_scope_wire_form() {
        assert_eq!(BreachScope::PerRun.as_str(), "per-run");
        assert_eq!(BreachScope::Cumulative.as_str(), "cumulative");
        assert_eq!(BreachScope::PerRun.to_string(), "per-run");
        // serde kebab-case matches as_str.
        assert_eq!(
            serde_json::to_string(&BreachScope::PerRun).unwrap(),
            "\"per-run\""
        );
        assert_eq!(
            serde_json::to_string(&BreachScope::Cumulative).unwrap(),
            "\"cumulative\""
        );
    }

    // ---- BudgetEvaluator: the pure decision (the bulk of coverage) ----

    #[test]
    fn no_budget_is_always_within_budget() {
        // An all-None budget never breaches, no matter the totals.
        let d = BudgetEvaluator::evaluate(
            u64::MAX,
            u64::MAX,
            &TokenBudget::none(),
            BreachAction::Pause,
        );
        assert_eq!(d, BreachDecision::WithinBudget);
        assert!(!d.is_breached());
    }

    #[test]
    fn below_budget_is_within_budget() {
        let budget = TokenBudget {
            per_run: Some(100),
            cumulative: Some(1000),
        };
        // run 99 < 100 and cumulative 999 < 1000 → within.
        let d = BudgetEvaluator::evaluate(99, 999, &budget, BreachAction::Warn);
        assert_eq!(d, BreachDecision::WithinBudget);
    }

    #[test]
    fn equal_to_ceiling_breaches_the_ge_boundary() {
        // AC-A: reaches = `>=`. A total landing EXACTLY on the ceiling breaches.
        let budget = TokenBudget {
            per_run: None,
            cumulative: Some(100),
        };
        let d = BudgetEvaluator::evaluate(0, 100, &budget, BreachAction::Pause);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::Cumulative,
                action: BreachAction::Pause,
                limit: 100,
                observed: 100,
            }
        );
        assert!(d.is_breached());
    }

    #[test]
    fn one_below_ceiling_does_not_breach_the_ge_boundary() {
        // The exact companion of the boundary test: N-1 is still within.
        let budget = TokenBudget {
            per_run: None,
            cumulative: Some(100),
        };
        assert_eq!(
            BudgetEvaluator::evaluate(0, 99, &budget, BreachAction::Pause),
            BreachDecision::WithinBudget
        );
    }

    #[test]
    fn over_ceiling_breaches_and_reports_observed() {
        let budget = TokenBudget {
            per_run: None,
            cumulative: Some(100),
        };
        let d = BudgetEvaluator::evaluate(0, 250, &budget, BreachAction::Stop);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::Cumulative,
                action: BreachAction::Stop,
                limit: 100,
                observed: 250,
            }
        );
    }

    #[test]
    fn per_run_scope_breaches_independently_of_cumulative() {
        // Only the per-run ceiling set: a run total at/over it breaches PerRun even
        // though cumulative is unset.
        let budget = TokenBudget {
            per_run: Some(50),
            cumulative: None,
        };
        let d = BudgetEvaluator::evaluate(50, 9999, &budget, BreachAction::Pause);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 50,
                observed: 50,
            }
        );
    }

    #[test]
    fn an_unset_scope_never_breaches_even_with_huge_totals() {
        // per_run unset: a massive run total does NOT breach; cumulative set + not
        // reached → within.
        let budget = TokenBudget {
            per_run: None,
            cumulative: Some(1_000_000),
        };
        assert_eq!(
            BudgetEvaluator::evaluate(u64::MAX, 10, &budget, BreachAction::Pause),
            BreachDecision::WithinBudget
        );
    }

    #[test]
    fn both_set_and_both_would_trip_reports_per_run_first() {
        // PRECEDENCE: when both scopes would trip on the same event, the per-run
        // scope is reported (the action is identical regardless of scope).
        let budget = TokenBudget {
            per_run: Some(10),
            cumulative: Some(20),
        };
        let d = BudgetEvaluator::evaluate(15, 30, &budget, BreachAction::Pause);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::PerRun,
                action: BreachAction::Pause,
                limit: 10,
                observed: 15,
            }
        );
    }

    #[test]
    fn both_set_but_only_cumulative_trips_reports_cumulative() {
        // Per-run NOT reached, cumulative reached → the cumulative scope trips
        // (proves the per-run-first check falls through correctly).
        let budget = TokenBudget {
            per_run: Some(1000),
            cumulative: Some(20),
        };
        let d = BudgetEvaluator::evaluate(15, 25, &budget, BreachAction::Warn);
        assert_eq!(
            d,
            BreachDecision::Breached {
                scope: BreachScope::Cumulative,
                action: BreachAction::Warn,
                limit: 20,
                observed: 25,
            }
        );
    }

    #[test]
    fn a_zero_ceiling_breaches_on_any_committed_total() {
        // Degenerate but well-defined: a ceiling of 0 means "no budget to spend",
        // so any committed total (even 0) is `>= 0` → an immediate breach. Recorded
        // so the boundary is unambiguous.
        let budget = TokenBudget {
            per_run: Some(0),
            cumulative: None,
        };
        let d = BudgetEvaluator::evaluate(0, 0, &budget, BreachAction::Pause);
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

    #[test]
    fn the_reported_action_is_the_resolved_action() {
        // The evaluator threads the resolved action straight through (it does not
        // pick one) — proves each action arm rides the decision unchanged.
        for action in [BreachAction::Pause, BreachAction::Stop, BreachAction::Warn] {
            let budget = TokenBudget {
                per_run: None,
                cumulative: Some(1),
            };
            match BudgetEvaluator::evaluate(0, 1, &budget, action) {
                BreachDecision::Breached { action: a, .. } => assert_eq!(a, action),
                other => panic!("expected a breach, got {other:?}"),
            }
        }
    }
}
