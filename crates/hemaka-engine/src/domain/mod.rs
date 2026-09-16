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
mod instance;
mod lifecycle;
mod name;
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
    ResolvedValue, SourceLayer, BUDGET_BREACH_ACTION_KEY, BUDGET_DOLLARS_CUMULATIVE_KEY,
    BUDGET_DOLLARS_PER_RUN_KEY, BUDGET_TOKENS_CUMULATIVE_KEY, BUDGET_TOKENS_PER_RUN_KEY,
    COST_RATE_INPUT_KEY, COST_RATE_OUTPUT_KEY, MEMORY_DIR_KEY, METERING_BASE_URL_KEY,
    METERING_UPSTREAM_BASE_URL_KEY, PASS_THROUGH_PREFIX, SECRET_MASK, SECRET_PREFIX,
};
pub use cost::{
    cost_micros, render_dollars, render_dollars_bare, CostCap, CostEvaluator, EstimateLabel,
    Micros, Rate, MICROS_PER_DOLLAR,
};
pub use error::{EngineError, RegistryError};
pub use event::{
    BreachDimension, BudgetBreachEvent, LogLine, LogStream, TransitionCause, TransitionEvent,
    BUDGET_SCHEMA_VERSION, EVENT_SCHEMA_VERSION, FLEET_SCHEMA_VERSION, LOG_SCHEMA_VERSION,
};
pub use fleet::{BudgetView, FleetEntry, FleetListing, FleetTotals, UsageView};
pub use instance::AgentInstance;
pub use lifecycle::LifecycleState;
pub use name::{InstanceName, NameError};
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
