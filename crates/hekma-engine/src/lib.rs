//! # hekma-engine
//!
//! Home of the Ktesio engine: agent lifecycle, governance, and interaction
//! logic live here, behind a public API that *is* the Embedding Interface
//! (architecture spine AD-1, AD-2, AD-13).
//!
//! ## What this crate exposes (stories 1.2–1.3)
//!
//! The registration capability (FR-1/FR-2/FR-3), now with adapter resolution
//! and validation (story 1.3, FR-1 path registration / FR-19 metering / FR-27
//! contract seed). The public surface is deliberately small — the registry
//! service, the domain types it returns, and the adapter-contract types `kt`
//! renders:
//!
//! - [`Registry`] — `open`, `register`, `register_with_adapter`, `remove`,
//!   `list`, `get` (the Embedding Interface for registration; `kt` drives these
//!   directly).
//! - [`AdapterRef`] — a native-kind or manifest-path adapter request;
//!   [`ResolvedAdapter`] — the validated adapter view registration persists.
//! - [`RemoveDisposition`] — retain-or-delete choice for `remove`.
//! - [`AgentInstance`], [`InstanceName`], [`LifecycleState`] — returned domain
//!   types.
//! - [`CapabilityDeclaration`], [`EffectiveCapabilities`], [`Capability`],
//!   [`SupportLevel`], [`MeteringSource`], [`OsId`] — re-exported from
//!   `hekma-adapter-api` so `kt` can render the effective per-OS declaration.
//! - [`RegistryError`] / [`NameError`] — the `thiserror` error surface `kt`
//!   wraps into `miette` diagnostics (no `miette` in this lib — conventions).
//!
//! Adapter resolution PARSES + VALIDATES only; it executes no lifecycle op
//! (story 1.4 owns the manifest executor and process launch).
//!
//! Everything else — the [`StateStore`] port, its SQLite implementation, and
//! the path authority — stays crate-internal (AD-1/AD-2). The `ports` module is
//! `pub` so the *port trait* is nameable by future in-workspace collaborators,
//! but the concrete store is not re-exported.
//!
//! ## Observing the engine: the event subscription (story 7-2, FR-33/AD-14)
//!
//! A Host subscribes via [`Engine::subscribe`] (async consumers, raw
//! [`broadcast::Receiver`]) or [`Blocking::subscribe`] (sync consumers, an
//! [`EventSubscription`] with a blocking `recv`). The bounded bus
//! ([`EVENT_BUS_CAPACITY`]) is fed at the same commit points where the event
//! logs append, so subscribers observe exactly the committed truth in commit
//! order; payloads are the [`EngineEvent`] wrapper over the existing versioned
//! AD-14 structs verbatim. The full contract — commit-point guarantee,
//! per-instance FIFO, the `Lagged` slow-subscriber policy — is documented on
//! the `domain::bus` module.
//!
//! ## Routing engine diagnostics: the host-provided sink (story 10-2)
//!
//! The engine writes TWO operational diagnostics — the DC-10 memory-delivery
//! notice and the enforcement breadcrumb. With no sink installed they go to
//! stderr, byte-identical to every pre-sink release. A host that owns its
//! stderr installs a [`DiagnosticSink`] (any `std::io::Write`, thread-safely
//! wrapped) via [`Engine::open_with_diagnostics`] (in place before any
//! supervision work) or [`Engine::with_diagnostics`] /
//! [`Blocking::with_diagnostics`] (install or rotate later); the diagnostics
//! then route to the sink, receiving the exact bytes — same `[hekma] `
//! prefixed text, one newline-terminated line each — stderr would have
//! received. The sink is engine-embedder ergonomics only: the adapter-api
//! contract is untouched.
//!
//! ## Healing the crash window: the resync helper (story 10-3)
//!
//! The bus delivers at-most-once in the crash window between a durable append
//! and its publish. [`Engine::resync_events`] /
//! [`Blocking::resync_events`] close that window for hosts: one call reads the
//! instance's COMMITTED event records (transitions, breaches, ledger rows —
//! the same truth the query APIs serve) and returns them as the exact
//! [`EngineEvent`] payloads the live bus carries, past a [`ResyncCursor`] so
//! re-runs are idempotent. The contract is **resync first, then subscribe** —
//! that order gives backfilled-prefix + live-suffix continuity with no gap
//! and no duplicate. Ordering and tolerance details are documented on the
//! `domain::resync` module.
//!
//! ## Dependency law (AD-2)
//!
//! The `kt` binary depends only on this crate's public API (plus
//! `hekma-adapter-api` types); this crate depends on `hekma-adapter-api` and
//! never on `kt` or concrete adapters.
//!
//! [`StateStore`]: crate::ports::StateStore

