//! Governance — the pure decision core + the portable conformance vectors
//! (spine AD-7/FR-44, story 19-1) — the DECISION half of the metering
//! guardrail, extracted so the engine and the artifact share ONE
//! implementation.
//!
//! ## Why this module exists (D5)
//!
//! The budget/cost decision semantics (the `>=` threshold, the per-scope
//! latch, token-before-dollar order, integer micro-dollar arithmetic,
//! round-half-up, saturation, no retroactive repricing) lived only in Rust
//! code and Rust tests. The first production Host enforces money at its own
//! external choke point and wants those rules THERE, provably. This module
//! gives them a portable form twice over:
//!
//! 1. **[`decide`]** — the pure decision core `enforce_budget` routes
//!    through. Both dimension evaluations + the `(dimension, scope)` latch
//!    filter, in fire order, I/O-free. The supervisor keeps the config/ledger
//!    reads and the record-then-act halves, so there is exactly ONE decision
//!    implementation and the artifact cannot drift from the engine by
//!    construction.
//! 2. **The vectors artifact** — `governance/vectors-v1.json` (embedded via
//!    [`crate::governance_vectors_json`]), a versioned, SHA-256-checksummed
//!    JSON document a NON-Rust Host pins by version + checksum: ledger state
//!    and ceilings in, decisions out. Every case is generated FROM the real
//!    evaluators ([`BudgetEvaluator`], [`CostEvaluator`], [`cost_micros`],
//!    [`cost_micros_with_cached`], [`decide`]) and is re-run against them by
//!    the verifier tests — byte-identical or RED.
//!
//! ## Boundary (what this is NOT)
//!
//! PURE + I/O-free decisions only (mirroring [`super::budget`] and
//! [`super::cost`]): no registry, no lifecycle, no clock. The artifact
//! describes DECISIONS, never enforcement power — Hekma is a cooperative
//! meter, not a sandbox: the Host's own choke point stays the enforcement of
//! record. The `enforce_budget` supervision path (the record-first-then-act
//! execution) stays in [`super::supervisor`].

use serde::{Deserialize, Serialize};

use super::budget::{BreachAction, BreachDecision, BreachScope, BudgetEvaluator, TokenBudget};
use super::cost::{cost_micros, cost_micros_with_cached, CostCap, CostEvaluator, Micros, Rate};
use super::event::BreachDimension;

/// The artifact id — the `artifact` field's fixed value and the name every
/// surface (docs, CLI verb, release asset prose) uses.
pub const GOVERNANCE_VECTORS_ARTIFACT_ID: &str = "hekma-governance-vectors";

/// The artifact's repo path — the canonical location the docs name, the
/// release job copies to the release assets, and the `.sha256` file cites.
pub const GOVERNANCE_VECTORS_ARTIFACT_PATH: &str = "crates/hekma-engine/governance/vectors-v1.json";

/// The vectors version — MAJOR.MINOR.PATCH over the CASE SET: additive cases
/// are a MINOR (a consumer's re-run stays green); ANY change to an expected
/// output, an input, or the schema is a MAJOR and is announced ahead in the
/// release notes (the pinning contract, docs/governance.md). Mirrored inside
/// the artifact's `vectors_version` field by the generator — they cannot
/// drift.
pub const GOVERNANCE_VECTORS_VERSION: &str = "1.1.0";

/// The artifact's schema version — `1` until the DOCUMENT shape itself
/// changes (a breaking schema change is a major `vectors_version` bump).
pub const GOVERNANCE_VECTORS_SCHEMA_VERSION: u32 = 1;

/// The provenance list — the engine symbols every case is generated from and
/// verified against (fixed order; the byte-compare pins it).
const GENERATED_FROM: [&str; 6] = [
    "BudgetEvaluator",
    "CostEvaluator",
    "cost_micros",
    "cost_micros_with_cached",
    "governance::decide",
    "governance::fence_refusal",
];

// ---------------------------------------------------------------------------
// The decision core (the shape `enforce_budget` routes through)
// ---------------------------------------------------------------------------

/// ONE dimension's enforcement inputs for [`decide`] — the TOKEN half
/// (story 3-2's shape): the just-committed totals + the resolved budget +
/// action, exactly what [`BudgetEvaluator::evaluate`] consumes. `None` =
/// the dimension is UNARMED (no ceiling set) and is skipped entirely — the
/// same inert rule `enforce_budget`'s armed checks apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenArm {
    /// The just-committed per-run token total (`0` when the run read failed —
    /// the supervisor's degrade-to-`0` read).
    pub run_total: u64,
    /// The just-committed cumulative token total.
    pub cumulative_total: u64,
    /// The resolved token budget (FR-18's two scopes; `None` scopes never
    /// breach).
    pub budget: TokenBudget,
    /// The resolved Breach Action to thread through any breach.
    pub action: BreachAction,
}

/// ONE dimension's enforcement inputs for [`decide`] — the DOLLAR half
/// (story 3-3's shape): the derived costs in integer micro-dollars + the
/// resolved cap + action, exactly what [`CostEvaluator::evaluate`]
/// consumes. `None` = the dimension is UNARMED (no Rate, or no cap scope —
/// AC-B's inert rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DollarArm {
    /// The derived per-run cost (integer micro-dollars).
    pub run_cost: Micros,
    /// The derived cumulative cost (integer micro-dollars).
    pub cumulative_cost: Micros,
    /// The resolved dollar cap (FR-21's two scopes; `None` scopes never
    /// breach).
    pub cap: CostCap,
    /// The resolved Breach Action to thread through any breach.
    pub action: BreachAction,
}

/// A latched `(dimension, scope)` pair — the per-Run breach latch's key
/// (story 3-2/3-3), surfaced as data so [`decide`] can filter without
/// touching supervisor state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LatchKey {
    /// The dimension that fired (tokens / dollars).
    pub dimension: BreachDimension,
    /// The scope that fired (per-run / cumulative).
    pub scope: BreachScope,
}

impl LatchKey {
    /// The key for a `(dimension, scope)` tuple straight off the supervisor's
    /// latch set.
    pub(crate) const fn new(dimension: BreachDimension, scope: BreachScope) -> Self {
        Self { dimension, scope }
    }
}

/// The pure decision core's input (story 19-1): at most one arm per
/// dimension (the armed rule folds into `None`) + the per-Run latch
/// snapshot. Serialize/Deserialize (snake_case keys, kebab-case enums — the
/// house wire convention) because the vectors artifact carries exactly this
/// shape as its `decision`-case input.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceInput {
    /// The TOKEN arm — `None` when no token ceiling is configured (skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<TokenArm>,
    /// The DOLLAR arm — `None` when the dollar dimension is inert (no Rate
    /// or no cap scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar: Option<DollarArm>,
    /// The per-Run latch snapshot: every `(dimension, scope)` pair that has
    /// already fired this Run. An already-latched pair NEVER re-fires (the
    /// idempotence rule); the v1 re-arm is a FRESH latch at a Run boundary.
    #[serde(default)]
    pub latched: Vec<LatchKey>,
}

/// ONE fired decision in [`GovernanceOutcome::decisions`] — the fields the
/// breach event and the transition cause are built from (the same shape
/// [`BreachDecision::Breached`] carries, plus its dimension).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceDecision {
    /// Which dimension fired (tokens / dollars).
    pub dimension: BreachDimension,
    /// Which scope tripped (per-run reported before cumulative).
    pub scope: BreachScope,
    /// The configured Breach Action to execute.
    pub action: BreachAction,
    /// The ceiling that was reached (tokens, or micro-dollars for a dollar
    /// breach — the decision fields are unit-agnostic `u64`, AD-8).
    pub limit: u64,
    /// The committed total that reached it (`>= limit`).
    pub observed: u64,
}

/// The pure decision core's output: the fired decisions in FIRE ORDER (the
/// token dimension FIRST, then the dollar — the order `enforce_budget`
/// applies and the `token-before-dollar` vectors rule pins) + the latch
/// state AFTER those firings (input pairs + the newly fired ones, so a
/// consumer can chain the next evaluation).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceOutcome {
    /// The decisions to act on, in fire order (token before dollar). At most
    /// one per dimension — each evaluator returns at most one breach.
    pub decisions: Vec<GovernanceDecision>,
    /// The latch state after the firings: every input pair plus every newly
    /// fired `(dimension, scope)`. A consumer feeding this back as the next
    /// evaluation's `latched` gets the exact once-per-Run idempotence the
    /// supervisor's in-memory latch gives.
    pub latched_after: Vec<LatchKey>,
}

