//! Transition events (spine AD-14 SEED) — one schema, two consumers.
//!
//! A [`TransitionEvent`] is the "event" for this story: a RECORDED state
//! transition carrying the prior state, the new state, a cause, and an RFC 3339
//! UTC timestamp. It is a versioned serde struct so story 7-2 (the host
//! subscription bus — shipped as `Engine::subscribe`) and `kt --json` (story
//! 1-7 / 4-3) reuse the SAME schema ("one event schema, two consumers").
//! [`TransitionEvent::schema_version`] is carried from the start so the schema
//! can evolve compatibly.
//!
//! ## Boundary (what this is NOT)
//!
//! This story SEEDS the struct and RECORDS it (to the per-instance log, and
//! returns it so tests can assert). The bounded-channel subscription bus it
//! seeds for is story 7-2's (shipped: `Engine::subscribe` /
//! `Blocking::subscribe` over the `domain::bus` [`crate::EngineEvent`]
//! wrapper). AC1's "each transition emits an event" is satisfied here by "each
//! transition records a `TransitionEvent`".

use serde::{Deserialize, Serialize};

use super::budget::{BreachAction, BreachScope};
use super::cost::{EstimateLabel, Micros};
use super::lifecycle::LifecycleState;

/// The schema version stamped on every emitted [`TransitionEvent`].
///
/// Bumped only on an incompatible change to the event shape. 7-2 / `--json`
/// negotiate on this; seeding it now means those consumers never see an
/// unversioned event.
///
/// NOTE (additive vs breaking): story 1-5 ADDS `TransitionCause` variants
/// (`pause-best-effort` / `resume-best-effort`), story 1-6 ADDS `crashed` /
/// `restarted`, and story 3-2 ADDS `budget-exceeded` (the Breach-Action cause on
/// the `running → paused`/`stopping` edge). Story 3-3 ADDS a `dimension`
/// discriminator + optional dollar fields to the EXISTING `budget-exceeded` cause
/// (a token breach's `dimension` defaults to `tokens`, its dollar fields absent) —
/// a NEW reader parses every OLD event and no field is renamed/removed, so this is
/// backward-additive and does NOT bump the version. Adding a new closed-vocabulary
/// variant is likewise backward-ADDITIVE: a NEW reader parses every OLD event, and
/// no field is renamed or removed, so the version is NOT bumped. (The
/// converse — an OLD reader meeting a NEW cause — is a separate forward-compat
/// question: because `TransitionCause` is `#[serde(tag = "kind")]` with no
/// `#[serde(other)]` fallback, an old reader that hits an unknown tag ERRORS
/// rather than silently skipping it. That is acceptable precisely because 7-2 /
/// `--json` negotiate on THIS version field — a consumer that understands
/// version N knows exactly which cause tags exist at N, so it never meets a tag
/// it cannot match.) Only a shape change (renaming/removing a field, or an
/// incompatible restructure) would bump the version.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// The schema version stamped on the `kt --json` Fleet document (story 1-7,
/// AD-14).
///
/// AD-14 requires `kt --json` and the 7-2 Host event stream to be ONE
/// contract, so the Fleet document is a versioned serde struct just like
/// [`TransitionEvent`]. This starts at the SAME value as [`EVENT_SCHEMA_VERSION`]
/// so the two versioning stories begin aligned; it is a SEPARATE constant so the
/// Fleet document can evolve independently of the event schema (a change to one
/// shape must not force a version bump on the other). It rides on the
/// [`crate::FleetListing`] wrapper (for `list`) and each `show --json` object.
///
/// Bumped only on a change to the Fleet document shape that consumers negotiate on.
/// POPULATING an existing field from `null` to a real type (e.g. the Epic-3
/// `budget`/`usage`) is transparently backward-additive and did NOT bump it. Story
/// 3-5 bumps it **1 → 2**: the `list --json` document GAINS a first-class top-level
/// `totals` object (the Fleet-WIDE [`crate::FleetTotals`] aggregate) that consumers
/// and the 7-2 Host stream will want to negotiate on. The change is ADDITIVE
/// — a v2 reader parses every v1 document (no field is renamed or removed), and a v1
/// consumer that ignores the new `totals` still parses `instances` — but the bump is
/// the honest signal that a new first-class field exists, matching the 1-7/3-1/3-3
/// discipline of treating the Fleet document version as the `--json` contract. (The
/// `show --json` document carries this same version but does NOT gain `totals`: a
/// single instance has no Fleet total — its own `usage` IS its total.) A future
/// INCOMPATIBLE change (renaming/removing a field) would bump it again.
pub const FLEET_SCHEMA_VERSION: u32 = 2;

/// The schema version stamped on every emitted [`BudgetBreachEvent`] (story 3-2,
/// AD-14).
///
/// AD-14 names "breaches" explicitly among the versioned engine event structs the
/// subscription API + `kt --json` share. 3-2 FREEZES the breach-event wire shape
/// now — a versioned serde struct carrying the TOKEN breach fields — so `kt --json`
/// and the 7-2 Host stream cannot drift into two dialects. A SEPARATE
/// constant from the sibling schemas ([`EVENT_SCHEMA_VERSION`],
/// [`FLEET_SCHEMA_VERSION`], [`crate::USAGE_SCHEMA_VERSION`]) — the wire shapes
/// evolve independently, so a change to one must not force a version bump on the
/// others. It starts at 1, aligned with the siblings. Bumped only on an
/// INCOMPATIBLE change; adding a field is backward-additive and does NOT bump it.
pub const BUDGET_SCHEMA_VERSION: u32 = 1;