pub mod adapter;
// The ACP transport core (spine AD-19, story 14-1): engine-INTERNAL — the
// client-side codec/handshake/connection for the builtin `acp` kind. `kt`
// never sees it; the public surface change for 14-1 is the kind's
// registration availability only.
mod acp;
mod backends;
pub mod domain;
mod engine;
// Engine-observed metering (AD-7 / FR-19), story 3-4: the loopback forward
// listener + the OpenAI `usage` parse. Engine-INTERNAL (AD-2 — `kt` never sees
// the listener); the supervisor drives it. Portable — NO OS cfg (in core, not
// `backends/`; the OS-cfg gate stays green).
mod metering;
pub mod paths;
pub mod ports;
mod sha256;
// The shutdown drain (issue #328 item 3): the fleet-wide stop ladder behind
// `Engine::shutdown` — engine-owned so a plain embedding gets the graceful
// path without the host crate (`serve`'s drain delegates here).
mod shutdown;
mod store;
mod time;

// Re-export the registration + lifecycle public surface at the crate root
// (the Embedding Interface). `kt` drives the engine through the async [`Engine`]
// and its `blocking()` facade (AD-13); `Registry` stays public for in-workspace
// collaborators + tests but is no longer what `kt` uses directly.
pub use adapter::{AdapterRef, ResolvedAdapter};
/// Story 21-4: the detached spawn the `hekma` CLI uses to auto-start its
/// Service Host (the engine owns process control; the OS parts live in
/// `backends/`).
pub use backends::spawn_detached_process;

