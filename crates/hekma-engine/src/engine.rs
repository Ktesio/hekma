//! The async engine handle + its blocking facade (spine AD-13).
//!
//! [`Engine`] is the async-first Embedding Interface. It owns a multi-thread
//! tokio [`Runtime`](tokio::runtime::Runtime), the registration [`Registry`]
//! (an internal collaborator), and the in-memory lifecycle [`Supervisor`]. Its
//! public methods are `async`; the blocking DB/FS work runs on tokio's blocking
//! pool via [`tokio::task::spawn_blocking`] (rusqlite is a synchronous C binding
//! that must never run on an async worker — AD-13, Approach A).
//!
//! ## The `blocking()` facade (FR-34 / FR-31 / story 7-3 seed)
//!
//! Sync callers — `hekma` today — drive the engine through [`Engine::blocking`],
//! which returns a [`Blocking`] view whose methods are the sync equivalents,
//! each `runtime.block_on(async_method(..))`. `hekma` stays a synchronous binary
//! (no `#[tokio::main]`); a Host with its OWN runtime (story 7-1/7-3) calls the
//! async methods directly. This story covers exactly the commands `hekma` uses
//! today (register / remove / list / effective-capabilities) plus `start` /
//! `stop`; the FULL facade + the embedding proof are 7-1/7-3.
//!
//! ## No global state, no ambient runtime (AD-13 forward contract)
//!
//! The engine takes its state-dir base explicitly (mirroring [`Registry::open`])
//! and owns its runtime; it never assumes an ambient runtime elsewhere and holds
//! no thread-locals or globals. This is what keeps the facade sound.

//! ## The event subscription (story 7-2, FR-33 / AD-14)
//!
//! A Host OBSERVES the engine through [`Engine::subscribe`], which hands out a
//! [`broadcast::Receiver`](crate::broadcast::Receiver) over the bounded event
//! bus (capacity [`EVENT_BUS_CAPACITY`](crate::EVENT_BUS_CAPACITY)). The bus is
//! fed at the SAME commit points where the event logs are appended — a
//! subscriber sees exactly the committed truth, in commit order, per-instance
//! FIFO — and the payload is the [`EngineEvent`](crate::EngineEvent) wrapper
//! over the EXISTING versioned AD-14 structs verbatim (no new schema family).
//! Slow subscribers are policy, not failure: `send` is non-blocking, so a
//! stalled receiver can never stall supervision; past the capacity it observes
//! `Lagged` and resyncs at the tail (see the `domain::bus` module docs for the
//! full contract). Sync consumers get [`Blocking::subscribe`], whose
//! [`EventSubscription`] owns a blocking `recv` bridged through the engine
//! runtime exactly like every other facade method.

//! ## The diagnostic sink (story 10-2)
//!
//! The engine writes TWO operational diagnostics (the DC-10 memory-delivery
//! notice and the enforcement breadcrumb). By default they go to stderr,
//! exactly as they always have. A host that owns its stderr installs a
//! [`DiagnosticSink`](crate::DiagnosticSink) — any `std::io::Write` — at open
//! ([`Engine::open_with_diagnostics`]) or any time later
//! ([`Engine::with_diagnostics`] / [`Blocking::with_diagnostics`]); the
//! diagnostics then route to the sink, receiving the exact bytes (same
//! `[hekma] `-prefixed text, one line each) stderr would have received.
//! Installing REPLACES any earlier sink (rotation) and is one-way — there is
//! no uninstall back to the stderr default; a host that wants the default
//! back re-opens the engine. The sink is engine-embedder ergonomics — it
//! never touches the adapter-api contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Runtime;

use hekma_adapter_api::EffectiveCapabilities;

use crate::adapter::AdapterRef;
use crate::domain::{
    ack_core, broadcast, export_core, AgentInstance, BudgetBreachEvent, ConfigError, ConfigLayer,
    DiagnosticSink, EffectiveConfig, EngineError, EngineEvent, EventBus, ExportBatch, FleetEntry,
    InstanceName, LifecycleState, LogLine, OutboxCursor, Registry, RegistryError,
    RemoveDisposition, RestartPolicy, ResyncBatch, ResyncCursor, Supervisor, TransitionCause,
    TransitionEvent,
};
use crate::domain::{
    ChildRunRequest, Label, LabelChange, LabelOutcome, LimitActor, ListRunsQuery, PolicyChange,
    PolicyOutcome, PolicyRecord, RunError, RunId, RunLifecycleVerb, RunLimitChange, RunLimitsView,
    RunListing, RunRecord, RunSummary, RunVerbError, RunView, Selector,
};
// Story 23-1: the endpoint bindings the facade writes and reads.
use crate::domain::{
    Binding, BindingCredential, BindingCredentialClass, BindingError, BindingPatch, BindingSpec,
    IssuedBindingCredential, RouteSpec,
};
use crate::ports::{LogCaptureMode, SpawnRecord};
use crate::shutdown::DrainOutcome;

/// How often the crash-detection reaper polls supervised processes (story 1-6,
/// `[ASSUMPTION]`). Small enough that a crash is detected promptly, large enough
/// to avoid busy-work; not spine-mandated. The engine owns this cadence and
/// calls the sync [`Supervisor::poll_once`] via `spawn_blocking`, keeping the
/// supervisor cfg-free + sync.
const CRASH_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The per-instance runtime status the CLI surfaces (story 1-6, AC9): the
/// current Lifecycle State, the effective Restart Policy, the restart count, and
/// (for a `failed` instance) the last-known failed cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceStatus {
    /// The instance the status is for.
    pub instance: AgentInstance,
    /// The effective per-instance Restart Policy (AD-15).
    pub restart_policy: RestartPolicy,
    /// The consecutive-failure restart count (0 when never restarted / reset on a
    /// clean run).
    pub restart_count: u32,
    /// The last-known cause (e.g. the crash / crash-loop detail), if any. Present
    /// for a `failed` instance whose spawn record recorded a cause.
    pub failed_cause: Option<String>,
}

/// The `drain` verb's report (story 18-4): the STOPPED instance plus the
/// composite's durable [`DrainOutcome`]. Both outcomes — completed and
/// timed-out — are a SUCCESS at the type level (the drain's last step is
/// unconditional; a timed-out drain still stopped the agent honestly), so
/// the outcome fields — not the `Result` — carry the wait's truth, and the
/// caller surfaces a timed-out drain as a note, never an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainReport {
    /// The instance in its terminal `stopped` state (with the drain's
    /// durable cause on its last transition edge).
    pub instance: AgentInstance,
    /// What the bounded wait actually did.
    pub outcome: crate::domain::DrainOutcome,
}

/// Story 21-4: the failed cause a READ-ONLY read shows for a `running` or
/// `paused` row whose recorded process is gone — the fact adoption's
/// reconciliation records, plus that nothing has recorded it yet.
fn gone_cause(pid: u32, last_known: Option<&str>) -> String {
    format!(
        "process pid {pid} is gone (not recorded yet: the next hekma write reconciles the \
         row to failed); last known: {}",
        last_known.unwrap_or("no prior cause recorded")
    )
}

/// Extract a human-readable failed-cause detail from a transition [`TransitionCause`]
/// for the `instance_status` event-log fallback (AC9). Returns the carried detail
/// for the failure-bearing causes (a launch error, a crash — the 18-3 charged
/// detail may also carry the stable-window reset naming — a readiness-probe
/// timeout — the timeout names the probe + the honest wait — an unresponsive
/// declared liveness check — naming the check + its failing streak — or a
/// declared restart-requested exit — naming the code + the recycle verdict), or
/// `None` for a cause that does not describe a failure.
fn failed_cause_detail(cause: &TransitionCause) -> Option<String> {
    match cause {
        TransitionCause::LaunchError { detail } => Some(detail.clone()),
        TransitionCause::Crashed { detail } => Some(detail.clone()),
        // The readiness gate never opened: name the probe + how long it was
        // honestly waited, so `show` explains the failed start without a log
        // dig.
        TransitionCause::ReadinessTimeout { probe, waited_ms } => {
            Some(format!("{probe} did not pass within {waited_ms}ms"))
        }
        // The declared signal died: name the check + how long it had been
        // failing, so `show` explains the unresponsive failure the same way
        // (story 18-2 — no new verb, the same fallback shape). When the
        // reaper recorded expiry-verdict detail (18-2 review round 1 — e.g.
        // the heartbeat file's pre-existing staleness at first observation),
        // it rides along here so `show` carries the whole verdict, not just
        // the streak.
        TransitionCause::Unresponsive {
            check,
            failing_for_ms,
            detail,
        } => Some(match detail {
            Some(detail) => format!("{check} failing for {failing_for_ms}ms ({detail})"),
            None => format!("{check} failing for {failing_for_ms}ms"),
        }),
        // The agent asked for a relaunch on a declared code (story 18-3):
        // name the code + the verdict so `show` explains the `failed`
        // landing without a log dig. The reaper ALWAYS records a detail for
        // this cause — the declared class plus the requested-recycle depth
        // (18-3 review: the pacing is surfaced), or the `never`-policy
        // verdict when it outranked the ask — so the bare-class fallback
        // below renders only for detail-less, hand-built events. (The
        // `CleanExit` twin never lands here — `stopped` is
        // not a failure, so the fallback stays `None` for it.)
        TransitionCause::RestartRequested { code, detail } => Some(match detail {
            Some(detail) => format!("exit code {code} requested a restart ({detail})"),
            None => {
                format!("exit code {code} requested a restart (declared `restart_requested`)")
            }
        }),
        _ => None,
    }
}

/// Map a [`Supervisor::stop`] failure encountered during [`Engine::remove`]'s
/// live-instance teardown (AI-11) into a [`RegistryError`], since `remove` speaks
/// the registry error type. A store failure passes through as
/// [`RegistryError::Store`]; any other stop failure (a backend terminate/signal
/// error, a log-write error, …) surfaces as a filesystem-shaped
/// [`RegistryError::Io`] naming the instance — so a teardown failure ABORTS the
/// remove (the row is NOT deleted and the process is NOT orphaned) with a
/// diagnostic the CLI already renders, rather than inventing a new public variant.
fn stop_error_to_registry(err: EngineError) -> RegistryError {
    match err {
        EngineError::Store(inner) => RegistryError::Store(inner),
        EngineError::InvalidName { name, reason } => RegistryError::InvalidName { name, reason },
        EngineError::NotFound { name } => RegistryError::NotFound { name },
        other => RegistryError::Io {
            name: "<remove-teardown>".to_string(),
            path: "<process teardown>".to_string(),
            source: std::io::Error::other(format!(
                "could not stop the live instance before removal: {other}"
            )),
        },
    }
}

/// The async engine handle (the Embedding Interface, AD-2/AD-13).
///
/// Constructed once per embedding via [`Engine::open`]. Owns the runtime, the
/// registry, and the supervisor for a single engine lifetime (cross-restart
/// orphan adoption is story 1-6).
pub struct Engine {
    /// The engine-owned multi-thread runtime the blocking facade drives.
    rt: Arc<Runtime>,
    /// Shared engine state (registry + supervisor), guarded for `spawn_blocking`.
    inner: Arc<EngineInner>,
    /// The crash-detection reaper task's abort handle (story 1-6). Aborted on
    /// [`Engine::drop`] so the background poll stops with the engine. `None`
    /// for a read-only engine (story 21-4), which runs no reaper.
    reaper: Option<tokio::task::AbortHandle>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the background reaper when the engine goes away (its supervised
        // handles drop with the supervisor, killing spawned processes).
        if let Some(reaper) = &self.reaper {
            reaper.abort();
        }
        // Story 15-2 (Fibery 178): before the supervised handles drop with the
        // supervisor, release every DETACHED PAUSED one — SIGCONT the child
        // and record paused→running — so a detached child is never left
        // SIGSTOPped and unsupervised between commands. This runs while
        // `inner` still holds the supervisor's handle map (the Arc teardown
        // happens after this body), and BEFORE the map drops. Plain sync work
        // on the dropping thread (locks + one DB write per released instance):
        // `drop` may run inside a host's async context, so this deliberately
        // does NOT enter the runtime. A POISONED lock is recovered via
        // `into_inner` rather than panicking — a panic inside `Drop` would
        // abort the whole process, and the release pass is best-effort by
        // contract.
        let inner = Arc::clone(&self.inner);
        let registry = inner
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut supervisor = inner
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        supervisor.release_detached_paused(&registry);
    }
}

/// Shared, `Send + Sync` engine state moved onto the blocking pool.
///
/// The [`Registry`] owns a rusqlite `Connection` (`Send` but `!Sync`); wrapping
/// it in a [`Mutex`] makes the whole `EngineInner` `Send + Sync` so a
/// `spawn_blocking` closure can capture an [`Arc<EngineInner>`]. Registration is
/// not a hot path and the engine is single-lifetime, so a coarse mutex is the
/// correct, simplest altitude here.
struct EngineInner {
    /// Story 23-1: the shared Metering Proxy, when it runs. FIRST so it
    /// drops (closing its store-writer handle) before the registry, whose
    /// writer joins once every handle is gone.
    metering_proxy: Mutex<Option<crate::metering::proxy::MeteringProxy>>,
    /// Story 23-1: why the proxy is not running, when it is enabled.
    metering_proxy_error: Option<String>,
    /// Story 21-4: this engine was opened read-only ([`Engine::open_read_only`])
    /// — no writer, no adoption, no reaper. Its Fleet reads take a live
    /// instance's current Run id from the persisted spawn record (the id the
    /// owning Service Host's supervisor holds), since its own supervisor is
    /// empty.
    read_only: bool,
    registry: Mutex<Registry>,
    supervisor: Mutex<Supervisor>,
    /// The supervisor's event bus clone (story 7-2): held OUTSIDE the mutex so
    /// [`Engine::subscribe`] hands out receivers without ever contending on
    /// the supervisor lock. Both clones share one broadcast channel.
    events: EventBus,
    /// The registry's published Run snapshot, held OUTSIDE the registry
    /// mutex (story 21-5): the SSE Selector filter resolves a snapshot-
    /// resident subject Run's labels lock-free, so a filtered stream never
    /// contends with supervision on the registry lock per event.
    run_snapshot: Arc<crate::domain::runs::snapshot::SnapshotPublisher>,
    /// Story 22-1: a read-only connection for the credential lookups an
    /// UNAUTHENTICATED request can cause (a snapshot miss, a refresh
    /// presentation, a token seen in a URL). Held OUTSIDE the registry and
    /// supervisor mutexes, and never a writer job: only a credential that
    /// resolves here goes on to take those locks or the store writer.
    credential_reader: Mutex<crate::store::SqliteStore>,
    /// The calls store's directory: the per-call read surface opens its own
    /// read-only segment connections under this path, taking NO registry or
    /// supervisor lock and sending nothing to the writer — the read cannot
    /// slow supervision or the proxy's hot path.
    calls_dir: std::path::PathBuf,
}

/// Open-time options for the long-running Host (story 17-2) — the additive,
/// backward-compatible seam behind
/// [`Engine::open_with_options`]. `Default` reproduces
/// [`Engine::open`]'s behavior byte-for-byte (ephemeral listener ports, full
/// child-environment inheritance, no diagnostic sink), so every pre-17-2
/// caller is unchanged.
///
/// These are OPEN-TIME only (there is no post-open mutation): a port change
/// mid-flight would strand already-started listeners, and an allowlist flip
/// would make siblings inconsistent, so the supervisor reads them from the
/// first moment adoption and any start can run.
#[derive(Clone, Default)]
pub struct EngineOptions {
    /// The shared Metering Proxy (story 23-1): on by default on an
    /// ephemeral, unrecorded port; the Service Host makes the port sticky
    /// per state directory. A taken port never hops: the engine opens
    /// without a proxy, says why ([`Engine::metering_proxy_error`]), and
    /// refuses `engine-observed` starts until it is free.
    pub metering_proxy: crate::metering::proxy::MeteringProxyOptions,
    /// When `true`, every child spawn CLEARS the inherited environment first
    /// and applies ONLY the manifest-declared mappings (the Host's
    /// `child_env_allowlist`: no ambient Host-shell variables leak into hosted
    /// agents). `false` (the default) keeps full inheritance.
    pub child_env_allowlist: bool,
    /// The engine's log-capture posture (story 17-4, the AD-12 amendment):
    /// what is written to disk for a spawned agent's stdout/stderr. `full`
    /// (the default) is byte-identical to the pre-17-4 capture; `metadata`
    /// keeps raw content only transiently (metadata-only at rest, raw files
    /// unlinked at stop); `off` captures nothing and every surface shows
    /// honest absence. Open-time only, like the rest of the serve options.
    pub log_capture: LogCaptureMode,
    /// The host-provided diagnostic sink, installed BEFORE orphan adoption
    /// and the crash-detection reaper start — identical semantics to
    /// [`Engine::open_with_diagnostics`]'s `sink` parameter.
    pub diagnostics: Option<DiagnosticSink>,
    /// Story 18-3's TEST SCALING SEAM for the ENGINE-OWNED stable-run
    /// window: `Some(window)` overrides [`STABLE_RUN_WINDOW`] (30 s) on this
    /// engine's supervisor, so integration tests can drive the
    /// resets-then-charges semantics in milliseconds — the same injection
    /// posture [`BackoffSchedule::with_base_and_cap`] gives the in-module
    /// tests, which production callers never take. `None` (the default) is
    /// byte-identical production behavior. A `Some` ZERO is IGNORED (18-3
    /// review): every run is trivially "stable" against a zero window, so
    /// the supervisor keeps the production window and emits a diagnostic —
    /// a zero can never silently disable the counter's charging. NOT an
    /// operator config surface —
    /// the tuning knob is deferred with a trigger (an operator asks to tune
    /// it), per the 18-3 spec; this field stays `#[doc(hidden)]` so it
    /// never reads as one.
    #[doc(hidden)]
    pub stable_run_window_override: Option<Duration>,
    /// Story 18-4's TEST SCALING SEAM for the ENGINE-OWNED standalone
    /// pause-command cap: `Some(cap)` overrides [`PAUSE_COMMAND_CAP`] (30 s)
    /// on this engine's supervisor, so integration tests can drive the
    /// cap-overrun rejection in milliseconds — the same injection posture as
    /// [`EngineOptions::stable_run_window_override`], which production
    /// callers never take. `None` (the default) is byte-identical production
    /// behavior. A `Some` ZERO is IGNORED: a zero bound would kill every
    /// declared pause command before it could start, refusing every declared
    /// pause, so the supervisor keeps the production cap and emits a
    /// diagnostic. NOT an operator config surface; this field stays
    /// `#[doc(hidden)]` so it never reads as one.
    #[doc(hidden)]
    pub pause_command_cap_override: Option<Duration>,
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: `DiagnosticSink` (a boxed writer) is not `Debug`, and a
        // sink must never be printed anyway (no-leak discipline).
        f.debug_struct("EngineOptions")
            .field("metering_proxy", &self.metering_proxy)
            .field("child_env_allowlist", &self.child_env_allowlist)
            .field("log_capture", &self.log_capture)
            .field("diagnostics", &self.diagnostics.as_ref().map(|_| "<sink>"))
            .field(
                "stable_run_window_override",
                &self.stable_run_window_override,
            )
            .field(
                "pause_command_cap_override",
                &self.pause_command_cap_override,
            )
            .finish()
    }
}

/// The story-17-4 capture-read guard (AI-18 — honest absence, never a
/// fabricated read), evaluated at the [`Engine`] facade because only the
/// supervisor knows the engine-wide capture mode. Returns the typed refusal
/// when the requested read cannot return what it promises under the active
/// mode; `None` means "proceed".
///
/// * [`LogCaptureMode::Off`] refuses EVERY capture read: nothing was ever
///   captured, so even an empty success would imply "captured, then
///   exhausted" — a lie by shape. The refusal names the mode and the
///   remediation (`--log-capture full|metadata`).
/// * [`LogCaptureMode::Metadata`] refuses the ATTRIBUTED `output.log` reads
///   only: that file holds hashed metadata records (size/sha256/attribution),
///   not attributable line content — parsing it as [`LogLine`]s would either
///   fail or mislead. The RAW reads proceed (the transient-while-running
///   window is real content; the post-stop unlink surfaces as the existing
///   honest missing-file semantics).
fn capture_read_refusal(
    mode: LogCaptureMode,
    name: &InstanceName,
    attributed: bool,
    registry: &Registry,
) -> Option<EngineError> {
    let path = if attributed {
        registry.attributed_output_log_path(name)
    } else {
        registry.agent_output_log_path(name)
    };
    let path = path.to_string_lossy().into_owned();
    match (mode, attributed) {
        (LogCaptureMode::Off, _) => Some(EngineError::Log {
            name: name.as_str().to_string(),
            path,
            detail: "log capture is off for this engine: no output was ever captured \
                     (fleet shows the `capture-off` gap); to capture output, restart \
                     `hekma serve` with --log-capture full (or metadata), or set \
                     EngineOptions::log_capture in the program that embeds the engine; \
                     `hekma agent` commands have no flag for it"
                .to_string(),
        }),
        (LogCaptureMode::Metadata, true) => Some(EngineError::Log {
            name: name.as_str().to_string(),
            path,
            detail: "this engine runs with --log-capture metadata: output.log holds hashed \
                     metadata records (timestamp/stream/attribution/byte_len/sha256), not \
                     attributable line content, for every Run THIS engine spawned; an \
                     ADOPTED instance's output.log was written under its original engine's \
                     mode, which this engine does not know, so its shape is not assumed \
                     here. Read the raw agent.log while the instance runs, or restart the \
                     serve surface with --log-capture full"
                .to_string(),
        }),
        _ => None,
    }
}

impl Engine {
    /// Open an engine rooted at an optional state-dir base.
    ///
    /// `base` is threaded straight into [`Registry::open`] (see its docs for the
    /// resolution order). Builds the multi-thread tokio runtime the blocking
    /// facade owns and an empty in-memory supervisor, then:
    /// 1. ADOPTS ORPHANS (story 1-6, AC-B): reconciles every write-ahead spawn
    ///    record against live processes — a live fingerprint match is re-held
    ///    under supervision, a non-match is reconciled to `failed`, so no agent
    ///    process is left unsupervised and no phantom `running` row survives.
    /// 2. Spawns the crash-detection reaper (story 1-6, AC-A): a tokio interval
    ///    task that periodically runs [`Supervisor::poll_once`] via
    ///    `spawn_blocking` and times the Restart Policy backoffs.
    pub fn open(base: Option<PathBuf>) -> Result<Self, RegistryError> {
        Self::open_inner(base, EngineOptions::default())
    }

    /// Open an engine with a host-provided diagnostic sink INSTALLED FROM THE
    /// FIRST MOMENT (story 10-2).
    ///
    /// Behaviorally [`Engine::open`] in every respect except one: the sink is
    /// installed BEFORE orphan adoption runs and BEFORE the crash-detection
    /// reaper starts, so even a diagnostic fired by the reaper for an adopted
    /// instance (an enforcement breadcrumb for a budget breach committed right
    /// after open) routes to the sink — no stderr write can slip through
    /// between open and a later install. Hosts that can pass the sink at open
    /// should; [`Engine::with_diagnostics`] exists for installing or rotating
    /// a sink later in the engine's life.
    ///
    /// See [`DiagnosticSink`](crate::DiagnosticSink) for the sink contract
    /// (what it receives, thread-safety, the no-re-entry rule, the one-way
    /// install) and docs/embedding.md for the host-facing guide.
    pub fn open_with_diagnostics(
        base: Option<PathBuf>,
        sink: DiagnosticSink,
    ) -> Result<Self, RegistryError> {
        Self::open_inner(
            base,
            EngineOptions {
                diagnostics: Some(sink),
                ..EngineOptions::default()
            },
        )
    }