/// Why a lifecycle transition happened (the transition event's `cause`).
///
/// A small closed vocabulary so consumers (log readers, 7-2, `--json`) can match
/// on the reason rather than parse free text. `LaunchError`/`StopForced` carry a
/// detail string (the adapter diagnostic / escalation note); the rest are plain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TransitionCause {
    /// An operator command drove the transition (`start` / `stop`).
    Command {
        /// The command label (`"start"` / `"stop"`).
        command: String,
    },
    /// The adapter became ready (process spawned and did not immediately die):
    /// `starting → running`.
    AdapterReady,
    /// The launch failed: `starting → failed`. Carries the preserved diagnostic.
    LaunchError {
        /// The adapter's launch diagnostic, preserved verbatim (AC2).
        detail: String,
    },
    /// Graceful shutdown succeeded within the window: `stopping → stopped`.
    StopGraceful,
    /// The graceful window elapsed and the process was force-killed:
    /// `stopping → stopped` after escalation (AC3). Carries the escalation note.
    StopForced {
        /// The escalation detail recorded in the instance log (AC3).
        detail: String,
    },
    /// A pause that was BEST-EFFORT, not a real suspension (story 1-5, AC2):
    /// `running → paused` on an OS/adapter where pause is
    /// [`SupportLevel::BestEffort`](hemaka_adapter_api::SupportLevel). This is the
    /// machine-readable half of "surfaced not silent" — a dedicated, matchable
    /// wire tag (`pause-best-effort`) so log/`--json`/7-2 consumers can tell a
    /// cooperative pause from a guaranteed one. A GUARANTEED pause emits a plain
    /// [`TransitionCause::Command`] (`"pause"`), never this. Carries a detail
    /// (the OS + declared level) for the record.
    PauseBestEffort {
        /// The best-effort detail (names the OS + declared level) recorded in the
        /// instance log (AC2).
        detail: String,
    },
    /// A resume that was BEST-EFFORT, the counterpart of [`TransitionCause::PauseBestEffort`]
    /// (story 1-5, AC2): `paused → running` on a best-effort OS/adapter. Wire tag
    /// `resume-best-effort`.
    ResumeBestEffort {
        /// The best-effort detail (names the OS + declared level) recorded in the
        /// instance log (AC2).
        detail: String,
    },
    /// The supervised process CRASHED — exited without a requested stop (story
    /// 1-6, AC5): the EVENT-driven `running → failed` edge the reaper applies.
    /// Wire tag `crashed`. Carries the exit code / signal detail so the
    /// log/`--json`/7-2 consumers can match on it. DISTINCT from
    /// [`TransitionCause::LaunchError`] (a startup failure) — a crash is a
    /// running process dying unrequested.
    Crashed {
        /// The exit detail (e.g. `"exited with code 1"` / `"exited via signal"`),
        /// preserved for the record (AC5).
        detail: String,
    },
    /// A Restart Policy RESTART of a crashed instance (story 1-6, AC4): the
    /// `failed → starting` edge the restart executor drives. Wire tag `restarted`.
    /// Records the consecutive restart `count` and the backoff `waited_ms` so the
    /// CLI + 7-2/`--json` consumers can surface both (AC9).
    Restarted {
        /// The consecutive restart count this restart represents (1-based).
        count: u32,
        /// The backoff waited before this restart, in milliseconds.
        waited_ms: u64,
    },
    /// A budget BREACH drove the transition (story 3-2 tokens / story 3-3 dollars,
    /// AD-7/AD-15): the Breach Action `pause`/`stop` pulled the EXISTING
    /// `running → paused` / `running → stopping` lever, so the lifecycle log itself
    /// explains WHY. Wire tag `budget-exceeded` — the SAME edge for both dimensions
    /// (3-3 adds a REASON, not a new edge). Carries the breached scope + the ceiling
    /// reached + the observed total.
    ///
    /// DIMENSION (story 3-3, additive per AD-14): [`dimension`](Self::BudgetExceeded)
    /// distinguishes a TOKEN breach (`tokens`, the 3-2 default — `limit`/`observed`
    /// are token counts, the dollar fields ABSENT) from a DOLLAR breach (`dollars`
    /// — `limit`/`observed` mirror the dollar micros, and `dollar_limit`/
    /// `dollar_observed`/`estimate_label` carry the honest labeled money). The
    /// discriminator + optional fields are backward-ADDITIVE: an OLD reader that
    /// predates 3-3 parses a token `budget-exceeded` unchanged (the `dimension`
    /// defaults to `tokens`, the dollar fields are absent). A `warn` action produces
    /// NO transition, so it NEVER carries this cause (only the standalone
    /// [`BudgetBreachEvent`] records a `warn`).
    BudgetExceeded {
        /// Which budget scope tripped (`per-run` / `cumulative`).
        scope: BreachScope,
        /// The dimension that tripped (`tokens` / `dollars`) — story 3-3. Defaults
        /// to `tokens` on the wire when absent (a pre-3-3 token breach).
        #[serde(default)]
        dimension: BreachDimension,
        /// The ceiling that was reached (token count for a `tokens` breach; the
        /// dollar-cap micros for a `dollars` breach).
        limit: u64,
        /// The committed total that reached it (`>= limit`).
        observed: u64,
        /// The dollar cap that was reached, in micro-dollars (story 3-3) — present
        /// ONLY for a `dollars` breach, absent (JSON omitted) for a `tokens` breach.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dollar_limit: Option<Micros>,
        /// The derived cost that reached the dollar cap, in micro-dollars (story
        /// 3-3) — present ONLY for a `dollars` breach.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dollar_observed: Option<Micros>,
        /// The estimate label on the dollar figures (story 3-3, AD-8) — present
        /// ONLY for a `dollars` breach; v1 always `estimated`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        estimate_label: Option<EstimateLabel>,
    },
}

