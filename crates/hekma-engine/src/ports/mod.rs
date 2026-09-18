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
mod secret_resolver;
mod state_store;
mod usage_source;

pub use memory_backing::{GuaranteeLevel, MemoryBacking, MemoryBackingKind, MemoryBackingStatus};

pub(crate) use process_backend::{
    spawn_output_capture, write_stdin_bounded, LogCapture, LOG_ROTATE_GENERATIONS,
};
pub use process_backend::{
    BackendError, ProcessBackend, ProcessFingerprint, ProcessStatus, SpawnSpec, StdinState,
    StopOutcome, KILL_CONFIRM_TIMEOUT, STDIN_WRITE_TIMEOUT,
};
pub use secret_resolver::{
    file_permissions_error, mode_is_owner_only, CompositeSecretResolver, EnvSecretResolver,
    FileSecretResolver, SecretError, SecretResolver,
};
pub use state_store::{SpawnRecord, StateStore};
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

    /// Any other backend failure (open, migrate, I/O, SQL execution).
    #[error("state store backend error: {0}")]
    Backend(String),
}