    /// Open an engine with the story-17-2 Host options (the long-running
    /// Host's only open seam).
    ///
    /// Behaviorally [`Engine::open`] with the given
    /// [`EngineOptions`] applied from the FIRST moment: the fixed
    /// metering port and the child-environment allowlist are on the supervisor
    /// before orphan adoption runs (so an ADOPTED `engine-observed` instance
    /// re-binds the pinned port and adopted children already spawned with the
    /// clear-then-apply environment by their original engine keep their
    /// contract on any restart this engine performs), and the diagnostic sink
    /// — when given — is installed before any supervision work starts
    /// (identical to [`Engine::open_with_diagnostics`]).
    ///
    /// [`EngineOptions::default()`] is byte-identical to [`Engine::open`]:
    /// ephemeral listener ports, full environment inheritance, no sink.
    pub fn open_with_options(
        base: Option<PathBuf>,
        options: EngineOptions,
    ) -> Result<Self, RegistryError> {
        Self::open_inner(base, options)
    }

    /// The shared open path (story 10-2 + 17-2): `open` passes the default
    /// options (the diagnostics keep their default stderr behavior,
    /// byte-identical to the pre-sink engine), `open_with_diagnostics`
    /// installs the host's writer before any supervision work starts, and
    /// `open_with_options` threads the Host's serve options through.
    fn open_inner(base: Option<PathBuf>, options: EngineOptions) -> Result<Self, RegistryError> {
        let EngineOptions {
            metering_proxy,
            child_env_allowlist,
            log_capture,
            diagnostics,
            stable_run_window_override,
            pause_command_cap_override,
        } = options;
        let registry = Registry::open(base)?;
        let rt = Self::build_runtime()?;
        // Story 3-4: thread the engine runtime handle into the supervisor so an
        // `engine-observed` start can spawn its loopback forward listener's
        // accept loop on this runtime (the sync start path runs on the blocking
        // pool, where `Handle::current` is unavailable). A `Handle` spawns onto
        // its runtime from any thread, so this is sound. Story 7-2: keep a bus
        // clone OUTSIDE the supervisor mutex so `subscribe()` is lock-free (see
        // EngineInner::events). Story 10-2: install the host's diagnostic sink
        // (when given) BEFORE adoption + the reaper spawn, so no diagnostic can
        // fire before the sink is in place. Story 17-2: the Host's fixed
        // metering port + child-env allowlist are ALSO in place before
        // adoption — an adopted observed instance re-binds the pinned port on
        // this very open.
        let mut supervisor = Supervisor::with_runtime(rt.handle().clone());
        if let Some(sink) = diagnostics {
            supervisor.install_diagnostics(sink);
        }
        // Story 22-2: the custody cache — the one in-memory home for
        // custodied provider values — is created BEFORE the supervisor and
        // the proxy (both hold it), with its refresher on this runtime (a
        // rotated secrets entry is picked up within the documented bound).
        let custody_cache = Arc::new(crate::domain::custody::CustodyCache::new(
            registry.secrets_file(),
        ));
        custody_cache.install_diagnostics(supervisor.diagnostic_fn());
        supervisor.install_custody_cache(Arc::clone(&custody_cache));
        let _custody_refresher = custody_cache.start_refresher(
            rt.handle(),
            std::time::Duration::from_secs(crate::domain::custody::CUSTODY_REFRESH_SECS),
        );
        // Story 23-1: the shared Metering Proxy starts BEFORE adoption and the
        // reaper, so an adopted engine-observed agent's base URL is live on
        // this very open and the first snapshot it reads already carries
        // every durable `block` reason. A bind failure is surfaced, never a
        // port hop.
        let (proxy, proxy_error) = Self::start_metering_proxy(
            &rt,
            &registry,
            &supervisor,
            &metering_proxy,
            &custody_cache,
        );
        let proxy_url = proxy.as_ref().map(|proxy| proxy.info().url.clone());
        if let Some(error) = &proxy_error {
            supervisor.emit_open_note(error);
        }
        supervisor
            .set_serve_options(proxy_url, child_env_allowlist, log_capture)
            .map_err(|err| RegistryError::ServeOptionsOpenTimeOnly {
                detail: err.to_string(),
            })?;
        // Story 18-3: thread the TEST SCALING SEAM through — `None` (every
        // production caller) leaves the pinned 30 s window untouched.
        if let Some(window) = stable_run_window_override {
            supervisor.override_stable_run_window(window);
        }
        // Story 18-4: thread the pause-command cap's TEST SCALING SEAM
        // through the same way — `None` (every production caller) leaves the
        // pinned 30 s standalone cap untouched.
        if let Some(cap) = pause_command_cap_override {
            supervisor.override_pause_command_cap(cap);
        }
        // Story 21-1: anything the one-time v12 import could not carry into
        // state.db (a torn or corrupt legacy log line) is surfaced once, on
        // the engine's diagnostic surface, before supervision starts.
        for note in registry.take_open_notes() {
            supervisor.emit_open_note(&note);
        }
        let events = supervisor.event_bus();
        let run_snapshot = Arc::clone(registry.run_publisher());
        let credential_reader = Mutex::new(registry.open_credential_reader(false)?);
        let calls_dir = registry.calls_dir();
        let inner = Arc::new(EngineInner {
            metering_proxy: Mutex::new(proxy),
            metering_proxy_error: proxy_error,
            read_only: false,
            registry: Mutex::new(registry),
            supervisor: Mutex::new(supervisor),
            events,
            run_snapshot,
            credential_reader,
            calls_dir,
        });

        // (1) Orphan adoption on open (AC-B / AI-7 / AI-8). Reconcile BEFORE the
        // reaper starts so an adopted process is already held when the first poll
        // runs (and a phantom row is already `failed`).
        {
            let mut registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.adopt_orphans(&registry);
            // Story 21-3: publish what the open produced (the policy sweep's
            // Run events, the FR-21 upgrade's one event) on the bus — they
            // are already durable in the outbox; this makes them live too
            // for any subscriber seated before the engine was handed out.
            for (seq, event) in registry.take_open_events() {
                supervisor.publish(seq, event);
            }
        }

        // (2) Spawn the crash-detection reaper on the engine's runtime. It is
        // aborted on Engine::drop. Cloning the Arc<EngineInner> keeps the shared
        // state alive for the task without keeping the Engine itself alive.
        let reaper = Self::spawn_reaper(&rt, Arc::clone(&inner));

        Ok(Self {
            rt: Arc::new(rt),
            inner,
            reaper: Some(reaper),
        })
    }

    /// Open a READ-ONLY engine (story 21-4, AD-20's one data-path owner):
    /// the same facade over a `query_only` connection with a CLOSED store
    /// writer (every write answers a typed error, never a panic), an empty
    /// supervisor, NO orphan adoption and NO crash-detection reaper — so it
    /// never touches a process another engine supervises and never writes
    /// state.db.
    ///
    /// * A missing state.db reads as an EMPTY fleet; nothing is created
    ///   (not the state directory, not the database).
    /// * A state.db below the current schema answers
    ///   [`StoreError::NeedsMigration`](crate::ports::StoreError::NeedsMigration)
    ///   (wrapped in [`RegistryError::Store`]); the migration is a write, so
    ///   the caller lets a writer open first.
    /// * A live instance's current-Run totals come from the spawn record's
    ///   persisted Run id — the same id adoption resumes.
    ///
    /// This is the `hekma` CLI's read path (`list`, `show`, `usage`, `logs`,
    /// `config get`); every CLI write goes to the Service Host instead.
    pub fn open_read_only(base: Option<PathBuf>) -> Result<Self, RegistryError> {
        let registry = Registry::open_read_only(base)?;
        let rt = Self::build_runtime()?;
        let supervisor = Supervisor::with_runtime(rt.handle().clone());
        let events = supervisor.event_bus();
        let run_snapshot = Arc::clone(registry.run_publisher());
        let credential_reader = Mutex::new(registry.open_credential_reader(true)?);
        let calls_dir = registry.calls_dir();
        let inner = Arc::new(EngineInner {
            metering_proxy: Mutex::new(None),
            metering_proxy_error: None,
            read_only: true,
            registry: Mutex::new(registry),
            supervisor: Mutex::new(supervisor),
            events,
            run_snapshot,
            credential_reader,
            calls_dir,
        });
        Ok(Self {
            rt: Arc::new(rt),
            inner,
            reaper: None,
        })
    }

    /// Story 21-4: every mutating facade method on a READ-ONLY engine answers
    /// this typed refusal before doing anything — not only the `state.db`
    /// writes (which the closed writer refuses anyway) but the file writes
    /// beside them (a config layer, a snapshot, a memory directory, a spawn).
    fn refuse_if_read_only(&self) -> Result<(), crate::ports::StoreError> {
        if self.inner.read_only {
            Err(crate::store::read_only_refusal())
        } else {
            Ok(())
        }
    }

    /// The ONE runtime construction site (the embed-clean audit pins it):
    /// every engine — writing or read-only — owns its own multi-thread
    /// runtime, built here and dropped with the engine.
    fn build_runtime() -> Result<Runtime, RegistryError> {
        Runtime::new().map_err(|e| RegistryError::Io {
            name: "<engine-runtime>".to_string(),
            path: "<tokio-runtime>".to_string(),
            source: e,
        })
    }

    /// Spawn the crash-detection reaper task on `rt` and return its abort handle.
    ///
    /// The task ticks every [`CRASH_POLL_INTERVAL`], running the sync
    /// [`Supervisor::poll_once`] via `spawn_blocking` (rusqlite + syscalls off the
    /// async workers, AD-13). For each returned [`RestartPlan`] it spawns a
    /// DELAYED restart: sleep the backoff, then run [`Supervisor::restart`] via
    /// `spawn_blocking`. A restart whose instance was meanwhile stopped is a
    /// harmless no-op (the transition gate rejects a non-`failed` start — AC7).
    /// Start the shared Metering Proxy (story 23-1) when enabled: the port
    /// is the given one, else (sticky) the state directory's recorded one,
    /// else an OS pick — recorded when sticky. Returns the proxy, or the
    /// surfaced reason it is not running.
    fn start_metering_proxy(
        rt: &Runtime,
        registry: &Registry,
        supervisor: &Supervisor,
        options: &crate::metering::proxy::MeteringProxyOptions,
        custody: &Arc<crate::domain::custody::CustodyCache>,
    ) -> (
        Option<crate::metering::proxy::MeteringProxy>,
        Option<String>,
    ) {
        if !options.enabled {
            return (None, None);
        }
        let Some(writer) = registry.writer().handle() else {
            return (None, None);
        };
        let recorded = if options.sticky && options.port == 0 {
            registry.reader().proxy_port().ok().flatten()
        } else {
            None
        };
        let mut chosen = options.clone();
        if let Some(port) = recorded {
            chosen.port = port;
        }
        let diagnostics = supervisor.diagnostic_fn();
        let deps = crate::metering::proxy::ProxyDeps {
            snapshot: Arc::clone(registry.run_publisher()),
            writer,
            inbox: supervisor.proxy_inbox(),
            diagnostics,
            custody: Arc::clone(custody),
        };
        match crate::metering::proxy::MeteringProxy::start(rt.handle(), &chosen, deps) {
            Ok(proxy) => {
                if options.sticky && recorded != Some(proxy.info().port) {
                    let port = proxy.info().port;
                    if let Err(err) = registry
                        .writer()
                        .call(move |ctx| ctx.store().set_proxy_port(port))
                    {
                        supervisor.emit_open_note(&format!(
                            "the metering proxy port {port} could not be recorded for this \
                             state directory ({err}); the next start may pick another port"
                        ));
                    }
                }
                (Some(proxy), None)
            }
            Err(err) => {
                let port = chosen.port;
                (
                    None,
                    Some(format!(
                        "Port {port} is in use or not available ({err}), so the metering proxy \
                         is not running. Agents that meter through it cannot start until the \
                         port is free or you pass --metering-listen."
                    )),
                )
            }
        }
    }

    /// The running proxy's shared call state (the proxy seam tests).
    #[cfg(test)]
    pub(crate) fn proxy_shared_for_tests(
        &self,
    ) -> Option<Arc<crate::metering::proxy::forward::ProxyShared>> {
        self.inner
            .metering_proxy
            .lock()
            .expect("metering proxy mutex poisoned")
            .as_ref()
            .map(|proxy| Arc::clone(proxy.shared()))
    }

    /// Where the shared Metering Proxy listens (story 23-1), when it runs.
    pub fn metering_proxy(&self) -> Option<crate::metering::proxy::ProxyInfo> {
        self.inner
            .metering_proxy
            .lock()
            .expect("metering proxy mutex poisoned")
            .as_ref()
            .map(|proxy| proxy.info().clone())
    }

    /// The metering proxy port RECORDED for this state directory (story
    /// 23-1) — the sticky address a client points at even when this engine
    /// (a read-only CLI, say) runs no proxy itself. `None` before any host
    /// recorded one.
    pub fn recorded_metering_port(&self) -> Option<u16> {
        self.inner
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .reader()
            .proxy_port()
            .ok()
            .flatten()
    }

    /// Why the shared Metering Proxy is not running, when it was enabled
    /// and could not start (story 23-1).
    pub fn metering_proxy_error(&self) -> Option<String> {
        self.inner.metering_proxy_error.clone()
    }

    /// Stop the shared Metering Proxy gracefully (story 23-1): stop
    /// accepting, let in-flight calls finish within `grace`, abort the rest
    /// (recorded `shutdown`), then close its record pump. Idempotent.
    pub async fn stop_metering_proxy(&self, grace: Duration) {
        let proxy = self
            .inner
            .metering_proxy
            .lock()
            .expect("metering proxy mutex poisoned")
            .take();
        if let Some(mut proxy) = proxy {
            proxy.shutdown(grace).await;
        }
    }

    /// The binding a write job answers with (every binding write but
    /// delete returns one; a job that returned none is a store fault).
    fn bound_binding(
        outcome: crate::domain::runs::bindings::BindingOutcome,
        job: &str,
    ) -> Result<Binding, BindingError> {
        outcome.binding.ok_or_else(|| {
            BindingError::Store(crate::ports::StoreError::Backend(format!(
                "the {job} job returned no binding"
            )))
        })
    }

    /// Run one binding job and publish its committed events on the bus, in
    /// commit order (story 23-1). Every public binding write runs through
    /// this internal helper.
    fn binding_write(
        inner: &Arc<EngineInner>,
        op: impl FnOnce(
                &mut crate::domain::runs::bindings::BindingJob<'_, '_>,
                Option<RunId>,
            )
                -> Result<crate::domain::runs::bindings::BindingOutcome, BindingError>
            + Send
            + 'static,
    ) -> Result<crate::domain::runs::bindings::BindingOutcome, BindingError> {
        inner
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .run_binding(op)
            .inspect(|outcome| {
                let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
                supervisor.publish_run_events(outcome.events.clone());
            })
    }

