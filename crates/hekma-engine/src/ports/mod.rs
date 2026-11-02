//! Hexagonal ports (spine AD-1).
//!
//! Ports are the traits through which all variability enters the engine core.
//! This crate realizes four: [`StateStore`] (persistence), [`ProcessBackend`]
//! (per-OS process control, AD-4 — its per-OS impls live in `backends/`, the sole
//! allowlisted `#[cfg]` home), from story 2-4 [`SecretResolver`] (secret
//! resolution, AD-10 — env + the 0600 secrets file; OS-keychain stays a deferred
//! resolver behind the same port), and, from story 3-1, [`UsageSource`] (the AD-7
//! metering INGESTION seam — the self-reported channel that yields usage from a
//! running instance). Story 3-4 landed the SECOND source, [`ObservedUsageSource`]
//! (`engine-observed`): it is fed by the loopback forward listener
//! (`crate::metering`) and, being event-driven rather than a log-tail drainer,
//! mints its [`ParsedUsage`] directly while yielding the SAME shape into the SAME
//! commit choke point. Story 5-1 landed the Memory Backing surface (AD-11) as the
//! [`memory_backing`] module: deliberately NOT a trait (A-8 — the `filesystem`
//! implementation is pure path authority inside the engine), the module and its
//! types ARE the seam richer backings land behind later. No speculative port
//! trees beyond it.

mod memory_backing;
mod process_backend;
mod run_store;
mod secret_resolver;
mod state_store;
mod usage_source;

pub use memory_backing::{GuaranteeLevel, MemoryBacking, MemoryBackingKind, MemoryBackingStatus};

pub(crate) use process_backend::{
    spawn_output_capture, write_stdin_bounded, LogCapture, LOG_ROTATE_GENERATIONS,
    LOG_ROTATE_MAX_BYTES,
};
pub use process_backend::{
    BackendError, LogCaptureMode, ProcessBackend, ProcessFingerprint, ProcessStatus, SpawnSpec,
    StdinState, StopOutcome, KILL_CONFIRM_TIMEOUT, STDIN_WRITE_TIMEOUT,
};
pub(crate) use run_store::{
    clamp_page, OutboxAppend, OutboxBounds, Page, Rollup, RunStore, PAGE_MAX,
};
pub use secret_resolver::{
    file_permissions_error, mode_is_owner_only, CompositeSecretResolver, EnvSecretResolver,
    FileSecretResolver, SecretError, SecretResolver,
};
pub use state_store::{MemoryBackingTxn, SpawnRecord, StateStore};
pub use usage_source::{
    assemble_usage_event, format_usage_line, parse_usage_block, parse_usage_line,
    ObservedUsageSource, ParsedUsage, SelfReportedUsageSource, UsageSource, USAGE_SENTINEL_PREFIX,
};

use thiserror::Error;

/// Errors surfaced by a [`StateStore`] implementation, in domain terms.
///
/// The port speaks these — not SQLite error codes. The SQLite implementation
/// maps `rusqlite::Error` into these variants (e.g. a `UNIQUE` constraint
/// violation on `name` becomes [`StoreError::DuplicateName`]). Kept `thiserror`
/// only (no `miette` in the lib — conventions).
#[derive(Debug, Error)]
pub enum StoreError {
    /// A row with the same instance name already exists.
    #[error("an Agent Instance named '{name}' already exists")]
    DuplicateName {
        /// The conflicting instance name.
        name: String,
    },

    /// The requested instance does not exist.
    #[error("no Agent Instance named '{name}' exists")]
    NotFound {
        /// The missing instance name.
        name: String,
    },

    /// A stored row held a value the domain could not decode (e.g. an
    /// unrecognized Lifecycle State written by a future schema version).
    #[error("corrupt state row for '{name}': {detail}")]
    CorruptRow {
        /// The instance name whose row failed to decode.
        name: String,
        /// What specifically failed to decode.
        detail: String,
    },

