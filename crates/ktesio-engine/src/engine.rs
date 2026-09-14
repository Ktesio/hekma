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
//! Sync callers — `kt` today — drive the engine through [`Engine::blocking`],
//! which returns a [`Blocking`] view whose methods are the sync equivalents,
//! each `runtime.block_on(async_method(..))`. `kt` stays a synchronous binary
//! (no `#[tokio::main]`); a Host with its OWN runtime (story 7-1/7-3) calls the
//! async methods directly. This story covers exactly the commands `kt` uses
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
//! `[ktesio] `-prefixed text, one line each) stderr would have received.
//! Installing REPLACES any earlier sink (rotation) and is one-way — there is
//! no uninstall back to the stderr default; a host that wants the default
//! back re-opens the engine. The sink is engine-embedder ergonomics — it
//! never touches the adapter-api contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Runtime;

use ktesio_adapter_api::EffectiveCapabilities;

use crate::adapter::AdapterRef;
use crate::domain::{
    broadcast, AgentInstance, BudgetBreachEvent, ConfigError, ConfigLayer, DiagnosticSink,
    EffectiveConfig, EngineError, EngineEvent, EventBus, FleetEntry, InstanceName, LifecycleState,
    LogLine, Registry, RegistryError, RemoveDisposition, RestartPolicy, ResyncBatch, ResyncCursor,
    Supervisor, TransitionCause, TransitionEvent,
};
use crate::ports::SpawnRecord;

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

