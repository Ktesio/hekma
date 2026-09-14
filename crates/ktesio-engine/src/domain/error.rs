//! [`RegistryError`] — the registry service's error type (thiserror, no miette).
//!
//! Each variant carries enough for `kt` to render a remediation hint (NFR-1:
//! every partial failure names the instance + reason + remediation). `miette`
//! wrapping happens in `kt`, never here (conventions).

use thiserror::Error;

use crate::ports::{BackendError, StoreError};

use super::name::NameError;
use super::transition::LifecycleError;

/// Errors from the registry service (`register` / `remove`).
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The requested name collides with an existing instance.
    ///
    /// Distinct from [`StoreError::DuplicateName`] so the service layer can
    /// attach registry-level context; the store variant is the low-level cause.
    #[error("an Agent Instance named '{name}' already exists")]
    DuplicateName {
        /// The conflicting instance name.
        name: String,
    },

    /// The supplied name failed the naming rule at construction.
    #[error("invalid Agent Instance name '{name}': {reason}")]
    InvalidName {
        /// The rejected candidate string.
        name: String,
        /// The specific rule that failed.
        reason: NameError,
    },

    /// `remove` targeted a name that is not registered.
    #[error("no Agent Instance named '{name}' is registered")]
    NotFound {
        /// The missing instance name.
        name: String,
    },

    /// `remove` targeted a `running` instance without `--force` (AC5).
    #[error("Agent Instance '{name}' is running; stop it first or pass --force")]
    RunningRequiresForce {
        /// The running instance's name.
        name: String,
    },

    /// `memory attach`/`detach` targeted an instance in a NON-terminal Lifecycle
    /// State (story 5-1, AC3 / spine AD-11 "attach/detach permitted only while
    /// the Agent Instance is not `running"`; architect ruling A-5 narrows the
    /// permission to the TERMINAL states only — `registered`/`stopped`/`failed`).
    /// Like [`EngineError::NotRunning`]'s doctrine, attach/detach are NOT
    /// lifecycle verbs — no transition is being attempted — so this is a
    /// dedicated pre-flight check against the PERSISTED state (pure
    /// state-machine validation, deterministically testable with no live
    /// process) rather than an entry in the transition table. There is
    /// deliberately NO `--force` escape (unlike [`RegistryError::RunningRequiresForce`],
    /// whose "or pass --force" message would be FALSE here): AD-11 forbids
    /// hot-swapping a backing under a live or transitioning agent outright.
    /// Names the instance + its actual state + the remediation.
    #[error(
        "Agent Instance '{name}' is '{state}'; a Memory Backing cannot be hot-swapped — \
         attach/detach need a terminal state (registered, stopped, or failed). Bring it to \
         a terminal state first: kt agent stop {name} from running or paused"
    )]
    MemoryBackingHotSwap {
        /// The instance whose backing was being changed.
        name: String,
        /// The instance's current Lifecycle State (wire form).
        state: String,
    },

    /// `memory attach` requested a DIFFERENT kind than the one already attached
    /// (story 5-1, A-6). Exactly ONE Memory Backing exists per instance and
    /// kinds never hot-swap; the operator detaches first. Re-attaching the SAME
    /// kind is an idempotent success and never reaches this error. Names both
    /// kinds + the remediation.
    #[error(
        "Agent Instance '{name}' already has a '{attached}' Memory Backing attached; detach it before attaching '{requested}': kt agent memory detach {name}"
    )]
    MemoryBackingKindConflict {
        /// The instance whose backing conflicts.
        name: String,
        /// The currently attached kind (wire form).
        attached: String,
        /// The requested kind (wire form).
        requested: String,
    },

    /// A filesystem operation on the Agent Home failed.
    ///
    /// Carries the offending path so the diagnostic can name it (NFR-1). Used
    /// both for creation failures (rolled back) and the removal partial-failure
    /// case (row already deleted, directory could not be removed).
    #[error("filesystem error for Agent Instance '{name}' at {path}: {source}")]
    Io {
        /// The instance the operation was for.
        name: String,
        /// The path that could not be created/written/removed.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The Agent Home directory was deleted but the DB row already gone, or a
    /// removal left an artifact behind — a partial-failure state needing
    /// operator attention. Kept distinct from [`RegistryError::Io`] so `kt` can
    /// phrase "removed from the Fleet, but ..." precisely.
    #[error(
        "Agent Instance '{name}' was removed from the Fleet, but its Agent Home at {path} could not be deleted: {detail}"
    )]
    RemoveLeftoverHome {
        /// The removed instance's name.
        name: String,
        /// The leftover Agent Home path.
        path: String,
        /// Why deletion failed.
        detail: String,
    },

    /// Registration's Agent Home step failed AND the compensating row delete
    /// also failed, so a `registered` row survives with no Agent Home behind
    /// it — a partial-failure state needing operator attention. Distinct from
    /// [`RegistryError::Io`] so `kt` can name the orphaned row and its cleanup
    /// (mirrors [`RegistryError::RemoveLeftoverHome`]).
    #[error(
        "Agent Instance '{name}' left an orphaned registry row after its Agent Home could not be created ({home_error}) and the rollback delete also failed ({rollback_error}); remove it with: kt agent remove {name} --force"
    )]
    RegisterOrphanRow {
        /// The orphaned instance's name.
        name: String,
        /// Why the Agent Home could not be created (the original failure).
        home_error: String,
        /// Why the compensating row delete failed.
        rollback_error: String,
    },

    /// The effective-config snapshot could not be written to the Agent Home at
    /// start (story 2-3, AD-9/AD-6). Names the instance + the snapshot path so the
    /// diagnostic can point at it (NFR-1). Distinct from [`RegistryError::Io`] so
    /// `kt` can phrase the "could not write the effective-config snapshot" case
    /// precisely; the start rejects cleanly on this error (no state change).
    #[error("could not write the effective-config snapshot for Agent Instance '{name}' at {path}: {detail}")]
    SnapshotWrite {
        /// The instance the snapshot is for.
        name: String,
        /// The snapshot path that could not be written.
        path: String,
        /// The underlying serialize / I/O detail.
        detail: String,
    },

    /// A native adapter `kind` was requested that no builtin provides (story
    /// 1.3). Carries the unrecognized kind so `kt` can suggest alternatives.
    #[error("unknown adapter kind '{kind}'")]
    UnknownAdapterKind {
        /// The unrecognized native kind string.
        kind: String,
    },

    /// A manifest adapter was requested but no `adapter.toml` was found at the
    /// resolved path (story 1.3). Names the path searched.
    #[error("no adapter.toml found at {path}")]
    ManifestNotFound {
        /// The path searched (the file, or `<dir>/adapter.toml`).
        path: String,
    },

    /// A manifest adapter's `adapter.toml` exists but could not be read (an I/O
    /// error — e.g. permissions, or the path is a directory). Distinct from
    /// [`RegistryError::ManifestInvalid`] because the operator's remediation is
    /// different: check existence/readability, not "fix the section" (F4).
    #[error("could not read adapter.toml at {path}: {detail}")]
    ManifestUnreadable {
        /// The manifest path that could not be read.
        path: String,
        /// The underlying I/O error.
        detail: String,
    },

    /// A manifest adapter's `adapter.toml` failed to parse or validate (story
    /// 1.3). `detail` NAMES the failing section (AC2) so the diagnostic can
    /// quote it.
    #[error("adapter.toml at {path} is invalid: {detail}")]
    ManifestInvalid {
        /// The manifest path.
        path: String,
        /// The section-naming validation detail.
        detail: String,
    },

    /// A manifest adapter declares a `contract_version` whose MAJOR differs
    /// from this engine's Adapter Contract (story 6-6, FR-30 — the v1 freeze;
    /// registration refuses the load). Distinct from
    /// [`RegistryError::ManifestInvalid`] because the manifest is well-formed —
    /// the VERSION does not negotiate — and the remediation belongs to the
    /// adapter author (retarget the manifest's `contract_version`), not to a
    /// section edit. `detail` names BOTH versions and quotes the compatibility
    /// rule (rendered from `ktesio_adapter_api::ContractVersionError`, so the
    /// rule text lives in one place).
    #[error("adapter.toml at {path} is incompatible: {detail}")]
    ContractIncompatible {
        /// The manifest path.
        path: String,
        /// The both-versions + rule message from the negotiation.
        detail: String,
    },

    /// An adapter declared no viable Metering Source and was rejected at
    /// registration (story 1.3; FR-19 hard line, AC4). Names the adapter.
    #[error("adapter '{adapter}' declares no viable Metering Source; add a `[metering]` section")]
    NoMeteringSource {
        /// The adapter kind/identity that lacked a source.
        adapter: String,
    },

    /// An adapter declared no capabilities and was rejected at registration
    /// (story 1.3; AC2). Names the adapter.
    #[error("adapter '{adapter}' declares no capabilities; add a `[capabilities]` section")]
    NoCapabilities {
        /// The adapter kind/identity that lacked capabilities.
        adapter: String,
    },

    /// A [`StateStore`](crate::ports::StateStore) operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Errors from the lifecycle supervision surface (`start` / `stop`, story 1.4).
