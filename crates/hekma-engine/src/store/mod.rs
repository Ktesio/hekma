//! State store implementations (spine AD-6).
//!
//! The SQLite [`StateStore`](crate::ports::StateStore) implementation lives
//! here, behind the port. SQL stays in this module; domain code speaks domain
//! types (AD-1). Nothing outside the engine sees this — the registry service is
//! the public surface, the store is an internal collaborator.

mod run_store_sqlite;
mod sqlite;
pub(crate) mod writer;

pub(crate) use run_store_sqlite::{LegacyLogs, LegacyOutbox, LegacySource};
#[cfg(test)]
pub(crate) use sqlite::create_v11_for_test;
pub(crate) use sqlite::SqliteStore;
pub(crate) use sqlite::BUSY_TIMEOUT_MS;
pub(crate) use writer::{StoreWriter, WriterCtx};