/// Decide — the PURE governance decision core (story 19-1): evaluate the
/// TOKEN dimension first (story 3-2), then the DOLLAR dimension (story 3-3),
/// filtering each evaluator's verdict through the per-Run
/// `(dimension, scope)` latch.
///
/// This is EXACTLY the decision half of
/// [`super::supervisor::Supervisor::enforce_budget`], extracted verbatim so
/// the supervisor and the vectors artifact share one implementation:
///
/// * **Order** — token decisions always precede dollar decisions in
///   [`GovernanceOutcome::decisions`] (both armed and both tripping → the
///   token decision is FIRST).
/// * **Threshold** — each evaluator's `>=` (at-or-over) semantics, per-run
///   scope before cumulative (unchanged, budget.rs/cost.rs).
/// * **Latch** — an input pair in `latched` suppresses that dimension+scope
///   entirely (no decision, no re-fire); a fresh latch + the same totals
///   fires again (the Run-boundary re-arm). A newly fired pair is appended
///   to [`GovernanceOutcome::latched_after`].
/// * **Normalization** — the `latched` input is DEDUPLICATED (first
///   occurrence kept, order preserved), so `latched_after` NEVER carries a
///   duplicate `(dimension, scope)` pair no matter what the caller passes in
///   (19-1 review round 1: the boundary contract — a consumer chaining
///   [`GovernanceOutcome::latched_after`] back in gets a well-formed latch
///   set, and a doubled input key behaves exactly like a single one).
/// * **Arms** — a `None` arm is an unarmed dimension: skipped entirely, no
///   totals read, no decision (the supervisor's armed checks, now data).
///
/// NO I/O, no clock, no state — the same input always yields the same
/// output, on every OS (the artifact's determinism rides on this).
pub fn decide(input: &GovernanceInput) -> GovernanceOutcome {
    let mut decisions = Vec::new();
    let mut latched_after = normalize_latched(&input.latched);

    // (1) TOKEN dimension FIRST (the story-3-2 evaluation order).
    if let Some(token) = &input.token {
        if let BreachDecision::Breached {
            scope,
            action,
            limit,
            observed,
        } = BudgetEvaluator::evaluate(
            token.run_total,
            token.cumulative_total,
            &token.budget,
            token.action,
        ) {
            fire_unless_latched(
                &mut decisions,
                &mut latched_after,
                BreachDimension::Tokens,
                scope,
                action,
                limit,
                observed,
            );
        }
    }

    // (2) THEN the DOLLAR dimension (the story-3-3 evaluation, same choke
    // point). The token firing above can never suppress this one: the latch
    // is keyed by (dimension, scope) and the dimensions differ.
    if let Some(dollar) = &input.dollar {
        if let BreachDecision::Breached {
            scope,
            action,
            limit,
            observed,
        } = CostEvaluator::evaluate(
            dollar.run_cost,
            dollar.cumulative_cost,
            &dollar.cap,
            dollar.action,
        ) {
            fire_unless_latched(
                &mut decisions,
                &mut latched_after,
                BreachDimension::Dollars,
                scope,
                action,
                limit,
                observed,
            );
        }
    }

    GovernanceOutcome {
        decisions,
        latched_after,
    }
}

/// The latch-input normalization (19-1 review round 1): deduplicate the
/// `(dimension, scope)` pairs — first occurrence kept, input order preserved
/// (a linear scan, not a set, so the output ORDER stays deterministic for the
/// artifact). `decide()` runs this on its `latched` input, so
/// [`GovernanceOutcome::latched_after`] is always a well-formed set: a
/// consumer chaining it back can never accumulate duplicates across
/// evaluations.
fn normalize_latched(keys: &[LatchKey]) -> Vec<LatchKey> {
    let mut seen: Vec<LatchKey> = Vec::with_capacity(keys.len());
    for key in keys {
        if !seen.contains(key) {
            seen.push(*key);
        }
    }
    seen
}

/// The latch filter, shared by both dimensions: an already-latched
/// `(dimension, scope)` pair short-circuits (no decision, no re-fire — the
/// story 3-2/3-3 idempotence rule); anything else fires and latches.
#[allow(clippy::too_many_arguments)]
fn fire_unless_latched(
    decisions: &mut Vec<GovernanceDecision>,
    latched_after: &mut Vec<LatchKey>,
    dimension: BreachDimension,
    scope: BreachScope,
    action: BreachAction,
    limit: u64,
    observed: u64,
) {
    let key = LatchKey::new(dimension, scope);
    if latched_after.contains(&key) {
        // The latch holds: this pair already fired this Run. The breach event
        // and the action are BOTH suppressed (the supervisor's
        // short-circuit, now decided here).
        return;
    }
    latched_after.push(key);
    decisions.push(GovernanceDecision {
        dimension,
        scope,
        action,
        limit,
        observed,
    });
}

// ---------------------------------------------------------------------------
// The wire fence — `block`'s decision consumer (story 19-2)
// ---------------------------------------------------------------------------

/// The HTTP status of the fence refusal (`proxy-wire v1`, story 19-2):
/// **402 Payment Required**. Common SDKs (OpenAI, Anthropic, the Vercel AI
/// SDK) retry 408, 409, 429 and 5xx — never 402 — so a fenced agent fails
/// fast instead of hammering the listener; 403 is avoided because agents
/// read it as a credential failure.
pub const FENCE_REFUSAL_STATUS: u16 = 402;

/// The refusal body's stable error class (`error.type`) — the same for every
/// fence reason; `error.code` names the reason.
pub const FENCE_ERROR_TYPE: &str = "hekma_budget_block";

/// The `proxy-wire` contract version the refusal belongs to (story 19-2
/// ships its first slice; story 23-1 documents the rest unchanged).
pub const PROXY_WIRE_VERSION: &str = "v1";

/// The canonical fence-reason order: tokens before dollars (the
/// `token-before-dollar` rule), per-run before cumulative (the evaluators'
/// scope precedence). The refusal names the FIRST raised reason in this
/// order, and the listener's reason bitmask uses each reason's index here
/// as its bit.
pub const FENCE_REASON_ORDER: [LatchKey; 4] = [
    LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
    LatchKey::new(BreachDimension::Tokens, BreachScope::Cumulative),
    LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun),
    LatchKey::new(BreachDimension::Dollars, BreachScope::Cumulative),
];

/// The stable `error.code` for one fence reason — dimension and scope in the
/// snake_case code convention OpenAI-compatible clients expect.
pub fn fence_code(reason: LatchKey) -> &'static str {
    match (reason.dimension, reason.scope) {
        (BreachDimension::Tokens, BreachScope::PerRun) => "tokens_per_run",
        (BreachDimension::Tokens, BreachScope::Cumulative) => "tokens_cumulative",
        (BreachDimension::Dollars, BreachScope::PerRun) => "dollars_per_run",
        (BreachDimension::Dollars, BreachScope::Cumulative) => "dollars_cumulative",
    }
}

/// What a fenced listener answers (story 19-2): the status, the stable error
/// class and code, and the reason it reports. Pinned by the vectors'
/// `fence` cases, so a Host's guard and the engine refuse identically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceRefusal {
    /// The HTTP status — always [`FENCE_REFUSAL_STATUS`].
    pub status: u16,
    /// The body's `error.type` — always [`FENCE_ERROR_TYPE`].
    pub error_type: String,
    /// The body's `error.code` — [`fence_code`] of the reported reason.
    pub code: String,
    /// The reported reason's dimension.
    pub dimension: BreachDimension,
    /// The reported reason's scope.
    pub scope: BreachScope,
}

/// The fence's decision consumer (story 19-2): given the raised fence
/// reasons — every `(dimension, scope)` whose breach fired `block` this Run
/// — decide the refusal. No reason → `None` (the call passes). Otherwise the
/// FIRST reason in [`FENCE_REASON_ORDER`] is reported, whatever order the
/// reasons were raised in and however often one repeats. Pure, like
/// [`decide`]: the listener calls exactly this on every fenced request.
pub fn fence_refusal(reasons: &[LatchKey]) -> Option<FenceRefusal> {
    let reason = FENCE_REASON_ORDER
        .into_iter()
        .find(|candidate| reasons.contains(candidate))?;
    Some(FenceRefusal {
        status: FENCE_REFUSAL_STATUS,
        error_type: FENCE_ERROR_TYPE.to_string(),
        code: fence_code(reason).to_string(),
        dimension: reason.dimension,
        scope: reason.scope,
    })
}

// ---------------------------------------------------------------------------
// The vectors artifact (schema + generator)
// ---------------------------------------------------------------------------

/// A rule the D5 settlement names, tagged on every case that pins it. The
/// wire form is kebab-case. `ALL_COVERS` is the closed set the verifier
/// asserts is fully covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverTag {
    /// The `>=` threshold: exactly-at-ceiling breaches, one-below does not,
    /// a zero ceiling breaches on zero (tokens and micro-dollars alike).
    ThresholdGe,
    /// The per-scope latch: an already-fired `(dimension, scope)` pair never
    /// re-fires; the token and dollar dimensions of the SAME scope are
    /// independent and each fire once.
    LatchPerScope,
    /// The v1 re-arm: a FRESH latch at a Run boundary + the same totals
    /// fires again. (The ceiling-raise re-arm is a later additive
    /// extension — a minor vectors bump under the versioning policy.)
    LatchReArm,
    /// Token ceilings are evaluated (and reported) BEFORE dollar ceilings
    /// when both dimensions are armed and both trip.
    TokenBeforeDollar,
    /// Integer micro-dollar arithmetic: `u128` intermediates, per-direction
    /// rates, the cached re-split — never `f64`, never a wrap.
    IntegerMicros,
    /// The divide rounds HALF-UP: a tie rounds up, one-below-the-tie rounds
    /// down, each direction rounds independently.
    RoundHalfUp,
    /// Saturation: a runaway token count × a huge rate pins at `i64::MAX`
    /// micros — never wraps, so a cost can never silently un-breach.
    Saturation,
    /// No retroactive repricing: each ledger row is priced at the rate
    /// persisted WITH it; rows at different rates sum per-row, which differs
    /// from repricing the total at the latest rate.
    NoRetroRepricing,
    /// The `block` fence (story 19-2, vectors 1.1.0): a `block` decision
    /// fires under the same threshold, latch and order rules as any action,
    /// and a fenced Run refuses with status 402 and the
    /// `hekma_budget_block` class, reporting the first raised reason
    /// (tokens before dollars, per-run before cumulative).
    BlockFence,
}