/// Which DIMENSION of a budget tripped (story 3-3, AD-8) — a TOKEN ceiling
/// (story 3-2) or a DOLLAR Cost Cap (story 3-3). The discriminator that lets the
/// SINGLE [`BudgetBreachEvent`] + the SINGLE `budget-exceeded`
/// [`TransitionCause::BudgetExceeded`] carry BOTH dimensions (AD-14: one breach
/// struct for the subscription). `tokens` is the DEFAULT so a pre-3-3 breach
/// (which had no `dimension` field) parses as a token breach. Kebab-case on the
/// wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BreachDimension {
    /// A TOKEN-count ceiling tripped (story 3-2). The default (back-compat).
    #[default]
    Tokens,
    /// A DOLLAR Cost Cap tripped (story 3-3).
    Dollars,
}

impl BreachDimension {
    /// The stable wire/label form (`"tokens"` / `"dollars"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            BreachDimension::Tokens => "tokens",
            BreachDimension::Dollars => "dollars",
        }
    }
}

impl std::fmt::Display for BreachDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TransitionCause {
    /// A command cause for `command`.
    pub fn command(command: impl Into<String>) -> Self {
        TransitionCause::Command {
            command: command.into(),
        }
    }

    /// A launch-error cause preserving `detail`.
    pub fn launch_error(detail: impl Into<String>) -> Self {
        TransitionCause::LaunchError {
            detail: detail.into(),
        }
    }

    /// A forced-stop cause recording the escalation `detail`.
    pub fn stop_forced(detail: impl Into<String>) -> Self {
        TransitionCause::StopForced {
            detail: detail.into(),
        }
    }

    /// A best-effort PAUSE cause recording `detail` (the OS + declared level).
    pub fn pause_best_effort(detail: impl Into<String>) -> Self {
        TransitionCause::PauseBestEffort {
            detail: detail.into(),
        }
    }

    /// A best-effort RESUME cause recording `detail` (the OS + declared level).
    pub fn resume_best_effort(detail: impl Into<String>) -> Self {
        TransitionCause::ResumeBestEffort {
            detail: detail.into(),
        }
    }

    /// A CRASH cause recording the exit `detail` (story 1-6, AC5).
    pub fn crashed(detail: impl Into<String>) -> Self {
        TransitionCause::Crashed {
            detail: detail.into(),
        }
    }

    /// A RESTART cause recording the consecutive `count` + backoff `waited_ms`
    /// (story 1-6, AC4/AC9).
    pub fn restarted(count: u32, waited_ms: u64) -> Self {
        TransitionCause::Restarted { count, waited_ms }
    }

    /// A TOKEN BUDGET-EXCEEDED cause recording the breached `scope` + the `limit`
    /// reached + the `observed` total (story 3-2, AC7). The `dimension` is `tokens`
    /// and the dollar fields are absent. Mirrors the other constructors; used on the
    /// `running → paused`/`stopping` transition the Breach Action drives.
    pub fn budget_exceeded(scope: BreachScope, limit: u64, observed: u64) -> Self {
        TransitionCause::BudgetExceeded {
            scope,
            dimension: BreachDimension::Tokens,
            limit,
            observed,
            dollar_limit: None,
            dollar_observed: None,
            estimate_label: None,
        }
    }

    /// A DOLLAR Cost-Cap-EXCEEDED cause (story 3-3, AC10) — the `dimension` is
    /// `dollars`, `limit`/`observed` mirror the dollar micros, and the dedicated
    /// dollar fields carry the labeled money. Used on the SAME
    /// `running → paused`/`stopping` transition a dollar breach drives (a REASON,
    /// not a new edge — AD-15). `limit_micros`/`observed_micros` are the cap + the
    /// derived cost in micro-dollars; `label` is the estimate label (v1 `estimated`).
    pub fn cost_cap_exceeded(
        scope: BreachScope,
        limit_micros: Micros,
        observed_micros: Micros,
        label: EstimateLabel,
    ) -> Self {
        // The unit-agnostic limit/observed carry the same micros (a non-negative
        // cost clamps to 0 defensively) so an old reader still sees a numeric total.
        let limit = u64::try_from(limit_micros.get()).unwrap_or(0);
        let observed = u64::try_from(observed_micros.get()).unwrap_or(0);
        TransitionCause::BudgetExceeded {
            scope,
            dimension: BreachDimension::Dollars,
            limit,
            observed,
            dollar_limit: Some(limit_micros),
            dollar_observed: Some(observed_micros),
            estimate_label: Some(label),
        }
    }
}

