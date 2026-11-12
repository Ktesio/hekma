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

/// One stored binding (story 23-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingRow {
    pub name: String,
    pub prefix: String,
    pub upstream: String,
    pub custodied_key: Option<String>,
    pub labels: Vec<(String, String)>,
    pub created_at: String,
    pub updated_at: String,
}

/// One stored binding route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteRow {
    pub route: String,
    pub upstream: String,
    pub custodied_key: Option<String>,
    pub created_at: String,
}

/// One stored binding credential (metadata only; the hash stays here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingCredentialRow {
    pub token_id: String,
    pub class: String,
    pub state: String,
    pub issued_at: String,
    pub revoked_at: Option<String>,
}

impl super::SqliteStore {
    /// Every binding with its labels, by name.
    pub(crate) fn bindings(&self) -> Result<Vec<BindingRow>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT name, path_prefix, upstream_base_url, custodied_key, created_at, \
                 updated_at FROM run_bindings ORDER BY name",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(BindingRow {
                    name: row.get(0)?,
                    prefix: row.get(1)?,
                    upstream: row.get(2)?,
                    custodied_key: row.get(3)?,
                    labels: Vec::new(),
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        let mut out = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.labels = self.binding_labels(&row.name)?;
            out.push(row);
        }
        Ok(out)
    }

    /// One binding with its labels.
    pub(crate) fn binding(&self, name: &str) -> Result<Option<BindingRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT name, path_prefix, upstream_base_url, custodied_key, created_at, \
                 updated_at FROM run_bindings WHERE name = ?1",
                [name],
                |row| {
                    Ok(BindingRow {
                        name: row.get(0)?,
                        prefix: row.get(1)?,
                        upstream: row.get(2)?,
                        custodied_key: row.get(3)?,
                        labels: Vec::new(),
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(backend)?;
        match row {
            Some(mut row) => {
                row.labels = self.binding_labels(&row.name)?;
                Ok(Some(row))
            }
            None => Ok(None),
        }
    }

    fn binding_labels(&self, name: &str) -> Result<Vec<(String, String)>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT key, value FROM binding_labels WHERE binding = ?1 ORDER BY key")
            .map_err(backend)?;
        let rows = stmt
            .query_map([name], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    /// Insert or replace a binding and its labels (the caller validated it).
    pub(crate) fn put_binding(&self, row: &BindingRow) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO run_bindings (name, path_prefix, upstream_base_url, custodied_key, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(name) DO UPDATE SET path_prefix = excluded.path_prefix, \
                 upstream_base_url = excluded.upstream_base_url, \
                 custodied_key = excluded.custodied_key, updated_at = excluded.updated_at",
                rusqlite::params![
                    row.name,
                    row.prefix,
                    row.upstream,
                    row.custodied_key,
                    row.created_at,
                    row.updated_at
                ],
            )
            .map_err(backend)?;
        self.conn()
            .execute("DELETE FROM binding_labels WHERE binding = ?1", [&row.name])
            .map_err(backend)?;
        for (key, value) in &row.labels {
            self.conn()
                .execute(
                    "INSERT INTO binding_labels (binding, key, value) VALUES (?1, ?2, ?3)",
                    rusqlite::params![row.name, key, value],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    /// Delete a binding (its labels, routes and credentials cascade).
    pub(crate) fn delete_binding(&self, name: &str) -> Result<bool, StoreError> {
        // The cascade needs foreign keys; delete the children explicitly so
        // the delete holds on a connection without them.
        for table in ["binding_labels", "binding_routes", "binding_credentials"] {
            self.conn()
                .execute(&format!("DELETE FROM {table} WHERE binding = ?1"), [name])
                .map_err(backend)?;
        }
        let deleted = self
            .conn()
            .execute("DELETE FROM run_bindings WHERE name = ?1", [name])
            .map_err(backend)?;
        Ok(deleted == 1)
    }

    /// A binding's routes, by name.
    pub(crate) fn binding_routes(&self, name: &str) -> Result<Vec<RouteRow>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT route, upstream_base_url, custodied_key, created_at FROM binding_routes \
                 WHERE binding = ?1 ORDER BY route",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([name], |row| {
                Ok(RouteRow {
                    route: row.get(0)?,
                    upstream: row.get(1)?,
                    custodied_key: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    /// Insert or replace one route.
    pub(crate) fn put_binding_route(
        &self,
        binding: &str,
        row: &RouteRow,
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO binding_routes (binding, route, upstream_base_url, custodied_key, \
                 created_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(binding, route) DO UPDATE \
                 SET upstream_base_url = excluded.upstream_base_url, \
                 custodied_key = excluded.custodied_key",
                rusqlite::params![
                    binding,
                    row.route,
                    row.upstream,
                    row.custodied_key,
                    row.created_at
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Remove one route; whether it existed.
    pub(crate) fn remove_binding_route(
        &self,
        binding: &str,
        route: &str,
    ) -> Result<bool, StoreError> {
        self.conn()
            .execute(
                "DELETE FROM binding_routes WHERE binding = ?1 AND route = ?2",
                [binding, route],
            )
            .map_err(backend)
            .map(|n| n == 1)
    }

    /// Insert one active binding credential.
    pub(crate) fn insert_binding_credential(
        &self,
        binding: &str,
        hash: &crate::domain::credential::TokenHash,
        token_id: &str,
        class: &str,
        at: &str,
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO binding_credentials (token_hash, token_id, binding, class, state, \
                 issued_at) VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
                rusqlite::params![hash.as_bytes().as_slice(), token_id, binding, class, at],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Revoke a binding's active credential of `class`; its token id when
    /// one was active.
    pub(crate) fn revoke_binding_credential(
        &self,
        binding: &str,
        class: &str,
        at: &str,
        cause: &str,
    ) -> Result<Option<String>, StoreError> {
        self.conn()
            .query_row(
                "UPDATE binding_credentials SET state = 'revoked', revoked_at = ?3, \
                 revoke_cause = ?4 WHERE binding = ?1 AND class = ?2 AND state = 'active' \
                 RETURNING token_id",
                rusqlite::params![binding, class, at, cause],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(backend)
    }

    /// A binding's credentials (metadata only), newest first.
    pub(crate) fn binding_credentials(
        &self,
        binding: &str,
    ) -> Result<Vec<BindingCredentialRow>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT token_id, class, state, issued_at, revoked_at FROM binding_credentials \
                 WHERE binding = ?1 ORDER BY issued_at DESC, rowid DESC",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([binding], |row| {
                Ok(BindingCredentialRow {
                    token_id: row.get(0)?,
                    class: row.get(1)?,
                    state: row.get(2)?,
                    issued_at: row.get(3)?,
                    revoked_at: row.get(4)?,
                })
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    /// The open Runs of a binding: its own Run and its split Runs (story
    /// 23-1: identity namespaces `binding` / `<name>` and `binding:<name>`).
    pub(crate) fn binding_open_runs(
        &self,
        binding: &str,
    ) -> Result<Vec<crate::domain::RunId>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT i.run_id FROM run_identities i JOIN runs r ON r.run_id = i.run_id \
                 WHERE r.status <> 'ended' AND ((i.binding = 'binding' AND i.external_id = ?1) \
                 OR i.binding = 'binding:' || ?1) ORDER BY i.run_id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([binding], |row| row.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows
            .into_iter()
            .map(crate::domain::RunId::from_wire)
            .collect())
    }

    /// How many open Runs carry an identity in `namespace` (the breadth
    /// cap's count for a binding's splits).
    pub(crate) fn open_identities_in(&self, namespace: &str) -> Result<u64, StoreError> {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM run_identities i JOIN runs r ON r.run_id = i.run_id \
                 WHERE i.binding = ?1 AND r.status <> 'ended'",
                [namespace],
                |row| row.get::<_, i64>(0),
            )
            .map_err(backend)
            .map(|n| n.max(0) as u64)
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
