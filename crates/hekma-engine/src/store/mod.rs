//! State store implementations (spine AD-6).
//!
//! The SQLite [`StateStore`](crate::ports::StateStore) implementation lives
//! here, behind the port. SQL stays in this module; domain code speaks domain
//! types (AD-1). Nothing outside the engine sees this — the registry service is
//! the public surface, the store is an internal collaborator.

pub(crate) mod calls_read;
pub(crate) mod calls_sqlite;
pub(crate) mod proxy_sqlite;
pub(crate) mod retention_sqlite;
mod run_store_sqlite;
pub(crate) mod runs_sqlite;
pub(crate) use runs_sqlite::BreachDefaultSource;
mod sqlite;
mod tokens_sqlite;
pub(crate) mod writer;

pub(crate) use run_store_sqlite::{LegacyLogs, LegacyOutbox, LegacySource};
#[cfg(test)]
pub(crate) use sqlite::create_v11_for_test;
pub(crate) use sqlite::SqliteStore;
pub(crate) use sqlite::BUSY_TIMEOUT_MS;
#[cfg(test)]
pub(crate) use sqlite::SCHEMA_VERSION;
pub(crate) use writer::{read_only_refusal, StoreWriter, WriterCtx};