/// The schema version stamped on every emitted [`LogLine`] (story 4-2, spine
/// AD-12/AD-14).
///
/// AD-14's "one event schema, two consumers" rule extends to the unified
/// attributed-output stream: [`LogLine`] joins [`TransitionEvent`] /
/// [`BudgetBreachEvent`] as a versioned serde struct so `kt agent logs` and
/// the 7-2 Host event bus negotiate on the SAME shape. Starts at 1,
/// aligned with its siblings. Bumped only on an INCOMPATIBLE change; adding a
/// field is backward-additive and does NOT bump it.
pub const LOG_SCHEMA_VERSION: u32 = 1;

/// Which captured stream produced a [`LogLine`] (story 4-2, AD-12) — the
/// per-line attribution AC-A requires.
///
/// A small closed vocabulary, mirroring [`SupportLevel`](hemaka_adapter_api::SupportLevel)'s
/// seed-enum pattern exactly: kebab-case on the wire, `as_str()`/`Display`
/// pair. The wire tokens (`agent-out` / `agent-err` / `engine`) are AD-12's
/// own literal attribution labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogStream {
    /// The spawned agent process's stdout.
    AgentOut,
    /// The spawned agent process's stderr.
    AgentErr,
    /// A best-effort, human-readable projection of an engine
    /// [`TransitionEvent`] into the unified stream (story 4-2, Task 4) — NOT
    /// a replacement for `instance.log`, which remains the machine-
    /// authoritative record.
    Engine,
}

impl LogStream {
    /// The stable kebab-case wire/label form, matching the serde form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogStream::AgentOut => "agent-out",
            LogStream::AgentErr => "agent-err",
            LogStream::Engine => "engine",
        }
    }
}

impl std::fmt::Display for LogStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One captured, attributed, timestamped line of unified output (story 4-2,
/// spine AD-12/AD-14) — the record `kt agent logs` reads and the writer
/// thread (`ports::process_backend`) appends, JSON-Lines, to the rotated
/// attributed capture (`logs/output.log[.N]`). Distinct from `agent.log`
/// (CRITICAL SCOPING #3): that legacy file stays raw and unattributed,
/// byte-identical to before this story, for Epic 3's `drain_usage_for`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// The schema version ([`LOG_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The Agent Instance this line was captured from.
    pub instance: String,
    /// Which stream produced it (AC-A's attribution label).
    pub stream: LogStream,
    /// RFC 3339 UTC timestamp the engine stamped when it observed the line.
    /// Whole-second resolution ([`crate::time::now_rfc3339`]) — same-second
    /// lines are common and must never be re-sorted (AC-G); on-disk append
    /// order is the sole ordering authority everywhere this is read.
    pub at: String,
    /// The line's text, with any trailing `\n`/`\r\n` stripped (the raw,
    /// UNSTRIPPED bytes are what `agent.log` keeps, separately and
    /// unmodified — this field is the attributed VIEW, not the legacy
    /// capture).
    pub text: String,
}

impl LogLine {
    /// Build a log line, stamping the current schema version.
    ///
    /// `at` is an RFC 3339 UTC timestamp (the caller passes
    /// [`crate::time::now_rfc3339`] — kept a parameter so the struct stays
    /// pure and unit-testable with a fixed clock, exactly like
    /// [`TransitionEvent::new`]).
    pub fn new(
        instance: impl Into<String>,
        stream: LogStream,
        text: impl Into<String>,
        at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: LOG_SCHEMA_VERSION,
            instance: instance.into(),
            stream,
            at: at.into(),
            text: text.into(),
        }
    }
}

/// A recorded lifecycle state transition (spine AD-14 seed).
///
/// Emitted on every transition the supervisor applies, carrying everything AC1
/// requires: prior state, new state, cause, timestamp — plus the instance name
/// and the schema version. Serde-serializable so it round-trips through the
/// per-instance log, the 7-2 event bus, and `--json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvent {
    /// The event schema version ([`EVENT_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The Agent Instance the transition is for.
    pub instance: String,
    /// The state before the transition.
    pub prior_state: LifecycleState,
    /// The state after the transition.
    pub new_state: LifecycleState,
    /// Why the transition happened.
    pub cause: TransitionCause,
    /// RFC 3339 UTC timestamp of the transition.
    pub at: String,
}

impl TransitionEvent {
    /// Build a transition event, stamping the current schema version.
    ///
    /// `at` is an RFC 3339 UTC timestamp (the caller passes
    /// [`crate::time::now_rfc3339`] — kept a parameter so the struct stays pure
    /// and unit-testable with a fixed clock).
    pub fn new(
        instance: impl Into<String>,
        prior_state: LifecycleState,
        new_state: LifecycleState,
        cause: TransitionCause,
        at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: EVENT_SCHEMA_VERSION,
            instance: instance.into(),
            prior_state,
            new_state,
            cause,
            at: at.into(),
        }
    }
}

