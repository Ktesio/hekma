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
mod store;
mod time;

// Re-export the registration + lifecycle public surface at the crate root
// (the Embedding Interface). `kt` drives the engine through the async [`Engine`]
// and its `blocking()` facade (AD-13); `Registry` stays public for in-workspace
// collaborators + tests but is no longer what `kt` uses directly.
pub use adapter::{AdapterRef, ResolvedAdapter};
pub use domain::{
    broadcast, is_pass_through, render_dollars, render_dollars_bare, resolve, AcpContextCostView,
    AcpContextUsageView, AgentInstance, BreachAction, BreachDimension, BreachScope,
    BudgetBreachEvent, BudgetView, ConfigError, ConfigLayer, CostCap, DiagnosticSink, DrainOutcome,
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
pub use engine::{Blocking, DrainReport, Engine, EngineOptions, EventSubscription, InstanceStatus};
// Story 19-1: the governance conformance vectors (AD-7/FR-44) — the embedded
// artifact bytes plus the version/checksum accessors a Host pins against, and
// the pure `decide` core the artifact is generated from. Additive surface.
pub use domain::{
    canonical_json, checksum_file_content, decide, generate, governance_vectors_checksum,
    run_derivation, Case, CoverTag, DecisionCase, DerivationCase, DerivationSpec, DollarArm,
    GovernanceDecision, GovernanceInput, GovernanceOutcome, LatchKey, Provenance, RowPricing,
    TokenArm, VectorsDocument, ALL_COVERS, GOVERNANCE_VECTORS_ARTIFACT_ID,
    GOVERNANCE_VECTORS_ARTIFACT_PATH, GOVERNANCE_VECTORS_SCHEMA_VERSION,
    GOVERNANCE_VECTORS_VERSION,
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