impl CoverTag {
    /// The stable kebab-case wire form (matches the serde rename).
    pub fn as_str(&self) -> &'static str {
        match self {
            CoverTag::ThresholdGe => "threshold-ge",
            CoverTag::LatchPerScope => "latch-per-scope",
            CoverTag::LatchReArm => "latch-re-arm",
            CoverTag::TokenBeforeDollar => "token-before-dollar",
            CoverTag::IntegerMicros => "integer-micros",
            CoverTag::RoundHalfUp => "round-half-up",
            CoverTag::Saturation => "saturation",
            CoverTag::NoRetroRepricing => "no-retro-repricing",
            CoverTag::BlockFence => "block-fence",
        }
    }
}

/// The closed set of rules the vectors artifact must cover — the verifier
/// asserts every committed case's `covers` tags ⊇ this set (each rule pinned
/// by ≥1 case).
pub const ALL_COVERS: &[CoverTag] = &[
    CoverTag::ThresholdGe,
    CoverTag::LatchPerScope,
    CoverTag::LatchReArm,
    CoverTag::TokenBeforeDollar,
    CoverTag::IntegerMicros,
    CoverTag::RoundHalfUp,
    CoverTag::Saturation,
    CoverTag::NoRetroRepricing,
    CoverTag::BlockFence,
];

/// A `decision` case (story 19-1): a [`GovernanceInput`] (ledger state,
/// ceilings, resolved actions, the latch snapshot) and the EXACT expected
/// [`GovernanceOutcome`] — decisions in fire order + the latch-after state.
/// Generated from [`decide`] itself, so the engine and the artifact share
/// one implementation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionCase {
    /// The case's unique, stable id (kebab-case; the verifier asserts
    /// uniqueness).
    pub id: String,
    /// Which D5 rules this case pins (≥1; the verifier asserts full
    /// coverage across the case set).
    pub covers: Vec<CoverTag>,
    /// The inputs: ledger state + ceilings in.
    pub input: GovernanceInput,
    /// The expected decision: decisions out.
    pub expected: GovernanceOutcome,
}

/// Which pure derivation a `derivation` case pins, with that derivation's
/// inputs inline. The wire discriminator is the kebab-case `derivation` key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "derivation", rename_all = "kebab-case")]
pub enum DerivationSpec {
    /// [`cost_micros`] — the plain per-direction derivation: tokens × rate
    /// per direction, u128 intermediate, round-half-up divide, saturating
    /// sum.
    CostMicros {
        /// The input-direction token count.
        input_tokens: u64,
        /// The output-direction token count.
        output_tokens: u64,
        /// The rate to price BOTH directions with.
        rate: Rate,
    },
    /// [`cost_micros_with_cached`] — the cached-aware derivation: the input
    /// direction re-split (non-cached remainder at the input rate, cached
    /// subset at the cached rate) under the ledger's input-inclusive
    /// invariant.
    CostMicrosWithCached {
        /// The input-direction token count (cached subset INCLUDED).
        input_tokens: u64,
        /// The output-direction token count.
        output_tokens: u64,
        /// The known cached subset (`None` = unknown — the conservative
        /// whole-input-at-input-rate default).
        cached_tokens: Option<u64>,
        /// The rate (its optional cached price engages the re-split).
        rate: Rate,
    },
    /// The no-retro-repricing rule as a derivation: `rows` are ledger rows
    /// priced EACH at its own persisted rate (via
    /// [`cost_micros_with_cached`]) and summed per-row; `at_latest_rate` is
    /// the recorded ANTI-VALUE — the same total tokens repriced at the last
    /// row's rate — which the honest per-row sum MUST differ from.
    TwoRowOwnRateSum {
        /// The ledger rows in commit order (the LAST row carries the latest
        /// rate).
        rows: Vec<RowPricing>,
        /// The anti-value: the total tokens repriced at the latest rate (a
        /// re-pricing Host would produce this; the honest sum does not).
        at_latest_rate: i64,
    },
}

/// One ledger row's pricing shape for
/// [`DerivationSpec::TwoRowOwnRateSum`] — the tokens AND the rate persisted
/// with them (the no-retro-repricing contract: the rate travels with the
/// row, repricing never reaches back).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPricing {
    /// The row's input-direction token count.
    pub input_tokens: u64,
    /// The row's output-direction token count.
    pub output_tokens: u64,
    /// The row's known cached subset (`None` = unknown/pre-v7 row — the
    /// conservative default prices it at the input rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// The rate persisted WITH this row.
    pub rate: Rate,
}

/// A `derivation` case (story 19-1): pure cost-derivation inputs and the
/// EXACT expected [`Micros`] output. These pin the integer-micro-dollar
/// arithmetic rules (the money-correctness crux) for a Host implementing
/// the vectors outside Rust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivationCase {
    /// The case's unique, stable id (kebab-case; the verifier asserts
    /// uniqueness).
    pub id: String,
    /// Which D5 rules this case pins (≥1).
    pub covers: Vec<CoverTag>,
    /// The derivation + its inputs (the `derivation` key discriminates).
    #[serde(flatten)]
    pub derivation: DerivationSpec,
    /// The expected cost in integer micro-dollars (the [`Micros`] wire form
    /// — a bare integer, never a float, never a `$` string).
    pub expected_micros: i64,
}

/// A `fence` case (story 19-2, vectors 1.1.0): the raised fence reasons in,
/// the exact refusal out (`null` when nothing is raised) — what a Host's
/// wire guard answers for a fenced Run. Generated from [`fence_refusal`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceCase {
    /// The case's unique, stable id (kebab-case).
    pub id: String,
    /// Which rules this case pins (≥1).
    pub covers: Vec<CoverTag>,
    /// The raised fence reasons, in raise order (duplicates allowed).
    pub reasons: Vec<LatchKey>,
    /// The expected refusal, or `null` when the call passes.
    pub expected: Option<FenceRefusal>,
}

/// ONE case of the vectors artifact. The `kind` tag discriminates the case
/// kinds; the variant's fields are inlined beside it. A consumer pinned to
/// an older minor skips a kind it does not know (docs/governance.md).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Case {
    /// A full decision: [`GovernanceInput`] → [`GovernanceOutcome`].
    Decision(DecisionCase),
    /// A pure cost derivation: derivation inputs → expected [`Micros`].
    Derivation(DerivationCase),
    /// The wire fence's refusal: raised reasons → expected refusal (story
    /// 19-2, vectors 1.1.0).
    Fence(FenceCase),
}

/// Where the artifact's cases come from — the exact engine symbols the
/// generator drives and the verifier re-runs (fixed order; the
/// byte-compare pins the list). Provenance records the GENERATOR FUNCTIONS,
/// not the crate version: the crate version lives in the CHANGELOG and is
/// not a pin key — a Host pins `vectors_version` + the SHA-256, both of
/// which already cover the bytes (19-1 review round 1: `engine_version`
/// removed — it baked a build-time crate version into checksummed bytes,
/// forcing a checksum churn on every version bump with zero pin value).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The evaluated symbols, in evaluation order.
    pub generated_from: Vec<String>,
}

/// The governance conformance vectors document — the whole portable
/// artifact. Canonical bytes: `serde_json::to_string_pretty` (declaration
/// order everywhere — no maps where a struct can carry the field) plus ONE
/// trailing newline; the same input always yields byte-identical output, on
/// every platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorsDocument {
    /// The artifact id — always [`GOVERNANCE_VECTORS_ARTIFACT_ID`].
    pub artifact: String,
    /// The document schema version — [`GOVERNANCE_VECTORS_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The vectors version — [`GOVERNANCE_VECTORS_VERSION`]. THIS is the
    /// pin a Host declares: additive cases bump it minor; any expected-
    /// output change bumps it major and is announced.
    pub vectors_version: String,
    /// The provenance block (engine version + the generated-from list).
    pub provenance: Provenance,
    /// The cases, in fixed canonical order.
    pub cases: Vec<Case>,
}

impl VectorsDocument {
    /// Every case's `covers` tags, deduplicated — the verifier asserts this
    /// set ⊇ `ALL_COVERS`.
    pub fn covered_tags(&self) -> Vec<CoverTag> {
        let mut seen: Vec<CoverTag> = Vec::new();
        for case in &self.cases {
            let covers: &[CoverTag] = match case {
                Case::Decision(decision) => &decision.covers,
                Case::Derivation(derivation) => &derivation.covers,
                Case::Fence(fence) => &fence.covers,
            };
            for tag in covers {
                if !seen.contains(tag) {
                    seen.push(*tag);
                }
            }
        }
        seen
    }
}