    /// Create an endpoint binding (story 23-1, D3): the Operator surface the
    /// shared Metering Proxy routes on. One `binding` event.
    pub async fn create_binding(&self, spec: BindingSpec) -> Result<Binding, BindingError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::create(
                    job,
                    spec,
                    legacy.map(|run| run.to_string()),
                    &at,
                )
            })
        })
        .await
        .and_then(|outcome| Self::bound_binding(outcome, "create"))
    }

    /// Change a binding (story 23-1): a prefix or upstream change keeps its
    /// Runs; a label change re-resolves them in the same job. One event.
    pub async fn update_binding(
        &self,
        name: &str,
        patch: BindingPatch,
    ) -> Result<Binding, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::update(
                    job,
                    &name,
                    patch,
                    legacy.map(|run| run.to_string()),
                    &at,
                )
            })
        })
        .await
        .and_then(|outcome| Self::bound_binding(outcome, "update"))
    }

    /// Delete a binding (story 23-1): its Run and its split Runs end with
    /// cause `binding-removed`; further calls on its prefix answer no route.
    /// One event.
    pub async fn delete_binding(&self, name: &str) -> Result<(), BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, _legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::delete(job, &name, &at)
            })
            .map(|_| ())
        })
        .await
    }

    /// Add (or replace) one of a binding's reroute routes (story 23-1):
    /// stored and validated now, used by reroute when it ships. One event.
    pub async fn put_binding_route(
        &self,
        name: &str,
        route: RouteSpec,
    ) -> Result<Binding, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, _legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::put_route(job, &name, route, &at)
            })
        })
        .await
        .and_then(|outcome| Self::bound_binding(outcome, "route"))
    }

    /// Remove one of a binding's routes (story 23-1). One event.
    pub async fn remove_binding_route(
        &self,
        name: &str,
        route: &str,
    ) -> Result<Binding, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let route = route.to_string();
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, _legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::remove_route(job, &name, &route, &at)
            })
        })
        .await
        .and_then(|outcome| Self::bound_binding(outcome, "route"))
    }

    /// Issue (rotating any previous one) a binding credential (story 23-1,
    /// D3/P6-A): `hk_ann_` or `hk_pxy_`, shown ONCE — only its hash and
    /// token id are stored. Nothing accepts either credential in this
    /// release. One event.
    pub async fn issue_binding_credential(
        &self,
        name: &str,
        class: BindingCredentialClass,
    ) -> Result<IssuedBindingCredential, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            // Mint OUTSIDE the job (the cleartext never enters the writer);
            // a failure to record leaves a minted-but-unknown token, which
            // nothing accepts — harmless and never retried as a duplicate.
            let minted = crate::domain::runs::bindings::mint(class)?;
            let stub = minted.stub();
            let secret = minted.into_secret();
            Self::binding_write(&inner, move |job, _legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::issue_credential(job, &name, class, stub, &at)
                    .map(|(outcome, _token_id)| outcome)
            })
            .map(|_| IssuedBindingCredential {
                class,
                token_id: stub.hash.token_id(),
                token: secret,
            })
        })
        .await
    }

    /// Revoke a binding credential by class (story 23-1). One event.
    pub async fn revoke_binding_credential(
        &self,
        name: &str,
        class: BindingCredentialClass,
    ) -> Result<(), BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            Self::binding_write(&inner, move |job, _legacy| {
                let at = crate::time::now_rfc3339();
                crate::domain::runs::bindings::revoke_credential(job, &name, class, &at)
            })
            .map(|_| ())
        })
        .await
    }

    /// Read one binding (story 23-1): `None` when no binding has the name.
    pub async fn binding(&self, name: &str) -> Result<Option<Binding>, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            match registry.reader().binding(&name)? {
                Some(row) => crate::domain::runs::bindings::view(registry.reader(), row)
                    .map(Some)
                    .map_err(BindingError::Store),
                None => Ok(None),
            }
        })
        .await
    }

    /// Read every binding by name, after `after` (keyset), at most `limit`
    /// (story 23-1).
    pub async fn bindings(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Binding>, BindingError> {
        let inner = Arc::clone(&self.inner);
        let after = after.map(str::to_string);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            crate::domain::runs::bindings::list(registry.reader(), after.as_deref(), limit)
                .map_err(BindingError::Store)
        })
        .await
    }

    /// Read a binding's credentials (metadata only — token ids, classes,
    /// states; story 23-1: a token is never stored whole and never shown
    /// again).
    pub async fn binding_credentials(
        &self,
        name: &str,
    ) -> Result<Vec<BindingCredential>, BindingError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            crate::domain::runs::bindings::credentials(registry.reader(), &name)
        })
        .await
    }

    fn spawn_reaper(rt: &Runtime, inner: Arc<EngineInner>) -> tokio::task::AbortHandle {
        let handle = rt.spawn(async move {
            let mut ticker = tokio::time::interval(CRASH_POLL_INTERVAL);
            // Skip missed ticks rather than bursting after a slow poll.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let poll_inner = Arc::clone(&inner);
                let plans = tokio::task::spawn_blocking(move || {
                    let registry = poll_inner.registry.lock().expect("registry mutex poisoned");
                    let mut supervisor = poll_inner
                        .supervisor
                        .lock()
                        .expect("supervisor mutex poisoned");
                    supervisor.poll_once(&registry)
                })
                .await
                .unwrap_or_default();

                // Time each restart's backoff, then perform it. Spawned as
                // independent tasks so one instance's backoff does not delay
                // another's; each holds its own Arc<EngineInner> clone.
                for plan in plans {
                    let restart_inner = Arc::clone(&inner);
                    tokio::spawn(async move {
                        tokio::time::sleep(plan.delay).await;
                        let name = plan.name.as_str().to_string();
                        let _ = tokio::task::spawn_blocking(move || {
                            let registry = restart_inner
                                .registry
                                .lock()
                                .expect("registry mutex poisoned");
                            let mut supervisor = restart_inner
                                .supervisor
                                .lock()
                                .expect("supervisor mutex poisoned");
                            supervisor.restart(&registry, &name, plan.attempt, plan.delay)
                        })
                        .await;
                    });
                }
            }
        });
        handle.abort_handle()
    }

    /// A synchronous facade over the async API for non-async callers (`hekma`).
    ///
    /// Each [`Blocking`] method is `runtime.block_on(async_method(..))`. See the
    /// module docs for why `hekma` uses this instead of becoming an async binary.
    pub fn blocking(&self) -> Blocking<'_> {
        Blocking { engine: self }
    }

    /// Install (or replace) the host-provided diagnostic sink (story 10-2) on
    /// an already-open engine.
    ///
    /// The engine's two operational diagnostics — the DC-10 memory-delivery
    /// notice and the enforcement breadcrumb — then route to `sink` instead of
    /// stderr, receiving the exact bytes (same `[hekma] `-prefixed text, one
    /// `\n`-terminated line per diagnostic) stderr would have received. With
    /// no sink installed (the [`Engine::open`] default) the diagnostics keep
    /// their historical stderr behavior, byte-identical. See
    /// [`DiagnosticSink`](crate::DiagnosticSink) for the full contract.
    ///
    /// This is deliberately a SYNC method like [`Engine::subscribe`]: it takes
    /// the supervisor lock briefly (the sink is one field swap behind that
    /// mutex) and needs no blocking-pool trip — safe from any context.
    /// Installing replaces any earlier sink, so a host can rotate its writer
    /// mid-flight; the change takes effect for every later diagnostic (the
    /// outgoing writer is flushed before the swap, so a buffering writer does
    /// not silently lose its bytes across the rotation).
    ///
    /// ## Installing is ONE-WAY
    ///
    /// There is no uninstall back to the stderr default: this method (and
    /// every install path) REPLACES the current sink and never removes one.
    /// A host that wants the default stderr behavior back re-opens the
    /// engine ([`Engine::open`]), or installs its own writer that emits to
    /// the process's stderr — deliberately so: a host that installed a sink
    /// almost certainly does not own its stderr, and a silent revert to
    /// stderr would be the wrong fallback. For a sink that must also catch
    /// reaper-fired diagnostics for adopted instances in the open-to-install
    /// window, prefer [`Engine::open_with_diagnostics`].
    pub fn with_diagnostics(&self, sink: DiagnosticSink) {
        self.inner
            .supervisor
            .lock()
            .expect("supervisor mutex poisoned")
            .install_diagnostics(sink);
    }

    /// Subscribe to the engine's committed-event stream (story 7-2, FR-33).
    ///
    /// Returns a fresh [`broadcast::Receiver`](crate::broadcast::Receiver) over
    /// the [`EngineEvent`] wrapper (the versioned AD-14 payloads verbatim); it
    /// observes only events committed AFTER this call, in commit order —
    /// per-instance FIFO, exactly what the durable logs record. The full
    /// contract (committed-truth guarantee,
    /// [`EVENT_BUS_CAPACITY`](crate::EVENT_BUS_CAPACITY) bound, the
    /// `Lagged` slow-subscriber policy, publish-cannot-fail-supervision) is
    /// documented on the `domain::bus` module.
    ///
    /// This is deliberately a SYNC method: the bus handle lives outside the
    /// supervisor mutex, so handing out a receiver takes no lock and no
    /// blocking-pool trip — it is safe from any context, async or sync, at any
    /// point in the engine's life. There is NO await-related correctness rule
    /// here; the ONLY ordering requirement is to subscribe BEFORE the events
    /// you care about are committed (a receiver sees only later commits —
    /// earlier ones are readable via the query APIs). An async Host drives
    /// `rx.recv().await` on its own tasks; sync consumers use
    /// [`Blocking::subscribe`] instead (its [`EventSubscription`] owns a
    /// blocking `recv` bridged through the engine runtime).
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.inner.events.subscribe()
    }

    /// The SEQUENCED subscription (story 21-5): `(outbox seq, EngineEvent)`
    /// pairs in commit order — the SSE path's stream, whose frames carry
    /// the seq as their `id:` so a reconnect can replay the outbox from
    /// `Last-Event-ID` and hand over to live delivery with seq-dedup at
    /// the seam. Same contract as [`Engine::subscribe`] (bounded at
    /// [`EVENT_BUS_CAPACITY`], `Lagged` on overrun); the seq comes from the
    /// store writer's ack, so it is the durable outbox position of the
    /// event — what `export_events` serves.
    pub fn subscribe_sequenced(&self) -> broadcast::Receiver<(u64, EngineEvent)> {
        self.inner.events.subscribe_sequenced()
    }

    /// The engine-computed Agent Home path for `name` (display helper).
    ///
    /// Pure path arithmetic — no I/O, no blocking pool needed.
    pub fn agent_home(&self, name: &InstanceName) -> PathBuf {
        self.inner
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .agent_home(name)
    }

    /// Register a new Agent Instance of a native `kind`.
    ///
    /// Async wrapper over the blocking [`Registry::register`]; the FS+SQLite work
    /// runs on the blocking pool.
    pub async fn register(&self, name: &str, kind: &str) -> Result<AgentInstance, RegistryError> {
        self.refuse_if_read_only().map_err(RegistryError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let kind = kind.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .register(&name, &kind)
        })
        .await
    }

    /// Register a new Agent Instance, resolving `reference` to an adapter first.
    pub async fn register_with_adapter(
        &self,
        name: &str,
        reference: &AdapterRef,
    ) -> Result<AgentInstance, RegistryError> {
        self.refuse_if_read_only().map_err(RegistryError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let reference = reference.clone();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .register_with_adapter(&name, &reference)
        })
        .await
    }

    /// Remove an Agent Instance, honoring the retain/delete disposition (AC4)
    /// and the running-guard (AC5).
    ///
    /// ## Live/adopted-instance teardown — remove never leaves an orphan (AI-11)
    ///
    /// SEMANTICS DECISION (Away-Mode, conservative): `remove` must NEVER leave a
    /// live, unsupervised agent process behind — for BOTH plain and `--force`
    /// remove. If the supervisor still holds a handle for this instance
    /// (running/paused, including a process ADOPTED from a prior engine),
    /// [`Engine::remove`] STOPS it first — reusing the ordinary
    /// [`Supervisor::stop`] path, which terminates the whole process group/job,
    /// drops the handle, and CLEARS the write-ahead spawn record — and only THEN
    /// deletes the row. This closes the NFR-1 counterexample the story-1-2 `remove`
    /// docstring deferred to 1.4/1.6: without it, `hekma agent remove <live> --force`
    /// deleted the record while the process kept running, and because the
    /// write-ahead record was gone a later engine crash left a TRUE unsupervised
    /// orphan no future engine could adopt.
    ///
    /// `--force` keeps its EXISTING meaning — it governs whether a `running` row
    /// may be removed at all (the [`RegistryError::RunningRequiresForce`] guard),
    /// NOT whether the process is torn down. So the teardown runs whenever we are
    /// actually going to delete the row: `force`, or a non-`running` live state
    /// (a `paused` instance is removable without `--force`, yet may still hold a
    /// live — SIGSTOP'd — process, so it is stopped too). A `running` instance
    /// without `--force` is rejected by the registry guard BEFORE any teardown, so
    /// a refused remove never touches the process.
    ///
    /// PARTIAL FAILURE: the teardown and the row deletion are two steps. If the
    /// [`Supervisor::stop`] succeeds (process killed, write-ahead record cleared)
    /// but the subsequent `registry.remove` then fails, the instance is left
    /// `stopped` with NO live process and NO spawn record — a coherent, safe state
    /// (never an orphan): the operator can simply re-run `remove` to delete the now
    /// already-stopped row. (A teardown-step failure aborts BEFORE any deletion —
    /// see the teardown call — so that case leaves the instance exactly as it was.)
    pub async fn remove(
        &self,
        name: &str,
        disposition: RemoveDisposition,
        force: bool,
    ) -> Result<(), RegistryError> {
        self.refuse_if_read_only().map_err(RegistryError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            // (1) Stop a live/adopted instance FIRST so no unsupervised process is
            // ever orphaned (AI-11). Only when we will actually delete the row —
            // i.e. `force`, or a live state other than `running` (a `running`
            // instance without `--force` is refused by the registry guard below, so
            // we must NOT tear its process down for a remove that gets rejected).
            if let Ok(iname) = InstanceName::new(&name) {
                if let Ok(instance) = registry.lookup(&iname) {
                    let live = matches!(
                        instance.state,
                        LifecycleState::Running | LifecycleState::Paused
                    );
                    let will_delete = force || instance.state.is_removable_without_force();
                    if live && will_delete {
                        // Reuse the ordinary stop path: terminates the whole
                        // group/job, drops the handle, and clears the write-ahead
                        // record. A teardown failure aborts the remove (surfaced as
                        // a RegistryError) rather than deleting the row and leaking
                        // the process.
                        supervisor
                            .stop(&registry, &name, None)
                            .map_err(stop_error_to_registry)?;
                    }
                }
            }
            // (2) Delete the row (+ handle the Agent Home per disposition). The
            // authoritative name validation and the `running` running-guard live
            // here, so a malformed name / not-found / running-without-force is
            // reported exactly as before.
            registry.remove(&name, disposition, force)
        })
        .await
    }

    /// List the whole Fleet, ordered by name.
    pub async fn list(&self) -> Result<Vec<AgentInstance>, RegistryError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .list()
        })
        .await
    }

    /// The whole Fleet as serializable [`FleetEntry`] rows (story 1-7, FR-4 /
    /// AD-14). This is the read `hekma agent list [--json]` uses.
    ///
    /// Composes the existing reads in ONE blocking pass: [`Registry::list`]
    /// (ordered by name) plus, per instance, the same runtime status
    /// ([`Engine::instance_status`]) the CLI already surfaces — the restart
    /// count/policy + (for a `failed` instance) the last-known cause — plus the
    /// story-3-1 metering surface: the real Usage-Ledger token totals + the active
    /// Metering Source (AC-C/AC11), the story-3-2 real TOKEN `budget` (the configured
    /// ceilings + Breach Action + remaining, or an honest absent budget when none is
    /// configured), and — when a Rate is configured — the story-3-3 derived dollar
    /// cost + Cost Cap + dollars-remaining. Reading live persisted state on every call
    /// is what makes the listing ≤2s fresh (AC6): there is no cache, so any committed
    /// transition is reflected on the next `fleet()` (a single DB read, far under 2s).
    ///
    /// This returns the per-instance rows; the Fleet-WIDE aggregate
    /// ([`FleetTotals`](crate::domain::FleetTotals), story 3-5) is computed PURELY from
    /// these rows by [`FleetListing::new`](crate::domain::FleetListing::new) — one read
    /// pass, no second ledger query. Recorded surface decision (AC-A/AD-2): the
    /// aggregation RULE lives in the engine `domain`
    /// ([`FleetTotals::from_entries`](crate::domain::FleetTotals::from_entries)), and
    /// the CLI triggers it over these engine-provided rows via `FleetListing::new`, so
    /// `hekma` stays a thin renderer that never sums the ledger or derives dollars itself.
    ///
    /// The current-Run token totals come from the supervisor's live Run id (held in
    /// memory for a running instance), so `fleet()` locks the supervisor too — a
    /// non-running instance simply has no current Run (current-run totals are zero).
    ///
    /// A per-instance status read-back failure DEGRADES that entry's runtime
    /// fields (count `0`, policy default, no cause) rather than failing the whole
    /// Fleet — mirroring the 1-6 `list` fallback — so one bad row never hides the
    /// rest. The top-level `list` read is the only hard failure.
    pub async fn fleet(&self) -> Result<Vec<FleetEntry>, RegistryError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            let instances = registry.list()?;
            // AI-16: ONE batched spawn-record read while the locks are held,
            // keyed by name and threaded into `fleet_entry_for` — replacing the
            // N+1 (one `spawn_record` query per instance). Scoped to spawn
            // records ONLY: the other per-instance reads (usage totals /
            // effective config / cost totals) are deliberately unchanged. A
            // failed BATCH read falls back to the old per-instance reads (the
            // `None` arm below) so a transient failure degrades ONE row, not
            // the whole Fleet's runtime fields at once — the 1-6 `list`
            // fallback, preserved row-by-row.
            let records = match registry.list_spawn_records() {
                Ok(records) => Some(
                    records
                        .into_iter()
                        .map(|record| (record.name.clone(), record))
                        .collect::<HashMap<InstanceName, SpawnRecord>>(),
                ),
                Err(_) => None,
            };
            // Story 21-3 (the perf-budgets ratchet): ONE batched read of
            // every instance's Policy-resolution facts (the open
            // process-owning Runs' rows, labels and limits, and the
            // kept-on-pause markers), in the same AI-16 shape as the spawn
            // records above. A failed batch (`None`) falls back to each
            // row's own read, so it degrades one row at a time.
            let resolution_facts = registry.fleet_resolution_facts();
            let entries = instances
                .into_iter()
                .map(|instance| {
                    Self::fleet_entry_for(
                        &registry,
                        &supervisor,
                        records.as_ref(),
                        resolution_facts.as_ref(),
                        instance,
                        inner.read_only,
                    )
                })
                .collect();
            Ok(entries)
        })
        .await
    }

    /// The ONE Fleet entry for `name` (AI-15) — the read `hekma agent show <name>
    /// --json` uses. Locks ONCE, resolves the instance by name through the
    /// registry's keyed lookup (never a whole-Fleet scan), and composes the row
    /// through the SAME [`Engine::fleet_entry_for`] `list --json` uses, so the
    /// `show` object stays byte-identical to that instance's `list` row.
    ///
    /// Errors surface DIRECTLY — an unknown name is the registry's
    /// [`RegistryError::NotFound`] and a malformed name its `InvalidName` — the
    /// same diagnostics the human `show` path raises, instead of the old
    /// scan-then-synthesize degradation that inherited every degradation of a
    /// full Fleet read.
    pub async fn fleet_entry(&self, name: &str) -> Result<FleetEntry, RegistryError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            let iname = InstanceName::new(&name).map_err(|reason| RegistryError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            let instance = registry.lookup(&iname)?;
            // The single instance's spawn record (O(1)) — the same data the
            // batched map feeds `fleet_entry_for` for a whole-Fleet read.
            let records: HashMap<InstanceName, SpawnRecord> = registry
                .spawn_record(&iname)?
                .map(|record| (record.name.clone(), record))
                .into_iter()
                .collect();
            Ok(Self::fleet_entry_for(
                &registry,
                &supervisor,
                Some(&records),
                None,
                instance,
                inner.read_only,
            ))
        })
        .await
    }

    /// Build a [`FleetEntry`] from an already-listed instance by reading its
    /// runtime status through the SAME registry lock (one read pass, no
    /// re-entrancy). A status read-back failure degrades the runtime fields to
    /// their defaults rather than failing the whole Fleet (the 1-6 `list`
    /// fallback). `budget` (story 3-2), `usage` + `metering_source` (story 3-1) are
    /// all real; `budget` is `None` only when no budget is configured.
    fn fleet_entry_for(
        registry: &Registry,
        supervisor: &Supervisor,
        records: Option<&HashMap<InstanceName, SpawnRecord>>,
        resolution_facts: Option<&crate::domain::registry_policy::FleetResolutionFacts>,
        instance: AgentInstance,
        read_only: bool,
    ) -> FleetEntry {
        // The write-ahead spawn record for the restart count/policy + cause.
        // `Some(map)` is the AI-16 batched read (no per-instance store query on
        // the happy path; a record absent from a SUCCESSFUL batch is a
        // never-started instance — defaults, no re-query). `None` means the
        // batch read itself failed: fall back to this instance's own read so a
        // transient batch failure degrades ONE row instead of blanking the
        // whole Fleet's runtime fields. Either way an errored read degrades to
        // defaults — the 1-6 `list` fallback.
        let record = match records {
            Some(map) => map.get(&instance.name).cloned(),
            None => registry.spawn_record(&instance.name).unwrap_or(None),
        };
        // Story 15-2 (Fibery 176): the supervision indicator, composed from
        // the spawn record ALREADY in hand (the same `record.detach` source
        // adoption reads — no new read). A pid-0 record is a policy/settle
        // SEED, not supervision: a never-started or stopped instance reads
        // `None` (the honest absence a human cell renders as `—`), so a
        // stopped row can never imply supervised. Computed BEFORE the
        // `failed_cause` fold below consumes the record.
        // Story 21-4: a READ-ONLY engine reconciles nothing, so a
        // `running`/`paused` row whose recorded process is provably gone is
        // SHOWN as what the next write's reconciliation records (`failed`,
        // no supervision, the cause naming the pid), with the stored state
        // kept in `recorded_state` — written nowhere.
        let gone = read_only
            && matches!(
                instance.state,
                LifecycleState::Running | LifecycleState::Paused
            )
            && record
                .as_ref()
                .is_some_and(|record| supervisor.record_process_gone(record));
        let recorded_state = gone.then_some(instance.state);
        let mut instance = instance;
        if gone {
            instance.state = LifecycleState::Failed;
        }
        let supervision = record.as_ref().filter(|_| !gone).and_then(|record| {
            (record.fingerprint.pid != 0).then_some(if record.detach {
                crate::domain::Supervision::Detached
            } else {
                crate::domain::Supervision::Session
            })
        });
        // Story 21-4: a READ-ONLY engine's supervisor is empty, so a live
        // instance's current Run id comes from its persisted spawn record —
        // the id the owning host's supervisor holds (and adoption resumes).
        let persisted_run_id = if read_only
            && matches!(
                instance.state,
                LifecycleState::Starting
                    | LifecycleState::Running
                    | LifecycleState::Paused
                    | LifecycleState::Stopping
            ) {
            record.as_ref().and_then(|r| r.run_id.clone())
        } else {
            None
        };
        let restart_policy = record
            .as_ref()
            .map(|r| r.restart_policy)
            .unwrap_or_default();
        let restart_count = record.as_ref().map(|r| r.restart_count).unwrap_or(0);
        let gone_pid = record.as_ref().map(|r| r.fingerprint.pid);
        let failed_cause = record.and_then(|r| r.last_known_cause);
        let failed_cause = if gone {
            Some(gone_cause(gone_pid.unwrap_or(0), failed_cause.as_deref()))
        } else {
            failed_cause
        };
        let failed_cause = failed_cause.or_else(|| {
            // No record cause. For a `failed` instance, fall back to the last
            // event-log cause (a launch-error / cleared-terminal crash) — the same
            // fallback `instance_status` uses (AC9).
            if instance.state != LifecycleState::Failed {
                return None;
            }
            let events =
                Supervisor::read_events(registry, instance.name.as_str()).unwrap_or_default();
            events
                .iter()
                .rev()
                .find(|e| e.new_state == LifecycleState::Failed)
                .and_then(|e| failed_cause_detail(&e.cause))
        });
        // Story 3-1 metering surface (AC-C/AC11). The CUMULATIVE token totals come
        // straight from the Usage Ledger (sum over all Runs); the CURRENT-RUN totals
        // are scoped to the supervisor's live Run id (zero when not running / no run
        // id). A read-back failure degrades to zero totals (like the runtime fields),
        // never failing the whole Fleet. The totals equal the ledger exactly (FR-22).
        // Story 14-6 honesty split: a DEGRADED ledger read defaults to
        // `UsageTotals::default()` whose cached rollup is the UNKNOWN absence
        // (`None` — never a fabricated zero), while NO current Run at all is the
        // truthful `zero()` (known-zero cached — there is nothing to be unknown
        // about a Run that does not exist).
        let cumulative = registry.usage_totals(&instance.name).unwrap_or_default();
        let current_run_id = supervisor
            .current_run_id(&instance.name)
            .or(persisted_run_id);
        let current_run = match current_run_id.as_ref() {
            Some(run_id) => registry
                .run_usage_totals(&instance.name, run_id)
                .unwrap_or_default(),
            None => crate::domain::UsageTotals::zero(),
        };
        // Story 3-3 dollar surface + story 21-3 (the resolved limits): the
        // CURRENT Rate/cap/action resolve ONCE through the ONE Policy
        // resolver (`Registry::resolved_limits` — explicit config, the
        // kept-on-pause marker, matching Policies, the default Policy), so
        // the Fleet view matches what enforcement sees, names the source of
        // every limit, and reads `warn` for a fresh ungoverned instance. A
        // degraded read fails closed (honest absence), never failing the
        // Fleet. Story 14-3 (T2): the SAME resolved effective config decides
        // the acp kind's ACTIVE metering source (the
        // `metering.upstream_base_url` opt-in), so the surfaced source matches
        // what the start seam did — the snapshot alone would always read
        // self-reported for the acp builtin.
        let effective_opt = registry
            .effective_config(&instance.name, crate::domain::ConfigLayer::empty())
            .ok();
        let (resolution, breach_default_upgraded) = registry.resolved_limits_from_batch(
            &instance.name,
            current_run_id.as_ref(),
            effective_opt.as_ref(),
            resolution_facts,
        );
        let rate = resolution.rate;
        let cost_cap = resolution.dollars;
        let action = resolution.effective_action;
        let is_acp = crate::acp::is_acp_kind(&instance.kind);
        let metering_source = if is_acp {
            // The active source for the acp kind resolves from the effective
            // config (start + Fleet read agree); a degraded read stays the
            // honest "unknown" fallback (the same posture as the snapshot
            // read below).
            effective_opt
                .as_ref()
                .map(|effective| crate::acp::resolve_acp_metering_source(effective).to_string())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            registry
                .metering_source(&instance.name)
                .unwrap_or_else(|_| "unknown".to_string())
        };
        // The DERIVED dollar cost, present ONLY when a Rate is configured (AC-B: no
        // Rate ⇒ NO dollar figure, never a fabricated `$0.00`). Each row is priced at
        // its own persisted Rate (no retro-repricing), so these equal the ledger
        // exactly (FR-22). v1 the estimate label is always `estimated`.
        let (cumulative_cost, current_run_cost) = if rate.is_some() {
            let cum = registry.cost_totals(&instance.name).unwrap_or_default();
            let run = current_run_id
                .as_ref()
                .and_then(|run_id| registry.run_cost_totals(&instance.name, run_id).ok())
                .unwrap_or_default();
            (cum, run)
        } else {
            (crate::domain::Micros::ZERO, crate::domain::Micros::ZERO)
        };
        let label = crate::domain::EstimateLabel::Estimated;
        let usage = {
            let base = crate::domain::UsageView::new(cumulative, current_run);
            if rate.is_some() {
                base.with_dollars(cumulative_cost, current_run_cost, label)
            } else {
                base
            }
        };
        // Story 3-2 budget surface + story 3-3 dollar cap surface (AC9/AC10),
        // resolved through the ONE Policy resolver since story 21-3 (the
        // same `resolution` the Rate/action above used): the CURRENT
        // resolved Token Budget + Cost Cap + Breach Action + remaining per
        // scope. TOKEN remaining is `usage` tokens; DOLLAR cap/remaining are
        // present ONLY when a Rate exists (a cap with no Rate is inert —
        // AC-B). An instance with NEITHER a budget nor an enforceable cap
        // surfaces an honest absent budget (never a fabricated ceiling).
        let budget = effective_opt.as_ref().and_then(|_effective| {
            crate::domain::BudgetView::from_budget_and_cost(
                &resolution.tokens,
                &cost_cap,
                action,
                current_run.total_tokens(),
                cumulative.total_tokens(),
                rate.is_some(),
                current_run_cost,
                cumulative_cost,
                label,
                &resolution.sources.budget_policy_names(),
            )
        });
        // The active Metering Source is visible in Fleet detail (AC-C), resolved
        // per kind above (the acp kind's active source; every other kind's
        // persisted snapshot). A degraded read falls back to the honest
        // "unknown" marker rather than fabricating a source.
        // Story 14-3 (T1): the LATEST context-usage figure a live acp
        // connection has reported — CONTEXT-grain, surfaced under its own
        // `acp_context_usage` field, never inside the billing `usage` view.
        let acp_context_usage = supervisor.acp_context_usage(&instance.name).map(|usage| {
            crate::domain::AcpContextUsageView {
                used: usage.used,
                size: usage.size,
                cost: usage
                    .cost
                    .map(|(amount, currency)| crate::domain::AcpContextCostView {
                        amount,
                        currency,
                    }),
            }
        });
        // Story 14-3: the honest usage-gap notice — present ONLY for an acp
        // instance whose ledger holds NO billing-grade usage at all (the
        // cumulative total is the every-Run truth; a gap is about billing
        // availability, not just the current Run). The notice names the tiers
        // attempted (observed configured-ness from the ACTIVE source; the
        // sentinel lines-seen fact from live supervision — a non-running
        // instance with a zero ledger has honestly seen none) plus the
        // context-usage state. NEVER fabricated; every other kind (and a
        // usage-carrying acp instance) omits the field.
        let usage_gap = if is_acp && cumulative.total_tokens() == 0 {
            Some(crate::domain::UsageGapNotice::acp_with_capture(
                metering_source == "engine-observed",
                supervisor.sentinel_lines_seen(&instance.name),
                acp_context_usage.is_some(),
                // Story 17-4: an OFF engine never captured a log, so the
                // sentinel tier reports the `capture-off` sentinel — never a
                // "no-lines-seen" that implies a drain which never ran.
                supervisor.log_capture_mode() == LogCaptureMode::Off,
            ))
        } else {
            None
        };
        // Story 21-3: the FR-21 "kept on pause by upgrade" marker rides the
        // row (rendered as the note; the explicit-key write clears it). It
        // was read with the resolution above, in the same snapshot.
        FleetEntry {
            name: instance.name,
            kind: instance.kind,
            state: instance.state,
            restart_count,
            restart_policy,
            failed_cause,
            // `budget` is REAL for TOKENS (story 3-2): the configured ceilings +
            // Breach Action + remaining tokens per scope, PLUS the dollar Cost Cap +
            // dollars-remaining when a Rate is configured (story 3-3); or `None` when no
            // budget is configured (an honest absence, never a fabricated `0`).
            budget,
            // `usage` is REAL: the Usage-Ledger token totals (story 3-1) + the derived
            // dollar cost when a Rate is configured (story 3-3). `metering_source` is
            // surfaced too. The Fleet-WIDE sum across these rows is `FleetTotals`
            // (story 3-5), composed by the CLI via `FleetListing::new` (AD-2 — the
            // aggregation rule is engine-domain; `hekma` triggers it over these rows).
            usage,
            metering_source,
            acp_context_usage,
            usage_gap,
            // Story 15-2: detached vs session-bound (from the spawn record;
            // `None` — omitted — for a stopped / never-started instance).
            supervision,
            recorded_state,
            breach_default_upgraded,
            agent_home: instance.agent_home,
        }
    }

    /// The effective (current-OS) Capability Declaration for a registered
    /// instance (AC1 "visible for the instance"). `hekma agent show` renders this.
    pub async fn effective_capabilities(
        &self,
        name: &str,
    ) -> Result<EffectiveCapabilities, RegistryError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .effective_capabilities(&name)
        })
        .await
    }

    /// Start a registered Agent Instance (AC1/AC2).
    ///
    /// Drives the supervisor: resolve the launch spec, spawn via the per-OS
    /// [`ProcessBackend`](crate::ports::ProcessBackend), transition
    /// `registered/stopped → starting → running` on success or `starting →
    /// failed` on a launch error (diagnostic preserved, no zombie). Returns the
    /// instance in its new state.
    pub async fn start(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.start(&registry, &name)
        })
        .await
    }

    /// Start a registered Agent Instance DETACHED (story 12-1): the child's
    /// handle is disarmed at spawn so it survives THIS engine session's exit;
    /// the next engine open re-adopts it via the existing fingerprint path.
    /// An `engine-observed` instance is refused before any side effect
    /// ([`EngineError::DetachRefused`]). The caller owes the operator the
    /// honest enforcement-window statement (no crash detection / budget
    /// enforcement / event delivery between commands) — the `hekma` CLI states
    /// it on `--help` and its stderr notice.
    pub async fn start_detached(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.start_detached(&registry, &name)
        })
        .await
    }

    /// Stop a running Agent Instance (AC3/AC4).
    ///
    /// Transitions `running → stopping`, requests graceful shutdown via the
    /// backend, escalates to a forced kill after `window` (default 30s) if the
    /// process has not exited, records the escalation in the instance log, then
    /// `stopping → stopped`. No process of the instance survives (the whole
    /// group/job is killed).
    pub async fn stop(
        &self,
        name: &str,
        window: Option<Duration>,
    ) -> Result<AgentInstance, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.stop(&registry, &name, window)
        })
        .await
    }

    /// Stop EVERY live agent through the bounded stop ladder — the shutdown
    /// drain (issue #328 item 3's embedder facade).
    ///
    /// This is the graceful half of the embedder contract: dropping an
    /// [`Engine`] kills its attached agents outright (the kill-on-drop
    /// teardown), so an embedder that wants graceful stops calls this FIRST
    /// and drops the engine only after it settles. [`Engine::stop`] is the
    /// per-agent ladder; this runs it for the whole fleet:
    ///
    /// * every `running` AND `paused` row is stopped through the same
    ///   bounded ladder (a paused row is resumed first so the graceful
    ///   signal can be delivered — never the raw kill-on-drop teardown),
    ///   one agent at a time in fleet order (the ladders serialize under
    ///   the supervision lock regardless — worst case Σ(window) over the
    ///   fleet);
    /// * the fleet is re-enumerated once afterwards and any newcomer (a
    ///   concurrent `start`, a restart policy firing mid-drain) gets the
    ///   ladder too — capped at one extra pass;
    /// * anything still alive after both passes is surfaced honestly: it
    ///   will be terminated by the engine `Drop` teardown WITHOUT the
    ///   bounded ladder, and the roll-up says exactly that.
    ///
    /// The drain never aborts on a per-instance failure — one un-stoppable
    /// agent must not strand the others' stops — and every per-instance
    /// outcome ([`DrainOutcome`]) is returned. When `notes` carries a
    /// [`DiagnosticSink`], each step and outcome is ALSO emitted as a
    /// `[hekma] `-prefixed notice line — exactly the stream `hekma serve`
    /// prints at shutdown (the host's drain delegates here); pass `None` for
    /// return-values-only. A drain stops every live row this engine can see
    /// (adopted detached rows included — the same rows `serve` drains); the
    /// detached-survival contract governs what happens when an engine dies
    /// WITHOUT a drain (a crash — the next engine adopts the survivors).
    pub async fn shutdown(
        &self,
        window: Duration,
        notes: Option<&DiagnosticSink>,
    ) -> Vec<DrainOutcome> {
        // Story 23-1: the proxy stops first (in-flight calls get the grace),
        // so its records flush before the drain stops the agents.
        self.stop_metering_proxy(window).await;
        let notes = crate::shutdown::Notes::over(notes);
        crate::shutdown::drain_and_stop(self, window, &notes).await
    }

    /// Epic 20-1 coverage seam (cfg(test) only — the established
    /// fault-injection idiom, see [`Supervisor`]'s `stop_fault_pids` field
    /// docs): arm the stop fault for a HELD instance's pid, so the shutdown
    /// drain's failed-stop arms (the honest `Failed` outcome against a row
    /// the fleet still reports) are stageable hermetically through a real
    /// `Engine`. No-op when no live handle is held under `name`. Lib-test
    /// only; production binaries do not even compile it.
    #[cfg(test)]
    pub(crate) fn arm_test_stop_fault(&self, name: &str) {
        use crate::domain::StopFault;
        let inner = Arc::clone(&self.inner);
        let name = InstanceName::new(name).expect("test instance name");
        let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
        if let Some(pid) = supervisor.held_pid(&name) {
            supervisor.arm_stop_fault(pid, StopFault::Fail);
        }
    }

    /// Pause a running Agent Instance with honest, per-OS semantics (story 1-5,
    /// AC1/AC2/AC3). Drives the supervisor's three-level dispatch on the effective
    /// (current-OS) pause `SupportLevel`: guaranteed → real SIGSTOP suspension +
    /// `running→paused`; best-effort → `running→paused` with a visible
    /// `pause-best-effort` qualifier in the transition event; unsupported → fail
    /// fast ([`EngineError::CapabilityUnsupported`]) with no state change. Returns
    /// the instance in its new state.
    pub async fn pause(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.pause(&registry, &name)
        })
        .await
    }

    /// Resume a paused Agent Instance (story 1-5, AC1/AC2). The symmetric
    /// counterpart of [`Engine::pause`]: guaranteed → SIGCONT + `paused→running`;
    /// best-effort → `paused→running` with a `resume-best-effort` qualifier.
    pub async fn resume(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.resume(&registry, &name)
        })
        .await
    }

    /// The DRAIN composite (story 18-4): pause the running agent via its
    /// adapter's declared `[lifecycle.pause]` COMMAND (never a signal,
    /// OS-independent), wait — BOUNDED — for the declared idle signal, then
    /// stop through the ordinary ladder. A drain that outlives its bound
    /// STILL stops: both outcomes are a success at the type level (see
    /// [`DrainReport`]), and `outcome.completed == false` is the caller's
    /// honest timed-out signal. The bound is the three-tier rule: the
    /// caller's `timeout_override` (validated `1..=600_000ms` — zero/past-ceiling
    /// refuses with [`EngineError::DrainBoundInvalid`]), else the manifest
    /// `[drain].timeout_ms`, else the engine's 30 s default.
    pub async fn drain(
        &self,
        name: &str,
        timeout_override: Option<Duration>,
    ) -> Result<DrainReport, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor
                .drain(&registry, &name, timeout_override)
                .map(|(instance, outcome)| DrainReport { instance, outcome })
        })
        .await
    }

    /// Send text input to a running Agent Instance's native input channel
    /// (story 4.1, FR-24, spine AD-12). Drives
    /// [`Supervisor::send_input`](crate::domain::Supervisor::send_input)'s
    /// dispatch: `NotRunning` if the instance is not
    /// [`LifecycleState`](crate::domain::LifecycleState)`::Running`;
    /// [`EngineError::CapabilityUnsupported`] if the effective
    /// `Capability::Interaction` level is `unsupported` on this OS (fails
    /// fast, no I/O); [`EngineError::InteractionUnavailable`] if the instance
    /// is running but this engine session holds no live stdin pipe for it
    /// (e.g. an ADOPTED instance — see the method's docs); otherwise appends
    /// a trailing `\n` if absent and writes the bytes to the child's stdin.
    /// This is the FIRST interaction-shaped method on the public Embedding
    /// Interface (AD-2/AD-13): a Host embedding the engine gets `send_input`
    /// for free, no CLI required.
    pub async fn send_input(&self, name: &str, text: &str) -> Result<(), EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let text = text.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.send_input(&registry, &name, &text)
        })
        .await
    }

    /// The per-instance runtime status (story 1-6, AC9): Lifecycle State +
    /// effective Restart Policy + restart count + (for `failed`) the last-known
    /// cause. This is the read `hekma agent list`/`show` uses to surface the restart
    /// count and, for a failed instance, the failed cause + active policy.
    ///
    /// Failed-cause precedence (AC9 requires the cause for ANY `failed` instance):
    /// the write-ahead record's `last_known_cause` if present; otherwise, for a
    /// `failed` instance, a fallback to the LAST transition-event-log cause. The
    /// fallback covers two cases the record cannot: a LAUNCH-ERROR failure
    /// (`starting → failed` returns before any spawn record is written), and a
    /// TERMINAL crash (`never` / crash-loop) whose record was cleared — both keep
    /// their cause in the JSON-Lines event log.
    pub async fn instance_status(&self, name: &str) -> Result<InstanceStatus, EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            let mut instance = registry
                .lookup(&iname)
                .map_err(crate::domain::registry_error_to_engine)?;
            let record = registry
                .spawn_record(&iname)
                .map_err(crate::domain::registry_error_to_engine)?;
            // Story 21-4: the read-only view of a provably-dead process (the
            // Fleet read's rule, [`Engine::fleet_entry_for`]) — shown
            // `failed`, nothing written.
            let gone = inner.read_only
                && matches!(
                    instance.state,
                    LifecycleState::Running | LifecycleState::Paused
                )
                && record.as_ref().is_some_and(|record| {
                    inner
                        .supervisor
                        .lock()
                        .expect("supervisor mutex poisoned")
                        .record_process_gone(record)
                });
            if gone {
                instance.state = LifecycleState::Failed;
            }
            let restart_policy = record
                .as_ref()
                .map(|r| r.restart_policy)
                .unwrap_or_default();
            let restart_count = record.as_ref().map(|r| r.restart_count).unwrap_or(0);
            let gone_pid = record.as_ref().map(|r| r.fingerprint.pid).unwrap_or(0);
            let failed_cause = record.and_then(|r| r.last_known_cause);
            let failed_cause = if gone {
                Some(gone_cause(gone_pid, failed_cause.as_deref()))
            } else {
                failed_cause
            };
            let failed_cause = failed_cause.or_else(|| {
                // No record cause. For a `failed` instance, fall back to the last
                // event-log cause so a launch-error or a cleared-terminal crash
                // still surfaces a reason (AC9).
                if instance.state != LifecycleState::Failed {
                    return None;
                }
                let events = Supervisor::read_events(&registry, &name).unwrap_or_default();
                events
                    .iter()
                    .rev()
                    .find(|e| e.new_state == LifecycleState::Failed)
                    .and_then(|e| failed_cause_detail(&e.cause))
            });
            Ok(InstanceStatus {
                instance,
                restart_policy,
                restart_count,
                failed_cause,
            })
        })
        .await
    }

    /// Set the per-instance Restart Policy (story 1-6, AC4 "per-instance
    /// configurable") — the config SEED (Epic-2 layered TOML config is later).
    /// Persists the policy so a subsequent `start` reads it and the reaper honors
    /// it on a crash. Runs behind the blocking pool like the other mutations.
    pub async fn set_restart_policy(
        &self,
        name: &str,
        policy: RestartPolicy,
    ) -> Result<(), EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            registry
                .set_restart_policy(&iname, policy)
                .map_err(crate::domain::registry_error_to_engine)
        })
        .await
    }

    /// The effective (resolved) unified config for an instance (story 2-1,
    /// spine AD-9, AC-A / AC10). This is the read `hekma agent config get` uses.
    ///
    /// Loads the four layers through path authority and folds them with the pure
    /// resolver (engine defaults < kind defaults < instance `config.toml` <
    /// invocation overrides), returning the [`EffectiveConfig`] (values + per-key
    /// [`SourceLayer`](crate::domain::SourceLayer) provenance — the 2-3 seam; 2-1
    /// renders values only). `overrides` is the ephemeral invocation layer,
    /// EMPTY for a plain `get`; it is a parameter now so a future `start --set
    /// k=v` threads it without an API change (Decision 8). Runs on the blocking
    /// pool like the other reads. An invalid name / missing instance / malformed
    /// layer surfaces a typed [`ConfigError`] (never a panic).
    pub async fn effective_config(
        &self,
        name: &str,
        overrides: ConfigLayer,
    ) -> Result<EffectiveConfig, ConfigError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let iname = InstanceName::new(&name).map_err(|reason| ConfigError::InvalidName {
                name: name.clone(),
                reason: reason.to_string(),
            })?;
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .effective_config(&iname, overrides)
        })
        .await
    }

    /// Reveal the resolved cleartext of every `secret:NAME` leaf for a
    /// `config get --reveal` read (story 2-4, AC-C/AC11). Returns the dotted key →
    /// REVEALED cleartext string for the secret leaves ONLY; `hekma` overlays them onto
    /// the (masked) effective config to un-mask exactly those leaves. The engine
    /// re-resolves secrets LIVE (env → the 0600 file); `hekma` never resolves secrets
    /// itself (AD-2). A resolution failure is a typed [`ConfigError::SecretReveal`]
    /// (a stderr diagnostic in `hekma`, never a crash). This NEVER touches the
    /// snapshot/logs/events — it is a read-only path. Runs on the blocking pool.
    pub async fn reveal_secrets(
        &self,
        name: &str,
        overrides: ConfigLayer,
    ) -> Result<std::collections::BTreeMap<String, String>, ConfigError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let iname = InstanceName::new(&name).map_err(|reason| ConfigError::InvalidName {
                name: name.clone(),
                reason: reason.to_string(),
            })?;
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .reveal_secrets(&iname, overrides)
        })
        .await
    }

    /// Set one unified-config key on an instance's INSTANCE layer (story 2-1,
    /// spine AD-9, AC-B / AC10). This is the write `hekma agent config set` uses.
    ///
    /// Validates at WRITE time first (an unknown key outside the `agent.*`
    /// pass-through namespace is rejected with the nearest key suggested), THEN
    /// persists ATOMICALLY (story 11-2, AI-24 — temp file + rename, so a crash
    /// mid-write cannot truncate the instance `config.toml`) to the Agent Home
    /// `config.toml` through path authority. A rejected write persists NOTHING
    /// (the instance config is byte-unchanged — AC-B). Runs on the blocking pool
    /// like the other mutations.
    ///
    /// On success the returned vec carries ZERO OR MORE WARN-ONLY steering
    /// warnings (story 11-2, AI-33 — today: a `secret:NAME` value set on a
    /// FLAG-targeted key). A Host should render each entry on ITS diagnostic
    /// surface (`hekma` prints them to stderr); the vec is empty on the common
    /// path, and the write is never rejected for a warned combination.
    pub async fn set_config(
        &self,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<Vec<String>, ConfigError> {
        self.refuse_if_read_only()
            .map_err(|err| ConfigError::Store {
                name: name.to_string(),
                detail: err.to_string(),
            })?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let key = key.to_string();
        let value = value.to_string();
        self.run_blocking(move || {
            let iname = InstanceName::new(&name).map_err(|reason| ConfigError::InvalidName {
                name: name.clone(),
                reason: reason.to_string(),
            })?;
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let warnings = registry.set_config(&iname, &key, &value)?;
            // Story 21-2: a `labels.*` write re-binds the open Run's labels
            // (Registry → Supervisor, the fixed lock order).
            if key.starts_with(crate::domain::runs::labels::LABELS_PREFIX) {
                inner
                    .supervisor
                    .lock()
                    .expect("supervisor mutex poisoned")
                    .sync_run_labels(&registry, &iname);
            }
            // Story 21-3: an explicit `budget.breach_action` write clears
            // the FR-21 upgrade marker (the "kept on pause" note ends), and
            // a budget or Rate write re-resolves the instance's open
            // process-owning Run's Policies in the same change.
            let mut warnings = warnings;
            if key == crate::domain::config::BUDGET_BREACH_ACTION_KEY {
                if let Some(note) = registry.clear_breach_default_marker(&iname) {
                    warnings.push(note);
                }
            }
            if key.starts_with("budget.") || key.starts_with("cost.rate.") {
                inner
                    .supervisor
                    .lock()
                    .expect("supervisor mutex poisoned")
                    .sync_run_policy(&registry, &iname);
            }
            Ok(warnings)
        })
        .await
    }

    /// Open a child Run under a supervised Run (story 21-2, PRD FR-43 /
    /// FR-49): the Host's unit of work — the old Task scope. The child is
    /// depth `announced`, hosted by its parent's process, and carries the
    /// caller labels and per-run ceilings of `request`; its usage rolls up to
    /// every ancestor and every ancestor's ceilings apply. Refused (typed)
    /// under a missing or ended parent, past [`crate::MAX_RUN_NESTING`]
    /// levels, or with a `pause` / `stop` / `block` Breach Action the child's
    /// depth cannot execute. A retried request with the same external id
    /// returns the same Run.
    pub async fn open_child_run(&self, request: ChildRunRequest) -> Result<RunView, RunError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.open_child_run(&registry, request)
        })
        .await
    }

    /// Close a child Run (story 21-2). Idempotent on an ended child; a Run
    /// that owns a supervised process is stopped, never closed. Its own
    /// children keep running.
    pub async fn close_run(&self, run_id: &str) -> Result<RunView, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.close_run(&registry, &run_id)
        })
        .await
    }

    /// Read one Run (story 21-2): its row, labels, limits, subtree usage and
    /// open children. Reads only (registry lock, read-only connection).
    pub async fn run(&self, run_id: &str) -> Result<RunView, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .run_view(&run_id)
        })
        .await
    }

    /// The open Run that owns an instance's supervised process (story
    /// 21-9): what an instance name means wherever a verb takes a Run.
    /// [`RunError::NoOpenRun`] when the instance has none. Reads only.
    pub async fn instance_run(&self, instance: &str) -> Result<RunView, RunError> {
        let inner = Arc::clone(&self.inner);
        let instance = instance.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .instance_run(&instance)
        })
        .await
    }

    /// Bind and unbind labels on one Run as the Operator (story 21-9, PRD
    /// FR-49): the label verb behind `hekma label` and
    /// `PATCH /v0/runs/{id}/labels`. A set label is bound with origin
    /// `label-verb` and outranks the instance configuration and any Caller
    /// label on its key; removing one restores the configured value it
    /// shadowed. Removing a label the configuration binds
    /// ([`RunError::LabelFromConfig`]) or a Caller label
    /// ([`RunError::LabelNotBound`]) is refused, as is any change on an
    /// ended Run ([`RunError::RunEnded`]). All-or-nothing; `dry_run` plans
    /// and writes nothing. A change commits one `labels-changed` event and
    /// re-resolves the Run's Policies in the same writer job.
    pub async fn label_run(
        &self,
        run_id: &str,
        change: LabelChange,
        dry_run: bool,
    ) -> Result<LabelOutcome, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.label_run(&registry, &run_id, change, dry_run)
        })
        .await
    }

    /// The non-ended Runs a Selector matches (story 21-9, PRD FR-50): every
    /// Run carrying all its `key=value` terms, bound or Caller, ordered by run
    /// id. More than [`crate::MAX_SELECTED_RUNS`] matches is refused
    /// ([`RunError::TooManyRuns`]). Reads only.
    pub async fn select_runs(&self, selector: &Selector) -> Result<Vec<RunRecord>, RunError> {
        let inner = Arc::clone(&self.inner);
        let selector = selector.clone();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .select_runs(&selector)
        })
        .await
    }

    /// [`Engine::select_runs`] with each matched Run assembled as its
    /// listing row ([`RunSummary`]: labels, subtree usage, latches and
    /// headroom) — the act set `POST /v0/runs/select` answers (story 21-6).
    /// The same cap ([`RunError::TooManyRuns`]); reads only.
    pub async fn select_run_summaries(
        &self,
        selector: &Selector,
    ) -> Result<Vec<RunSummary>, RunError> {
        let inner = Arc::clone(&self.inner);
        let selector = selector.clone();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .select_run_summaries(&selector)
        })
        .await
    }

    /// Whether `verb` would act on Run `run_id` (story 21-6): the Run's
    /// row when the verb maps to its instance's verb, else the per-Run
    /// refusal ([`RunError::RunEnded`], [`RunError::VerbUnavailable`]) the
    /// act would answer. Reads only — the `--dry-run` planner; an act can
    /// still meet the instance verb's own refusals (capability, state).
    pub async fn plan_run_verb(
        &self,
        run_id: &str,
        verb: RunLifecycleVerb,
    ) -> Result<RunRecord, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let record = registry
                .run_record(&run_id)?
                .ok_or_else(|| RunError::NotFound {
                    run_id: run_id.as_str().to_string(),
                })?;
            crate::domain::runs::verbs::run_verb_target(verb, &record)?;
            Ok(record)
        })
        .await
    }

    /// The instance a Run-grain verb acts on, checked and acted under the
    /// SAME locks (so a Run that ends — and a fresh start that replaces it
    /// — between the check and the act cannot redirect the verb onto
    /// another Run).
    async fn act_on_run<T: Send + 'static>(
        &self,
        run_id: &str,
        verb: RunLifecycleVerb,
        act: impl FnOnce(&Registry, &mut Supervisor, &str) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, RunVerbError> {
        self.refuse_if_read_only()
            .map_err(|err| RunVerbError::Run(RunError::Store(err)))?;
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            let record = registry
                .run_record(&run_id)?
                .ok_or_else(|| RunError::NotFound {
                    run_id: run_id.as_str().to_string(),
                })?;
            let instance = crate::domain::runs::verbs::run_verb_target(verb, &record)?;
            act(&registry, &mut supervisor, &instance).map_err(RunVerbError::Engine)
        })
        .await
    }

    /// Stop the instance whose supervised process Run `run_id` owns (story
    /// 21-6, PRD FR-50): exactly [`Engine::stop`] on that instance. A child
    /// Run, an ended Run, and any depth the verb × depth table does not
    /// serve yet are refused per Run ([`RunVerbError::Run`]).
    pub async fn stop_run(
        &self,
        run_id: &str,
        window: Option<Duration>,
    ) -> Result<AgentInstance, RunVerbError> {
        self.act_on_run(
            run_id,
            RunLifecycleVerb::Stop,
            move |registry, supervisor, name| supervisor.stop(registry, name, window),
        )
        .await
    }

    /// Pause the instance whose process Run `run_id` owns (story 21-6):
    /// exactly [`Engine::pause`] on it (the Run goes `held`). Other Runs
    /// are refused per Run.
    pub async fn pause_run(&self, run_id: &str) -> Result<AgentInstance, RunVerbError> {
        self.act_on_run(
            run_id,
            RunLifecycleVerb::Pause,
            |registry, supervisor, name| supervisor.pause(registry, name),
        )
        .await
    }

    /// Resume the instance whose process Run `run_id` owns (story 21-6):
    /// exactly [`Engine::resume`] on it. A child held because its parent is
    /// paused is refused, naming the parent.
    pub async fn resume_run(&self, run_id: &str) -> Result<AgentInstance, RunVerbError> {
        self.act_on_run(
            run_id,
            RunLifecycleVerb::Resume,
            |registry, supervisor, name| supervisor.resume(registry, name),
        )
        .await
    }

    /// Drain the instance whose process Run `run_id` owns (story 21-6):
    /// exactly [`Engine::drain`] on it, with its own bound. Other Runs are
    /// refused per Run.
    pub async fn drain_run(
        &self,
        run_id: &str,
        timeout_override: Option<Duration>,
    ) -> Result<DrainReport, RunVerbError> {
        self.act_on_run(
            run_id,
            RunLifecycleVerb::Drain,
            move |registry, supervisor, name| {
                supervisor
                    .drain(registry, name, timeout_override)
                    .map(|(instance, outcome)| DrainReport { instance, outcome })
            },
        )
        .await
    }

    /// Set limits on Runs as `actor` (story 21-6, PRD FR-50 / FR-60): the
    /// budget job behind `hekma budget set` and `PATCH /v0/runs/{id}/limits`.
    /// Each Run is one writer job and answers on its own (a refusal on one
    /// never stops the others): `scope-unavailable` (a cumulative ceiling on
    /// a child Run), `action-unavailable` (an action its depth cannot
    /// execute, naming the Metering Source), `raise-forbidden` (a Caller
    /// loosening a limit), `run-ended`, `run-not-found`. A committed change
    /// writes the Run's `run_limits`, re-resolves it, commits one
    /// `limits-changed` event and runs the shared re-arm pass: a ceiling
    /// raised above the totals re-arms its latch and lifts the matching
    /// `block` fence reason per reason; one lowered to at or below the usage
    /// breaches at once, its Breach Action applied before this returns. A
    /// limit set here belongs to the Run — it ends with the Run. `dry_run`
    /// plans each Run and writes nothing. A malformed change is refused
    /// whole.
    pub async fn set_run_limits(
        &self,
        run_ids: Vec<String>,
        change: RunLimitChange,
        actor: LimitActor,
        dry_run: bool,
    ) -> Result<crate::domain::RunLimitsResults, RunError> {
        if !dry_run {
            self.refuse_if_read_only().map_err(RunError::Store)?;
        }
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            let ids: Vec<RunId> = run_ids.into_iter().map(RunId::from_wire).collect();
            supervisor.set_run_limits(&registry, &ids, &change, actor, dry_run)
        })
        .await
    }

    /// Recognise a presented credential (story 22-1): a lock-free read of
    /// the published snapshot's key index — no SQLite, no engine lock. The
    /// presentation is shape-checked on public structure (prefix, length,
    /// alphabet), hashed, looked up by the FULL hash, and the matched entry's
    /// 32 bytes compared in constant time. `None` for anything the index does
    /// not hold (an unknown, revoked or refresh credential —
    /// [`Engine::credential_state`] tells them apart).
    pub fn authenticate(&self, presented: &str) -> Option<crate::domain::CredentialMatch> {
        let (_, secret) = crate::domain::parse_presented(presented)?;
        let hash = crate::domain::credential::hash_presented(&secret);
        let snapshot = self.inner.run_snapshot.current();
        let (stored, entry) = snapshot.keys.get_key_value(&hash)?;
        if !stored.ct_eq(&hash) {
            return None;
        }
        Some(crate::domain::CredentialMatch {
            class: entry.class,
            run_id: entry.run_id.clone(),
            token_id: entry.token_id.clone(),
            supervised: entry.launch_generation.is_some(),
            hash: *stored,
        })
    }

    /// What a presented credential is (story 22-1): the lock-free snapshot
    /// first, then ONE read on the engine's credential reader — unknown,
    /// revoked, a used refresh credential, or active (a refresh credential,
    /// which the snapshot never indexes). Never takes the registry or
    /// supervisor mutex and never submits a writer job, so a flood of
    /// unrecognised presentations cannot queue behind (or ahead of)
    /// supervision. A presentation that is not token-shaped is unknown.
    pub async fn credential_state(
        &self,
        presented: &str,
    ) -> Result<crate::domain::PresentedState, RunError> {
        if let Some(matched) = self.authenticate(presented) {
            return Ok(crate::domain::PresentedState::Active(matched));
        }
        let Some((_, secret)) = crate::domain::parse_presented(presented) else {
            return Ok(crate::domain::PresentedState::Unknown);
        };
        self.read_presented_state(secret).await
    }

    /// One credential-reader lookup (no registry or supervisor lock).
    async fn read_presented_state(
        &self,
        secret: crate::domain::SecretString,
    ) -> Result<crate::domain::PresentedState, RunError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let reader = inner
                .credential_reader
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::domain::registry_keys::presented_state(&reader, &secret)
        })
        .await
    }

    /// Open a child Run as `actor` (story 22-1): the Operator or a Host
    /// anywhere, a Caller (a Run Key) only beneath its own Run or a
    /// descendant, at most [`crate::MAX_CALLER_OPEN_CHILDREN`] open children
    /// per parent, with its key re-validated inside the job. When
    /// `request.issue_credentials` is set the child's Run Key and refresh
    /// credential are minted (from the OS random source, no fallback) and
    /// returned ONCE in [`crate::OpenedRun::credentials`]; nothing can show
    /// them again.
    pub async fn open_child_run_as(
        &self,
        actor: crate::domain::RunActor,
        request: ChildRunRequest,
    ) -> Result<crate::domain::OpenedRun, RunError> {
        self.refuse_if_read_only().map_err(RunError::Store)?;
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.open_child_run_as(&registry, actor, request)
        })
        .await
    }

    /// Close a child Run as `actor` (story 22-1): a Caller only within its
    /// own Run's subtree.
    pub async fn close_run_as(
        &self,
        actor: crate::domain::RunActor,
        run_id: &str,
    ) -> Result<RunView, RunError> {
        self.refuse_if_read_only().map_err(RunError::Store)?;
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.close_run_as(&registry, actor, &run_id)
        })
        .await
    }

    /// Set Run-level limits as `actor` (story 22-1): a Caller writes its own
    /// layer, may only tighten, only within its own Run's subtree, with its
    /// key re-validated inside each job.
    pub async fn set_run_limits_as(
        &self,
        run_ids: Vec<String>,
        change: RunLimitChange,
        actor: crate::domain::RunActor,
        dry_run: bool,
    ) -> Result<crate::domain::RunLimitsResults, RunError> {
        if !dry_run {
            self.refuse_if_read_only().map_err(RunError::Store)?;
        }
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            let ids: Vec<RunId> = run_ids.into_iter().map(RunId::from_wire).collect();
            supervisor.set_run_limits_as(&registry, &ids, &change, actor, dry_run)
        })
        .await
    }

    /// Trade a Run's active refresh credential for a new Run Key and refresh
    /// credential, once (story 22-1). The old Run Key is refused from the
    /// next request on. Presenting a USED refresh credential again revokes
    /// the Run's current credentials of that credential's own launch
    /// generation, raises the `credential-reuse` Anomaly and answers
    /// [`RunError::CredentialRevoked`].
    pub async fn refresh_credentials(
        &self,
        presented: &crate::domain::SecretString,
    ) -> Result<crate::domain::IssuedCredentials, RunError> {
        use crate::domain::{PresentedState, SecretString, TokenClass};
        self.refuse_if_read_only().map_err(RunError::Store)?;
        // Resolve first, on the credential reader: only a refresh credential
        // that was issued and not revoked goes on to mint, take the engine's
        // locks and the store writer (the job re-reads the row and decides).
        match self
            .read_presented_state(SecretString::new(presented.expose_secret()))
            .await?
        {
            PresentedState::Active(matched) | PresentedState::Used(matched)
                if matched.class == TokenClass::Refresh => {}
            PresentedState::Revoked => return Err(RunError::CredentialRevoked),
            _ => return Err(RunError::CredentialUnknown),
        }
        let presented = SecretString::new(presented.expose_secret());
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.refresh_credentials(&registry, &presented)
        })
        .await
    }

    /// Revoke a Run's active credentials (story 22-1, `hekma run
    /// revoke-keys`) and, with `cascade`, every active credential issued
    /// under any of them. Committed and published before this returns: the
    /// next request presenting any of them is refused. Clears the
    /// `credential-reuse` Anomaly. Never ends a Run.
    pub async fn revoke_run_keys(
        &self,
        run_id: &str,
        cascade: bool,
    ) -> Result<crate::domain::RevokeOutcome, RunError> {
        self.refuse_if_read_only().map_err(RunError::Store)?;
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.revoke_run_keys(&registry, &run_id, cascade)
        })
        .await
    }

    /// Revoke credentials seen in a URL (story 22-1): an active one among
    /// `presented`, or a used refresh credential, is revoked
    /// (`exposed-in-url`) — it has reached an access log. Returns how many
    /// were revoked. Each value is resolved first — the lock-free snapshot,
    /// then the credential reader — and only one that resolves to an active
    /// or used credential takes the engine's locks and a writer job: a URL
    /// full of token-shaped noise costs reads, never a write.
    pub async fn revoke_exposed(
        &self,
        presented: Vec<crate::domain::SecretString>,
    ) -> Result<usize, RunError> {
        self.refuse_if_read_only().map_err(RunError::Store)?;
        let mut resolved = Vec::new();
        for secret in presented {
            if self.authenticate(secret.expose_secret()).is_some()
                || matches!(
                    self.read_presented_state(crate::domain::SecretString::new(
                        secret.expose_secret()
                    ))
                    .await?,
                    crate::domain::PresentedState::Active(_)
                        | crate::domain::PresentedState::Used(_)
                )
            {
                resolved.push(secret);
            }
        }
        if resolved.is_empty() {
            return Ok(0);
        }
        let presented = resolved;
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.revoke_exposed(&registry, &presented)
        })
        .await
    }

    /// Every credential of one Run, any state, oldest first (story 22-1,
    /// `hekma run keys`): metadata only — token ids, classes, states,
    /// generations and causes; never a token or a hash. Reads only.
    pub async fn run_keys(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::domain::CredentialSummary>, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .run_keys(&run_id)
        })
        .await
    }

    /// The budget read of one Run (story 21-6, `hekma budget show`): the
    /// limits set on the Run, the resolved limits with each source, its
    /// subtree and cumulative usage, the headroom and the latches. Reads
    /// only.
    pub async fn run_limits_view(&self, run_id: &str) -> Result<RunLimitsView, RunError> {
        let inner = Arc::clone(&self.inner);
        let run_id = RunId::from_wire(run_id);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .run_limits_view(&run_id)
        })
        .await
    }

    /// The Run listing (story 21-5, PRD FR-50): one exception-first (or
    /// `--all`, inside-the-ended-Run-window) keyset page, newest run id
    /// first, with each row's CONTROL depth, Run Status, Anomalies with
    /// their latched breach actions, labels with source and origin, usage
    /// (subtree always; the instance cumulative on process-owning rows),
    /// and headroom against the Run's RESOLVED limits with each limit's
    /// source. A Listing Selectors PAGES past [`crate::MAX_SELECTED_RUNS`]
    /// (the verb cap stays verb-only). A cursor that is not the codec's
    /// form is the typed `invalid-cursor` refusal. Reads only (registry
    /// lock, read-only connection) — this is `hekma ps`'s read.
    pub async fn list_runs(&self, query: ListRunsQuery) -> Result<RunListing, RunError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .list_runs(&query)
        })
        .await
    }

    /// The per-call records listing (the calls store's read surface): one
    /// keyset page over the `calls/` segment files, newest call id first,
    /// filtered by Run, token id, outcome, disposition, status class,
    /// model, Control Depth, binding and a half-open call-start window,
    /// with a `problems` view that lists only calls that were not a clean
    /// pass. Every page is bounded: at most [`CALLS_MAX_LIMIT`] items and
    /// at most [`CALLS_SCAN_BUDGET`] rows examined — a sparse filter stops
    /// there and says so (`scan_limited`, the continuing cursor, how far
    /// back the page looked). Works on a read-only engine and on an engine
    /// whose proxy is disabled; a missing calls directory is an empty
    /// listing. Takes NO registry or supervisor lock and sends nothing to
    /// the writer: the reader opens its own read-only connection per
    /// segment and closes it on return, so it cannot slow supervision or
    /// the proxy's hot path. A cursor that is not the codec's form, or one
    /// naming anything but a call id, is the typed `invalid-cursor`
    /// refusal.
    pub async fn list_calls(
        &self,
        query: crate::domain::calls::CallsQuery,
    ) -> Result<crate::domain::calls::CallListing, crate::domain::calls::CallError> {
        let dir = self.inner.calls_dir.clone();
        self.run_blocking(move || crate::store::calls_read::read_page(&dir, &query))
            .await
    }

    /// One per-call record by its id (the calls store's read surface): the
    /// newest segment that holds the id serves it. The same lock-free,
    /// read-only discipline as [`Engine::list_calls`]. A malformed id is
    /// the typed `invalid-call-id` refusal; an unknown one is
    /// [`CallError::NotFound`](crate::domain::calls::CallError::NotFound).
    pub async fn call(
        &self,
        call_id: &str,
    ) -> Result<crate::domain::calls::CallRecord, crate::domain::calls::CallError> {
        let dir = self.inner.calls_dir.clone();
        let call_id = call_id.to_string();
        self.run_blocking(move || crate::store::calls_read::read_call(&dir, &call_id))
            .await
    }

    /// Seed bench Run rows under one instance (story 21-5's perf harness,
    /// `#[doc(hidden)]`): written directly through the store writer — no
    /// events, no snapshot publish, no processes. NOT a supported
    /// embedding surface; the `perf-budgets` example is the only caller.
    #[doc(hidden)]
    pub async fn seed_runs_for_perf(
        &self,
        instance: &str,
        rows: Vec<crate::domain::runs::PerfRunRow>,
    ) -> Result<(), RunError> {
        let instance = instance.to_string();
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .seed_perf_rows(&instance, &rows)
        })
        .await
    }

    /// The subject Run of `event` and the labels it carries NOW (story
    /// 21-5): the SSE Selector filter's one read per filtered event —
    /// snapshot first (no SQL), one read-only store read when the Run is
    /// absent from it (ended Runs keep matching until compaction).
    /// `Ok(None)` when the event names no Run (`policy`,
    /// `breach_default_upgraded`) or the named Run has no row — both are
    /// dropped under an active Selector and delivered without one.
    /// Honest limitation, by design: the filter sees CURRENT labels, not
    /// labels-at-event-time (nothing stores the latter).
    pub async fn event_subject_labels(
        &self,
        event: &EngineEvent,
    ) -> Result<Option<Vec<Label>>, RunError> {
        // The fast path: the published snapshot, read lock-free and without
        // a blocking-pool hop — an open subject Run (the common case) never
        // touches the registry mutex the supervisor's poll needs.
        match crate::domain::runs::snapshot::subject_labels(
            &self.inner.run_snapshot.current(),
            event,
        ) {
            crate::domain::runs::snapshot::SubjectLabels::Found(labels) => return Ok(Some(labels)),
            crate::domain::runs::snapshot::SubjectLabels::NoSubject => return Ok(None),
            crate::domain::runs::snapshot::SubjectLabels::NotInSnapshot => {}
        }
        let inner = Arc::clone(&self.inner);
        let event = event.clone();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let Some(run_id) = registry.event_subject_run(&event) else {
                return Ok(None);
            };
            registry
                .run_current_labels(&run_id)
                .map_err(RunError::Store)
        })
        .await
    }

    /// Every stored Policy (story 21-3, PRD FR-60), the `default` Policy
    /// included, ordered by name. Reads only.
    pub async fn policies(&self) -> Result<Vec<PolicyRecord>, RunError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .policies()
                .map_err(RunError::Store)
        })
        .await
    }

    /// One stored Policy by name (story 21-3); [`RunError::PolicyNotFound`]
    /// when none has it. Reads only.
    pub async fn policy(&self, name: &str) -> Result<PolicyRecord, RunError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .policy(&name)
                .map_err(RunError::Store)?
                .ok_or(RunError::PolicyNotFound { name })
        })
        .await
    }

    /// Create a Policy (story 21-3, PRD FR-60): Operator- or Host-set limits
    /// for the Runs its Selector matches by bound label. Refused with
    /// [`RunError::PolicyExists`] when the name is taken. The change applies
    /// to every matched open Run at once — a ceiling at or below a Run's
    /// usage breaches in the same writer job — and commits one `policy`
    /// event. The outcome reports how many Runs matched and how many fall
    /// back to `warn` at their Control Depth. `dry_run` plans and writes
    /// nothing.
    pub async fn create_policy(
        &self,
        change: PolicyChange,
        dry_run: bool,
    ) -> Result<PolicyOutcome, RunError> {
        if !dry_run {
            self.refuse_if_read_only().map_err(RunError::Store)?;
        }
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            if registry
                .policy(&change.name)
                .map_err(RunError::Store)?
                .is_some()
            {
                return Err(RunError::PolicyExists { name: change.name });
            }
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.change_policy(&registry, change, dry_run)
        })
        .await
    }

    /// Change a Policy, the `default` Policy included (story 21-3): set
    /// limits, clear them, and (on `default`) edit the model-keyed Rate
    /// table. [`RunError::PolicyNotFound`] when no Policy has the name.
    /// Follows the latest: every matched open Run re-resolves at once; a
    /// lowered ceiling at or below usage breaches, a raised one above usage
    /// re-arms the latch and clears the matching `block` fence reasons.
    /// `dry_run` plans and writes nothing.
    pub async fn change_policy(
        &self,
        change: PolicyChange,
        dry_run: bool,
    ) -> Result<PolicyOutcome, RunError> {
        if !dry_run {
            self.refuse_if_read_only().map_err(RunError::Store)?;
        }
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            if registry
                .policy(&change.name)
                .map_err(RunError::Store)?
                .is_none()
            {
                return Err(RunError::PolicyNotFound { name: change.name });
            }
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.change_policy(&registry, change, dry_run)
        })
        .await
    }

    /// Delete a Policy (story 21-3); its matched Runs re-resolve under the
    /// remaining Policies. The `default` Policy cannot be deleted.
    /// `dry_run` reports the Runs it would re-resolve and writes nothing.
    pub async fn delete_policy(
        &self,
        name: &str,
        dry_run: bool,
    ) -> Result<PolicyOutcome, RunError> {
        if !dry_run {
            self.refuse_if_read_only().map_err(RunError::Store)?;
        }
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.delete_policy(&registry, &name, dry_run)
        })
        .await
    }

    /// Attach a Memory Backing of `kind` to an Agent Instance (story 5-1,
    /// FR-15 / spine AD-11). The engine creates the managed directory inside the
    /// Agent Home (path authority — the returned path IS the engine's, `hekma`
    /// never constructs it) and persists the attachment; the descriptor is
    /// handed to the adapter at next start via the reserved unified-config key.
    ///
    /// Permitted ONLY while the instance sits in a TERMINAL persisted state
    /// (`registered`/`stopped`/`failed`) — every non-terminal state is refused
    /// with no side effect, and there is NO force escape (AD-11 forbids
    /// hot-swap outright).
    ///
    /// REGISTRY LOCK ONLY (AD-17): unlike `start`/`stop`, this never takes the
    /// supervisor lock — it is one bounded DB write plus, for a `filesystem`
    /// backing, one bounded directory creation (a non-filesystem kind is pure
    /// delegation metadata and creates nothing), so it cannot widen the
    /// fleet-wide stall.
    pub async fn attach_memory(
        &self,
        name: &str,
        kind: crate::ports::MemoryBackingKind,
    ) -> Result<PathBuf, RegistryError> {
        self.refuse_if_read_only().map_err(RegistryError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .attach_memory(&name, kind)
        })
        .await
    }

    /// Detach an Agent Instance's Memory Backing (story 5-1). METADATA ONLY:
    /// clears the persisted attachment and leaves the managed directory and its
    /// contents on disk (operator data is never silently deleted — A-4);
    /// re-attaching later re-adopts them. Same terminal-state guard as
    /// [`Engine::attach_memory`] (no hot-swap, no force), same registry-lock-only
    /// discipline. Detaching when nothing is attached is a successful no-op.
    pub async fn detach_memory(&self, name: &str) -> Result<(), RegistryError> {
        self.refuse_if_read_only().map_err(RegistryError::Store)?;
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .detach_memory(&name)
        })
        .await
    }

    /// Read an instance's Memory Backing status through the public API (story
    /// 5-1, Task 4.5): `None` when nothing is attached; otherwise the kind, the
    /// engine-computed managed directory, whether the adapter's declared
    /// config mapping targets the reserved key (the DC-10 delivery fact — the
    /// path is OFFERED at every start for a `filesystem` backing; receiving it
    /// is the adapter's declared choice; a non-filesystem backing delivers
    /// nothing and reads `declared: false`), and the typed guarantee level
    /// (story 5-2).
    pub async fn memory_status(
        &self,
        name: &str,
    ) -> Result<Option<crate::ports::MemoryBackingStatus>, RegistryError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .memory_status(&name)
        })
        .await
    }

    /// Read the recorded transition events for an instance from its log (AC1
    /// "each transition emits an event"; AC3 escalation recorded). Test/embedding
    /// observation helper — this is the AD-14 seed, NOT the 7-2 subscription bus.
    pub async fn transition_events(&self, name: &str) -> Result<Vec<TransitionEvent>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            Supervisor::read_events(&registry, &name)
        })
        .await
    }

    /// The recorded [`BudgetBreachEvent`]s for an instance (story 3-2, AC7 — the
    /// ALWAYS-recorded breach log). Reads back the durable per-instance breach log
    /// (an observation helper for embedders / tests — the AD-14 seed, NOT the 7-2
    /// subscription bus). Empty vec if no breach has been recorded yet.
    pub async fn budget_breach_events(
        &self,
        name: &str,
    ) -> Result<Vec<BudgetBreachEvent>, EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            Supervisor::read_breach_events(&registry, &name)
        })
        .await
    }

    /// Backfill an instance's COMMITTED events as bus payloads (story 10-3) —
    /// the one-call remedy for the event bus's documented crash-window
    /// at-most-once delivery.
    ///
    /// Reads the same committed truth the query APIs serve (the instance's
    /// `instance.log` transitions, `breaches.log` breach records, and
    /// `usage_events` ledger rows — COMMITTED records only: a failed append
    /// never appears here either) and converts every record past `after` into
    /// the exact [`EngineEvent`] wrapper the live bus delivers. The returned
    /// [`ResyncBatch`] carries those events plus the [`ResyncCursor`] to pass
    /// to the NEXT call, so re-running a resync never re-delivers what a host
    /// already consumed.
    ///
    /// ## Ordering: both orders work; pick by the gap/duplicate tradeoff
    ///
    /// Within each family the batch is exact commit order (log line order /
    /// ledger `rowid` order — the same orders the durable record keeps);
    /// across families the batch is family-major (transitions, then breaches,
    /// then usage), because the durable record carries no global cross-family
    /// sequence. How to combine the backfill with the live stream is the
    /// host's ordering choice:
    ///
    /// * **Subscribe FIRST, then resync** — RECOMMENDED for gap-sensitive
    ///   hosts. Gap-free by construction (every commit after the subscribe
    ///   is delivered live), at the cost of overlap with the backfill, which
    ///   the host dedups per family against its own cursor position
    ///   (duplicates, never gaps).
    /// * **Resync first, then subscribe** — no seam duplicates (the backfill
    ///   is precisely the prefix, the live stream precisely the suffix), but
    ///   NOT gap-free: a commit+publish landing between `resync_events`
    ///   returning and `subscribe()` is delivered by NEITHER. Use it across
    ///   a quiescent agent (stopped/paused, no traffic) or accept the
    ///   window.
    ///
    /// The backfill is also NOT a cross-family point-in-time snapshot: the
    /// three family reads are sequential, so a commit landing between them
    /// skews the families relative to each other (per-family slices stay
    /// exact and cursor-lossless). See `domain::resync` for the full
    /// contract, including both honesty notes.
    ///
    /// This is a READ-side helper: the bus, its publish points, and the
    /// subscribe semantics are untouched. Runs on the blocking pool (registry
    /// lock + file/DB reads), like the other reads. An unregistered name fails
    /// [`EngineError::NotFound`] and a malformed name
    /// [`EngineError::InvalidName`] (the `read_agent_log` precedent — a
    /// mistyped name must not read as a silent empty backfill). The reads are
    /// torn-tail tolerant (ONE unparseable trailing line per log is skipped —
    /// and only when it carries the torn-append signature, the file not
    /// ending with a newline — with the skip SURFACED on the batch's
    /// `torn_tail_skipped`, because the engine's next append fuses onto the
    /// torn fragment and the skipped line may be carrying a good record);
    /// every other malformed line is a typed [`EngineError::Log`], as is a
    /// cursor position past a family's committed count (a truncated/rotated
    /// log — never silently clamped into a re-delivery). Sync consumers use
    /// [`Blocking::resync_events`].
    pub async fn resync_events(
        &self,
        name: &str,
        after: ResyncCursor,
    ) -> Result<ResyncBatch, EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            registry
                .lookup(&iname)
                .map_err(crate::domain::registry_error_to_engine)?;
            crate::domain::read_committed(&registry, &iname, &after)
        })
        .await
    }

    /// Whether this engine holds a live attributed capture (a tailer) for
    /// `name` (story 21-4): `true` for an instance it spawned and still
    /// supervises; `false` for an adopted instance, one it does not hold, an
    /// unknown or malformed name, and every instance of a read-only engine.
    /// A reader that also polls the raw `agent.log` uses it to avoid
    /// reporting the same bytes twice.
    pub async fn has_live_capture(&self, name: &str) -> bool {
        let Ok(name) = InstanceName::new(name) else {
            return false;
        };
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .supervisor
                .lock()
                .expect("supervisor mutex poisoned")
                .has_live_capture(&name)
        })
        .await
    }

    /// The engine's configured log-capture posture (story 17-4, review
    /// finding 21): `full` | `metadata` | `off` — `/v0/version`'s
    /// self-description carries it, so a Host learns what `output.log`
    /// holds and which capture surfaces serve, without a failed round-trip.
    pub async fn log_capture_mode(&self) -> LogCaptureMode {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            inner
                .supervisor
                .lock()
                .expect("supervisor mutex poisoned")
                .log_capture_mode()
        })
        .await
    }

    /// Pull ONE bounded batch of committed events from the engine's global
    /// outbox journal (story 17-4) — the export surface ephemeral hosts use
    /// to mirror committed truth to durable storage before the engine (and
    /// its state dir) disappears. ONE global order across every event family
    /// (transitions, budget breaches, usage updates): the journal append
    /// order, which is the commit order (each hook fires at the same commit
    /// point as the bus publish).
    ///
    /// `after` is the resume cursor: `None` starts from the DURABLE ACK
    /// cursor (what this host has already persisted — the common "give me
    /// what's new" pull), an explicit cursor overrides it. `max` bounds the
    /// batch (`0` = the [`crate::domain::EXPORT_BATCH_DEFAULT_MAX`] default;
    /// capped at [`crate::domain::EXPORT_BATCH_MAX_CAP`]). Every entry carries
    /// its own `seq`, so a consumer can resume from the last one it durably
    /// stored. A cursor below the journal's retained floor (compaction
    /// already dropped those entries) or past the committed tail refuses with
    /// the STABLE marker sentences the Control API maps to 409
    /// `cursor_invalid` — never a silent clamp.
    ///
    /// The read takes the REGISTRY lock only (tolerant of concurrent
    /// appends); the tolerant reader skips a torn crash-cut tail fragment.
    pub async fn export_events(
        &self,
        after: Option<OutboxCursor>,
        max: u32,
    ) -> Result<ExportBatch, EngineError> {
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            export_core(&registry, after, max)
        })
        .await
    }

    /// Advance the durable export ACK cursor (story 17-4): everything with
    /// `seq <= ack` is CONSUMED and becomes compaction-eligible (the journal
    /// drops the acked prefix once the size cap demands space). MONOTONIC —
    /// the persisted cursor is `max(current, given)`, so a re-ack or a
    /// backward ack is the current cursor (idempotent), never a rewind. An
    /// ack PAST the journal's committed tail refuses with the stable
    /// past-tail sentence (409 `cursor_invalid` at the Control API) — an ack
    /// is a claim about entries that exist. The cursor persists
    /// (`outbox-ack.json`, atomic write), so it survives a host restart.
    ///
    /// Holds the SUPERVISOR lock: ack advances what compaction may drop, so
    /// it must not interleave an append's compaction decision.
    pub async fn ack_export(&self, seq: u64) -> Result<u64, EngineError> {
        self.refuse_if_read_only().map_err(EngineError::Store)?;
        let inner = Arc::clone(&self.inner);
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            // The SUPERVISOR lock is held across the ack: it serializes the
            // cursor advance against any append's compaction decision (a
            // compaction deciding what to keep against a MOVING ack cursor is
            // the one unsound interleaving). The guard binding is what holds
            // it.
            let _supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            ack_core(&registry, seq)
        })
        .await
    }

    /// Read the retained ATTRIBUTED output log for an instance (story 4-2,
    /// AC-A) — a ONE-SHOT full read of whatever is currently retained (the
    /// current generation plus any rotated predecessors), in on-disk append
    /// order (AC-G). This is the FIRST CLI-facing consumer of this shape
    /// (`hekma agent logs`) — an unregistered name fails
    /// [`EngineError::NotFound`] (see
    /// [`Supervisor::read_agent_log`]'s docs for the full existence-check
    /// rationale, a deliberate improvement over
    /// [`Engine::transition_events`]/[`Engine::budget_breach_events`]'s
    /// precedent above). ALSO returns the byte-cursor (into the current
    /// generation) this read reached (fix pass M1, review of #80), so a
    /// caller priming a `--follow` loop needs no separate, discarding
    /// `read_agent_log_since(name, 0)` call.
    pub async fn read_agent_log(&self, name: &str) -> Result<(Vec<LogLine>, u64), EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            // Story 17-4: the capture-mode guard is evaluated HERE (the
            // supervisor owns the engine-wide mode) — an Off engine never
            // captured anything, and a Metadata engine's `output.log` is
            // metadata-shaped, so neither can return attributable lines.
            let mode = inner
                .supervisor
                .lock()
                .expect("supervisor mutex poisoned")
                .log_capture_mode();
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            if let Some(err) = capture_read_refusal(mode, &iname, true, &registry) {
                return Err(err);
            }
            Supervisor::read_agent_log(&registry, &name)
        })
        .await
    }

    /// A cursor-based follow read (story 4-2, AC-B/AC-C/AC-H) — `hekma agent
    /// logs --follow`'s poll loop drives this in a loop with the previously
    /// returned cursor. See [`Supervisor::read_agent_log_since`]'s docs for
    /// the cursor semantics (current-generation-only) and the
    /// rotation-shrink signal (a returned cursor less than the one just
    /// passed in).
    pub async fn read_agent_log_since(
        &self,
        name: &str,
        cursor: u64,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            // Story 17-4: same capture-mode guard as `read_agent_log` (the
            // follow loop surfaces the refusal every poll).
            let mode = inner
                .supervisor
                .lock()
                .expect("supervisor mutex poisoned")
                .log_capture_mode();
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            if let Some(err) = capture_read_refusal(mode, &iname, true, &registry) {
                return Err(err);
            }
            Supervisor::read_agent_log_since(&registry, &name, cursor)
        })
        .await
    }

    /// The RAW `agent.log` FALLBACK follow read (story 15-2, Fibery 207) —
    /// `hekma agent logs --follow`'s companion poll for the
    /// between-commands window of a detached instance. See
    /// [`Supervisor::read_raw_output_since`]'s docs for the honesty rules
    /// (the un-attributed qualifier the CALLER surfaces once per session,
    /// the live-capture guard, and the `None`-cursor priming).
    pub async fn read_raw_output_since(
        &self,
        name: &str,
        cursor: Option<u64>,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            // Story 17-4: OFF refuses honestly (nothing was ever captured —
            // an empty vector would LIE by implying a captured-then-exhausted
            // log). METADATA proceeds: the raw file exists while the instance
            // runs (that is the metadata contract's transient window) and its
            // honest absence after the stop-time unlink is exactly what the
            // existing missing-file semantics already report.
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            if let Some(err) =
                capture_read_refusal(supervisor.log_capture_mode(), &iname, false, &registry)
            {
                return Err(err);
            }
            supervisor.read_raw_output_since(&registry, &name, cursor)
        })
        .await
    }

    /// The bounded raw-BACKLOG prime (the between-commands gap) —
    /// `logs --follow` resolves its raw fallback cursor's START through this
    /// before polling, so the last `backlog_lines` complete raw lines stream
    /// at follow start. See
    /// [`Supervisor::prime_raw_backlog`]'s docs for the
    /// `(cursor, skipped_beyond)` contract and the live-capture guard.
    pub async fn prime_raw_backlog(
        &self,
        name: &str,
        backlog_lines: u64,
    ) -> Result<(u64, u64), EngineError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            // Story 17-4: the same Off guard as `read_raw_output_since` (its
            // companion priming call — an Off engine refuses at the prime,
            // before the follow loop starts polling).
            let iname = InstanceName::new(&name).map_err(|reason| EngineError::InvalidName {
                name: name.clone(),
                reason,
            })?;
            if let Some(err) =
                capture_read_refusal(supervisor.log_capture_mode(), &iname, false, &registry)
            {
                return Err(err);
            }
            supervisor.prime_raw_backlog(&registry, &name, backlog_lines)
        })
        .await
    }

    /// Run a blocking closure on tokio's blocking pool and await its result.
    ///
    /// Centralizes the `spawn_blocking` bridge so every async wrapper follows the
    /// same shape: rusqlite/FS work never touches an async worker. A join failure
    /// (the blocking task panicked) re-panics on the awaiting task rather than
    /// being silently swallowed.
    async fn run_blocking<T, F>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .expect("engine blocking task panicked")
    }
}