/// A recorded Token-Budget BREACH (spine AD-14, story 3-2) — the ALWAYS-recorded
/// event FR-21 requires "regardless of action".
///
/// Emitted from the ledger-commit choke point the instant a just-committed total
/// reaches a configured ceiling ([`super::budget::BudgetEvaluator`] returns
/// `Breached`), recorded BEFORE/independently of the lifecycle side-effect so a
/// best-effort/unsupported/failed pause NEVER loses the breach record (the FR-21
/// invariant + the NFR safety note). Recorded for EVERY action — including `warn`
/// (no transition) — as a durable JSON line, and (for `pause`/`stop`) mirrored as
/// a [`TransitionCause::BudgetExceeded`] on the resulting transition.
///
/// DIMENSION (story 3-3, AD-8/AD-14 — additive, back-compatible): a TOKEN breach
/// (3-2) keeps its shape — `dimension` defaults to `tokens`, the dollar fields
/// ABSENT — while a DOLLAR breach carries `dimension = dollars`, the dollar
/// cap/observed cost as integer micros, and the [`EstimateLabel`]. This is the ONE
/// breach struct the AD-14 subscription publishes for BOTH dimensions (the story's
/// recommendation: a discriminator + optional fields over a schema bump). The
/// additive optional fields default-null, so an OLD (`schema_version = 1`) reader
/// parses every token breach unchanged and never sees a dollar field it does not
/// understand — so [`BUDGET_SCHEMA_VERSION`] does NOT bump. The wire carries INTEGER
/// MICROS + the label, NEVER a pre-formatted `$` string (AD-14 — a Host formats its
/// own currency). Subscription DELIVERY shipped in story 7-2 (published onto the
/// event bus right after the breach-log append); 3-2/3-3 record + freeze the
/// struct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetBreachEvent {
    /// The breach-event schema version ([`BUDGET_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The Agent Instance whose ledger crossed the ceiling.
    pub instance: String,
    /// The Run (spine AD-7) the breaching event was committed under.
    pub run_id: String,
    /// Which budget scope tripped (`per-run` / `cumulative`).
    pub scope: BreachScope,
    /// Which DIMENSION tripped (`tokens` / `dollars`) — story 3-3. Defaults to
    /// `tokens` when absent on the wire (a pre-3-3 token breach).
    #[serde(default)]
    pub dimension: BreachDimension,
    /// The ceiling that was reached — a token count for a `tokens` breach, the
    /// dollar-cap micros for a `dollars` breach.
    pub limit: u64,
    /// The committed total that reached it (`>= limit`) — tokens or micros per
    /// `dimension`.
    pub observed: u64,
    /// The dollar cap reached, in micro-dollars (story 3-3) — present ONLY for a
    /// `dollars` breach, absent (JSON omitted) for a `tokens` breach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar_limit: Option<Micros>,
    /// The derived cost that reached the dollar cap, in micro-dollars (story 3-3) —
    /// present ONLY for a `dollars` breach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar_observed: Option<Micros>,
    /// The estimate label on the dollar figures (story 3-3, AD-8) — present ONLY
    /// for a `dollars` breach; v1 always `estimated`. NEVER a pre-formatted `$`
    /// string — the render module is human-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_label: Option<EstimateLabel>,
    /// The Breach Action taken (`pause` / `stop` / `warn`).
    pub action: BreachAction,
    /// The Metering Source that produced the breaching event's usage, as its wire
    /// string (`self-reported` / `engine-observed`).
    pub metering_source: String,
    /// RFC 3339 UTC timestamp the engine stamped when it recorded the breach.
    pub at: String,
}