/// The canonical JSON bytes for the document: `to_string_pretty` (two-space
/// indent, declaration order — structs all the way down) plus ONE trailing
/// newline. THE serialization: the committed file, the embedded copy, the
/// checksum, and the CLI `--json` output are all exactly these bytes.
pub fn canonical_json(document: &VectorsDocument) -> String {
    let mut json = serde_json::to_string_pretty(document).expect("vectors document serializes");
    json.push('\n');
    json
}

/// The SHA-256 hex of the embedded canonical artifact bytes — the checksum
/// the `.sha256` file carries, the docs pin, and the release assets
/// publish. Computed from the EMBEDDED bytes (not a hand-pinned constant),
/// so it cannot drift from what the binary actually ships. Recomputed per
/// call: the artifact is ~14 KB (a microsecond-scale digest), and the
/// engine holds no lazy/global cell by the embed-cleanliness rule.
pub fn governance_vectors_checksum() -> String {
    crate::sha256::to_hex(&crate::sha256::digest(
        crate::governance_vectors_json().as_bytes(),
    ))
}

/// Generate the vectors document from the engine's own evaluators — the
/// artifact's ONLY writer. Case INPUTS are authored here (they define the
/// coverage); every case's EXPECTED output is computed by driving the real
/// pure functions ([`decide`], [`cost_micros`],
/// [`cost_micros_with_cached`]), so the committed artifact is a faithful
/// transcript of engine behavior: a semantic change to any evaluator makes
/// regeneration differ and the verifier's byte-compare goes RED.
pub fn generate() -> VectorsDocument {
    VectorsDocument {
        artifact: GOVERNANCE_VECTORS_ARTIFACT_ID.to_string(),
        schema_version: GOVERNANCE_VECTORS_SCHEMA_VERSION,
        vectors_version: GOVERNANCE_VECTORS_VERSION.to_string(),
        provenance: Provenance {
            generated_from: GENERATED_FROM
                .iter()
                .map(|symbol| symbol.to_string())
                .collect(),
        },
        // Story 19-2 (1.1.0) APPENDS its cases after the 1.0.0 set, so every
        // 1.0.0 case keeps its bytes and its position.
        cases: decision_cases()
            .into_iter()
            .chain(derivation_cases())
            .chain(block_fence_cases())
            .collect(),
    }
}

/// The `decision` cases, in fixed canonical order. Each case's `expected`
/// is computed by [`decide`] itself (via [`decision_case`]) — the authoring
/// below states only the INPUTS and the coverage tags.
fn decision_cases() -> Vec<Case> {
    vec![
        // ---- threshold-ge: exactly AT the ceiling breaches ----
        decision_case(
            "decision-threshold-ge-exact-at-ceiling",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 0,
                    cumulative_total: 100,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(100),
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- threshold-ge: one BELOW the ceiling does not breach ----
        decision_case(
            "decision-threshold-ge-one-below-ceiling",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 0,
                    cumulative_total: 99,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(100),
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- threshold-ge: a ZERO ceiling breaches on zero ----
        decision_case(
            "decision-threshold-ge-zero-ceiling-breaches-on-zero",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 0,
                    cumulative_total: 0,
                    budget: TokenBudget {
                        per_run: Some(0),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- threshold-ge: a dollar cap trips at exactly the cap (both
        // dimensions armed; the token total stays under its ceiling) ----
        decision_case(
            "decision-threshold-ge-dollar-exact-at-cap",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 10,
                    cumulative_total: 10,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(1_000),
                    },
                    action: BreachAction::Pause,
                }),
                dollar: Some(DollarArm {
                    run_cost: Micros(5_000_000),
                    cumulative_cost: Micros(5_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(5_000_000)),
                        cumulative: Some(Micros(50_000_000)),
                    },
                    action: BreachAction::Warn,
                }),
                latched: vec![],
            },
        ),
        // ---- threshold-ge: a ZERO dollar cap breaches on a zero cost (the
        // dollar arm of the zero-ceiling rule; 19-1 review round 1) ----
        decision_case(
            "decision-threshold-ge-zero-dollar-cap-breaches-on-zero",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: None,
                dollar: Some(DollarArm {
                    run_cost: Micros(0),
                    cumulative_cost: Micros(0),
                    cap: CostCap {
                        per_run: Some(Micros(0)),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                latched: vec![],
            },
        ),
        // ---- threshold-ge: precedence WITHIN a dimension — BOTH token
        // scopes trip and the PER-RUN bound is the one reported (the
        // evaluator's tighter-run-local-bound-first rule, 19-1 review
        // round 1) ----
        decision_case(
            "decision-threshold-ge-token-both-scopes-trip-per-run-reported",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 15,
                    cumulative_total: 30,
                    budget: TokenBudget {
                        per_run: Some(10),
                        cumulative: Some(20),
                    },
                    action: BreachAction::Warn,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- threshold-ge: the same within-dimension precedence on the
        // DOLLAR arm (both cap scopes trip → per-run reported) ----
        decision_case(
            "decision-threshold-ge-dollar-both-scopes-trip-per-run-reported",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: None,
                dollar: Some(DollarArm {
                    run_cost: Micros(15_000_000),
                    cumulative_cost: Micros(30_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(10_000_000)),
                        cumulative: Some(Micros(20_000_000)),
                    },
                    action: BreachAction::Warn,
                }),
                latched: vec![],
            },
        ),
        // ---- latch-per-scope: an already-latched pair never re-fires ----
        decision_case(
            "decision-latch-per-scope-holds-across-re-evaluation",
            &[CoverTag::LatchPerScope],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 100,
                    cumulative_total: 100,
                    budget: TokenBudget {
                        per_run: Some(100),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
            },
        ),
        // ---- latch-per-scope: token AND dollar of the SAME scope each fire
        // (dimensions are independent), token first ----
        decision_case(
            "decision-latch-per-scope-token-and-dollar-same-scope-fire",
            &[CoverTag::LatchPerScope, CoverTag::TokenBeforeDollar],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 100,
                    cumulative_total: 100,
                    budget: TokenBudget {
                        per_run: Some(100),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                dollar: Some(DollarArm {
                    run_cost: Micros(2_000_000),
                    cumulative_cost: Micros(2_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(2_000_000)),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                latched: vec![],
            },
        ),
        // ---- latch-per-scope: a latched (tokens, per-run) pair suppresses
        // ONLY that pair — the same dimension's OTHER scope and the other
        // dimension's SAME scope still fire ----
        decision_case(
            "decision-latch-scope-and-dimension-independence",
            &[CoverTag::LatchPerScope],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 50,
                    cumulative_total: 5_000,
                    budget: TokenBudget {
                        per_run: Some(1_000),
                        cumulative: Some(1_000),
                    },
                    action: BreachAction::Warn,
                }),
                dollar: Some(DollarArm {
                    run_cost: Micros(3_000_000),
                    cumulative_cost: Micros(3_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(3_000_000)),
                        cumulative: None,
                    },
                    action: BreachAction::Warn,
                }),
                latched: vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
            },
        ),
        // ---- latch-per-scope: the DOLLAR dimension's latch HOLD (19-1
        // review round 1: the token hold case above must not be the only
        // hold — the dollar latch suppresses its dimension the same way) ----
        decision_case(
            "decision-latch-per-scope-dollar-hold-suppresses-the-dollar-dimension",
            &[CoverTag::LatchPerScope],
            GovernanceInput {
                token: None,
                dollar: Some(DollarArm {
                    run_cost: Micros(2_000_000),
                    cumulative_cost: Micros(2_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(2_000_000)),
                        cumulative: None,
                    },
                    action: BreachAction::Stop,
                }),
                latched: vec![LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun)],
            },
        ),
        // ---- latch-per-scope: a DOUBLED latch key (the same pair twice in
        // the snapshot — 19-1 review round 1's normalization contract) holds
        // exactly like a single one, and latched_after carries the pair
        // ONCE ----
        decision_case(
            "decision-latch-per-scope-doubled-key-normalizes-to-one",
            &[CoverTag::LatchPerScope],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 100,
                    cumulative_total: 100,
                    budget: TokenBudget {
                        per_run: Some(100),
                        cumulative: None,
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![
                    LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
                    LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
                ],
            },
        ),
        // ---- latch-re-arm: a FRESH latch at a Run boundary — the case's
        // input latch is EMPTY and the same tripping totals refire. The
        // totals match the exact-at-ceiling case byte for byte; the HOLD
        // cases pin the suppression that this empty latch resets. (19-1
        // review round 1: renamed + recommented — the old comment claimed
        // "only the latch state differs", which was false: the input latch
        // here is empty, not merely different.) ----
        decision_case(
            "decision-latch-re-arm-empty-latch-refires",
            &[CoverTag::LatchReArm],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 0,
                    cumulative_total: 100,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(100),
                    },
                    action: BreachAction::Pause,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- token-before-dollar: both armed, BOTH tripping → the token
        // decision is FIRST in the list ----
        decision_case(
            "decision-token-before-dollar-both-armed-both-tripping",
            &[CoverTag::TokenBeforeDollar],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 0,
                    cumulative_total: 500,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(500),
                    },
                    action: BreachAction::Stop,
                }),
                dollar: Some(DollarArm {
                    run_cost: Micros(0),
                    cumulative_cost: Micros(9_000_000),
                    cap: CostCap {
                        per_run: None,
                        cumulative: Some(Micros(9_000_000)),
                    },
                    action: BreachAction::Stop,
                }),
                latched: vec![],
            },
        ),
        // ---- the unarmed dimension is SKIPPED entirely: a cap-only
        // instance (no token budget configured) evaluates the dollar arm
        // alone ----
        decision_case(
            "decision-unarmed-token-dimension-skipped-dollar-only",
            &[CoverTag::ThresholdGe],
            GovernanceInput {
                token: None,
                dollar: Some(DollarArm {
                    run_cost: Micros(0),
                    cumulative_cost: Micros(12_500_000),
                    cap: CostCap {
                        per_run: None,
                        cumulative: Some(Micros(5_000_000)),
                    },
                    action: BreachAction::Stop,
                }),
                latched: vec![],
            },
        ),
    ]
}

