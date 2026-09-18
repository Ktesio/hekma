//! State store implementations (spine AD-6).
//!
//! The SQLite [`StateStore`](crate::ports::StateStore) implementation lives
//! here, behind the port. SQL stays in this module; domain code speaks domain
//! types (AD-1). Nothing outside the engine sees this — the registry service is
//! the public surface, the store is an internal collaborator.

mod sqlite;

pub(crate) use sqlite::SqliteStore;