/// Story 22-2 (P10): harden the calling process's memory — on Linux it
/// becomes non-dumpable (no core dump, no same-user `ptrace` or
/// `/proc/<pid>/mem` read) with `RLIMIT_CORE` 0; on other systems nothing
/// changes. The Service Host calls it once at start, because its custody
/// cache holds provider keys in memory. An embedding host may call it too.
pub fn harden_host_memory() -> Result<(), String> {
    backends::harden_host_memory()
}
pub use domain::{
    broadcast, is_pass_through, render_dollars, render_dollars_bare, resolve, AcpContextCostView,
    AcpContextUsageView, AgentInstance, BreachAction, BreachDimension, BreachScope,
    BudgetBreachEvent, BudgetView, ConfigError, ConfigLayer, CostCap, DiagnosticSink,
    EffectiveConfig, EngineError, EngineEvent, EstimateLabel, ExportBatch, FleetEntry,
    FleetListing, FleetTotals, InstanceName, LifecycleCommand, LifecycleError, LifecycleState,
    LogLine, LogStream, Micros, NameError, OutboxCursor, OutboxEntry, Rate, Registry,
    RegistryError, RemoveDisposition, ResolvedValue, RestartPolicy, ResyncBatch, ResyncCursor,
    RunId, SourceLayer, Supervision, TokenBudget, TransitionCause, TransitionEvent, UsageEvent,
    UsageGapNotice, UsageTotals, UsageUpdateEvent, UsageView, BELOW_FLOOR_SENTENCE,
    BUDGET_SCHEMA_VERSION, EVENT_BUS_CAPACITY, EVENT_SCHEMA_VERSION, EXPORT_BATCH_DEFAULT_MAX,
    EXPORT_BATCH_MAX_CAP, FLEET_SCHEMA_VERSION, LOG_SCHEMA_VERSION, MICROS_PER_DOLLAR,
    OUTBOX_MAX_BYTES, OUTBOX_SCHEMA_VERSION, PASS_THROUGH_PREFIX, PAST_TAIL_SENTENCE, SECRET_MASK,
    USAGE_SCHEMA_VERSION,
};
// Story 21-2: Runs as first-class entities (AD-25) — the Run model, its
// events and errors, and the verb × Control Depth table.
// Story 21-9: bound labels — the label verb's request and outcome, label
// origins, and Selectors.
pub use domain::{
    parse_dollars_to_micros, validate_label_key, validate_label_value, verb_cell, Anomaly,
    CellBehavior, ChildRunRequest, ControlDepth, Label, LabelChange, LabelOrigin, LabelOutcome,
    LabelSource, RunError, RunEvent, RunEventKind, RunLimits, RunParent, RunRecord, RunStatus,
    RunUsage, RunVerb, RunView, Selector, SelectorScope, VerbCell, HOST_IDENTITY_BINDING,
    MAX_RUN_NESTING, MAX_SELECTED_RUNS, RUN_EVENT_SCHEMA_VERSION, RUN_VERBS,
};
// Story 21-6: Selectors on every verb and `hekma budget` — the Run-grain
// lifecycle verbs, Run-level limits as one actor, and the budget read.
pub use domain::{
    LimitActor, LimitsDirection, RunLifecycleVerb, RunLimitChange, RunLimitField, RunLimitsOutcome,
    RunLimitsResults, RunLimitsView, RunVerbError,
};
// Story 21-5: the Run listing — `hekma ps` and `GET /v0/runs`'s read model.
pub use domain::{
    decode_cursor, encode_cursor, ended_run_cutoff, LatchedBreach, ListRunsQuery, ListingCounts,
    ListingView, PerfRunRow, RunBudgetView, RunListing, RunListingDocument, RunSummary,
    ENDED_RUN_RETENTION_DAYS, LISTING_DEFAULT_LIMIT, LISTING_MAX_LIMIT, RUN_LISTING_SCHEMA_VERSION,
};
// The per-call read surface over the calls store: the query, the record,
// the listing and its document, the forward-compatible enums, the cursor
// wrappers, the error surface, and the helpers the surfaces share.
pub use domain::calls::{
    decode_call_cursor, encode_call_cursor, expose_model, is_call_id, is_run_id, CallDisposition,
    CallError, CallFilters, CallListing, CallListingDocument, CallOutcome, CallRecord, CallView,
    CallsQuery, StatusClass, CALLS_DEFAULT_LIMIT, CALLS_LISTING_SCHEMA_VERSION, CALLS_MAX_LIMIT,
    CALLS_SCAN_BUDGET, MAX_MODEL_CHARS, MODEL_REDACTED,
};
// The RFC 3339 formatters behind the window arithmetic (story 21-5: the
// bench seeds ended rows at fixed distances from now). The parser and the
// microsecond formatter serve the per-call read surface's time window.
pub use crate::time::{format_rfc3339, format_rfc3339_micros, now_unix_secs, parse_rfc3339};
// Story 21-3: Policies — the request and outcome of a Policy change, the
// stored Policy, its limits and events, and the resolution a Run carries.
pub use domain::{
    BreachDefaultUpgradedEvent, LimitKind, LimitSource, LimitSources, PolicyChange, PolicyEvent,
    PolicyEventKind, PolicyLimits, PolicyOutcome, PolicyRecord, PolicyResolution, RunPolicy,
    ScentCheckMode, UpgradedInstance, DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
};
pub use engine::{Blocking, DrainReport, Engine, EngineOptions, EventSubscription, InstanceStatus};
// Story 22-1: Run Keys — the credential classes, the one-time issued pair,
// the snapshot's match, the metadata listing and revocation report, the
// credential event, and the actor a Caller acts as.
/// Story 22-1: the URL scan the Control API and the metering listener use
/// to refuse a credential in a path or query.
pub use domain::credential::{percent_decode, scan_url, UrlScan, URL_SCAN_MAX_TOKENS};
pub use domain::{
    parse_presented, CredentialChange, CredentialError, CredentialEvent, CredentialMatch,
    CredentialSummary, IssuedCredentials, MintedToken, OpenedRun, PresentedState, RevokeOutcome,
    RevokedCredential, RunActor, SecretString, TokenClass, TokenHash, TokenState, ANNOUNCE_PREFIX,
    CREDENTIAL_EVENT_SCHEMA_VERSION, MAX_CALLER_OPEN_CHILDREN, REFRESH_PREFIX, RUN_KEY_PREFIX,
    TOKEN_BODY_LEN, TOKEN_ID_LEN,
};
// Story 23-1: the shared Metering Proxy's endpoint bindings.
pub use domain::{
    Binding, BindingChange, BindingCredential, BindingCredentialClass, BindingError, BindingEvent,
    BindingOutcome, BindingPatch, BindingRoute, BindingSpec, IssuedBindingCredential, RouteSpec,
    BINDING_EVENT_SCHEMA_VERSION, MAX_BINDINGS, MAX_BINDING_ROUTES,
};
// Story 23-1: the shared Metering Proxy's options and address (the type of
// `EngineOptions::metering_proxy` and `Engine::metering_proxy`).
/// Story 22-1: the reserved credential config keys and the variable every
/// supervised agent receives its Run Key in.
pub use domain::{RUN_KEY_ENV, RUN_KEY_KEY, RUN_KEY_MARKER_PREFIX, RUN_REFRESH_KEY_KEY};
pub use metering::proxy::{MeteringProxyOptions, ProxyInfo, ProxyStartError};
// The shutdown drain's public surface (issue #328 item 3): the planner, the
// per-instance outcome vocabulary, and (via `Engine::shutdown` / its
// `blocking()` twin) the drain itself.
pub use shutdown::{drain_plan, DrainOutcome, StopResult};
// Story 19-1: the governance conformance vectors (AD-7/FR-44) — the embedded
// artifact bytes plus the version/checksum accessors a Host pins against, and
// the pure `decide` core the artifact is generated from. Additive surface.
// Story 19-2: the wire fence's pure consumer (`fence_refusal`) and the
// `proxy-wire v1` refusal constants join it.
pub use domain::{
    canonical_json, checksum_file_content, decide, decide_chain, fence_code, fence_refusal,
    generate, governance_vectors_checksum, re_armed_keys, rearm, run_derivation, Case, ChainCase,
    CoverTag, DecisionCase, DerivationCase, DerivationSpec, DollarArm, FenceCase, FenceRefusal,
    GovernanceDecision, GovernanceInput, GovernanceOutcome, LatchKey, LatchedReason, LevelArms,
    LevelTotals, Provenance, RearmCase, RearmInput, RearmOutcome, RearmTrigger, RowPricing,
    TokenArm, VectorsDocument, ALL_COVERS, FENCE_ERROR_TYPE, FENCE_REASON_ORDER,
    FENCE_REFUSAL_STATUS, GOVERNANCE_VECTORS_ARTIFACT_ID, GOVERNANCE_VECTORS_ARTIFACT_PATH,
    GOVERNANCE_VECTORS_SCHEMA_VERSION, GOVERNANCE_VECTORS_VERSION, PROXY_WIRE_VERSION,
};
/// The governance conformance vectors artifact, embedded verbatim at build
/// time (`governance/vectors-v1.json` — the canonical bytes the committed
/// file, the `.sha256` sidecar, and the checksum accessor all agree on).
/// Byte-for-byte what `kt agent governance-vectors --json` prints.
pub fn governance_vectors_json() -> &'static str {
    include_str!("../governance/vectors-v1.json")
}
// Story 17-4: the log-capture mode vocabulary (the `metering_listen_port`
// thread — serve/config surface, named by `kt serve --log-capture` and the
// host's `ServeOptions`).
pub use ports::LogCaptureMode;
// Re-export the Memory Backing surface (story 5-1, AD-11): the kind vocabulary
// `kt` parses `--kind` against and the status shape the public read returns.
pub use ports::{GuaranteeLevel, MemoryBackingKind, MemoryBackingStatus};

// Re-export the adapter-contract types `kt` needs to render the effective
// per-OS Capability Declaration (AD-2: `kt` names these types, not the schema).
pub use hekma_adapter_api::{
    Capability, CapabilityDeclaration, EffectiveCapabilities, MeteringSource, OsId, SupportLevel,
};