/// The story-19-2 cases (vectors 1.1.0), appended after the 1.0.0 set:
/// `block` decisions under the shared rules, then the fence refusals.
fn block_fence_cases() -> Vec<Case> {
    let key = LatchKey::new;
    vec![
        // ---- block-fence: `block` fires at exactly the ceiling like any
        // action (the threshold is action-agnostic) ----
        decision_case(
            "decision-block-fires-at-exact-ceiling",
            &[CoverTag::BlockFence, CoverTag::ThresholdGe],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 1_000,
                    cumulative_total: 1_000,
                    budget: TokenBudget {
                        per_run: Some(1_000),
                        cumulative: None,
                    },
                    action: BreachAction::Block,
                }),
                dollar: None,
                latched: vec![],
            },
        ),
        // ---- block-fence: both dimensions trip under `block` — token
        // first, both latch, both carry `block` ----
        decision_case(
            "decision-block-both-dimensions-token-first",
            &[CoverTag::BlockFence, CoverTag::TokenBeforeDollar],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 50,
                    cumulative_total: 5_000,
                    budget: TokenBudget {
                        per_run: None,
                        cumulative: Some(5_000),
                    },
                    action: BreachAction::Block,
                }),
                dollar: Some(DollarArm {
                    run_cost: Micros(2_000_000),
                    cumulative_cost: Micros(2_000_000),
                    cap: CostCap {
                        per_run: Some(Micros(1_000_000)),
                        cumulative: None,
                    },
                    action: BreachAction::Block,
                }),
                latched: vec![],
            },
        ),
        // ---- block-fence: an already-latched `block` pair never re-fires
        // (the fence stays raised; nothing re-records) ----
        decision_case(
            "decision-block-latch-holds",
            &[CoverTag::BlockFence, CoverTag::LatchPerScope],
            GovernanceInput {
                token: Some(TokenArm {
                    run_total: 2_000,
                    cumulative_total: 2_000,
                    budget: TokenBudget {
                        per_run: Some(1_000),
                        cumulative: None,
                    },
                    action: BreachAction::Block,
                }),
                dollar: None,
                latched: vec![key(BreachDimension::Tokens, BreachScope::PerRun)],
            },
        ),
        // ---- block-fence: no raised reason → the call passes ----
        fence_case("fence-no-reason-passes", &[]),
        // ---- block-fence: a single reason is reported as itself ----
        fence_case(
            "fence-single-dollar-cumulative",
            &[key(BreachDimension::Dollars, BreachScope::Cumulative)],
        ),
        // ---- block-fence: tokens are reported before dollars, whatever the
        // raise order ----
        fence_case(
            "fence-reports-tokens-before-dollars",
            &[
                key(BreachDimension::Dollars, BreachScope::PerRun),
                key(BreachDimension::Tokens, BreachScope::Cumulative),
            ],
        ),
        // ---- block-fence: per-run is reported before cumulative ----
        fence_case(
            "fence-reports-per-run-before-cumulative",
            &[
                key(BreachDimension::Tokens, BreachScope::Cumulative),
                key(BreachDimension::Tokens, BreachScope::PerRun),
            ],
        ),
        // ---- block-fence: a repeated reason behaves like a single one ----
        fence_case(
            "fence-duplicate-reason-is-one-reason",
            &[
                key(BreachDimension::Dollars, BreachScope::PerRun),
                key(BreachDimension::Dollars, BreachScope::PerRun),
            ],
        ),
    ]
}

/// Author ONE fence case: the raised reasons as written, the expected
/// refusal computed by [`fence_refusal`].
fn fence_case(id: &str, reasons: &[LatchKey]) -> Case {
    Case::Fence(FenceCase {
        id: id.to_string(),
        covers: vec![CoverTag::BlockFence],
        reasons: reasons.to_vec(),
        expected: fence_refusal(reasons),
    })
}

/// Author ONE decision case: the inputs + coverage tags as written, the
/// expected outcome computed by [`decide`] — the generator states inputs,
/// never outputs.
fn decision_case(id: &str, covers: &[CoverTag], input: GovernanceInput) -> Case {
    Case::Decision(DecisionCase {
        id: id.to_string(),
        covers: covers.to_vec(),
        expected: decide(&input),
        input,
    })
}

/// The `derivation` cases, in fixed canonical order. Each case's
/// `expected_micros` is computed by the real derivation (via
/// [`derivation_case`]).
fn derivation_cases() -> Vec<Case> {
    vec![
        // ---- integer-micros: split per-direction rates price
        // independently then sum (the common real shape) ----
        derivation_case(
            "derivation-integer-micros-split-per-direction-rates",
            &[CoverTag::IntegerMicros],
            DerivationSpec::CostMicros {
                input_tokens: 500_000,
                output_tokens: 250_000,
                rate: Rate::new(2_000_000, 8_000_000),
            },
        ),
        // ---- integer-micros: a single token costs its sub-cent micro
        // fraction (why micro-dollars exist; cents would truncate to 0) ----
        derivation_case(
            "derivation-integer-micros-single-token-sub-cent",
            &[CoverTag::IntegerMicros],
            DerivationSpec::CostMicros {
                input_tokens: 1,
                output_tokens: 0,
                rate: Rate::new(3_000_000, 15_000_000),
            },
        ),
        // ---- integer-micros: the cached re-split (input direction
        // re-split under the input-inclusive invariant) ----
        derivation_case(
            "derivation-integer-micros-cached-resplit",
            &[CoverTag::IntegerMicros],
            DerivationSpec::CostMicrosWithCached {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cached_tokens: Some(800_000),
                rate: Rate::new(3_000_000, 15_000_000).with_cached(300_000),
            },
        ),
        // ---- integer-micros: an UNSET cached rate prices the cached subset
        // at the input rate (the conservative default — overstate, never
        // understate) ----
        derivation_case(
            "derivation-integer-micros-unset-cached-rate-conservative",
            &[CoverTag::IntegerMicros],
            DerivationSpec::CostMicrosWithCached {
                input_tokens: 1_000_000,
                output_tokens: 2_000_000,
                cached_tokens: Some(800_000),
                rate: Rate::new(3_000_000, 15_000_000),
            },
        ),
        // ---- round-half-up: the tie (exactly .5 micro) rounds UP ----
        derivation_case(
            "derivation-round-half-up-tie-rounds-up",
            &[CoverTag::RoundHalfUp],
            DerivationSpec::CostMicros {
                input_tokens: 1_500_000,
                output_tokens: 0,
                rate: Rate::new(1, 0),
            },
        ),
        // ---- round-half-up: the OUTPUT direction's tie rounds UP too
        // (19-1 review round 1: the input-direction tie alone did not pin
        // the output half of the rule) ----
        derivation_case(
            "derivation-round-half-up-output-tie-rounds-up",
            &[CoverTag::RoundHalfUp],
            DerivationSpec::CostMicros {
                input_tokens: 0,
                output_tokens: 1_500_000,
                rate: Rate::new(0, 1),
            },
        ),
        // ---- round-half-up: one BELOW the tie rounds down ----
        derivation_case(
            "derivation-round-half-up-below-tie-rounds-down",
            &[CoverTag::RoundHalfUp],
            DerivationSpec::CostMicros {
                input_tokens: 1_499_999,
                output_tokens: 0,
                rate: Rate::new(1, 0),
            },
        ),
        // ---- round-half-up: the two directions round INDEPENDENTLY — BOTH
        // at the tie (19-1 review round 1: the previous shape, one direction
        // at the tie and one below it, was passed by an aggregate-then-round
        // Host too: 1.5 + 1.499999 = 2.999999 rounds to 3 either way. With
        // both directions at the tie the honest per-direction sum is
        // 2 + 2 = 4 while the wrong aggregate 3.0 rounds to 3 — the case now
        // DISCRIMINATES the implementations.) ----
        derivation_case(
            "derivation-round-half-up-per-direction-independence",
            &[CoverTag::RoundHalfUp],
            DerivationSpec::CostMicros {
                input_tokens: 1_500_000,
                output_tokens: 1_500_000,
                rate: Rate::new(1, 1),
            },
        ),
        // ---- saturation: a runaway token count × a max rate pins at
        // i64::MAX — never wraps, never silently un-breaches ----
        derivation_case(
            "derivation-saturation-max-tokens-max-rate",
            &[CoverTag::Saturation],
            DerivationSpec::CostMicros {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                rate: Rate::new(u64::MAX, u64::MAX),
            },
        ),
        // ---- saturation: the cached-aware derivation saturates the same
        // way (every direction × every rate maximal) ----
        derivation_case(
            "derivation-saturation-cached-derivation-saturates",
            &[CoverTag::Saturation],
            DerivationSpec::CostMicrosWithCached {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cached_tokens: Some(u64::MAX),
                rate: Rate::new(u64::MAX, u64::MAX).with_cached(u64::MAX),
            },
        ),
        // ---- no-retro-repricing: two rows at DIFFERENT rates sum per-row;
        // the anti-value input (the same totals repriced at the latest rate)
        // is what a re-pricing Host would wrongly produce. The expected
        // micros are COMPUTED here through the same machinery as every other
        // case (19-1 review round 1: the case previously bypassed
        // `derivation_case` with a hand-pinned expected value its verifier
        // never re-derived) ----
        derivation_case(
            "derivation-no-retro-repricing-rows-sum-at-their-own-rates",
            &[CoverTag::NoRetroRepricing],
            DerivationSpec::TwoRowOwnRateSum {
                rows: vec![
                    RowPricing {
                        input_tokens: 1_000_000,
                        output_tokens: 0,
                        cached_tokens: None,
                        rate: Rate::new(1_000_000, 15_000_000),
                    },
                    RowPricing {
                        input_tokens: 1_000_000,
                        output_tokens: 0,
                        cached_tokens: None,
                        rate: Rate::new(3_000_000, 15_000_000),
                    },
                ],
                at_latest_rate: 6_000_000,
            },
        ),
    ]
}