/// Extract a human-readable failed-cause detail from a transition [`TransitionCause`]
/// for the `instance_status` event-log fallback (AC9). Returns the carried detail
/// for the failure-bearing causes (a launch error, or a crash), or `None` for a
/// cause that does not describe a failure.
fn failed_cause_detail(cause: &TransitionCause) -> Option<String> {
    match cause {
        TransitionCause::LaunchError { detail } => Some(detail.clone()),
        TransitionCause::Crashed { detail } => Some(detail.clone()),
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
    /// [`Engine::drop`] so the background poll stops with the engine.
    reaper: tokio::task::AbortHandle,
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Stop the background reaper when the engine goes away (its supervised
        // handles drop with the supervisor, killing spawned processes).
        self.reaper.abort();
    }
}

/// Shared, `Send + Sync` engine state moved onto the blocking pool.
///
/// The [`Registry`] owns a rusqlite `Connection` (`Send` but `!Sync`); wrapping
/// it in a [`Mutex`] makes the whole `EngineInner` `Send + Sync` so a
/// `spawn_blocking` closure can capture an `Arc<EngineInner>`. Registration is
/// not a hot path and the engine is single-lifetime, so a coarse mutex is the
/// correct, simplest altitude here.
struct EngineInner {
    registry: Mutex<Registry>,
    supervisor: Mutex<Supervisor>,
    /// The supervisor's event bus clone (story 7-2): held OUTSIDE the mutex so
    /// [`Engine::subscribe`] hands out receivers without ever contending on
    /// the supervisor lock. Both clones share one broadcast channel.
    events: EventBus,
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
        Self::open_inner(base, None)
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
        Self::open_inner(base, Some(sink))
    }

    /// The shared open path (story 10-2): `open` passes no sink (the
    /// diagnostics keep their default stderr behavior, byte-identical to the
    /// pre-sink engine), `open_with_diagnostics` installs the host's writer
    /// before any supervision work starts.
    fn open_inner(
        base: Option<PathBuf>,
        diagnostics: Option<DiagnosticSink>,
    ) -> Result<Self, RegistryError> {
        let registry = Registry::open(base)?;
        let rt = Runtime::new().map_err(|e| RegistryError::Io {
            name: "<engine-runtime>".to_string(),
            path: "<tokio-runtime>".to_string(),
            source: e,
        })?;
        // Story 3-4: thread the engine runtime handle into the supervisor so an
        // `engine-observed` start can spawn its loopback forward listener's
        // accept loop on this runtime (the sync start path runs on the blocking
        // pool, where `Handle::current` is unavailable). A `Handle` spawns onto
        // its runtime from any thread, so this is sound. Story 7-2: keep a bus
        // clone OUTSIDE the supervisor mutex so `subscribe()` is lock-free (see
        // EngineInner::events). Story 10-2: install the host's diagnostic sink
        // (when given) BEFORE adoption + the reaper spawn, so no diagnostic can
        // fire before the sink is in place.
        let mut supervisor = Supervisor::with_runtime(rt.handle().clone());
        if let Some(sink) = diagnostics {
            supervisor.install_diagnostics(sink);
        }
        let events = supervisor.event_bus();
        let inner = Arc::new(EngineInner {
            registry: Mutex::new(registry),
            supervisor: Mutex::new(supervisor),
            events,
        });

        // (1) Orphan adoption on open (AC-B / AI-7 / AI-8). Reconcile BEFORE the
        // reaper starts so an adopted process is already held when the first poll
        // runs (and a phantom row is already `failed`).
        {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.adopt_orphans(&registry);
        }

        // (2) Spawn the crash-detection reaper on the engine's runtime. It is
        // aborted on Engine::drop. Cloning the Arc<EngineInner> keeps the shared
        // state alive for the task without keeping the Engine itself alive.
        let reaper = Self::spawn_reaper(&rt, Arc::clone(&inner));

        Ok(Self {
            rt: Arc::new(rt),
            inner,
            reaper,
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

    /// A synchronous facade over the async API for non-async callers (`kt`).
    ///
    /// Each [`Blocking`] method is `runtime.block_on(async_method(..))`. See the
    /// module docs for why `kt` uses this instead of becoming an async binary.
    pub fn blocking(&self) -> Blocking<'_> {
        Blocking { engine: self }
    }

    /// Install (or replace) the host-provided diagnostic sink (story 10-2) on
    /// an already-open engine.
    ///
    /// The engine's two operational diagnostics — the DC-10 memory-delivery
    /// notice and the enforcement breadcrumb — then route to `sink` instead of
    /// stderr, receiving the exact bytes (same `[ktesio] `-prefixed text, one
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
    /// docstring deferred to 1.4/1.6: without it, `kt agent remove <live> --force`
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
    /// AD-14). This is the read `kt agent list [--json]` uses.
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
    /// `kt` stays a thin renderer that never sums the ledger or derives dollars itself.
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
            let entries = instances
                .into_iter()
                .map(|instance| {
                    Self::fleet_entry_for(&registry, &supervisor, records.as_ref(), instance)
                })
                .collect();
            Ok(entries)
        })
        .await
    }

    /// The ONE Fleet entry for `name` (AI-15) — the read `kt agent show <name>
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
                instance,
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
        instance: AgentInstance,
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
        let restart_policy = record
            .as_ref()
            .map(|r| r.restart_policy)
            .unwrap_or_default();
        let restart_count = record.as_ref().map(|r| r.restart_count).unwrap_or(0);
        let failed_cause = record.and_then(|r| r.last_known_cause).or_else(|| {
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
        let cumulative = registry.usage_totals(&instance.name).unwrap_or_default();
        let current_run_id = supervisor.current_run_id(&instance.name);
        let current_run = current_run_id
            .as_ref()
            .and_then(|run_id| registry.run_usage_totals(&instance.name, run_id).ok())
            .unwrap_or_default();
        // Story 3-3 dollar surface: resolve the CURRENT Rate/cap + action ONCE
        // through the SAME live config resolve enforcement uses (so the Fleet view
        // matches what enforcement sees). A degraded read → no Rate / no budget
        // (honest absence), never failing the Fleet.
        let (rate, cost_cap, action) = registry
            .effective_config(&instance.name, crate::domain::ConfigLayer::empty())
            .ok()
            .map(|effective| {
                let (_token_budget, action) = crate::domain::resolve_token_budget(&effective);
                let (rate, cost_cap, _cost_action) = crate::domain::resolve_cost(&effective);
                (rate, cost_cap, action)
            })
            .unwrap_or_else(|| (None, crate::domain::CostCap::none(), Default::default()));
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
        // Story 3-2 budget surface + story 3-3 dollar cap surface (AC9/AC10): the
        // CURRENT resolved Token Budget + Cost Cap + Breach Action + remaining per
        // scope. TOKEN remaining is `usage` tokens; DOLLAR cap/remaining are present
        // ONLY when a Rate exists (a cap with no Rate is inert — AC-B). An instance
        // with NEITHER a budget nor an enforceable cap surfaces an honest absent
        // budget (never a fabricated ceiling).
        let budget = registry
            .effective_config(&instance.name, crate::domain::ConfigLayer::empty())
            .ok()
            .and_then(|effective| {
                let (token_budget, _action) = crate::domain::resolve_token_budget(&effective);
                crate::domain::BudgetView::from_budget_and_cost(
                    &token_budget,
                    &cost_cap,
                    action,
                    current_run.total_tokens(),
                    cumulative.total_tokens(),
                    rate.is_some(),
                    current_run_cost,
                    cumulative_cost,
                    label,
                )
            });
        // The active Metering Source is visible in Fleet detail (AC-C), read from the
        // persisted adapter snapshot. A degraded read falls back to the honest
        // "unknown" marker rather than fabricating a source.
        let metering_source = registry
            .metering_source(&instance.name)
            .unwrap_or_else(|_| "unknown".to_string());
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
            // aggregation rule is engine-domain; `kt` triggers it over these rows).
            usage,
            metering_source,
            agent_home: instance.agent_home,
        }
    }

    /// The effective (current-OS) Capability Declaration for a registered
    /// instance (AC1 "visible for the instance"). `kt agent show` renders this.
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
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.start(&registry, &name)
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
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.stop(&registry, &name, window)
        })
        .await
    }

    /// Pause a running Agent Instance with honest, per-OS semantics (story 1-5,
    /// AC1/AC2/AC3). Drives the supervisor's three-level dispatch on the effective
    /// (current-OS) pause `SupportLevel`: guaranteed → real SIGSTOP suspension +
    /// `running→paused`; best-effort → `running→paused` with a visible
    /// `pause-best-effort` qualifier in the transition event; unsupported → fail
    /// fast ([`EngineError::CapabilityUnsupported`]) with no state change. Returns
    /// the instance in its new state.
    pub async fn pause(&self, name: &str) -> Result<AgentInstance, EngineError> {
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
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        self.run_blocking(move || {
            let registry = inner.registry.lock().expect("registry mutex poisoned");
            let mut supervisor = inner.supervisor.lock().expect("supervisor mutex poisoned");
            supervisor.resume(&registry, &name)
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
    /// cause. This is the read `kt agent list`/`show` uses to surface the restart
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
            let instance = registry
                .lookup(&iname)
                .map_err(crate::domain::registry_error_to_engine)?;
            let record = registry
                .spawn_record(&iname)
                .map_err(crate::domain::registry_error_to_engine)?;
            let restart_policy = record
                .as_ref()
                .map(|r| r.restart_policy)
                .unwrap_or_default();
            let restart_count = record.as_ref().map(|r| r.restart_count).unwrap_or(0);
            let failed_cause = record.and_then(|r| r.last_known_cause).or_else(|| {
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
    /// spine AD-9, AC-A / AC10). This is the read `kt agent config get` uses.
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
    /// REVEALED cleartext string for the secret leaves ONLY; `kt` overlays them onto
    /// the (masked) effective config to un-mask exactly those leaves. The engine
    /// re-resolves secrets LIVE (env → the 0600 file); `kt` never resolves secrets
    /// itself (AD-2). A resolution failure is a typed [`ConfigError::SecretReveal`]
    /// (a stderr diagnostic in `kt`, never a crash). This NEVER touches the
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
    /// spine AD-9, AC-B / AC10). This is the write `kt agent config set` uses.
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
    /// surface (`kt` prints them to stderr); the vec is empty on the common
    /// path, and the write is never rejected for a warned combination.
    pub async fn set_config(
        &self,
        name: &str,
        key: &str,
        value: &str,
    ) -> Result<Vec<String>, ConfigError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_string();
        let key = key.to_string();
        let value = value.to_string();
        self.run_blocking(move || {
            let iname = InstanceName::new(&name).map_err(|reason| ConfigError::InvalidName {
                name: name.clone(),
                reason: reason.to_string(),
            })?;
            inner
                .registry
                .lock()
                .expect("registry mutex poisoned")
                .set_config(&iname, &key, &value)
        })
        .await
    }

    /// Attach a Memory Backing of `kind` to an Agent Instance (story 5-1,
    /// FR-15 / spine AD-11). The engine creates the managed directory inside the
    /// Agent Home (path authority — the returned path IS the engine's, `kt`
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

    /// Read the retained ATTRIBUTED output log for an instance (story 4-2,
    /// AC-A) — a ONE-SHOT full read of whatever is currently retained (the
    /// current generation plus any rotated predecessors), in on-disk append
    /// order (AC-G). This is the FIRST CLI-facing consumer of this shape
    /// (`kt agent logs`) — an unregistered name fails
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
            Supervisor::read_agent_log(&registry, &name)
        })
        .await
    }

    /// A cursor-based follow read (story 4-2, AC-B/AC-C/AC-H) — `kt agent
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
            Supervisor::read_agent_log_since(&registry, &name, cursor)
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
/// the surface `kt` drives.
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

    /// Blocking [`Engine::fleet`] (story 1-7, FR-4). The Fleet as serializable
    /// [`FleetEntry`] rows — what `kt agent list [--json]` renders.
    pub fn fleet(&self) -> Result<Vec<FleetEntry>, RegistryError> {
        self.engine.rt.block_on(self.engine.fleet())
    }

    /// Blocking [`Engine::fleet_entry`] (AI-15) — the single-instance Fleet
    /// read `kt agent show <name> --json` uses. O(1) keyed lookup + the SAME
    /// row composition as `list`; unknown names surface the registry's
    /// `NotFound` (the CLI's exit-3 diagnostic).
    pub fn fleet_entry(&self, name: &str) -> Result<FleetEntry, RegistryError> {
        self.engine.rt.block_on(self.engine.fleet_entry(name))
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

    /// Blocking [`Engine::stop`].
    pub fn stop(&self, name: &str, window: Option<Duration>) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.stop(name, window))
    }

    /// Blocking [`Engine::pause`].
    pub fn pause(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.pause(name))
    }

    /// Blocking [`Engine::resume`].
    pub fn resume(&self, name: &str) -> Result<AgentInstance, EngineError> {
        self.engine.rt.block_on(self.engine.resume(name))
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
    /// `kt agent config get` uses.
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
    /// `kt agent config set` uses. On success carries the ZERO OR MORE
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
}