    /// The database was created by a newer ktesio (its `user_version` is
    /// ahead of the schema this build understands). Refuse rather than
    /// silently downgrade it (which would corrupt a forward schema).
    #[error(
        "state database was created by a newer ktesio (schema v{found}; this build understands v{supported}); upgrade ktesio"
    )]
    SchemaTooNew {
        /// The `user_version` found on disk.
        found: i64,
        /// The highest schema version this build applies.
        supported: i64,
    },

    /// A memory-backing re-attach tried to CHANGE the kind on a row that
    /// already holds one. Port-level A-6 (2026-09-22 hardening): the
    /// registry's same-kind idempotence is the only sanctioned re-attach,
    /// and the store now enforces it below the registry, so a caller that
    /// bypasses the registry cannot silently replace the attachment's kind.
    #[error(
        "memory backing for '{name}' is already attached as '{attached}'; refusing the kind change to '{requested}' (detach first)"
    )]
    MemoryBackingKindConflict {
        /// The instance name the row belongs to.
        name: String,
        /// The kind already stored on the row.
        attached: String,
        /// The kind the caller attempted to write.
        requested: String,
    },

    /// A memory-backing mutation's terminal-state guard refused INSIDE the
    /// transactional attach/detach (story 15-6): another process (a second CLI
    /// Engine on the same state root) moved the instance to a non-terminal
    /// Lifecycle State between the registry's pre-check read and the
    /// `BEGIN IMMEDIATE` commit. The transaction rolled back, so the row is
    /// byte-unchanged; the message mirrors the registry's
    /// `RegistryError::MemoryBackingHotSwap` refusal (this store variant —
    /// one of story 15-6's new surfaces — names the SHIPPED `hekma` binary;
    /// the registry string above it is the untouched story-5-1 legacy).
    #[error(
        "Agent Instance '{name}' is '{state}'; a Memory Backing cannot be hot-swapped — \
         attach/detach need a terminal state (registered, stopped, or failed). Bring it to \
         a terminal state first: hekma agent stop {name} from running or paused"
    )]
    MemoryBackingHotSwap {
        /// The instance whose backing was being changed.
        name: String,
        /// The non-terminal state the transactional guard found (wire form).
        state: String,
    },

    /// Story 15-6: start's AUTHORITATIVE backing read (the transition
    /// transaction's snapshot) disagrees with the start path's early
    /// pre-check read — a second process attached or detached the backing
    /// inside start's read→transition window. The conditional transition
    /// ROLLED BACK (the instance keeps its prior terminal state and no
    /// process was launched; the inert pre-transition artifacts — the
    /// effective-config snapshot and the log directories — remain,
    /// harmlessly rewritten by the next start); the caller retries the whole
    /// start. Names both sides.
    #[error(
        "the memory backing for '{name}' changed under start: the pre-check read {precheck}, \
         but the store transaction read {snapshot}; no agent was launched and the instance \
         state is unchanged (only the inert pre-start config snapshot and log directories \
         remain — the next start rewrites them) — retry the start (it now sees the new backing)"
    )]
    MemoryBackingChangedUnderStart {
        /// The instance being started.
        name: String,
        /// The pre-check's view: `"none"` or `kind '<k>' attached at <ts>`.
        precheck: String,
        /// The transaction snapshot's view: same format as `precheck`.
        snapshot: String,
    },

    /// Story 15-6: a CONDITIONAL lifecycle transition found the instance in a
    /// state other than the one the caller read — another process transitioned
    /// it first (e.g. a second `start` on the same instance). The transaction
    /// rolled back; nothing was persisted.
    #[error(
        "Agent Instance '{name}' is '{found}', not the expected '{expected}'; another \
         command transitioned it first, so the conditional transition was refused"
    )]
    StateConflict {
        /// The instance whose state was transitioned.
        name: String,
        /// The state the caller's read observed (wire form).
        expected: String,
        /// The state actually present inside the transaction (wire form).
        found: String,
    },

    /// Story 15-6 (the AD-17 busy bound): a writer could not take the write
    /// lock within the `busy_timeout` bounded-wait window (another hekma
    /// command holds a write transaction). Typed, never a raw
    /// `SQLITE_BUSY`/`database is locked` string — the bounded-wait window
    /// and the retry remediation are part of the message. Review round 1:
    /// the wording names the WINDOW, never a fully-elapsed duration — the
    /// carried value is the configured bound, not a measured elapsed time.
    #[error(
        "the state store remained busy within the bounded {waited_ms}ms window (another hekma \
         command holds the write transaction); retry the command"
    )]
    Busy {
        /// The bounded-wait window that was in force, in milliseconds (the
        /// configured `busy_timeout` — not a measured elapsed time).
        waited_ms: u64,
    },

    /// Any other backend failure (open, migrate, I/O, SQL execution).
    #[error("state store backend error: {0}")]
    Backend(String),
}
