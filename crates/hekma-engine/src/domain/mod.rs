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
pub(crate) mod config;
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
pub(crate) mod pipeline;
/// The shared per-attempt probe evaluators (contract `1.1.0`): the exact
/// `http-get` / file-fresh code BOTH declared polls run — the readiness gate
/// and the liveness check. Private to the crate.
pub(crate) mod probe_kinds;
/// The readiness-probe runner (contract `1.1.0`): engine-side evaluation of a
/// manifest's declared `[readiness]` gate. Private to the crate — the
/// supervisor's start path is the only caller.
pub(crate) mod readiness;
mod registry;
pub(crate) mod registry_limits;
#[cfg(test)]
mod registry_limits_tests;
pub(crate) mod registry_policy;
#[cfg(test)]
mod registry_policy_tests;
mod restart;
mod resync;
pub(crate) mod runs;
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
    is_pass_through, is_secret_ref, parse_dollars_to_micros, pass_through_tail, resolve,
    resolve_cost, resolve_token_budget, resolve_upstream_base_url, secret_name, ConfigError,
    ConfigLayer, EffectiveConfig, ResolvedValue, SourceLayer, ACP_ARGS_KEY, ACP_COMMAND_KEY,
    BUDGET_BREACH_ACTION_KEY, BUDGET_DOLLARS_CUMULATIVE_KEY, BUDGET_DOLLARS_PER_RUN_KEY,
    BUDGET_TOKENS_CUMULATIVE_KEY, BUDGET_TOKENS_PER_RUN_KEY, COST_RATE_INPUT_KEY,
    COST_RATE_OUTPUT_KEY, MEMORY_DIR_KEY, METERING_BASE_URL_KEY, METERING_UPSTREAM_BASE_URL_KEY,
    PASS_THROUGH_PREFIX, SECRET_MASK, SECRET_PREFIX,
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
    canonical_json, checksum_file_content, decide, decide_chain, fence_code, fence_refusal,
    generate, governance_vectors_checksum, re_armed_keys, rearm, run_derivation, Case, ChainCase,
    CoverTag, DecisionCase, DerivationCase, DerivationSpec, DollarArm, FenceCase, FenceRefusal,
    GovernanceDecision, GovernanceInput, GovernanceOutcome, LatchKey, LatchedReason, LevelArms,
    LevelTotals, Provenance, RearmCase, RearmInput, RearmOutcome, RearmTrigger, RowPricing,
    TokenArm, VectorsDocument, ALL_COVERS, FENCE_ERROR_TYPE, FENCE_REASON_ORDER,
    FENCE_REFUSAL_STATUS, GOVERNANCE_VECTORS_ARTIFACT_ID, GOVERNANCE_VECTORS_ARTIFACT_PATH,
    GOVERNANCE_VECTORS_SCHEMA_VERSION, GOVERNANCE_VECTORS_VERSION, PROXY_WIRE_VERSION,
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
pub use registry_policy::{PolicyChange, PolicyOutcome};
pub use resync::{ResyncBatch, ResyncCursor};
pub use runs::{
    validate_label_key, validate_label_value, verb_cell, Anomaly, CellBehavior, ChildRunRequest,
    ControlDepth, Label, LabelChange, LabelOrigin, LabelOutcome, LabelSource, RunError, RunEvent,
    RunEventKind, RunLimits, RunParent, RunRecord, RunStatus, RunUsage, RunVerb, RunView, Selector,
    SelectorScope, VerbCell, HOST_IDENTITY_BINDING, MAX_RUN_NESTING, MAX_SELECTED_RUNS,
    RUN_EVENT_SCHEMA_VERSION, RUN_VERBS,
};
pub use runs::{
    BreachDefaultUpgradedEvent, LimitKind, LimitSource, LimitSources, PolicyEvent, PolicyEventKind,
    PolicyLimits, PolicyRecord, PolicyResolution, RunPolicy, ScentCheckMode, UpgradedInstance,
    DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
};
pub use runs::{
    LimitActor, LimitsDirection, RunLifecycleVerb, RunLimitChange, RunLimitField, RunLimitsOutcome,
    RunLimitsView, RunVerbError,
};
// The resync core is engine-internal glue the facade calls (the story-10-3
// helper); only the cursor/batch TYPES are public surface.
pub use restart::{is_crash_loop, BackoffSchedule, RestartPolicy, MAX_CONSECUTIVE_FAILURES};
pub(crate) use resync::read_committed;
pub use secret::{SecretString, REDACTED};
pub(crate) use supervisor::registry_to_engine as registry_error_to_engine;
pub use supervisor::{DiagnosticSink, DrainOutcome, RestartPlan, Supervisor, DEFAULT_STOP_WINDOW};
// cfg(test) only (epic 20-1 coverage seam): the stop-fault vocabulary the
// engine-level test arming (`Engine::arm_test_stop_fault`) names — never
// compiled into production binaries.
#[cfg(test)]
pub(crate) use supervisor::StopFault;
pub use transition::{next_state, LifecycleCommand, LifecycleError};
pub use usage::{
    RecordOutcome, RunId, UsageEvent, UsageTotals, UsageUpdateEvent, USAGE_SCHEMA_VERSION,
};