impl BudgetBreachEvent {
    /// Build a TOKEN breach event (story 3-2), stamping the current
    /// [`BUDGET_SCHEMA_VERSION`]. `dimension` is `tokens`, the dollar fields absent.
    /// `at` is an RFC 3339 UTC timestamp (a parameter so the struct stays pure and
    /// unit-testable with a fixed clock, like [`TransitionEvent::new`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instance: impl Into<String>,
        run_id: impl Into<String>,
        scope: BreachScope,
        limit: u64,
        observed: u64,
        action: BreachAction,
        metering_source: impl Into<String>,
        at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: BUDGET_SCHEMA_VERSION,
            instance: instance.into(),
            run_id: run_id.into(),
            scope,
            dimension: BreachDimension::Tokens,
            limit,
            observed,
            dollar_limit: None,
            dollar_observed: None,
            estimate_label: None,
            action,
            metering_source: metering_source.into(),
            at: at.into(),
        }
    }

    /// Build a DOLLAR breach event (story 3-3, AC10), stamping the current
    /// [`BUDGET_SCHEMA_VERSION`]. `dimension` is `dollars`; the dollar cap +
    /// observed cost ride BOTH the unit-agnostic `limit`/`observed` (as micros, so
    /// an old reader still sees a numeric total) AND the dedicated
    /// `dollar_limit`/`dollar_observed` micro fields; `label` is the estimate label.
    #[allow(clippy::too_many_arguments)]
    pub fn new_cost(
        instance: impl Into<String>,
        run_id: impl Into<String>,
        scope: BreachScope,
        limit_micros: Micros,
        observed_micros: Micros,
        label: EstimateLabel,
        action: BreachAction,
        metering_source: impl Into<String>,
        at: impl Into<String>,
    ) -> Self {
        let limit = u64::try_from(limit_micros.get()).unwrap_or(0);
        let observed = u64::try_from(observed_micros.get()).unwrap_or(0);
        Self {
            schema_version: BUDGET_SCHEMA_VERSION,
            instance: instance.into(),
            run_id: run_id.into(),
            scope,
            dimension: BreachDimension::Dollars,
            limit,
            observed,
            dollar_limit: Some(limit_micros),
            dollar_observed: Some(observed_micros),
            estimate_label: Some(label),
            action,
            metering_source: metering_source.into(),
            at: at.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_carries_schema_version_and_all_fields() {
        let e = TransitionEvent::new(
            "demo",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::command("start"),
            "2026-07-04T00:00:00Z",
        );
        assert_eq!(e.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(e.instance, "demo");
        assert_eq!(e.prior_state, LifecycleState::Registered);
        assert_eq!(e.new_state, LifecycleState::Starting);
        assert_eq!(e.cause, TransitionCause::command("start"));
        assert_eq!(e.at, "2026-07-04T00:00:00Z");
    }

    #[test]
    fn event_round_trips_through_json_one_schema_two_consumers() {
        // AD-14: the same serde struct 7-2 / --json reuse. Prove a lossless
        // round-trip through JSON (the per-instance log's line format).
        let e = TransitionEvent::new(
            "demo",
            LifecycleState::Starting,
            LifecycleState::Failed,
            TransitionCause::launch_error("exec not found: no-such-bin"),
            "2026-07-04T01:02:03Z",
        );
        let json = serde_json::to_string(&e).unwrap();
        let back: TransitionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        // The preserved diagnostic survives (AC2).
        match back.cause {
            TransitionCause::LaunchError { detail } => {
                assert!(detail.contains("no-such-bin"))
            }
            other => panic!("expected LaunchError, got {other:?}"),
        }
    }

    #[test]
    fn cause_variants_serialize_with_stable_tags() {
        // The closed cause vocabulary uses a stable kebab-case tag so consumers
        // can match on it. Guard the wire tags.
        let cases = [
            (TransitionCause::command("start"), "command"),
            (TransitionCause::AdapterReady, "adapter-ready"),
            (TransitionCause::launch_error("x"), "launch-error"),
            (TransitionCause::StopGraceful, "stop-graceful"),
            (TransitionCause::stop_forced("x"), "stop-forced"),
            (TransitionCause::pause_best_effort("x"), "pause-best-effort"),
            (
                TransitionCause::resume_best_effort("x"),
                "resume-best-effort",
            ),
            (TransitionCause::crashed("x"), "crashed"),
            (TransitionCause::restarted(1, 1000), "restarted"),
            (
                TransitionCause::budget_exceeded(BreachScope::PerRun, 100, 120),
                "budget-exceeded",
            ),
        ];
        for (cause, tag) in cases {
            let json = serde_json::to_string(&cause).unwrap();
            assert!(json.contains(&format!("\"kind\":\"{tag}\"")), "{json}");
        }
    }

    #[test]
    fn budget_exceeded_cause_round_trips_with_its_fields() {
        // AC7: the Breach-Action cause carries the honest WHY (scope + limit +
        // observed, tokens only) and survives a JSON round-trip through the log.
        let cause = TransitionCause::budget_exceeded(BreachScope::Cumulative, 500, 512);
        let json = serde_json::to_string(&cause).unwrap();
        assert!(json.contains("\"kind\":\"budget-exceeded\""), "{json}");
        assert!(json.contains("\"scope\":\"cumulative\""), "{json}");
        // Tokens only — no dollar field leaked into the cause payload.
        assert!(!json.contains("cost"), "{json}");
        let back: TransitionCause = serde_json::from_str(&json).unwrap();
        match back {
            TransitionCause::BudgetExceeded {
                scope,
                dimension,
                limit,
                observed,
                dollar_limit,
                ..
            } => {
                assert_eq!(scope, BreachScope::Cumulative);
                // A TOKEN breach: dimension = tokens, no dollar fields.
                assert_eq!(dimension, BreachDimension::Tokens);
                assert_eq!(dollar_limit, None);
                assert_eq!(limit, 500);
                assert_eq!(observed, 512);
            }
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
    }

    #[test]
    fn budget_breach_event_round_trips_with_schema_version_and_snake_case() {
        // AC10: the versioned breach wire struct `kt --json` + 7-2 share. Carries
        // the schema version + the token breach fields, snake_case, tokens only.
        let e = BudgetBreachEvent::new(
            "web-1",
            "run-42-7",
            BreachScope::PerRun,
            1000,
            1000,
            BreachAction::Pause,
            "self-reported",
            "2026-07-08T00:00:00Z",
        );
        assert_eq!(e.schema_version, BUDGET_SCHEMA_VERSION);
        let value: serde_json::Value = serde_json::to_value(&e).unwrap();
        let obj = value.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "action",
                "at",
                "dimension",
                "instance",
                "limit",
                "metering_source",
                "observed",
                "run_id",
                "schema_version",
                "scope",
            ]
        );
        assert_eq!(value["scope"], serde_json::json!("per-run"));
        assert_eq!(value["action"], serde_json::json!("pause"));
        assert_eq!(value["limit"], serde_json::json!(1000));
        // A TOKEN breach: dimension = tokens, and the dollar fields are ABSENT
        // (skip_serializing_if) — no dollar figure leaks into a token breach.
        assert_eq!(value["dimension"], serde_json::json!("tokens"));
        assert!(obj.get("dollar_limit").is_none());
        assert!(obj.get("dollar_observed").is_none());
        assert!(obj.get("estimate_label").is_none());
        assert!(obj.get("cost").is_none());
        assert!(obj.get("dollars").is_none());
        let json = serde_json::to_string(&e).unwrap();
        let back: BudgetBreachEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn a_pre_3_3_token_breach_without_dimension_still_parses_as_tokens() {
        // BACK-COMPAT (AD-14): a breach JSON that predates 3-3 (NO `dimension`, NO
        // dollar fields) must still deserialize — `dimension` defaults to `tokens`
        // and the dollar fields to None. This proves the additive change does not
        // break an old wire document.
        let legacy = r#"{
            "schema_version": 1,
            "instance": "web-1",
            "run_id": "run-1-0",
            "scope": "cumulative",
            "limit": 500,
            "observed": 512,
            "action": "pause",
            "metering_source": "self-reported",
            "at": "2026-07-08T00:00:00Z"
        }"#;
        let back: BudgetBreachEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(back.dimension, BreachDimension::Tokens);
        assert_eq!(back.dollar_limit, None);
        assert_eq!(back.estimate_label, None);
        assert_eq!(back.limit, 500);
    }

    #[test]
    fn a_dollar_breach_event_round_trips_with_integer_micros_and_label_no_dollar_string() {
        // AC10: a DOLLAR breach carries dimension=dollars, integer-micro dollar
        // limit/observed, and the estimate label — snake_case, NO `$` string, NO
        // f64. A $0.50 cap reached by $0.50 of derived cost.
        let e = BudgetBreachEvent::new_cost(
            "web-1",
            "run-9-0",
            BreachScope::Cumulative,
            Micros(500_000),
            Micros(500_000),
            EstimateLabel::Estimated,
            BreachAction::Pause,
            "self-reported",
            "2026-07-08T00:00:00Z",
        );
        assert_eq!(e.schema_version, BUDGET_SCHEMA_VERSION);
        let value: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(value["dimension"], serde_json::json!("dollars"));
        // Integer micros on the wire — never a float, never a `$` string.
        assert_eq!(value["dollar_limit"], serde_json::json!(500_000));
        assert_eq!(value["dollar_observed"], serde_json::json!(500_000));
        assert_eq!(value["estimate_label"], serde_json::json!("estimated"));
        // The unit-agnostic limit/observed carry the same micros (an old reader sees
        // a numeric total).
        assert_eq!(value["limit"], serde_json::json!(500_000));
        // No pre-formatted `$` string anywhere in the payload.
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains('$'), "no `$` string on the wire: {json}");
        let back: BudgetBreachEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn breach_dimension_wire_form() {
        assert_eq!(BreachDimension::Tokens.as_str(), "tokens");
        assert_eq!(BreachDimension::Dollars.as_str(), "dollars");
        assert_eq!(BreachDimension::default(), BreachDimension::Tokens);
        // Display agrees with as_str (the BreachScope/BreachAction convention).
        assert_eq!(BreachDimension::Tokens.to_string(), "tokens");
        assert_eq!(BreachDimension::Dollars.to_string(), "dollars");
        assert_eq!(
            serde_json::to_string(&BreachDimension::Dollars).unwrap(),
            "\"dollars\""
        );
    }

    #[test]
    fn cost_cap_exceeded_cause_round_trips_with_dollar_fields() {
        // AC10: the dollar breach CAUSE reuses the `budget-exceeded` tag (the same
        // edge — AD-15) but carries dimension=dollars + the labeled micros.
        let cause = TransitionCause::cost_cap_exceeded(
            BreachScope::PerRun,
            Micros(5_000_000),
            Micros(5_250_000),
            EstimateLabel::Estimated,
        );
        let json = serde_json::to_string(&cause).unwrap();
        assert!(json.contains("\"kind\":\"budget-exceeded\""), "{json}");
        assert!(json.contains("\"dimension\":\"dollars\""), "{json}");
        assert!(json.contains("\"estimate_label\":\"estimated\""), "{json}");
        assert!(!json.contains('$'), "no `$` string on the wire: {json}");
        let back: TransitionCause = serde_json::from_str(&json).unwrap();
        match back {
            TransitionCause::BudgetExceeded {
                scope,
                dimension,
                dollar_limit,
                dollar_observed,
                estimate_label,
                ..
            } => {
                assert_eq!(scope, BreachScope::PerRun);
                assert_eq!(dimension, BreachDimension::Dollars);
                assert_eq!(dollar_limit, Some(Micros(5_000_000)));
                assert_eq!(dollar_observed, Some(Micros(5_250_000)));
                assert_eq!(estimate_label, Some(EstimateLabel::Estimated));
            }
            other => panic!("expected BudgetExceeded(dollars), got {other:?}"),
        }
    }

    #[test]
    fn crashed_and_restarted_causes_round_trip_with_their_fields() {
        // AC5: the crash detail rides IN the event payload (matchable `crashed`
        // tag). AC4/AC9: the restart cause carries the count + waited backoff.
        let crashed = TransitionCause::crashed("exited with code 137");
        let json = serde_json::to_string(&crashed).unwrap();
        assert!(json.contains("\"kind\":\"crashed\""), "{json}");
        let back: TransitionCause = serde_json::from_str(&json).unwrap();
        match back {
            TransitionCause::Crashed { detail } => assert!(detail.contains("137"), "{detail}"),
            other => panic!("expected Crashed, got {other:?}"),
        }

        let restarted = TransitionCause::restarted(3, 4000);
        let json = serde_json::to_string(&restarted).unwrap();
        assert!(json.contains("\"kind\":\"restarted\""), "{json}");
        let back: TransitionCause = serde_json::from_str(&json).unwrap();
        match back {
            TransitionCause::Restarted { count, waited_ms } => {
                assert_eq!(count, 3);
                assert_eq!(waited_ms, 4000);
            }
            other => panic!("expected Restarted, got {other:?}"),
        }
    }

    #[test]
    fn pause_best_effort_cause_carries_the_detail_and_round_trips() {
        // AC2: the best-effort qualifier rides IN the event payload (the
        // machine-readable half of "surfaced not silent") and survives a JSON
        // round-trip through the instance log.
        let cause = TransitionCause::pause_best_effort("pause is best-effort on windows");
        let json = serde_json::to_string(&cause).unwrap();
        assert!(json.contains("\"kind\":\"pause-best-effort\""), "{json}");
        let back: TransitionCause = serde_json::from_str(&json).unwrap();
        match back {
            TransitionCause::PauseBestEffort { detail } => {
                assert!(detail.contains("best-effort"), "{detail}");
                assert!(detail.contains("windows"), "{detail}");
            }
            other => panic!("expected PauseBestEffort, got {other:?}"),
        }
    }

    #[test]
    fn stop_forced_cause_carries_the_escalation_detail() {
        let cause = TransitionCause::stop_forced("graceful window (1s) elapsed; sent SIGKILL");
        match cause {
            TransitionCause::StopForced { detail } => {
                assert!(detail.contains("SIGKILL"))
            }
            other => panic!("expected StopForced, got {other:?}"),
        }
    }

    // ---- Story 4-2: LogStream / LogLine (AD-12 attribution seed) ----

    #[test]
    fn log_stream_as_str_and_display_agree_with_the_kebab_wire_form() {
        // Mirrors interaction_channel_kind_as_str_and_display_agree (4.1): the
        // closed AD-12 attribution vocabulary uses stable kebab-case tags.
        for (variant, wire) in [
            (LogStream::AgentOut, "agent-out"),
            (LogStream::AgentErr, "agent-err"),
            (LogStream::Engine, "engine"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(variant.to_string(), wire);
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                format!("\"{wire}\"")
            );
            let back: LogStream = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn log_line_carries_schema_version_and_all_fields_and_round_trips() {
        let line = LogLine::new("demo", LogStream::AgentOut, "hello", "2026-07-15T00:00:00Z");
        assert_eq!(line.schema_version, LOG_SCHEMA_VERSION);
        assert_eq!(line.instance, "demo");
        assert_eq!(line.stream, LogStream::AgentOut);
        assert_eq!(line.text, "hello");
        assert_eq!(line.at, "2026-07-15T00:00:00Z");

        let json = serde_json::to_string(&line).unwrap();
        let back: LogLine = serde_json::from_str(&json).unwrap();
        assert_eq!(back, line);
        // The wire attribution token is the literal AD-12 label, not a
        // Rust-derived variant name.
        assert!(json.contains("\"stream\":\"agent-out\""), "{json}");
    }

    #[test]
    fn log_schema_version_is_1_the_frozen_v1_wire_value() {
        // Story 4-3 fix pass (H5). The sibling assertion in
        // `log_line_carries_schema_version_and_all_fields_and_round_trips`
        // compares `line.schema_version` to the CONSTANT it was stamped from,
        // so it is tautological: bumping `LOG_SCHEMA_VERSION` cannot fail it.
        // Pin the LITERAL instead — exactly as `FLEET_SCHEMA_VERSION` is pinned
        // to a literal `2` by `fleet_schema_version_is_2_after_the_3_5_additive_bump`
        // — so a version bump on this v1 compatibility surface (PRD §7) must be
        // a deliberate, announced edit. `LogLine` is the `kt agent logs --json`
        // NDJSON payload (story 4-3) and the story-7-2 subscription payload.
        assert_eq!(LOG_SCHEMA_VERSION, 1);
    }
}