///
/// Distinct from [`RegistryError`] (registration) so `kt` can map lifecycle
/// failures — an invalid transition (AC4), a launch failure (AC2) — to their own
/// diagnostics. Every variant names the instance + reason so `kt` can render a
/// remediation (NFR-1). `thiserror`, never `miette` (conventions).
#[derive(Debug, Error)]
pub enum EngineError {
    /// The instance is not registered. Names it.
    #[error("no Agent Instance named '{name}' is registered")]
    NotFound {
        /// The missing instance name.
        name: String,
    },

    /// The supplied name failed the naming rule.
    #[error("invalid Agent Instance name '{name}': {reason}")]
    InvalidName {
        /// The rejected candidate string.
        name: String,
        /// The specific rule that failed.
        reason: NameError,
    },

    /// A lifecycle command was invalid from the instance's current state (AC4).
    /// The SAME error for every adapter (it comes from the shared transition
    /// table before any adapter code runs).
    #[error(transparent)]
    InvalidTransition(#[from] LifecycleError),

    /// A capability (this story: pause) is UNSUPPORTED for this Agent Instance on
    /// the current OS (story 1-5, AC3): the effective Capability Declaration
    /// projects to `Unsupported`, so the command FAILS FAST — quoting the
    /// declaration (the level + OS), with NO state change, NO process signal, and
    /// no fake attempt. Names the instance + capability + OS + declared level so
    /// `kt` can quote the declaration and point at `kt agent show`.
    #[error(
        "Agent Instance '{name}' cannot {capability}: this adapter declares {capability} '{level}' on {os} (see its Capability Declaration)"
    )]
    CapabilityUnsupported {
        /// The instance the command targeted.
        name: String,
        /// The capability that is unsupported (`"pause"`).
        capability: String,
        /// The current OS the declaration was projected onto.
        os: String,
        /// The declared support level for that capability on that OS
        /// (`"unsupported"`).
        level: String,
    },

    /// A `resume` targeted a PAUSED instance whose effective PAUSE level reads
    /// `Unsupported` on the current OS (AI-7): the engine cannot confidently
    /// signal the suspension awake, so the command fails fast — but the BARE
    /// pause-unsupported diagnostic would strand the operator with no way
    /// forward for an instance that is already `paused` (pause was allowed when
    /// it was suspended, or the row was paused under a prior declaration/OS).
    /// The diagnostic names the STATE (`paused`) + the adapter's pause
    /// declaration (the level + OS the dispatch consulted) and leads with the
    /// actionable path forward: `stop` works without pause support (it never
    /// consults the pause level). The supported-OS resume is demoted to an
    /// informational note (loop 2) — an operator at a CLI cannot change this
    /// host's OS. NO state change, NO signal, NO fake success.
    #[error(
        "Agent Instance '{name}' is paused, but this adapter declares pause '{level}' on {os}, \
         so resume cannot signal the suspension awake; stop the instance and start it again. \
         The same resume does work on hosts where the adapter declares pause support \
         (informational). See its Capability Declaration"
    )]
    ResumeUnsupported {
        /// The instance the command targeted (its state is `paused` — the
        /// transition gate guarantees it).
        name: String,
        /// The current OS the pause declaration was projected onto.
        os: String,
        /// The declared PAUSE support level for that OS (`"unsupported"`).
        level: String,
    },

    /// The agent failed to launch (AC2): the adapter/process diagnostic is
    /// PRESERVED in `detail`, the instance is left in `failed`, and no zombie
    /// remains. Names the instance.
    #[error("Agent Instance '{name}' failed to launch: {detail}")]
    LaunchFailed {
        /// The instance that failed to start.
        name: String,
        /// The preserved adapter/process diagnostic (verbatim, AC2).
        detail: String,
    },

    /// The instance's adapter could not be re-resolved for launch (a corrupt or
    /// now-missing manifest/snapshot). Names the instance + detail.
    #[error("could not resolve the adapter for Agent Instance '{name}': {detail}")]
    AdapterUnresolved {
        /// The instance whose adapter failed to resolve.
        name: String,
        /// Why resolution failed.
        detail: String,
    },

    /// The effective-config snapshot could not be written to the Agent Home at
    /// start (story 2-3, AD-9/AD-6). The snapshot is a promised AD-9 artifact, so
    /// a write failure FAILS the start cleanly (it lands before the `starting`
    /// transition, so the instance stays in its prior state — no spurious change).
    /// Names the instance + the snapshot path (NFR-1).
    #[error("could not write the effective-config snapshot for Agent Instance '{name}' at {path}: {detail}")]
    Snapshot {
        /// The instance the snapshot is for.
        name: String,
        /// The snapshot path that could not be written.
        path: String,
        /// The underlying serialize / I/O detail.
        detail: String,
    },

    /// A `secret:NAME` config reference could not be RESOLVED at start (story 2-4,
    /// spine AD-10, FR-14). The resolution runs BEFORE the config mapping + the
    /// `starting` transition, so a failure FAILS the start cleanly — the instance
    /// stays in its prior state, NO half-launch (mirroring the snapshot-write
    /// failure). `detail` carries the underlying [`crate::ports::SecretError`]
    /// message, which names the `NAME` + the resolvers tried (or the `chmod 600`
    /// remediation) but NEVER a resolved secret VALUE (NFR-6). Names the instance.
    #[error("could not resolve a secret for Agent Instance '{name}': {detail}")]
    Secret {
        /// The instance whose secret failed to resolve.
        name: String,
        /// The underlying secret-resolution detail (names the NAME + resolvers +
        /// remediation, never a value).
        detail: String,
    },

    /// The `engine-observed` loopback forward listener could not START at `start`
    /// (story 3-4, FR-19/AD-7). Reasons: no configured upstream provider URL
    /// (`metering.upstream_base_url` unset), a non-http/https upstream (v1
    /// HTTP-only), a loopback bind failure, or no engine runtime handle to spawn on.
    /// The listener starts BEFORE the `starting` transition, so a failure FAILS the
    /// start cleanly — the instance stays in its prior state, NO half-launch
    /// (mirroring the snapshot/secret failures). `detail` carries the underlying
    /// [`crate::ports`]-level listener reason, which is TRAFFIC-FREE by construction
    /// (no request/response body, header, URL, or API key — the 2-4 no-leak rigor,
    /// since this proxy carries the agent's model traffic). Names the instance.
    #[error("could not start engine-observed metering for Agent Instance '{name}': {detail}")]
    ObservedMetering {
        /// The instance whose observed listener failed to start.
        name: String,
        /// The underlying listener reason (traffic-free — never a body/header/key).
        detail: String,
    },

    /// A per-instance log I/O operation failed (AD-12 seed). Names the path.
    #[error("could not write the instance log for '{name}' at {path}: {detail}")]
    Log {
        /// The instance the log is for.
        name: String,
        /// The log path.
        path: String,
        /// The underlying I/O detail.
        detail: String,
    },

    /// A process-control backend operation failed unexpectedly (not a launch
    /// failure — a signal/terminate/wait error). Names the instance.
    #[error("process control failed for Agent Instance '{name}': {source}")]
    Backend {
        /// The instance the operation was for.
        name: String,
        /// The underlying backend error.
        source: BackendError,
    },

    /// `send` was targeted at an instance NOT in [`LifecycleState::Running`]
    /// (story 4.1, AC-C). Unlike a lifecycle verb, `send` is not itself a
    /// state transition, so this is a dedicated pre-flight check rather than
    /// [`EngineError::InvalidTransition`] — there is no transition being
    /// attempted. Checked BEFORE the capability-support read (mirrors the
    /// "transition gate before any side effect" convention). Names the
    /// instance + its current state.
    ///
    /// [`LifecycleState::Running`]: super::lifecycle::LifecycleState::Running
    #[error("Agent Instance '{name}' is not running (current state: {state}); start it first")]
    NotRunning {
        /// The instance `send` targeted.
        name: String,
        /// The instance's current Lifecycle State (wire form, e.g. `"paused"`).
        state: String,
    },

    /// `send` was targeted at an instance that IS genuinely
    /// [`LifecycleState::Running`](super::lifecycle::LifecycleState::Running)
    /// (its Capability Declaration may truthfully say `interaction:
    /// guaranteed`) but this engine session holds no live stdin pipe for it
    /// (story 4.1, AC-D) — most notably an instance ADOPTED from a prior
    /// engine session (AD-5), which has no OS-portable, documented way to
    /// recover a pipe file descriptor from a bare PID. Distinct from
    /// [`EngineError::CapabilityUnsupported`]: it is this engine session's
    /// REACH that is limited, never the adapter's declared capability, so
    /// this must NEVER be misattributed to `CapabilityUnsupported` and must
    /// NEVER resolve to a silent success. Also distinct from
    /// [`EngineError::InteractionTimedOut`]: THIS variant means "no pipe was
    /// EVER recoverable in this session" (no handle held at all, or an
    /// adopted/never-piped handle); that one means "we HAD a live pipe,
    /// attempted the write, and it did not come back in time." Names the
    /// instance + the honest underlying cause.
    #[error("Agent Instance '{name}' cannot receive input right now: {detail}")]
    InteractionUnavailable {
        /// The instance `send` targeted.
        name: String,
        /// The honest underlying reason (e.g. no live pipe held in this
        /// engine session) — never a misattribution to the adapter's
        /// Capability Declaration.
        detail: String,
    },

    /// A [`Supervisor::send_input`](super::supervisor::Supervisor::send_input)
    /// write did not complete within the bounded stdin-write timeout (story
    /// 4.1 fix pass — the CRITICAL finding, review of #79: a genuinely stuck
    /// agent that never drains its input could otherwise block the write
    /// forever, freezing the ENTIRE engine — every instance shares ONE
    /// supervisor lock, so no other `start`/`stop`/`pause`/`send`/the
    /// crash-detection reaper could proceed until the write returned; an
    /// adversarial audit reproduced this empirically against the original
    /// unbounded `write_all`). Distinct from
    /// [`EngineError::InteractionUnavailable`] (that variant means "no pipe
    /// was EVER recoverable" — no handle held, an adopted instance, or one
    /// whose declared interaction is unsupported); this means "we HAD a live
    /// pipe, attempted the write, and it did not come back in time." The
    /// instance's interaction channel is now PERMANENTLY broken for the
    /// remainder of this engine session (until it is stopped and started
    /// again, which opens an entirely fresh pipe) — every SUBSEQUENT `send`
    /// on the same instance returns this immediately, without attempting
    /// another doomed write (a cheap check, no new I/O). Names the instance +
    /// the bound that elapsed.
    #[error(
        "Agent Instance '{name}' is not draining its input within {timeout_secs}s (it may be \
         stuck); this engine session's interaction channel for it is now unavailable — stop and \
         start it again for a fresh one"
    )]
    InteractionTimedOut {
        /// The instance `send` targeted.
        name: String,
        /// The bound (seconds) that elapsed before the write was abandoned.
        timeout_secs: u64,
    },

    /// A [`Supervisor::stop`](super::supervisor::Supervisor::stop) call sent
    /// SIGKILL (or the platform equivalent) but could not CONFIRM the
    /// process's death within the bounded window (fix pass, review of #80
    /// follow-up — the CRITICAL finding: see
    /// [`crate::ports::KILL_CONFIRM_TIMEOUT`]'s docs for the full mechanism —
    /// removing the pipe from agent output capture, review of #80's earlier
    /// crash-safety fix, also removed the incidental backpressure it
    /// provided, so a fast writer can exhaust disk and enter an OS-level
    /// uninterruptible I/O wait immune to every signal, including SIGKILL).
    ///
    /// The instance remains [`LifecycleState::Stopping`](super::lifecycle::LifecycleState::Stopping)
    /// — NOT `Stopped` (that would be a lie we cannot back up) and NOT a
    /// fabricated new terminal state — until a LATER reconciliation confirms
    /// the process has actually exited: either a RETRY `stop()` call (which
    /// performs a cheap, non-blocking liveness check rather than re-running
    /// the whole SIGTERM/SIGKILL/confirm sequence — see
    /// [`Supervisor::stop`](super::supervisor::Supervisor::stop)'s docs) or
    /// the crash-detection reaper's own next poll, whichever observes the
    /// exit first. Mirrors [`EngineError::InteractionTimedOut`]'s shape
    /// (story 4.1 fix pass) for an analogous "we hit a bounded resilience
    /// wait and gave up honestly" case, applied to `stop` instead of `send`.
    /// Names the instance + the bound that elapsed.
    #[error(
        "Agent Instance '{name}' was sent SIGKILL but has not been confirmed dead within \
         {timeout_secs}s (it may be stuck in an OS-level I/O wait, e.g. disk pressure); it \
         remains 'stopping' — a later `stop` retry will check again without re-blocking if it \
         is still stuck, and will succeed once the process actually exits"
    )]
    StopUnconfirmed {
        /// The instance `stop` targeted.
        name: String,
        /// The bound (seconds) that elapsed before confirmation was abandoned.
        timeout_secs: u64,
    },

    /// A [`StateStore`](crate::ports::StateStore) operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}