/// Author ONE single-derivation case: the derivation + inputs as written,
/// the expected micros computed by the REAL function the `DerivationSpec`
/// names.
fn derivation_case(id: &str, covers: &[CoverTag], derivation: DerivationSpec) -> Case {
    let expected_micros = run_derivation(&derivation);
    Case::Derivation(DerivationCase {
        id: id.to_string(),
        covers: covers.to_vec(),
        derivation,
        expected_micros,
    })
}

/// Run ONE [`DerivationSpec`] against the REAL pure derivation it names —
/// the single place the artifact's expected outputs and the verifier's
/// re-runs get their numbers.
pub fn run_derivation(spec: &DerivationSpec) -> i64 {
    match spec {
        DerivationSpec::CostMicros {
            input_tokens,
            output_tokens,
            rate,
        } => cost_micros(*input_tokens, *output_tokens, rate).get(),
        DerivationSpec::CostMicrosWithCached {
            input_tokens,
            output_tokens,
            cached_tokens,
            rate,
        } => cost_micros_with_cached(*input_tokens, *output_tokens, *cached_tokens, rate).get(),
        DerivationSpec::TwoRowOwnRateSum { rows, .. } => rows
            .iter()
            .map(|row| {
                cost_micros_with_cached(
                    row.input_tokens,
                    row.output_tokens,
                    row.cached_tokens,
                    &row.rate,
                )
            })
            .fold(Micros::ZERO, Micros::saturating_add)
            .get(),
    }
}