/// A synchronous facade over [`Engine`]'s async API (AD-13; FR-34/7-3 seed).
///
/// Obtained via [`Engine::blocking`]. Each method blocks the calling thread on
/// the engine's runtime until the async operation completes. This is precisely
/// the surface `hekma` drives.
pub struct Blocking<'a> {
    engine: &'a Engine,
}

impl Blocking<'_> {
    /// Blocking [`Engine::register`].
    pub fn register(&self, name: &str, kind: &str) -> Result<AgentInstance, RegistryError> {
        self.engine.rt.block_on(self.engine.register(name, kind))
    }

    /// Blocking [`Engine::register_with_adapter`].
    pub fn register_with_adapter(
        &self,
        name: &str,
        reference: &AdapterRef,
    ) -> Result<AgentInstance, RegistryError> {
        self.engine
            .rt
            .block_on(self.engine.register_with_adapter(name, reference))
    }

    /// Blocking [`Engine::remove`].
    pub fn remove(
        &self,
        name: &str,
        disposition: RemoveDisposition,
        force: bool,
    ) -> Result<(), RegistryError> {
        self.engine
            .rt
            .block_on(self.engine.remove(name, disposition, force))
    }

    /// Blocking [`Engine::list`].
    pub fn list(&self) -> Result<Vec<AgentInstance>, RegistryError> {
        self.engine.rt.block_on(self.engine.list())
    }

    /// Blocking [`Engine::binding`] (story 23-1).
    pub fn binding(&self, name: &str) -> Result<Option<Binding>, BindingError> {
        self.engine.rt.block_on(self.engine.binding(name))
    }

    /// Blocking [`Engine::bindings`] (story 23-1).
    pub fn bindings(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Binding>, BindingError> {
        self.engine.rt.block_on(self.engine.bindings(after, limit))
    }

    /// Blocking [`Engine::binding_credentials`] (story 23-1).
    pub fn binding_credentials(&self, name: &str) -> Result<Vec<BindingCredential>, BindingError> {
        self.engine
            .rt
            .block_on(self.engine.binding_credentials(name))
    }

    /// Blocking [`Engine::fleet`] (story 1-7, FR-4). The Fleet as serializable
    /// [`FleetEntry`] rows — what `hekma agent list [--json]` renders.
    pub fn fleet(&self) -> Result<Vec<FleetEntry>, RegistryError> {
        self.engine.rt.block_on(self.engine.fleet())
    }

    /// Blocking [`Engine::fleet_entry`] (AI-15) — the single-instance Fleet
    /// read `hekma agent show <name> --json` uses. O(1) keyed lookup + the SAME
    /// row composition as `list`; unknown names surface the registry's
    /// `NotFound` (the CLI's exit-3 diagnostic).
    pub fn fleet_entry(&self, name: &str) -> Result<FleetEntry, RegistryError> {
        self.engine.rt.block_on(self.engine.fleet_entry(name))
    }

    /// Blocking [`Engine::log_capture_mode`] (story 17-4, review finding 21:
    /// the facade parity audit requires every public async entry point to
    /// have this twin — embedders on the sync facade read the same posture).
    pub fn log_capture_mode(&self) -> LogCaptureMode {
        self.engine.rt.block_on(self.engine.log_capture_mode())
    }

    /// Blocking [`Engine::has_live_capture`].
    pub fn has_live_capture(&self, name: &str) -> bool {
        self.engine.rt.block_on(self.engine.has_live_capture(name))
    }

    /// Blocking [`Engine::effective_capabilities`].
    pub fn effective_capabilities(
        &self,
        name: &str,
    ) -> Result<EffectiveCapabilities, RegistryError> {
        self.engine
            .rt
            .block_on(self.engine.effective_capabilities(name))
    }

    /// Blocking [`Engine::start`].
    pub fn start(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.start(name))
    }

    /// Blocking [`Engine::start_detached`] (story 12-1).
    pub fn start_detached(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.start_detached(name))
    }

    /// Blocking [`Engine::stop`].
    pub fn stop(&self, name: &str, window: Option<Duration>) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.stop(name, window))
    }

    /// Blocking [`Engine::shutdown`] — the fleet-wide stop ladder for
    /// embedders (stop every live agent gracefully, then drop the engine).
    pub fn shutdown(&self, window: Duration, notes: Option<&DiagnosticSink>) -> Vec<DrainOutcome> {
        self.engine.rt.block_on(self.engine.shutdown(window, notes))
    }

    /// Blocking [`Engine::pause`].
    pub fn pause(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.pause(name))
    }

    /// Blocking [`Engine::resume`].
    pub fn resume(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.resume(name))
    }

    /// Blocking [`Engine::drain`] (story 18-4): the composite returns the
    /// STOPPED instance plus the durable [`DrainOutcome`] — both outcomes
    /// (completed and timed-out) are successes here; check
    /// `report.outcome.completed` for the wait's truth.
    pub fn drain(
        &self,
        name: &str,
        timeout_override: Option<Duration>,
    ) -> Result<DrainReport, EngineError> {
        self.engine
            .rt
            .block_on(self.engine.drain(name, timeout_override))
    }

    /// Blocking [`Engine::send_input`].
    pub fn send_input(&self, name: &str, text: &str) -> Result<(), EngineError> {
        self.engine.rt.block_on(self.engine.send_input(name, text))
    }

    /// Blocking [`Engine::subscribe`] (story 7-2, FR-33): returns an
    /// [`EventSubscription`] whose `recv` bridges the async receiver through
    /// the engine runtime — the same facade pattern as every other method
    /// here. Async consumers can call [`Engine::subscribe`] directly and use
    /// the raw receiver instead.
    pub fn subscribe(&self) -> EventSubscription {
        EventSubscription {
            rx: self.engine.subscribe(),
            rt: Arc::clone(&self.engine.rt),
        }
    }

    /// Blocking install (or replace) of the story-10-2 diagnostic sink — the
    /// sync-surface form of [`Engine::with_diagnostics`]. Like `subscribe`,
    /// this is a DIRECT sync call, not a `block_on` bridge: it takes the
    /// supervisor lock for one field swap and returns. The engine's two
    /// operational diagnostics then route to `sink` (exact stderr bytes, one
    /// line each) instead of stderr; the no-sink default is unchanged.
    /// Installing REPLACES (never removes) — there is no uninstall back to
    /// the stderr default; see [`Engine::with_diagnostics`] for the one-way
    /// install note. See [`DiagnosticSink`](crate::DiagnosticSink) for the
    /// contract.
    pub fn with_diagnostics(&self, sink: DiagnosticSink) {
        self.engine.with_diagnostics(sink);
    }

    /// Blocking [`Engine::transition_events`].
    pub fn transition_events(&self, name: &str) -> Result<Vec<TransitionEvent>, EngineError> {
        self.engine.rt.block_on(self.engine.transition_events(name))
    }

    /// Blocking [`Engine::budget_breach_events`] (story 3-2, AC7).
    pub fn budget_breach_events(&self, name: &str) -> Result<Vec<BudgetBreachEvent>, EngineError> {
        self.engine
            .rt
            .block_on(self.engine.budget_breach_events(name))
    }

    /// Blocking [`Engine::resync_events`] (story 10-3) — the crash-window
    /// backfill: committed events past `after` as bus payloads, plus the
    /// cursor for the next call. See the async method for the full contract
    /// (both orderings and their tradeoffs, the surfaced torn-tail skip, and
    /// the truncation guard).
    pub fn resync_events(
        &self,
        name: &str,
        after: ResyncCursor,
    ) -> Result<ResyncBatch, EngineError> {
        self.engine
            .rt
            .block_on(self.engine.resync_events(name, after))
    }

    /// Blocking [`Engine::export_events`] (story 17-4) — the ephemeral-host
    /// export pull the `kt agent serve` Control API route drives.
    pub fn export_events(
        &self,
        after: Option<OutboxCursor>,
        max: u32,
    ) -> Result<ExportBatch, EngineError> {
        self.engine
            .rt
            .block_on(self.engine.export_events(after, max))
    }

    /// Blocking [`Engine::ack_export`] (story 17-4) — the monotonic durable
    /// ack advance the Control API's ack route drives.
    pub fn ack_export(&self, seq: u64) -> Result<u64, EngineError> {
        self.engine.rt.block_on(self.engine.ack_export(seq))
    }

    /// Blocking [`Engine::read_agent_log`] (story 4-2, AC-A).
    pub fn read_agent_log(&self, name: &str) -> Result<(Vec<LogLine>, u64), EngineError> {
        self.engine.rt.block_on(self.engine.read_agent_log(name))
    }

    /// Blocking [`Engine::read_agent_log_since`] (story 4-2, AC-B/AC-C/AC-H).
    pub fn read_agent_log_since(
        &self,
        name: &str,
        cursor: u64,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        self.engine
            .rt
            .block_on(self.engine.read_agent_log_since(name, cursor))
    }

    /// Blocking [`Engine::read_raw_output_since`] (story 15-2) — the raw
    /// `agent.log` fallback follow read.
    pub fn read_raw_output_since(
        &self,
        name: &str,
        cursor: Option<u64>,
    ) -> Result<(Vec<LogLine>, u64), EngineError> {
        self.engine
            .rt
            .block_on(self.engine.read_raw_output_since(name, cursor))
    }

    /// Blocking [`Engine::prime_raw_backlog`] — the bounded raw-backlog
    /// prime the follow loop resolves its starting cursor through.
    pub fn prime_raw_backlog(
        &self,
        name: &str,
        backlog_lines: u64,
    ) -> Result<(u64, u64), EngineError> {
        self.engine
            .rt
            .block_on(self.engine.prime_raw_backlog(name, backlog_lines))
    }

    /// Blocking [`Engine::instance_status`] (story 1-6, AC9).
    pub fn instance_status(&self, name: &str) -> Result<InstanceStatus, EngineError> {
        self.engine.rt.block_on(self.engine.instance_status(name))
    }

    /// Blocking [`Engine::set_restart_policy`] (story 1-6, AC4).
    pub fn set_restart_policy(&self, name: &str, policy: RestartPolicy) -> Result<(), EngineError> {
        self.engine
            .rt
            .block_on(self.engine.set_restart_policy(name, policy))
    }

    /// Blocking [`Engine::effective_config`] (story 2-1, AC-A/AC10). The read
    /// `hekma agent config get` uses.
    pub fn effective_config(
        &self,
        name: &str,
        overrides: ConfigLayer,
    ) -> Result<EffectiveConfig, ConfigError> {
        self.engine
            .rt
            .block_on(self.engine.effective_config(name, overrides))
    }

    /// Blocking [`Engine::set_config`] (story 2-1, AC-B/AC10). The write
    /// `hekma agent config set` uses. On success carries the ZERO OR MORE
    /// WARN-ONLY steering warnings (story 11-2, AI-33) the caller renders on
    /// its own stderr — empty on the common path.
    pub fn set_config(
        &self,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<Vec<String>, ConfigError> {
        self.engine
            .rt
            .block_on(self.engine.set_config(name, key, value))
    }

    /// Blocking twin of [`Engine::open_child_run`].
    pub fn open_child_run(&self, request: ChildRunRequest) -> Result<RunView, RunError> {
        self.engine.rt.block_on(self.engine.open_child_run(request))
    }

    /// Blocking twin of [`Engine::close_run`].
    pub fn close_run(&self, run_id: &str) -> Result<RunView, RunError> {
        self.engine.rt.block_on(self.engine.close_run(run_id))
    }

    /// Blocking twin of [`Engine::run`].
    pub fn run(&self, run_id: &str) -> Result<RunView, RunError> {
        self.engine.rt.block_on(self.engine.run(run_id))
    }

    /// Blocking twin of [`Engine::instance_run`].
    pub fn instance_run(&self, instance: &str) -> Result<RunView, RunError> {
        self.engine.rt.block_on(self.engine.instance_run(instance))
    }

    /// Blocking twin of [`Engine::label_run`].
    pub fn label_run(
        &self,
        run_id: &str,
        change: LabelChange,
        dry_run: bool,
    ) -> Result<LabelOutcome, RunError> {
        self.engine
            .rt
            .block_on(self.engine.label_run(run_id, change, dry_run))
    }

    /// Blocking twin of [`Engine::select_runs`].
    pub fn select_runs(&self, selector: &Selector) -> Result<Vec<RunRecord>, RunError> {
        self.engine.rt.block_on(self.engine.select_runs(selector))
    }

    /// Blocking twin of [`Engine::select_run_summaries`].
    pub fn select_run_summaries(&self, selector: &Selector) -> Result<Vec<RunSummary>, RunError> {
        self.engine
            .rt
            .block_on(self.engine.select_run_summaries(selector))
    }

    /// Blocking twin of [`Engine::plan_run_verb`].
    pub fn plan_run_verb(
        &self,
        run_id: &str,
        verb: RunLifecycleVerb,
    ) -> Result<RunRecord, RunError> {
        self.engine
            .rt
            .block_on(self.engine.plan_run_verb(run_id, verb))
    }

    /// Blocking twin of [`Engine::stop_run`].
    pub fn stop_run(
        &self,
        run_id: &str,
        window: Option<Duration>,
    ) -> Result<AgentInstance, RunVerbError> {
        self.engine
            .rt
            .block_on(self.engine.stop_run(run_id, window))
    }

    /// Blocking twin of [`Engine::pause_run`].
    pub fn pause_run(&self, run_id: &str) -> Result<AgentInstance, RunVerbError> {
        self.engine.rt.block_on(self.engine.pause_run(run_id))
    }

    /// Blocking twin of [`Engine::resume_run`].
    pub fn resume_run(&self, run_id: &str) -> Result<AgentInstance, RunVerbError> {
        self.engine.rt.block_on(self.engine.resume_run(run_id))
    }

    /// Blocking twin of [`Engine::drain_run`].
    pub fn drain_run(
        &self,
        run_id: &str,
        timeout_override: Option<Duration>,
    ) -> Result<DrainReport, RunVerbError> {
        self.engine
            .rt
            .block_on(self.engine.drain_run(run_id, timeout_override))
    }

    /// Blocking twin of [`Engine::set_run_limits`].
    pub fn set_run_limits(
        &self,
        run_ids: Vec<String>,
        change: RunLimitChange,
        actor: LimitActor,
        dry_run: bool,
    ) -> Result<crate::domain::RunLimitsResults, RunError> {
        self.engine
            .rt
            .block_on(self.engine.set_run_limits(run_ids, change, actor, dry_run))
    }

    /// Blocking twin of [`Engine::credential_state`].
    pub fn credential_state(
        &self,
        presented: &str,
    ) -> Result<crate::domain::PresentedState, RunError> {
        self.engine
            .rt
            .block_on(self.engine.credential_state(presented))
    }

    /// Blocking twin of [`Engine::open_child_run_as`].
    pub fn open_child_run_as(
        &self,
        actor: crate::domain::RunActor,
        request: ChildRunRequest,
    ) -> Result<crate::domain::OpenedRun, RunError> {
        self.engine
            .rt
            .block_on(self.engine.open_child_run_as(actor, request))
    }

    /// Blocking twin of [`Engine::close_run_as`].
    pub fn close_run_as(
        &self,
        actor: crate::domain::RunActor,
        run_id: &str,
    ) -> Result<RunView, RunError> {
        self.engine
            .rt
            .block_on(self.engine.close_run_as(actor, run_id))
    }

    /// Blocking twin of [`Engine::set_run_limits_as`].
    pub fn set_run_limits_as(
        &self,
        run_ids: Vec<String>,
        change: RunLimitChange,
        actor: crate::domain::RunActor,
        dry_run: bool,
    ) -> Result<crate::domain::RunLimitsResults, RunError> {
        self.engine.rt.block_on(
            self.engine
                .set_run_limits_as(run_ids, change, actor, dry_run),
        )
    }

    /// Blocking twin of [`Engine::refresh_credentials`].
    pub fn refresh_credentials(
        &self,
        presented: &crate::domain::SecretString,
    ) -> Result<crate::domain::IssuedCredentials, RunError> {
        self.engine
            .rt
            .block_on(self.engine.refresh_credentials(presented))
    }

    /// Blocking twin of [`Engine::revoke_run_keys`].
    pub fn revoke_run_keys(
        &self,
        run_id: &str,
        cascade: bool,
    ) -> Result<crate::domain::RevokeOutcome, RunError> {
        self.engine
            .rt
            .block_on(self.engine.revoke_run_keys(run_id, cascade))
    }

    /// Blocking twin of [`Engine::revoke_exposed`].
    pub fn revoke_exposed(
        &self,
        presented: Vec<crate::domain::SecretString>,
    ) -> Result<usize, RunError> {
        self.engine
            .rt
            .block_on(self.engine.revoke_exposed(presented))
    }

    /// Blocking twin of [`Engine::run_keys`].
    pub fn run_keys(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::domain::CredentialSummary>, RunError> {
        self.engine.rt.block_on(self.engine.run_keys(run_id))
    }

    /// Blocking twin of [`Engine::run_limits_view`].
    pub fn run_limits_view(&self, run_id: &str) -> Result<RunLimitsView, RunError> {
        self.engine.rt.block_on(self.engine.run_limits_view(run_id))
    }

    /// Blocking twin of [`Engine::seed_runs_for_perf`] (the perf harness's
    /// `#[doc(hidden)]` seeding seam).
    #[doc(hidden)]
    pub fn seed_runs_for_perf(
        &self,
        instance: &str,
        rows: Vec<crate::domain::runs::PerfRunRow>,
    ) -> Result<(), RunError> {
        self.engine
            .rt
            .block_on(self.engine.seed_runs_for_perf(instance, rows))
    }

    /// Blocking twin of [`Engine::list_runs`] — `hekma ps`'s read.
    pub fn list_runs(&self, query: ListRunsQuery) -> Result<RunListing, RunError> {
        self.engine.rt.block_on(self.engine.list_runs(query))
    }

    /// Blocking twin of [`Engine::event_subject_labels`] — the SSE
    /// Selector filter's read for sync consumers.
    pub fn event_subject_labels(
        &self,
        event: &EngineEvent,
    ) -> Result<Option<Vec<Label>>, RunError> {
        self.engine
            .rt
            .block_on(self.engine.event_subject_labels(event))
    }

    /// Blocking twin of [`Engine::policies`].
    pub fn policies(&self) -> Result<Vec<PolicyRecord>, RunError> {
        self.engine.rt.block_on(self.engine.policies())
    }

    /// Blocking twin of [`Engine::policy`].
    pub fn policy(&self, name: &str) -> Result<PolicyRecord, RunError> {
        self.engine.rt.block_on(self.engine.policy(name))
    }

    /// Blocking twin of [`Engine::create_policy`].
    pub fn create_policy(
        &self,
        change: PolicyChange,
        dry_run: bool,
    ) -> Result<PolicyOutcome, RunError> {
        self.engine
            .rt
            .block_on(self.engine.create_policy(change, dry_run))
    }

    /// Blocking twin of [`Engine::change_policy`].
    pub fn change_policy(
        &self,
        change: PolicyChange,
        dry_run: bool,
    ) -> Result<PolicyOutcome, RunError> {
        self.engine
            .rt
            .block_on(self.engine.change_policy(change, dry_run))
    }

    /// Blocking twin of [`Engine::list_calls`].
    pub fn list_calls(
        &self,
        query: crate::domain::calls::CallsQuery,
    ) -> Result<crate::domain::calls::CallListing, crate::domain::calls::CallError> {
        self.engine.rt.block_on(self.engine.list_calls(query))
    }

    /// Blocking twin of [`Engine::call`].
    pub fn call(
        &self,
        call_id: &str,
    ) -> Result<crate::domain::calls::CallRecord, crate::domain::calls::CallError> {
        self.engine.rt.block_on(self.engine.call(call_id))
    }

    /// Blocking twin of [`Engine::delete_policy`].
    pub fn delete_policy(&self, name: &str, dry_run: bool) -> Result<PolicyOutcome, RunError> {
        self.engine
            .rt
            .block_on(self.engine.delete_policy(name, dry_run))
    }

    /// Blocking [`Engine::reveal_secrets`] (story 2-4, AC-C/AC11). The
    /// `config get --reveal` un-mask uses this to fetch the resolved cleartext of
    /// the secret leaves.
    pub fn reveal_secrets(
        &self,
        name: &str,
        overrides: ConfigLayer,
    ) -> Result<std::collections::BTreeMap<String, String>, ConfigError> {
        self.engine
            .rt
            .block_on(self.engine.reveal_secrets(name, overrides))
    }

    /// Blocking [`Engine::attach_memory`] (story 5-1). Returns the engine-computed
    /// managed directory path (path authority — display it, never reconstruct it).
    pub fn attach_memory(
        &self,
        name: &str,
        kind: crate::ports::MemoryBackingKind,
    ) -> Result<PathBuf, RegistryError> {
        self.engine
            .rt
            .block_on(self.engine.attach_memory(name, kind))
    }

    /// Blocking [`Engine::detach_memory`] (story 5-1).
    pub fn detach_memory(&self, name: &str) -> Result<(), RegistryError> {
        self.engine.rt.block_on(self.engine.detach_memory(name))
    }

    /// Blocking [`Engine::memory_status`] (story 5-1).
    pub fn memory_status(
        &self,
        name: &str,
    ) -> Result<Option<crate::ports::MemoryBackingStatus>, RegistryError> {
        self.engine.rt.block_on(self.engine.memory_status(name))
    }

    /// Blocking twin of [`Engine::stop_metering_proxy`].
    pub fn stop_metering_proxy(&self, grace: Duration) {
        self.engine
            .rt
            .block_on(self.engine.stop_metering_proxy(grace))
    }

    /// Blocking twin of [`Engine::create_binding`].
    pub fn create_binding(&self, spec: BindingSpec) -> Result<Binding, BindingError> {
        self.engine.rt.block_on(self.engine.create_binding(spec))
    }

    /// Blocking twin of [`Engine::update_binding`].
    pub fn update_binding(&self, name: &str, patch: BindingPatch) -> Result<Binding, BindingError> {
        self.engine
            .rt
            .block_on(self.engine.update_binding(name, patch))
    }

    /// Blocking twin of [`Engine::delete_binding`].
    pub fn delete_binding(&self, name: &str) -> Result<(), BindingError> {
        self.engine.rt.block_on(self.engine.delete_binding(name))
    }

    /// Blocking twin of [`Engine::put_binding_route`].
    pub fn put_binding_route(&self, name: &str, route: RouteSpec) -> Result<Binding, BindingError> {
        self.engine
            .rt
            .block_on(self.engine.put_binding_route(name, route))
    }

    /// Blocking twin of [`Engine::remove_binding_route`].
    pub fn remove_binding_route(&self, name: &str, route: &str) -> Result<Binding, BindingError> {
        self.engine
            .rt
            .block_on(self.engine.remove_binding_route(name, route))
    }

    /// Blocking twin of [`Engine::issue_binding_credential`].
    pub fn issue_binding_credential(
        &self,
        name: &str,
        class: BindingCredentialClass,
    ) -> Result<IssuedBindingCredential, BindingError> {
        self.engine
            .rt
            .block_on(self.engine.issue_binding_credential(name, class))
    }

    /// Blocking twin of [`Engine::revoke_binding_credential`].
    pub fn revoke_binding_credential(
        &self,
        name: &str,
        class: BindingCredentialClass,
    ) -> Result<(), BindingError> {
        self.engine
            .rt
            .block_on(self.engine.revoke_binding_credential(name, class))
    }
}

/// A live, sync-side subscription to the engine's event bus (story 7-2).
///
/// Obtained via [`Blocking::subscribe`]. Wraps the broadcast receiver plus the
/// engine runtime handle so a synchronous Host can consume events without an
/// async context — the same bridge every [`Blocking`] method uses, applied to
/// `recv`.
///
/// ## The do-not-call-from-a-worker contract
///
/// `recv` (and only `recv`) runs `runtime.block_on`, so it MUST NOT be called
/// from a thread that is itself driving the engine's runtime — a tokio worker
/// or a `spawn_blocking` closure inside an async context (e.g. from a task
/// that awaited another `Engine` method). `Runtime::block_on` panics there
/// ("cannot block the calling thread"/"cannot start a runtime from within a
/// runtime"), by tokio's rules — the same contract the rest of the facade
/// carries. Consume the subscription on your OWN thread (`std::thread::spawn`
/// or the host's executor), or take [`Engine::subscribe`]'s raw receiver and
/// `await` it inside the runtime instead.
///
/// ## The runtime-keepalive contract (a Host must DROP its subscriptions)
///
/// This struct holds an `Arc` to the engine's runtime — deliberately a STRONG
/// handle, not a `Weak`. The consequence is a runtime-lifetime contract a
/// host must know: **while any [`EventSubscription`] is alive, the engine's
/// async runtime stays alive**, even after the `Engine` (and every facade
/// view of it) has been dropped. This is what makes a subscription usable for
/// the natural drain-at-shutdown pattern (drop the engine, then keep reading
/// the already-published events to the tail); it cannot deadlock or dangle —
/// `recv` simply keeps serving buffered events, and returns
/// `RecvError::Closed` once the buffer is drained and the engine's sender
/// side is gone. The flip side: a host that leaks a subscription leaks the
/// runtime's threads with it. **To release the runtime, drop every
/// [`EventSubscription`] (and then the engine).** A `Weak` handle was
/// considered and rejected: a runtime that could vanish under a live
/// subscription would turn `recv` into a surprise-panic surface, which is
/// worse for an embedder than an explicit drop obligation.
pub struct EventSubscription {
    rx: broadcast::Receiver<EngineEvent>,
    rt: Arc<Runtime>,
}

impl EventSubscription {
    /// Block the calling thread until the next committed event arrives.
    ///
    /// Errors with `RecvError::Closed` when the engine (and every other
    /// receiver's sender side) is gone, or with `RecvError::Lagged(n)` after
    /// this receiver fell more than
    /// [`EVENT_BUS_CAPACITY`](crate::EVENT_BUS_CAPACITY) events behind — the
    /// documented slow-subscriber policy: supervision was NEVER stalled by the
    /// stall here, the `n` dropped events stay readable in the durable logs
    /// (the query APIs), and the receiver resyncs at the current tail and
    /// keeps working.
    pub fn recv(&mut self) -> Result<EngineEvent, broadcast::error::RecvError> {
        self.rt.block_on(self.rx.recv())
    }

    /// Non-blocking variant: the next buffered event, `TryRecvError::Empty`
    /// when the tail is reached, `Lagged`/`Closed` per [`Self::recv`].
    pub fn try_recv(&mut self) -> Result<EngineEvent, broadcast::error::TryRecvError> {
        self.rx.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NameError;
    use crate::ports::StoreError;

    #[test]
    fn the_sse_filter_resolves_an_open_run_without_the_registry_lock() {
        // An open Run lives in the published snapshot: the Selector filter's
        // label read answers from it lock-free — even while the registry
        // mutex (the one the supervisor's poll needs) is held elsewhere.
        let tmp = tempfile::TempDir::new().unwrap();
        drop(Engine::open(Some(tmp.path().to_path_buf())).unwrap());
        {
            let conn = rusqlite::Connection::open(tmp.path().join("state.db")).unwrap();
            conn.execute_batch(
                "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES ('demo', 'mock', 'running', '/h', 't', 't'); \
                 INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 instance_id, process_owner, created_at, updated_at) SELECT 'run_S', \
                 'supervised', 0, 'active', '', id, 0, 't', 't' FROM agent_instances \
                 WHERE name = 'demo'; \
                 INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
                 ('run_S', 'team', 'core', 'bound', 'config');",
            )
            .unwrap();
        }
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let event = EngineEvent::BudgetBreach(crate::domain::BudgetBreachEvent::new(
            "demo",
            "run_S",
            crate::domain::BreachScope::PerRun,
            1,
            1,
            crate::domain::BreachAction::Warn,
            "self-reported",
            "t",
        ));
        let guard = engine.inner.registry.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let engine = &engine;
            let event = &event;
            scope.spawn(move || {
                let _ = tx.send(engine.blocking().event_subject_labels(event));
            });
            let answer = rx.recv_timeout(std::time::Duration::from_secs(5));
            drop(guard);
            let labels = answer
                .expect("answered while the registry lock was held")
                .unwrap()
                .expect("the open Run's labels");
            assert!(labels.contains(&Label::bound("team", "core")), "{labels:?}");
        });
        // An event naming no Run answers None on the same fast path.
        let policy = EngineEvent::Policy(Box::new(crate::domain::runs::PolicyEvent {
            schema_version: crate::domain::runs::POLICY_EVENT_SCHEMA_VERSION,
            kind: crate::domain::runs::PolicyEventKind::Created,
            name: "p".to_string(),
            selector: None,
            limits: None,
            prior_limits: None,
            rates: Default::default(),
            matched: 0,
            fallbacks: 0,
            at: "t".to_string(),
        }));
        assert_eq!(
            engine.blocking().event_subject_labels(&policy).unwrap(),
            None
        );
    }

    #[test]
    fn an_embedder_drives_the_whole_run_key_lifecycle_through_the_blocking_facade() {
        // Story 22-1, the embedder's path (no host): the Operator opens a
        // child with credentials; its Run Key opens, tightens and closes
        // beneath it; the listing shows metadata only; a refresh rotates
        // once; revoke-keys cascades; and the revoked keys are refused.
        use crate::domain::{
            ChildRunRequest, PresentedState, RunActor, RunLimitChange, RunLimits, RunParent,
        };
        let tmp = tempfile::TempDir::new().unwrap();
        drop(Engine::open(Some(tmp.path().to_path_buf())).unwrap());
        rusqlite::Connection::open(tmp.path().join("state.db"))
            .unwrap()
            .execute_batch(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 process_owner, created_at, updated_at) \
                 VALUES ('run_ROOT', 'announced', 0, 'active', '', 0, 't', 't');",
            )
            .unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        let under = |parent: &str| {
            let mut request = ChildRunRequest::under(RunParent::Run(RunId::from_wire(parent)));
            request.issue_credentials = true;
            request
        };
        let child = facade
            .open_child_run_as(RunActor::Operator, under("run_ROOT"))
            .unwrap();
        let child_id = child.view.record.run_id.to_string();
        let issued = child.credentials.expect("issued once");
        let caller = engine
            .authenticate(issued.run_key().expose_secret())
            .expect("the key is indexed")
            .actor();
        let grandchild = facade
            .open_child_run_as(caller.clone(), under(&child_id))
            .unwrap();
        let gc_id = grandchild.view.record.run_id.to_string();
        // Tighten-only on its own subtree.
        let tighten = RunLimitChange {
            set: RunLimits {
                token_per_run: Some(5_000),
                ..RunLimits::default()
            },
            clear: Vec::new(),
        };
        let mut results = facade
            .set_run_limits_as(vec![gc_id.clone()], tighten, caller.clone(), false)
            .unwrap();
        assert!(results.pop().unwrap().1.is_ok());
        let closed = facade.close_run_as(caller.clone(), &gc_id).unwrap();
        assert!(closed.record.is_ended());
        // The listing is metadata only.
        let keys = facade.run_keys(&child_id).unwrap();
        assert_eq!(keys.len(), 2);
        let listed = format!("{keys:?}");
        assert!(
            !listed.contains(issued.run_key().expose_secret()),
            "{listed}"
        );
        // A refresh rotates once; the old key is refused from then on.
        let rotated = facade
            .refresh_credentials(issued.refresh_credential())
            .unwrap();
        assert!(engine
            .authenticate(issued.run_key().expose_secret())
            .is_none());
        assert_eq!(
            facade
                .credential_state(issued.run_key().expose_secret())
                .unwrap(),
            PresentedState::Revoked
        );
        assert!(matches!(
            facade
                .credential_state(issued.refresh_credential().expose_secret())
                .unwrap(),
            PresentedState::Used(_)
        ));
        // revoke-keys cascades, and a request authenticated just before it
        // is refused inside its own job.
        let rotated_caller = engine
            .authenticate(rotated.run_key().expose_secret())
            .expect("the rotated key is indexed")
            .actor();
        let outcome = facade.revoke_run_keys(&child_id, true).unwrap();
        assert!(!outcome.revoked.is_empty());
        assert!(engine
            .authenticate(rotated.run_key().expose_secret())
            .is_none());
        assert!(matches!(
            facade.close_run_as(rotated_caller, &child_id),
            Err(RunError::CredentialRevoked)
        ));
        let _ = caller;
        assert!(matches!(
            facade.run_keys("run_missing"),
            Err(RunError::NotFound { .. })
        ));
    }

    #[test]
    fn unresolvable_credentials_never_take_an_engine_lock_or_a_writer_job() {
        // Story 22-1 (the security review's M1): a credential that does not
        // resolve costs reads only. A flood of token-shaped values that were
        // never issued — Run Keys on any route, refresh credentials on the
        // refresh route, tokens in a URL — is answered while another thread
        // HOLDS the registry and supervisor mutexes, and the store writer
        // runs no job for any of it.
        use crate::domain::credential::{mint, TokenClass};
        use crate::domain::SecretString;
        use std::sync::atomic::Ordering;
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let registry = engine.inner.registry.lock().unwrap();
        let supervisor = engine.inner.supervisor.lock().unwrap();
        // Let anything already in flight land before counting.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let jobs = |registry: &Registry| registry.writer().stats().jobs.load(Ordering::Relaxed);
        let before = jobs(&registry);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let engine = &engine;
            scope.spawn(move || {
                let facade = engine.blocking();
                let token = |class| mint(class).unwrap().secret().expose_secret().to_string();
                for _ in 0..100 {
                    for class in [
                        TokenClass::RunKey,
                        TokenClass::Refresh,
                        TokenClass::Announce,
                    ] {
                        assert_eq!(
                            facade.credential_state(&token(class)).unwrap(),
                            crate::domain::PresentedState::Unknown
                        );
                    }
                    let refresh = SecretString::new(token(TokenClass::Refresh));
                    assert!(matches!(
                        facade.refresh_credentials(&refresh),
                        Err(RunError::CredentialUnknown)
                    ));
                    // A Run Key presented on the refresh route is not a
                    // refresh credential either.
                    let key = SecretString::new(token(TokenClass::RunKey));
                    assert!(matches!(
                        facade.refresh_credentials(&key),
                        Err(RunError::CredentialUnknown)
                    ));
                    let exposed = [
                        TokenClass::RunKey,
                        TokenClass::Refresh,
                        TokenClass::Announce,
                    ]
                    .map(|class| SecretString::new(token(class)))
                    .into_iter()
                    .collect();
                    assert_eq!(facade.revoke_exposed(exposed).unwrap(), 0);
                }
                let _ = tx.send(());
            });
            let answered = rx.recv_timeout(std::time::Duration::from_secs(20));
            let after = jobs(&registry);
            drop(supervisor);
            drop(registry);
            answered.expect("every unresolvable presentation answered while both locks were held");
            assert_eq!(
                after, before,
                "no writer job ran for an unresolvable credential"
            );
        });
    }

    #[test]
    fn config_facade_round_trip_and_invalid_name_arms() {
        // Story 2-1: cover the engine config facade end-to-end through the
        // blocking wrapper — a set/get round trip plus the InvalidName arm on both
        // methods (a malformed name is rejected as ConfigError::InvalidName before
        // any layer is touched).
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade.register("demo", "mock").unwrap();

        // Round trip through the facade: set then effective_config reflects it.
        facade.set_config("demo", "model", "gpt-4").unwrap();
        let eff = facade
            .effective_config("demo", ConfigLayer::empty())
            .unwrap();
        assert_eq!(eff.value_display("model").as_deref(), Some("gpt-4"));

        // InvalidName arm on effective_config (a space is an illegal name char).
        assert!(matches!(
            facade
                .effective_config("Bad Name", ConfigLayer::empty())
                .unwrap_err(),
            ConfigError::InvalidName { name, .. } if name == "Bad Name"
        ));
        // InvalidName arm on set_config.
        assert!(matches!(
            facade.set_config("Bad Name", "model", "x").unwrap_err(),
            ConfigError::InvalidName { name, .. } if name == "Bad Name"
        ));
    }

    #[test]
    fn reveal_secrets_rejects_a_malformed_name_before_any_resolution() {
        // The reveal path is read-only, but it still validates the name
        // FIRST: a malformed name is the typed InvalidName — never a path
        // into the registry layout, never a resolution attempt.
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        assert!(matches!(
            facade
                .reveal_secrets("Bad Name", ConfigLayer::empty())
                .unwrap_err(),
            ConfigError::InvalidName { name, .. } if name == "Bad Name"
        ));
    }

    #[test]
    fn failed_cause_detail_is_none_for_a_cause_that_is_not_a_failure() {
        // The `instance_status` event-log fallback must not invent a failure
        // detail for a cause that does not describe one — a graceful stop
        // carries no crash text, so the status line stays plain.
        assert_eq!(
            failed_cause_detail(&TransitionCause::StopGraceful),
            None,
            "a graceful stop is not a failure cause"
        );
        assert_eq!(
            failed_cause_detail(&TransitionCause::AdapterReady),
            None,
            "an adapter-ready transition is not a failure cause"
        );
        // 18-3 review pin: a CLEAN EXIT is `stopped`, not `failed` — the
        // fallback must never dress a declared clean finish up as a
        // failure detail (a mutant that routes CleanExit into the
        // failure-bearing arms would fail here).
        assert_eq!(
            failed_cause_detail(&TransitionCause::CleanExit { code: 0 }),
            None,
            "a declared clean exit is not a failure cause"
        );
        // 18-4 review pin (B12): the cooperative lifecycle causes are state
        // transitions, not failures — a paused/resumed instance and a
        // drain's honest stopped outcome (COMPLETED or NOT — even the
        // overrun edge is a bounded stop, not a crash) must all render
        // detail-free here. A mutant that routes any of them into the
        // failure-bearing arms would invent failure text for a pause.
        assert_eq!(
            failed_cause_detail(&TransitionCause::pause_cooperative("the-pause --graceful")),
            None,
            "a cooperative pause is not a failure cause"
        );
        assert_eq!(
            failed_cause_detail(&TransitionCause::resume_cooperative("the-resume")),
            None,
            "a cooperative resume is not a failure cause"
        );
        assert_eq!(
            failed_cause_detail(&TransitionCause::drain_stopped(true, 5_000)),
            None,
            "a completed drain is not a failure cause"
        );
        assert_eq!(
            failed_cause_detail(&TransitionCause::drain_stopped(false, 5_000)),
            None,
            "an overran drain is a bounded stop, not a failure cause"
        );
    }

    #[test]
    fn stop_error_to_registry_maps_each_arm() {
        // AI-11: a live-instance teardown failure during `remove` is mapped into a
        // RegistryError so `remove` (which speaks RegistryError) aborts WITHOUT
        // deleting the row or orphaning the process. Exercise every arm.

        // Store passes through as Store.
        assert!(matches!(
            stop_error_to_registry(EngineError::Store(StoreError::Backend("db gone".into()))),
            RegistryError::Store(_)
        ));

        // InvalidName / NotFound keep their shape (defensive — a well-formed remove
        // has already resolved the name, but the mapping must stay total).
        assert!(matches!(
            stop_error_to_registry(EngineError::InvalidName {
                name: "Bad Name".into(),
                reason: NameError::BadChar,
            }),
            RegistryError::InvalidName { .. }
        ));
        assert!(matches!(
            stop_error_to_registry(EngineError::NotFound { name: "ghost".into() }),
            RegistryError::NotFound { name } if name == "ghost"
        ));

        // Any other stop failure (a backend terminate/signal error, a log error)
        // surfaces as a filesystem-shaped Io naming the teardown, preserving the
        // underlying detail so the operator sees WHY the remove aborted.
        let mapped = stop_error_to_registry(EngineError::Backend {
            name: "live".into(),
            source: crate::ports::BackendError::Control {
                op: "terminate",
                detail: "boom".into(),
            },
        });
        match mapped {
            RegistryError::Io { source, .. } => {
                let msg = source.to_string();
                assert!(msg.contains("could not stop the live instance"), "{msg}");
                assert!(msg.contains("boom"), "underlying detail preserved: {msg}");
            }
            other => panic!("expected Io for a backend teardown failure, got {other:?}"),
        }

        // A log-write teardown failure also folds into the Io arm.
        assert!(matches!(
            stop_error_to_registry(EngineError::Log {
                name: "live".into(),
                path: "/x/logs/instance.log".into(),
                detail: "disk full".into(),
            }),
            RegistryError::Io { .. }
        ));
    }

    #[test]
    fn the_run_facade_opens_closes_and_reads_runs_and_labels_rebind() {
        // Story 21-2: the async + blocking Run facade end to end, over a
        // supervised Run seeded through the registry (the builtin `mock`
        // cannot start a process).
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade.register("demo", "mock").unwrap();
        // No open Run yet: a labels write has nothing to rebind, and a child
        // has no parent.
        facade.set_config("demo", "labels.team", "core").unwrap();
        assert!(matches!(
            facade.open_child_run(ChildRunRequest::under(crate::domain::RunParent::Instance(
                "demo".to_string()
            ))),
            Err(RunError::NoOpenRun { .. })
        ));
        assert!(matches!(
            facade
                .set_config("demo", "labels.team", "has space")
                .unwrap_err(),
            ConfigError::InvalidValue { .. }
        ));
        let owner = RunId::mint();
        {
            let registry = engine.inner.registry.lock().unwrap();
            registry
                .ensure_supervised_run(crate::domain::runs::ops::SupervisedRun {
                    instance: InstanceName::new("demo").unwrap(),
                    run_id: owner.clone(),
                    wire_path: false,
                    labels: Vec::new(),
                    fresh: true,
                    inputs: None,
                    upstream: None,
                    custody: None,
                })
                .unwrap();
        }
        // A labels write while the Run is open rebinds it.
        facade
            .set_config("demo", "labels.team", "payments")
            .unwrap();
        let run = facade.run(owner.as_str()).unwrap();
        assert_eq!(
            run.labels,
            vec![crate::domain::Label::bound("team", "payments")]
        );
        let child = facade
            .open_child_run(ChildRunRequest::under(crate::domain::RunParent::Run(
                owner.clone(),
            )))
            .unwrap();
        assert_eq!(child.record.parent_run_id, Some(owner.clone()));
        let closed = facade.close_run(child.record.run_id.as_str()).unwrap();
        assert_eq!(closed.record.end_cause.as_deref(), Some("closed"));
        assert!(matches!(
            facade.close_run(owner.as_str()),
            Err(RunError::OwnsProcess { .. })
        ));
        assert!(matches!(
            facade.run("run_none"),
            Err(RunError::NotFound { .. })
        ));
    }

    #[test]
    fn the_run_verb_and_limit_facades_map_refuse_and_plan_per_run() {
        // Story 21-6: the Run-grain verbs and the budget job through the
        // async + blocking facade, over a supervised Run seeded through the
        // registry (the builtin `mock` cannot start a process).
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade.register("demo", "mock").unwrap();
        let owner = RunId::mint();
        {
            let registry = engine.inner.registry.lock().unwrap();
            registry
                .ensure_supervised_run(crate::domain::runs::ops::SupervisedRun {
                    instance: InstanceName::new("demo").unwrap(),
                    run_id: owner.clone(),
                    wire_path: false,
                    labels: Vec::new(),
                    fresh: true,
                    inputs: None,
                    upstream: None,
                    custody: None,
                })
                .unwrap();
        }
        let child = facade
            .open_child_run(ChildRunRequest::under(crate::domain::RunParent::Run(
                owner.clone(),
            )))
            .unwrap()
            .record
            .run_id;
        // The owner maps to the instance's verb (which refuses here: the
        // registered mock is not running — an ENGINE error, decoded as such).
        assert_eq!(
            facade
                .plan_run_verb(owner.as_str(), RunLifecycleVerb::Stop)
                .unwrap()
                .instance
                .as_deref(),
            Some("demo")
        );
        for result in [
            facade.stop_run(owner.as_str(), None).map(|_| ()),
            facade.pause_run(owner.as_str()).map(|_| ()),
            facade.resume_run(owner.as_str()).map(|_| ()),
            facade.drain_run(owner.as_str(), None).map(|_| ()),
        ] {
            assert!(matches!(result, Err(RunVerbError::Engine(_))), "{result:?}");
        }
        // The child is refused per Run, naming its depth.
        for result in [
            facade.stop_run(child.as_str(), None).map(|_| ()),
            facade.pause_run(child.as_str()).map(|_| ()),
            facade.resume_run(child.as_str()).map(|_| ()),
            facade.drain_run(child.as_str(), None).map(|_| ()),
        ] {
            match result {
                Err(RunVerbError::Run(err)) => assert_eq!(err.code(), "verb-unavailable"),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            facade
                .plan_run_verb(child.as_str(), RunLifecycleVerb::Pause)
                .unwrap_err()
                .code(),
            "verb-unavailable"
        );
        assert_eq!(
            facade
                .plan_run_verb("run_none", RunLifecycleVerb::Pause)
                .unwrap_err()
                .code(),
            "run-not-found"
        );
        assert!(matches!(
            facade.stop_run("run_none", None),
            Err(RunVerbError::Run(RunError::NotFound { .. }))
        ));
        // The budget job: a dry run plans, a write commits, a child's
        // cumulative ceiling is that Run's refusal.
        let change = RunLimitChange {
            set: crate::domain::RunLimits {
                token_per_run: Some(5_000),
                ..Default::default()
            },
            clear: Vec::new(),
        };
        let planned = facade
            .set_run_limits(
                vec![owner.to_string()],
                change.clone(),
                LimitActor::Operator,
                true,
            )
            .unwrap();
        assert!(planned[0].1.as_ref().unwrap().dry_run);
        assert_eq!(
            facade.run_limits_view(owner.as_str()).unwrap().limits,
            crate::domain::RunLimits::default()
        );
        let results = facade
            .set_run_limits(
                vec![owner.to_string(), child.to_string()],
                RunLimitChange {
                    set: crate::domain::RunLimits {
                        token_per_run: Some(5_000),
                        token_cumulative: Some(9_000),
                        ..Default::default()
                    },
                    clear: Vec::new(),
                },
                LimitActor::Operator,
                false,
            )
            .unwrap();
        assert!(results[0].1.is_ok());
        assert_eq!(
            results[1].1.as_ref().unwrap_err().code(),
            "scope-unavailable"
        );
        let view = facade.run_limits_view(owner.as_str()).unwrap();
        assert_eq!(view.limits.token_cumulative, Some(9_000));
        assert_eq!(
            view.resolution.sources.tokens_cumulative,
            crate::domain::LimitSource::Run
        );
        // A config write runs the re-arm pass (nothing latched here) and
        // leaves the Run-level limit on top.
        facade
            .set_config("demo", "budget.tokens.per_run", "100")
            .unwrap();
        assert_eq!(
            facade
                .run_limits_view(owner.as_str())
                .unwrap()
                .resolution
                .tokens
                .per_run,
            Some(5_000)
        );
        // A malformed change is refused whole.
        assert!(facade
            .set_run_limits(
                vec![owner.to_string()],
                RunLimitChange::default(),
                LimitActor::Operator,
                false
            )
            .is_err());
        // A read-only engine plans but refuses every write.
        drop(engine);
        let read_only = Engine::open_read_only(Some(tmp.path().to_path_buf())).unwrap();
        let ro = read_only.blocking();
        assert!(ro
            .set_run_limits(
                vec![owner.to_string()],
                change.clone(),
                LimitActor::Operator,
                true
            )
            .is_ok());
        assert!(ro
            .set_run_limits(vec![owner.to_string()], change, LimitActor::Operator, false)
            .is_err());
        assert!(matches!(
            ro.stop_run(owner.as_str(), None),
            Err(RunVerbError::Run(RunError::Store(_)))
        ));
    }

    #[test]
    fn the_label_facade_binds_selects_and_publishes_one_committed_event() {
        // Story 21-9: the async + blocking label verb end to end, over a
        // supervised Run seeded through the registry (the builtin `mock`
        // cannot start a process).
        use crate::domain::{Label, LabelChange, Selector};
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade.register("demo", "mock").unwrap();
        facade
            .set_config("demo", "labels.team", "payments")
            .unwrap();
        assert!(matches!(
            facade.instance_run("demo"),
            Err(RunError::NoOpenRun { .. })
        ));
        let missing = facade.instance_run("ghost").unwrap_err();
        assert!(
            matches!(missing, RunError::InstanceNotFound { .. }),
            "{missing}"
        );
        assert_eq!(missing.code(), "instance-not-found");
        assert!(matches!(
            facade.instance_run("Bad Name"),
            Err(RunError::Invalid { .. })
        ));
        let owner = RunId::mint();
        {
            let registry = engine.inner.registry.lock().unwrap();
            registry
                .ensure_supervised_run(crate::domain::runs::ops::SupervisedRun {
                    instance: InstanceName::new("demo").unwrap(),
                    run_id: owner.clone(),
                    wire_path: false,
                    labels: vec![Label::bound("team", "payments")],
                    fresh: true,
                    inputs: None,
                    upstream: None,
                    custody: None,
                })
                .unwrap();
        }
        assert_eq!(facade.instance_run("demo").unwrap().record.run_id, owner);
        let mut bus = engine.subscribe();
        let outcome = facade
            .label_run(owner.as_str(), LabelChange::set([("team", "core")]), false)
            .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.labels, vec![Label::verb("team", "core")]);
        // One labels-changed event on the bus …
        let mut seen = Vec::new();
        while let Ok(event) = bus.try_recv() {
            seen.push(event);
        }
        assert_eq!(seen.len(), 1, "{seen:?}");
        let EngineEvent::Run(event) = &seen[0] else {
            panic!("expected a Run event, got {:?}", seen[0]);
        };
        assert_eq!(event.cause.as_deref(), Some("label-verb"));
        assert_eq!(
            event.prior_labels.as_deref(),
            Some(&[Label::bound("team", "payments")][..])
        );
        // … the same one in the outbox, and the snapshot shows the label.
        let batch = facade.export_events(None, 1_000).unwrap();
        let committed: Vec<_> = batch
            .events
            .iter()
            .filter(|entry| {
                matches!(&entry.event, EngineEvent::Run(run)
                    if run.change == crate::domain::RunEventKind::LabelsChanged
                        && run.cause.as_deref() == Some("label-verb"))
            })
            .collect();
        assert_eq!(committed.len(), 1);
        {
            let registry = engine.inner.registry.lock().unwrap();
            let snapshot = registry.run_snapshot();
            assert!(snapshot
                .run(&owner)
                .unwrap()
                .labels
                .contains(&Label::verb("team", "core")));
        }
        // The same set again is unchanged and silent.
        let again = facade
            .label_run(owner.as_str(), LabelChange::set([("team", "core")]), false)
            .unwrap();
        assert!(!again.changed);
        assert!(bus.try_recv().is_err());
        // A config write keeps the verb's value; removal restores config.
        facade.set_config("demo", "labels.team", "billing").unwrap();
        assert_eq!(
            facade.run(owner.as_str()).unwrap().labels,
            vec![Label::verb("team", "core")]
        );
        let restored = facade
            .label_run(owner.as_str(), LabelChange::remove(["team"]), false)
            .unwrap();
        assert_eq!(restored.labels, vec![Label::bound("team", "billing")]);
        assert!(matches!(
            facade.label_run(owner.as_str(), LabelChange::remove(["team"]), false),
            Err(RunError::LabelFromConfig { .. })
        ));
        // Selection reads open Runs by label.
        let matched = facade
            .select_runs(&Selector::parse("team=billing").unwrap())
            .unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].run_id, owner);
        assert!(matches!(
            facade.label_run("run_none", LabelChange::set([("a", "b")]), false),
            Err(RunError::NotFound { .. })
        ));
        assert!(matches!(
            facade.label_run(owner.as_str(), LabelChange::default(), false),
            Err(RunError::Invalid { .. })
        ));
        // An unreadable configuration refuses a removal (it could not
        // restore the configured value) instead of dropping the key.
        facade
            .label_run(owner.as_str(), LabelChange::set([("team", "core")]), false)
            .unwrap();
        let config_file = tmp
            .path()
            .join("agents")
            .join("demo")
            .join(crate::paths::INSTANCE_CONFIG_FILE);
        let config_text = std::fs::read(&config_file).unwrap();
        std::fs::write(&config_file, b"[labels\nnot toml").unwrap();
        for dry_run in [true, false] {
            let refused = facade
                .label_run(owner.as_str(), LabelChange::remove(["team"]), dry_run)
                .unwrap_err();
            assert!(
                refused.to_string().contains("could not be read"),
                "{refused}"
            );
        }
        std::fs::write(&config_file, config_text).unwrap();
        facade
            .label_run(owner.as_str(), LabelChange::remove(["team"]), false)
            .unwrap();
        // A dry run needs no store writer: a read-only engine plans it
        // (configuration included), while its real write is refused.
        drop(engine);
        let reader = Engine::open_read_only(Some(tmp.path().to_path_buf())).unwrap();
        let planned = reader
            .blocking()
            .label_run(owner.as_str(), LabelChange::set([("team", "ops")]), true)
            .unwrap();
        assert!(planned.changed && planned.dry_run);
        assert_eq!(planned.labels, vec![Label::verb("team", "ops")]);
        assert_eq!(planned.prior_labels, vec![Label::bound("team", "billing")]);
        assert!(reader
            .blocking()
            .label_run(owner.as_str(), LabelChange::set([("team", "ops")]), false)
            .is_err());
        assert_eq!(
            reader.blocking().run(owner.as_str()).unwrap().labels,
            vec![Label::bound("team", "billing")]
        );
    }

    // ---- Story 21-4: the read-only open ----

    #[test]
    fn a_read_only_open_of_a_missing_state_dir_reads_an_empty_fleet_and_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("never-created");
        let engine = Engine::open_read_only(Some(base.clone())).unwrap();
        let facade = engine.blocking();
        assert!(facade.fleet().unwrap().is_empty());
        assert!(facade.list().unwrap().is_empty());
        assert!(matches!(
            facade.fleet_entry("ghost").unwrap_err(),
            RegistryError::NotFound { .. }
        ));
        // A write answers the typed read-only refusal — never a panic.
        match facade.register("demo", "mock").unwrap_err() {
            RegistryError::Store(StoreError::Backend(detail)) => {
                assert!(detail.contains("read-only"), "{detail}")
            }
            other => panic!("expected the read-only refusal, got {other:?}"),
        }
        drop(engine);
        assert!(!base.exists(), "a read-only open creates nothing");
    }

    #[test]
    fn the_blocking_policy_facades_round_trip_and_a_budget_write_re_resolves() {
        // The blocking twins of the async Policy facade, end to end over one
        // engine: create (dry then real), show, list, change, delete — and
        // the `budget.*` config write's same-job re-resolution of the open
        // Run (the review round's creation-time resolution made the Run
        // resolvable from the start).
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = Engine::open(Some(tmp.path().to_path_buf())).unwrap();
        let facade = engine.blocking();
        facade.register("demo", "mock").unwrap();
        facade.set_config("demo", "labels.team", "core").unwrap();
        let change = PolicyChange {
            name: "cap".to_string(),
            selector: Some("team=core".parse().unwrap()),
            set: crate::domain::PolicyLimits {
                tokens_per_run: Some(2_000),
                ..Default::default()
            },
            clear: Vec::new(),
            rates: Default::default(),
        };
        // The dry run plans and writes nothing.
        let dry = facade.create_policy(change.clone(), true).unwrap();
        assert_eq!(dry.matched, 0);
        assert!(facade.policy("cap").is_err());
        // The real create answers, the list carries it, the change edits it.
        let created = facade.create_policy(change, false).unwrap();
        assert_eq!(created.matched, 0);
        assert_eq!(facade.policy("cap").unwrap().name, "cap");
        assert!(facade
            .policies()
            .unwrap()
            .iter()
            .any(|record| record.name == "cap"));
        facade
            .change_policy(
                PolicyChange {
                    name: "cap".to_string(),
                    selector: None,
                    set: crate::domain::PolicyLimits {
                        tokens_per_run: Some(3_000),
                        ..Default::default()
                    },
                    clear: Vec::new(),
                    rates: Default::default(),
                },
                false,
            )
            .unwrap();
        assert_eq!(
            facade.policy("cap").unwrap().limits.tokens_per_run,
            Some(3_000)
        );
        // A `budget.*` write on an instance with an open Run re-resolves it
        // in the same job: the run's stored resolution carries the explicit
        // ceiling (the Run is created by the registry; the mock cannot
        // start).
        let owner = RunId::mint();
        {
            let registry = engine.inner.registry.lock().unwrap();
            registry
                .ensure_supervised_run(crate::domain::runs::ops::SupervisedRun {
                    instance: InstanceName::new("demo").unwrap(),
                    run_id: owner.clone(),
                    wire_path: false,
                    labels: vec![crate::domain::Label::bound("team", "core")],
                    fresh: true,
                    inputs: None,
                    upstream: None,
                    custody: None,
                })
                .unwrap();
        }
        facade
            .set_config("demo", "budget.tokens.per_run", "1234")
            .unwrap();
        let run = facade.run(owner.as_str()).unwrap();
        assert_eq!(run.policy.as_ref().unwrap().tokens.per_run, Some(1_234));
        // A `budget.breach_action` write is the FR-21 marker-clearing
        // key: the action re-resolves, and a kept-on-pause marker (one is
        // seeded here as the upgrade would leave it) clears in the same
        // write.
        {
            let registry = engine.inner.registry.lock().unwrap();
            registry
                .writer
                .call(|ctx| {
                    ctx.store().execute_batch_for_test(
                        "INSERT INTO breach_default_upgrade (instance_id, listed_at, done_at) \
                         VALUES ((SELECT id FROM agent_instances WHERE name = 'demo'), 't', 't')",
                    );
                    Ok(())
                })
                .unwrap();
        }
        assert!(facade.fleet().unwrap()[0].breach_default_upgraded);
        facade
            .set_config("demo", "budget.breach_action", "warn")
            .unwrap();
        assert!(!facade.fleet().unwrap()[0].breach_default_upgraded);
        let run = facade.run(owner.as_str()).unwrap();
        assert_eq!(
            run.policy.as_ref().unwrap().effective_action,
            crate::domain::BreachAction::Warn
        );
        // The delete dry run plans; the real delete removes it.
        let dry = facade.delete_policy("cap", true).unwrap();
        assert!(dry.record.is_none());
        assert!(facade.policy("cap").is_ok());
        facade.delete_policy("cap", false).unwrap();
        assert!(facade.policy("cap").is_err());
    }

    #[test]
    fn a_read_only_open_reads_what_a_writer_committed_and_never_writes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        {
            let writer = Engine::open(Some(base.clone())).unwrap();
            writer.blocking().register("demo", "mock").unwrap();
        }
        let db = base.join(crate::paths::STATE_DB_FILE);
        let before = std::fs::read(&db).unwrap();
        let engine = Engine::open_read_only(Some(base.clone())).unwrap();
        let facade = engine.blocking();
        let fleet = facade.fleet().unwrap();
        assert_eq!(fleet.len(), 1);
        assert_eq!(fleet[0].name.as_str(), "demo");
        assert!(facade.effective_capabilities("demo").is_ok());
        assert!(facade
            .effective_config("demo", ConfigLayer::empty())
            .is_ok());
        // Writes refuse; nothing changed on disk.
        assert!(facade.set_config("demo", "model", "x").is_err());
        assert!(facade
            .remove("demo", RemoveDisposition::Retain, false)
            .is_err());
        drop(engine);
        assert_eq!(std::fs::read(&db).unwrap(), before, "state.db is untouched");
    }

    #[test]
    fn a_read_only_open_of_an_older_schema_answers_needs_migration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        let db = base.join(crate::paths::STATE_DB_FILE);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA user_version = 3").unwrap();
        }
        match Engine::open_read_only(Some(base.clone())) {
            Err(RegistryError::Store(StoreError::NeedsMigration { found, current })) => {
                assert_eq!(found, 3);
                assert!(current > found);
            }
            Err(other) => panic!("expected NeedsMigration, got {other:?}"),
            Ok(_) => panic!("an older schema must not open read-only"),
        }
        // A newer schema is still the typed too-new refusal.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("PRAGMA user_version = 9999").unwrap();
        }
        assert!(matches!(
            Engine::open_read_only(Some(base)),
            Err(RegistryError::Store(StoreError::SchemaTooNew { .. }))
        ));
    }

    #[test]
    fn a_read_only_fleet_reads_the_current_run_from_the_persisted_spawn_record() {
        // The live supervisor's only Fleet input — the current Run id — comes
        // from the persisted spawn record for a live instance in a read-only
        // engine, so its current-run totals match the owning host's. The
        // "live" record carries THIS test process's own fingerprint (a
        // process that is provably alive); the "ghost" record a pid whose
        // start time can never match, so the read shows it `failed` with the
        // stored state kept in `recorded_state` — and writes nothing.
        use crate::ports::ProcessBackend;
        let me = crate::backends::current()
            .self_fingerprint()
            .expect("this process's own fingerprint");
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().to_path_buf();
        {
            let writer = Engine::open(Some(base.clone())).unwrap();
            writer.blocking().register("live", "mock").unwrap();
            writer.blocking().register("ghost", "mock").unwrap();
        }
        let db = base.join(crate::paths::STATE_DB_FILE);
        let seed = |conn: &rusqlite::Connection, name: &str, pid: u32, start: u64, run: &str| {
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM agent_instances WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "UPDATE agent_instances SET state = 'running' WHERE id = ?1",
                [id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO agent_runtime \
                 (instance_id, pid, start_time, restart_policy, restart_count, run_id) \
                 VALUES (?1, ?2, ?3, 'on-failure', 0, ?4)",
                rusqlite::params![id, pid, start as i64, run],
            )
            .unwrap();
            // The per-Run rollup is what the Fleet's current-run totals read.
            conn.execute(
                "INSERT INTO usage_rollup_run \
                 (instance_id, run_id, events, input_tokens, output_tokens) \
                 VALUES (?1, ?2, 1, 7, 3)",
                rusqlite::params![id, run],
            )
            .unwrap();
        };
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            seed(&conn, "live", me.pid, me.start_time, "run-persisted");
            seed(
                &conn,
                "ghost",
                me.pid,
                me.start_time.wrapping_add(1),
                "run-ghost",
            );
        }
        let before = std::fs::read(&db).unwrap();
        let engine = Engine::open_read_only(Some(base)).unwrap();
        let entry = engine.blocking().fleet_entry("live").unwrap();
        assert_eq!(entry.state, LifecycleState::Running);
        assert_eq!(entry.recorded_state, None, "{entry:?}");
        assert_eq!(entry.usage.current_run_input_tokens, 7, "{entry:?}");
        assert_eq!(entry.usage.current_run_output_tokens, 3, "{entry:?}");

        let ghost = engine.blocking().fleet_entry("ghost").unwrap();
        assert_eq!(ghost.state, LifecycleState::Failed, "{ghost:?}");
        assert_eq!(ghost.recorded_state, Some(LifecycleState::Running));
        assert_eq!(ghost.supervision, None, "a dead process is not supervised");
        assert_eq!(ghost.usage.current_run_input_tokens, 0, "its Run ended");
        let cause = ghost.failed_cause.clone().unwrap_or_default();
        assert!(
            cause.contains(&format!("pid {} is gone", me.pid)),
            "{cause}"
        );
        let status = engine.blocking().instance_status("ghost").unwrap();
        assert_eq!(status.instance.state, LifecycleState::Failed);
        assert_eq!(status.failed_cause.as_deref(), Some(cause.as_str()));
        drop(engine);
        assert_eq!(
            std::fs::read(&db).unwrap(),
            before,
            "the reads wrote nothing"
        );
    }
}
