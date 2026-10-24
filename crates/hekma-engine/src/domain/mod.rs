//! Engine domain core (spine AD-1).
//!
//! Pure domain: no dependency on OS-conditional code or terminal/UX crates. It
//! names the [`ProcessBackend`](crate::ports::ProcessBackend) trait (and the
//! cfg-selected backend aliases in `backends/`) but no OS type. This crate now
//! realizes the registration slice PLUS the story-1.4 lifecycle core — the
//! [`LifecycleState`] set, the data-driven transition table
//! ([`next_state`](transition::next_state)), the [`TransitionEvent`] (AD-14
//! seed), and the [`Supervisor`] that drives start/stop through the process
//! backend. Budgets and config resolution arrive with their stories.

mod budget;
mod bus;
mod config;
mod cost;
mod error;
mod event;
mod fleet;
mod governance;
mod instance;
mod lifecycle;
/// The liveness-check evaluator (contract `1.1.0`): engine-side evaluation of
/// a manifest's declared `[liveness]` check. Private to the crate — the
/// reaper's poll is the only caller.
pub(crate) mod liveness;
mod name;
mod outbox;
/// The shared per-attempt probe evaluators (contract `1.1.0`): the exact
/// `http-get` / file-fresh code BOTH declared polls run — the readiness gate
/// and the liveness check. Private to the crate.
pub(crate) mod probe_kinds;
/// The readiness-probe runner (contract `1.1.0`): engine-side evaluation of a
/// manifest's declared `[readiness]` gate. Private to the crate — the
/// supervisor's start path is the only caller.
pub(crate) mod readiness;
mod registry;
mod restart;
mod resync;
mod secret;
mod supervisor;
mod transition;
mod usage;

pub use budget::{
    BreachAction, BreachDecision, BreachScope, BudgetEvaluator, ParseBreachActionError, TokenBudget,
};
pub(crate) use bus::EventBus;
pub use bus::{broadcast, EngineEvent, EVENT_BUS_CAPACITY};
pub use config::{
    is_pass_through, is_secret_ref, pass_through_tail, resolve, resolve_cost, resolve_token_budget,
    resolve_upstream_base_url, secret_name, ConfigError, ConfigLayer, EffectiveConfig,
    ResolvedValue, SourceLayer, ACP_ARGS_KEY, ACP_COMMAND_KEY, BUDGET_BREACH_ACTION_KEY,
    BUDGET_DOLLARS_CUMULATIVE_KEY, BUDGET_DOLLARS_PER_RUN_KEY, BUDGET_TOKENS_CUMULATIVE_KEY,
    BUDGET_TOKENS_PER_RUN_KEY, COST_RATE_INPUT_KEY, COST_RATE_OUTPUT_KEY, MEMORY_DIR_KEY,
    METERING_BASE_URL_KEY, METERING_UPSTREAM_BASE_URL_KEY, PASS_THROUGH_PREFIX, SECRET_MASK,
    SECRET_PREFIX,
};
pub use cost::{
    cost_micros, cost_micros_with_cached, render_dollars, render_dollars_bare, CostCap,
    CostEvaluator, EstimateLabel, Micros, Rate, MICROS_PER_DOLLAR,
};
pub use error::{EngineError, RegistryError};
pub use event::{
    BreachDimension, BudgetBreachEvent, LogLine, LogStream, TransitionCause, TransitionEvent,
    BUDGET_SCHEMA_VERSION, EVENT_SCHEMA_VERSION, FLEET_SCHEMA_VERSION, LOG_SCHEMA_VERSION,
};
pub use fleet::{
    AcpContextCostView, AcpContextUsageView, BudgetView, FleetEntry, FleetListing, FleetTotals,
    Supervision, UsageGapNotice, UsageView,
};
pub use governance::{
    canonical_json, checksum_file_content, decide, generate, governance_vectors_checksum,
    run_derivation, Case, CoverTag, DecisionCase, DerivationCase, DerivationSpec, DollarArm,
    GovernanceDecision, GovernanceInput, GovernanceOutcome, LatchKey, Provenance, RowPricing,
    TokenArm, VectorsDocument, ALL_COVERS, GOVERNANCE_VECTORS_ARTIFACT_ID,
    GOVERNANCE_VECTORS_ARTIFACT_PATH, GOVERNANCE_VECTORS_SCHEMA_VERSION,
    GOVERNANCE_VECTORS_VERSION,
};
pub use instance::AgentInstance;
pub use lifecycle::LifecycleState;
pub use name::{InstanceName, NameError};
pub(crate) use outbox::{ack_core, export_core};
pub use outbox::{
    ExportBatch, OutboxCursor, OutboxEntry, BELOW_FLOOR_SENTENCE, EXPORT_BATCH_DEFAULT_MAX,
    EXPORT_BATCH_MAX_CAP, OUTBOX_MAX_BYTES, OUTBOX_SCHEMA_VERSION, PAST_TAIL_SENTENCE,
};
pub use registry::{Registry, RemoveDisposition};
pub use resync::{ResyncBatch, ResyncCursor};
// The resync core is engine-internal glue the facade calls (the story-10-3
// helper); only the cursor/batch TYPES are public surface.
pub use restart::{is_crash_loop, BackoffSchedule, RestartPolicy, MAX_CONSECUTIVE_FAILURES};
pub(crate) use resync::read_committed;
pub use secret::{SecretString, REDACTED};
pub(crate) use supervisor::registry_to_engine as registry_error_to_engine;
pub use supervisor::{DiagnosticSink, RestartPlan, Supervisor, DEFAULT_STOP_WINDOW};
pub use transition::{next_state, LifecycleCommand, LifecycleError};
pub use usage::{
    RecordOutcome, RunId, UsageEvent, UsageTotals, UsageUpdateEvent, USAGE_SCHEMA_VERSION,
};