/// The `.sha256` sidecar's exact content for the given canonical bytes:
/// `<sha256-hex>  <repo-relative artifact path>` + newline (the
/// `sha256sum -c` format, pathed from the repo root so the repo-root gate
/// verifies in place).
pub fn checksum_file_content(canonical_bytes: &str) -> String {
    format!(
        "{}  {}\n",
        crate::sha256::to_hex(&crate::sha256::digest(canonical_bytes.as_bytes())),
        GOVERNANCE_VECTORS_ARTIFACT_PATH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed artifact's exact bytes (the same `include_str!` the
    /// crate root serves) and the committed `.sha256` sidecar.
    const COMMITTED: &str = include_str!("../../governance/vectors-v1.json");
    const COMMITTED_CHECKSUM_FILE: &str = include_str!("../../governance/vectors-v1.json.sha256");

    /// The on-disk artifact path, resolved from the crate root (the bless
    /// lever's write target) — the SAME file the `include_str!`s embed. The
    /// manifest dir IS the crate root, so the artifact is its `governance/`
    /// child.
    fn artifact_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("governance/vectors-v1.json")
    }

    /// Whether the bless lever is armed (`HEKMA_UPDATE_GOVERNANCE_VECTORS=1`):
    /// the byte-compare test then REWRITES the artifact + its checksum
    /// instead of asserting — the ratified-change lever (diff the PR to
    /// review every case change).
    fn bless_lever_armed() -> bool {
        std::env::var("HEKMA_UPDATE_GOVERNANCE_VECTORS").is_ok_and(|value| value == "1")
    }

    // ---- decide(): the pure decision core (the supervisor's equivalence) ----

    #[test]
    fn decide_runs_token_before_dollar_when_both_armed_and_both_tripping() {
        // THE ORDER PIN: both dimensions armed and both tripping → the token
        // decision is FIRST in the list (the enforce_budget evaluation
        // order, unchanged).
        let input = GovernanceInput {
            token: Some(TokenArm {
                run_total: 0,
                cumulative_total: 100,
                budget: TokenBudget {
                    per_run: None,
                    cumulative: Some(100),
                },
                action: BreachAction::Pause,
            }),
            dollar: Some(DollarArm {
                run_cost: Micros(0),
                cumulative_cost: Micros(5_000_000),
                cap: CostCap {
                    per_run: None,
                    cumulative: Some(Micros(5_000_000)),
                },
                action: BreachAction::Stop,
            }),
            latched: vec![],
        };
        let outcome = decide(&input);
        assert_eq!(outcome.decisions.len(), 2, "both dimensions fire");
        assert_eq!(
            outcome.decisions[0].dimension,
            BreachDimension::Tokens,
            "the TOKEN decision comes first: {:?}",
            outcome.decisions
        );
        assert_eq!(outcome.decisions[0].scope, BreachScope::Cumulative);
        assert_eq!(outcome.decisions[0].action, BreachAction::Pause);
        assert_eq!(
            outcome.decisions[1].dimension,
            BreachDimension::Dollars,
            "the DOLLAR decision comes second"
        );
        // The latch-after state carries both pairs, in fire order.
        assert_eq!(
            outcome.latched_after,
            vec![
                LatchKey::new(BreachDimension::Tokens, BreachScope::Cumulative),
                LatchKey::new(BreachDimension::Dollars, BreachScope::Cumulative),
            ]
        );
    }

    #[test]
    fn decide_latch_holds_an_already_fired_pair() {
        // THE IDEMPOTENCE PIN: a (dimension, scope) pair already in the
        // snapshot produces NO decision and stays latched (no re-fire, no
        // duplicate event, no re-run action).
        let input = GovernanceInput {
            token: Some(TokenArm {
                run_total: 200,
                cumulative_total: 200,
                budget: TokenBudget {
                    per_run: Some(100),
                    cumulative: None,
                },
                action: BreachAction::Pause,
            }),
            dollar: None,
            latched: vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
        };
        let outcome = decide(&input);
        assert!(
            outcome.decisions.is_empty(),
            "a latched pair must not re-fire: {:?}",
            outcome.decisions
        );
        assert_eq!(
            outcome.latched_after,
            vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
            "the latch state is unchanged"
        );
    }

    #[test]
    fn decide_normalizes_a_duplicated_latch_input_key() {
        // THE BOUNDARY NORMALIZATION (19-1 review round 1): a `latched` input
        // carrying the SAME (dimension, scope) pair twice behaves exactly like
        // a single one — the pair holds, and `latched_after` carries it ONCE
        // (a consumer chaining outcomes back can never accumulate
        // duplicates).
        let tripping = TokenArm {
            run_total: 100,
            cumulative_total: 100,
            budget: TokenBudget {
                per_run: Some(100),
                cumulative: None,
            },
            action: BreachAction::Pause,
        };
        let doubled = GovernanceInput {
            token: Some(tripping),
            dollar: None,
            latched: vec![
                LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
                LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
            ],
        };
        let outcome = decide(&doubled);
        assert!(
            outcome.decisions.is_empty(),
            "the doubled key still holds the latch: {:?}",
            outcome.decisions
        );
        assert_eq!(
            outcome.latched_after,
            vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
            "latched_after carries the pair exactly once"
        );
        // Order + first-occurrence semantics: dedup keeps the input order.
        let mixed = GovernanceInput {
            token: Some(tripping),
            dollar: Some(DollarArm {
                run_cost: Micros(2_000_000),
                cumulative_cost: Micros(2_000_000),
                cap: CostCap {
                    per_run: Some(Micros(2_000_000)),
                    cumulative: None,
                },
                action: BreachAction::Pause,
            }),
            latched: vec![
                LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun),
                LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
                LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun),
            ],
        };
        let outcome = decide(&mixed);
        assert_eq!(
            outcome.latched_after,
            vec![
                LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun),
                LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun),
            ],
            "the duplicated dollar key deduped in place; input order preserved"
        );
        assert!(
            outcome.decisions.is_empty(),
            "both pairs hold: {:?}",
            outcome.decisions
        );
    }

    #[test]
    fn decide_dimensions_of_the_same_scope_are_independent() {
        // EACH-DIMENSION-SAME-SCOPE INDEPENDENCE: a latched
        // (tokens, per-run) pair suppresses the token dimension's per-run
        // breach but NOT the dollar dimension's per-run breach — and the
        // token dimension's OTHER scope still fires.
        let input = GovernanceInput {
            token: Some(TokenArm {
                run_total: 50,
                cumulative_total: 500,
                budget: TokenBudget {
                    per_run: Some(100),
                    cumulative: Some(100),
                },
                action: BreachAction::Warn,
            }),
            dollar: Some(DollarArm {
                run_cost: Micros(1),
                cumulative_cost: Micros(1),
                cap: CostCap {
                    per_run: Some(Micros(1)),
                    cumulative: None,
                },
                action: BreachAction::Warn,
            }),
            latched: vec![LatchKey::new(BreachDimension::Tokens, BreachScope::PerRun)],
        };
        let outcome = decide(&input);
        assert_eq!(
            outcome.decisions,
            vec![
                GovernanceDecision {
                    dimension: BreachDimension::Tokens,
                    scope: BreachScope::Cumulative,
                    action: BreachAction::Warn,
                    limit: 100,
                    observed: 500,
                },
                GovernanceDecision {
                    dimension: BreachDimension::Dollars,
                    scope: BreachScope::PerRun,
                    action: BreachAction::Warn,
                    limit: 1,
                    observed: 1,
                },
            ],
            "the latched (tokens, per-run) pair holds; the other pairs fire"
        );
    }

    #[test]
    fn decide_an_unarmed_dimension_is_skipped_entirely() {
        // UNSET-SCOPE SKIP at the decide() level: `None` arms are skipped
        // (the supervisor's armed checks, now data), and an all-unset input
        // decides nothing.
        let dollar_only = GovernanceInput {
            token: None,
            dollar: Some(DollarArm {
                run_cost: Micros(7),
                cumulative_cost: Micros(7),
                cap: CostCap {
                    per_run: Some(Micros(7)),
                    cumulative: None,
                },
                action: BreachAction::Pause,
            }),
            latched: vec![],
        };
        let outcome = decide(&dollar_only);
        assert_eq!(outcome.decisions.len(), 1);
        assert_eq!(outcome.decisions[0].dimension, BreachDimension::Dollars);

        let all_unset = GovernanceInput::default();
        let outcome = decide(&all_unset);
        assert!(
            outcome.decisions.is_empty() && outcome.latched_after.is_empty(),
            "no arms → no decisions, latch untouched"
        );
    }

    #[test]
    fn decide_fresh_latch_and_same_totals_fire_again_the_run_boundary_re_arm() {
        // THE v1 RE-ARM: the same tripping totals with a FRESH latch fire
        // again — a new Run restarts the latch (ceiling-raise re-arm is NOT
        // v1; a same-latch re-fire stays suppressed per the hold test).
        let tripping = TokenArm {
            run_total: 0,
            cumulative_total: 100,
            budget: TokenBudget {
                per_run: None,
                cumulative: Some(100),
            },
            action: BreachAction::Pause,
        };
        let input = GovernanceInput {
            token: Some(tripping),
            dollar: None,
            latched: vec![],
        };
        assert_eq!(decide(&input).decisions.len(), 1, "fresh Run → fires");
        // ...and feeding the outcome's latch back suppresses the next
        // evaluation (the chain the supervisor's in-memory latch gives).
        let chained = GovernanceInput {
            token: Some(tripping),
            dollar: None,
            latched: decide(&input).latched_after,
        };
        assert!(
            decide(&chained).decisions.is_empty(),
            "the latched pair must not re-fire on the next event"
        );
    }

    // ---- The vectors artifact: schema, generator, verifier ----

    #[test]
    fn the_committed_artifact_is_byte_identical_to_regeneration() {
        // VERIFIER (a) — the DRIFT TRIPWIRE: the committed file must be
        // byte-identical to what the engine's own evaluators regenerate.
        // Any evaluator semantic change without a re-bless lands here. The
        // bless lever (`HEKMA_UPDATE_GOVERNANCE_VECTORS=1`) rewrites the
        // artifact + its `.sha256` for a ratified semantic change — review
        // the PR diff case by case.
        let regenerated = canonical_json(&generate());
        if bless_lever_armed() {
            let path = artifact_path();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create the governance dir");
            }
            std::fs::write(&path, &regenerated).expect("write the artifact");
            let checksum_path = path.with_extension("json.sha256");
            std::fs::write(&checksum_path, checksum_file_content(&regenerated))
                .expect("write the .sha256 sidecar");
            eprintln!(
                "HEKMA_UPDATE_GOVERNANCE_VECTORS=1: rewrote {} and {} — review the diff case by case",
                GOVERNANCE_VECTORS_ARTIFACT_PATH,
                checksum_path.display()
            );
            return;
        }
        assert_eq!(
            regenerated, COMMITTED,
            "the committed governance vectors drifted from the engine's evaluators — \
             re-bless with HEKMA_UPDATE_GOVERNANCE_VECTORS=1 and review the diff"
        );
    }

    #[test]
    fn the_committed_sha256_sidecar_matches_the_artifact_bytes() {
        // VERIFIER (b) — the `.sha256` sidecar names the artifact's exact
        // digest and its repo-relative path (the `sha256sum -c` contract).
        let digest = crate::sha256::to_hex(&crate::sha256::digest(COMMITTED.as_bytes()));
        let expected = format!("{digest}  {GOVERNANCE_VECTORS_ARTIFACT_PATH}\n");
        assert_eq!(
            COMMITTED_CHECKSUM_FILE, expected,
            "the committed .sha256 must be `<digest>  <repo path>` over the exact artifact bytes"
        );
        // The in-crate checksum accessor reads the same embedded bytes — it
        // cannot disagree with the sidecar.
        assert_eq!(governance_vectors_checksum(), digest);
    }

    #[test]
    fn every_committed_case_reruns_exactly_against_the_real_functions() {
        // VERIFIER (c) — the HOST'S PIN, executed by the engine itself:
        // parse the COMMITTED bytes and re-run every case against the real
        // evaluators/derivations, asserting exact equality — decision order
        // and latch-after state included.
        let document: VectorsDocument =
            serde_json::from_str(COMMITTED).expect("the committed artifact parses");
        for case in &document.cases {
            match case {
                Case::Decision(case) => {
                    let actual = decide(&case.input);
                    assert_eq!(
                        &actual, &case.expected,
                        "case `{}`: the committed expected outcome does not match decide()",
                        case.id
                    );
                }
                Case::Derivation(case) => {
                    let actual = run_derivation(&case.derivation);
                    assert_eq!(
                        actual, case.expected_micros,
                        "case `{}`: the committed expected micros does not match the derivation",
                        case.id
                    );
                    // The no-retro case also pins its ANTI-VALUE, COMPUTED
                    // (19-1 review round 1 — not the recorded literal): the
                    // summed tokens repriced at the LAST row's rate are what
                    // a re-pricing Host would produce. The honest per-row sum
                    // must differ from that computed anti-value, and the
                    // recorded `at_latest_rate` input must equal it — neither
                    // side of the inequality is a hardcoded literal.
                    if let DerivationSpec::TwoRowOwnRateSum {
                        rows,
                        at_latest_rate,
                    } = &case.derivation
                    {
                        let total_input = rows
                            .iter()
                            .map(|row| row.input_tokens)
                            .fold(0u64, u64::saturating_add);
                        let total_output = rows
                            .iter()
                            .map(|row| row.output_tokens)
                            .fold(0u64, u64::saturating_add);
                        let total_cached = rows
                            .iter()
                            .map(|row| row.cached_tokens.unwrap_or(0))
                            .fold(0u64, u64::saturating_add);
                        let latest_rate = &rows.last().expect("the case carries rows").rate;
                        let computed_anti = cost_micros_with_cached(
                            total_input,
                            total_output,
                            Some(total_cached),
                            latest_rate,
                        )
                        .get();
                        assert_eq!(
                            *at_latest_rate, computed_anti,
                            "case `{}`: the recorded anti-value must equal the totals \
                             repriced at the latest rate",
                            case.id
                        );
                        assert_ne!(
                            case.expected_micros, computed_anti,
                            "case `{}`: the honest per-row sum must differ from the \
                             computed repricing anti-value",
                            case.id
                        );
                    }
                }
                Case::Fence(case) => {
                    assert_eq!(
                        fence_refusal(&case.reasons),
                        case.expected,
                        "case `{}`: the committed refusal does not match fence_refusal()",
                        case.id
                    );
                }
            }
        }
    }

    #[test]
    fn case_ids_are_unique_and_every_rule_is_covered() {
        // VERIFIER (d) — ids unique; the covers set ⊇ the full rule list.
        let document: VectorsDocument =
            serde_json::from_str(COMMITTED).expect("the committed artifact parses");
        let mut ids: Vec<&str> = Vec::new();
        for case in &document.cases {
            let (id, covers): (&str, &[CoverTag]) = match case {
                Case::Decision(case) => (&case.id, &case.covers),
                Case::Derivation(case) => (&case.id, &case.covers),
                Case::Fence(case) => (&case.id, &case.covers),
            };
            assert!(!ids.contains(&id), "duplicate case id `{id}`");
            assert!(!covers.is_empty(), "case `{id}` carries no covers tags");
            ids.push(id);
        }
        let covered = document.covered_tags();
        for tag in ALL_COVERS {
            assert!(
                covered.contains(tag),
                "no committed case covers the `{}` rule",
                tag.as_str()
            );
        }
    }

    #[test]
    fn the_document_header_names_the_artifact_schema_and_provenance() {
        // The header contract a Host pins against: the artifact id, the
        // schema version, the vectors version, and the provenance list —
        // all from the constants, so they cannot drift from the code.
        // Provenance carries the generator FUNCTIONS only — no crate
        // version (not a pin key; the version lives in the CHANGELOG).
        let document = generate();
        assert_eq!(document.artifact, "hekma-governance-vectors");
        assert_eq!(document.schema_version, 1);
        assert_eq!(document.vectors_version, GOVERNANCE_VECTORS_VERSION);
        assert_eq!(
            document.provenance.generated_from,
            vec![
                "BudgetEvaluator",
                "CostEvaluator",
                "cost_micros",
                "cost_micros_with_cached",
                "governance::decide",
                "governance::fence_refusal",
            ]
        );
    }

    #[test]
    fn the_1_1_0_cases_are_appended_after_the_1_0_0_set() {
        // Story 19-2: the minor bump is ADDITIVE — the 26 cases 1.0.0
        // shipped keep their position, and every story-19-2 case follows
        // them. A consumer pinned to 1.0.0 sees its cases unchanged.
        let document = generate();
        let first_fence_era = document
            .cases
            .iter()
            .position(|case| match case {
                Case::Decision(case) => case.id.starts_with("decision-block-"),
                Case::Fence(_) => true,
                Case::Derivation(_) => false,
            })
            .expect("the 1.1.0 cases exist");
        assert_eq!(first_fence_era, 26, "the 1.0.0 set is 26 cases");
        assert!(document.cases[first_fence_era..]
            .iter()
            .all(|case| match case {
                Case::Decision(case) =>
                    case.input.token.map(|arm| arm.action) == Some(BreachAction::Block),
                Case::Fence(_) => true,
                Case::Derivation(_) => false,
            }));
    }

    #[test]
    fn fence_refusal_passes_with_no_reason() {
        assert_eq!(fence_refusal(&[]), None);
    }

    #[test]
    fn fence_refusal_reports_the_first_reason_in_canonical_order() {
        // Every subset of the four reasons reports its lowest canonical
        // member — the order the listener's bitmask relies on.
        for mask in 1u8..16 {
            let raised: Vec<LatchKey> = FENCE_REASON_ORDER
                .iter()
                .enumerate()
                .rev()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, key)| *key)
                .collect();
            let expected = FENCE_REASON_ORDER[mask.trailing_zeros() as usize];
            let refusal = fence_refusal(&raised).expect("a raised reason refuses");
            assert_eq!(
                (refusal.dimension, refusal.scope),
                (expected.dimension, expected.scope)
            );
            assert_eq!(refusal.code, fence_code(expected));
            assert_eq!(refusal.status, 402);
            assert_eq!(refusal.error_type, "hekma_budget_block");
        }
    }

    #[test]
    fn a_fence_case_serializes_its_kind_and_an_explicit_null_pass() {
        let refused = serde_json::to_value(fence_case(
            "x",
            &[LatchKey::new(BreachDimension::Dollars, BreachScope::PerRun)],
        ))
        .unwrap();
        assert_eq!(refused["kind"], "fence");
        assert_eq!(refused["covers"][0], "block-fence");
        assert_eq!(refused["reasons"][0]["dimension"], "dollars");
        assert_eq!(refused["reasons"][0]["scope"], "per-run");
        assert_eq!(refused["expected"]["status"], 402);
        assert_eq!(refused["expected"]["error_type"], "hekma_budget_block");
        assert_eq!(refused["expected"]["code"], "dollars_per_run");
        let passed = serde_json::to_value(fence_case("y", &[])).unwrap();
        assert!(passed["expected"].is_null(), "{passed}");
    }

    #[test]
    fn fence_codes_name_dimension_and_scope() {
        let codes: Vec<&str> = FENCE_REASON_ORDER
            .iter()
            .map(|key| fence_code(*key))
            .collect();
        assert_eq!(
            codes,
            vec![
                "tokens_per_run",
                "tokens_cumulative",
                "dollars_per_run",
                "dollars_cumulative"
            ]
        );
    }

    #[test]
    fn the_canonical_form_is_pretty_json_with_one_trailing_newline() {
        // DETERMINISM: the canonical bytes end with exactly one newline and
        // re-serialize identically (structs, declaration order — no map
        // iteration anywhere in the schema).
        let canonical = canonical_json(&generate());
        assert!(canonical.ends_with("}\n"));
        assert!(!canonical.ends_with("\n\n"));
        let document: VectorsDocument =
            serde_json::from_str(&canonical).expect("the canonical bytes parse");
        assert_eq!(
            canonical_json(&document),
            canonical,
            "re-serialization is stable"
        );
    }

    #[test]
    fn the_wire_form_is_snake_case_keys_and_kebab_case_enums() {
        // THE HOUSE WIRE CONVENTION, pinned on the artifact: object keys are
        // snake_case; enums serialize kebab-case (BreachAction/BreachScope/
        // BreachDimension/CoverTag/kind/derivation discriminators).
        let canonical = canonical_json(&generate());
        let value: serde_json::Value = serde_json::from_str(&canonical).unwrap();
        let case = &value["cases"][0];
        assert_eq!(case["kind"], "decision");
        // A decision case's input carries the evaluator's config shape with
        // kebab enums + snake keys.
        assert_eq!(
            case["input"]["token"]["budget"]["per_run"],
            serde_json::json!(null)
        );
        assert_eq!(
            case["input"]["token"]["budget"]["cumulative"],
            serde_json::json!(100)
        );
        assert_eq!(case["input"]["token"]["action"], "pause");
        assert_eq!(case["expected"]["decisions"][0]["dimension"], "tokens");
        assert_eq!(case["expected"]["decisions"][0]["scope"], "cumulative");
        assert_eq!(case["covers"][0], "threshold-ge");
        // Find a derivation case and pin its discriminator + micros.
        let derivation_case = value["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["kind"] == "derivation")
            .expect("the artifact carries derivation cases");
        assert!(derivation_case["expected_micros"].is_i64());
        // The document header keys.
        for key in [
            "artifact",
            "schema_version",
            "vectors_version",
            "provenance",
            "cases",
        ] {
            assert!(value.get(key).is_some(), "header key `{key}` present");
        }
    }

    #[test]
    fn every_derivation_variant_serializes_its_exact_wire_discriminator() {
        // THE DISCRIMINATOR PIN, per variant (19-1 review round 1: the old
        // assertion was an OR over serde's own renames — it could not fail).
        // Each `DerivationSpec` variant must serialize to its EXACT
        // kebab-case wire string; a `rename_all` regression to snake_case (or
        // a hand-rolled string drift) fails here per variant.
        let variants: [(DerivationSpec, &str); 3] = [
            (
                DerivationSpec::CostMicros {
                    input_tokens: 1,
                    output_tokens: 0,
                    rate: Rate::new(1, 0),
                },
                "cost-micros",
            ),
            (
                DerivationSpec::CostMicrosWithCached {
                    input_tokens: 1,
                    output_tokens: 0,
                    cached_tokens: Some(1),
                    rate: Rate::new(1, 0).with_cached(1),
                },
                "cost-micros-with-cached",
            ),
            (
                DerivationSpec::TwoRowOwnRateSum {
                    rows: vec![RowPricing {
                        input_tokens: 1,
                        output_tokens: 0,
                        cached_tokens: None,
                        rate: Rate::new(1, 0),
                    }],
                    at_latest_rate: 1,
                },
                "two-row-own-rate-sum",
            ),
        ];
        for (spec, wire) in variants {
            let value = serde_json::to_value(&spec).expect("the spec serializes");
            assert_eq!(
                value["derivation"], wire,
                "the discriminator wire form regressed for `{wire}`: {value}"
            );
        }
        // The COMMITTED artifact's derivation cases carry exactly these
        // discriminators — the wire contract a Host switches on.
        let document: VectorsDocument =
            serde_json::from_str(COMMITTED).expect("the committed artifact parses");
        for case in &document.cases {
            let Case::Derivation(case) = case else {
                continue;
            };
            let wire = match &case.derivation {
                DerivationSpec::CostMicros { .. } => "cost-micros",
                DerivationSpec::CostMicrosWithCached { .. } => "cost-micros-with-cached",
                DerivationSpec::TwoRowOwnRateSum { .. } => "two-row-own-rate-sum",
            };
            let raw = serde_json::to_value(&case.derivation).expect("the spec serializes");
            assert_eq!(
                raw["derivation"], wire,
                "case `{}`: committed discriminator drifted from the exact wire form",
                case.id
            );
        }
    }

    #[test]
    fn the_document_round_trips_through_serde() {
        // The typed document parses back from the canonical bytes with
        // total equality — a Rust Host can consume the artifact typed.
        let canonical = canonical_json(&generate());
        let back: VectorsDocument = serde_json::from_str(&canonical).unwrap();
        assert_eq!(back, generate());
    }
}
