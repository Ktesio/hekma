//! Schema v18 (story 23-1, spine AD-26 / AD-27): the shared Metering
//! Proxy's state in state.db — endpoint bindings, their labels, routes and
//! per-binding credentials, a process-owning Run's recorded upstream, the
//! Run rollup re-keyed by run id (instance-less wire Runs have totals too),
//! nullable breach instances, and the proxy's sticky port.
//!
//! v18 is one crash-atomic step (the 11-3 rule): DDL, the two table
//! rebuilds and the `user_version = 18` stamp in one `BEGIN IMMEDIATE`,
//! one-way, no backfill beyond the 1:1 copies.

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::backend;
use crate::ports::StoreError;

/// The v18 DDL.
///
/// * `run_bindings` — an Operator endpoint binding: a path prefix, its
///   upstream base URL and an inert custodied-key reference.
/// * `binding_labels` — the labels the binding's Runs carry (origin
///   `binding`).
/// * `binding_routes` — named alternative upstreams (used by reroute, a
///   later story; stored and validated now).
/// * `binding_credentials` — per-binding announce and proxy-access
///   credentials: the hash and token id only, one active per class.
/// * `runs.upstream_base_url` — a process-owning Run's upstream, recorded
///   in its launch job.
/// * `usage_rollup_run` — rebuilt keyed by `run_id` alone, `instance_id`
///   nullable (one row per Run already, so the copy is 1:1).
/// * `breach_events` — rebuilt with a nullable `instance_id` (a wire Run's
///   breach names no instance).
/// * `proxy_state` — the sticky proxy port of this state directory.
pub(super) const SCHEMA_V18: &str = "\
CREATE TABLE IF NOT EXISTS run_bindings (
    name              TEXT PRIMARY KEY,
    path_prefix       TEXT NOT NULL UNIQUE,
    upstream_base_url TEXT NOT NULL,
    custodied_key     TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS binding_labels (
    binding TEXT NOT NULL REFERENCES run_bindings(name) ON DELETE CASCADE,
    key     TEXT NOT NULL,
    value   TEXT NOT NULL,
    PRIMARY KEY (binding, key)
);
CREATE TABLE IF NOT EXISTS binding_routes (
    binding           TEXT NOT NULL REFERENCES run_bindings(name) ON DELETE CASCADE,
    route             TEXT NOT NULL,
    upstream_base_url TEXT NOT NULL,
    custodied_key     TEXT,
    created_at        TEXT NOT NULL,
    PRIMARY KEY (binding, route)
);
CREATE TABLE IF NOT EXISTS binding_credentials (
    token_hash   BLOB PRIMARY KEY CHECK (length(token_hash) = 32),
    token_id     TEXT NOT NULL,
    binding      TEXT NOT NULL REFERENCES run_bindings(name) ON DELETE CASCADE,
    class        TEXT NOT NULL CHECK (class IN ('announce','proxy-access')),
    state        TEXT NOT NULL CHECK (state IN ('active','revoked')),
    issued_at    TEXT NOT NULL,
    revoked_at   TEXT,
    revoke_cause TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_binding_credentials_active
    ON binding_credentials(binding, class) WHERE state = 'active';
ALTER TABLE runs ADD COLUMN upstream_base_url TEXT;
CREATE TABLE usage_rollup_run_v18 (
    run_id         TEXT PRIMARY KEY,
    instance_id    INTEGER REFERENCES agent_instances(id) ON DELETE CASCADE,
    events         INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_unknown INTEGER NOT NULL DEFAULT 0,
    cost_micros    INTEGER NOT NULL DEFAULT 0
);
INSERT INTO usage_rollup_run_v18 (run_id, instance_id, events, input_tokens, output_tokens,
    cached_tokens, cached_unknown, cost_micros)
    SELECT run_id, MIN(instance_id), SUM(events), SUM(input_tokens), SUM(output_tokens),
        SUM(cached_tokens), SUM(cached_unknown), SUM(cost_micros)
    FROM usage_rollup_run GROUP BY run_id;
DROP TABLE usage_rollup_run;
ALTER TABLE usage_rollup_run_v18 RENAME TO usage_rollup_run;
CREATE TABLE breach_events_v18 (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id INTEGER REFERENCES agent_instances(id) ON DELETE CASCADE,
    run_id      TEXT NOT NULL,
    dimension   TEXT NOT NULL,
    scope       TEXT NOT NULL,
    action      TEXT NOT NULL,
    event       TEXT NOT NULL
);
INSERT INTO breach_events_v18 (id, instance_id, run_id, dimension, scope, action, event)
    SELECT id, instance_id, run_id, dimension, scope, action, event
    FROM breach_events ORDER BY id;
DROP TABLE breach_events;
ALTER TABLE breach_events_v18 RENAME TO breach_events;
CREATE INDEX idx_breach_events_instance ON breach_events(instance_id, id);
CREATE INDEX idx_breach_events_run ON breach_events(run_id);
CREATE TABLE IF NOT EXISTS proxy_state (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    port INTEGER
);
";

/// Test-only: take a migrated store back below v18 for the older steps'
/// upgrade tests (drops v18's additions; the two rebuilt tables keep their
/// v18 shape, which the v18 copy reads just the same).
#[cfg(test)]
pub(crate) const UNDO_V18_FOR_TEST: &str = "\
DROP INDEX idx_binding_credentials_active;
DROP TABLE binding_credentials;
DROP TABLE binding_routes;
DROP TABLE binding_labels;
DROP TABLE run_bindings;
DROP TABLE proxy_state;
ALTER TABLE runs DROP COLUMN upstream_base_url;
";

/// Run the v18 step (DDL + rebuilds + stamp) in one `BEGIN IMMEDIATE`,
/// stepping aside when another process finished it while this one waited.
pub(super) fn migrate_v18(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 18 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V18).map_err(backend)?;
        conn.execute_batch("PRAGMA user_version = 18")
            .map_err(backend)?;
        Ok(true)
    };
    match body() {
        Ok(true) => conn.execute_batch("COMMIT").map_err(|err| {
            let _ = conn.execute_batch("ROLLBACK");
            backend(err)
        }),
        Ok(false) => {
            let _ = conn.execute_batch("ROLLBACK");
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

impl super::SqliteStore {
    /// The sticky proxy port recorded for this state directory (`None`
    /// before the first host bind recorded one).
    pub(crate) fn proxy_port(&self) -> Result<Option<u16>, StoreError> {
        let port: Option<Option<i64>> = self
            .conn()
            .query_row("SELECT port FROM proxy_state WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(backend)?;
        Ok(port
            .flatten()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0))
    }

    /// Record the sticky proxy port (a writer job).
    pub(crate) fn set_proxy_port(&self, port: u16) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO proxy_state (id, port) VALUES (1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET port = excluded.port",
                [i64::from(port)],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Record a process-owning Run's upstream (its launch job).
    pub(crate) fn set_run_upstream(
        &self,
        run_id: &crate::domain::RunId,
        upstream: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "UPDATE runs SET upstream_base_url = ?2 WHERE run_id = ?1",
                rusqlite::params![run_id.as_str(), upstream],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// A Run's recorded upstream.
    pub(crate) fn run_upstream(
        &self,
        run_id: &crate::domain::RunId,
    ) -> Result<Option<String>, StoreError> {
        self.conn()
            .query_row(
                "SELECT upstream_base_url FROM runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(backend)
            .map(Option::flatten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_v18_rebuild_copies_the_run_rollup_and_breaches_one_to_one() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_instances (id INTEGER PRIMARY KEY);
             INSERT INTO agent_instances (id) VALUES (1);
             CREATE TABLE runs (run_id TEXT PRIMARY KEY);
             CREATE TABLE usage_rollup_run (instance_id INTEGER NOT NULL, run_id TEXT NOT NULL,
                events INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0, cached_tokens INTEGER NOT NULL DEFAULT 0,
                cached_unknown INTEGER NOT NULL DEFAULT 0, cost_micros INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (instance_id, run_id));
             INSERT INTO usage_rollup_run VALUES (1, 'r1', 2, 10, 20, 3, 0, 99);
             CREATE TABLE breach_events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id INTEGER NOT NULL, run_id TEXT NOT NULL, dimension TEXT NOT NULL,
                scope TEXT NOT NULL, action TEXT NOT NULL, event TEXT NOT NULL);
             INSERT INTO breach_events VALUES (7, 1, 'r1', 'tokens', 'per-run', 'warn', '{}');
             PRAGMA user_version = 17;",
        )
        .unwrap();
        migrate_v18(&conn).unwrap();
        let row: (String, Option<i64>, i64, i64) = conn
            .query_row(
                "SELECT run_id, instance_id, events, cost_micros FROM usage_rollup_run",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, ("r1".to_string(), Some(1), 2, 99));
        let id: i64 = conn
            .query_row("SELECT id FROM breach_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, 7);
        // An instance-less breach row is accepted now.
        conn.execute(
            "INSERT INTO breach_events (instance_id, run_id, dimension, scope, action, event) \
             VALUES (NULL, 'w1', 'tokens', 'per-run', 'block', '{}')",
            [],
        )
        .unwrap();
        // The step is idempotent (stepping aside at 18).
        migrate_v18(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
}
