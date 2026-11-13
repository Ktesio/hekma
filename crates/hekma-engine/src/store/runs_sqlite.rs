//! The Run tables (story 21-2, spine AD-25 / AD-27): schema v13, its
//! backfill, and the SQLite side of the [`RunRecords`] port.
//!
//! v13 is one crash-atomic step (the 11-3 rule): the DDL, the backfill of the
//! live and historical run ids into `runs`, and the `run_latches` rebuild
//! with its foreign key commit with the `user_version = 13` stamp, or none of
//! it does.
//!
//! v14 (story 21-9) records each bound label's origin (`run_labels.origin`:
//! `config` or `label-verb`, NULL on a Caller row) and backfills `config` on
//! every bound row — the only bound source before it. One crash-atomic step,
//! like v13.

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::{backend, SqliteStore};
use crate::domain::runs::listing::{ListingCounts, ListingView};
use crate::domain::runs::policy::{PolicyRecord, RunPolicy, ScentCheckMode, DEFAULT_POLICY_NAME};
use crate::domain::{
    Anomaly, BreachAction, ControlDepth, InstanceName, Label, LabelOrigin, LabelSource, Micros,
    Rate, RunId, RunLimits, RunRecord, RunStatus, Selector,
};
use crate::ports::{NewRun, PolicyRecords, RunRecords, StoreError};

/// One listing statement and the parameters it binds, in order (story
/// 21-5). The production reads and the `EXPLAIN QUERY PLAN` pins build
/// their SQL through the SAME constructors below, so the plan gate pins
/// the statements that actually run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListingStatement {
    /// The SQL text (numbered `?N` parameters).
    pub sql: String,
    /// The values `?1`, `?2`, … bind, in order.
    pub params: Vec<String>,
}

/// The view arms' `run_id` selection (the v16 partial indexes' shape, story
/// 21-5): two disjoint arms whose WHERE clauses name the partial predicates
/// verbatim, merged `UNION ALL`, newest run id first. `?1` is the window
/// cutoff; `?2` (when `after`) is the cursor's run id; the Selector's terms
/// bind after them.
///
/// The keyset AND the Selector sit INSIDE each arm, ahead of the page
/// `LIMIT`: the page is the newest `limit` rows that match the view AND
/// the Selector. (Filtering after the limit returned the newest `limit`
/// view rows intersected with the Selector — short pages that ended the
/// read early whenever newer Runs did not match.)
pub(super) fn listing_arms(
    view: ListingView,
    selector: Option<&Selector>,
    after: Option<&str>,
    cutoff: &str,
    limit: Option<usize>,
) -> ListingStatement {
    let mut params = vec![cutoff.to_string()];
    let keyset = match after {
        Some(after) => {
            params.push(after.to_string());
            " AND run_id < ?2"
        }
        None => "",
    };
    let filter = match selector {
        Some(selector) => {
            let (arms, mut selector_params) = selector_arms_sql(selector, params.len() + 1);
            params.append(&mut selector_params);
            format!(" AND run_id IN ({arms})")
        }
        None => String::new(),
    };
    let page = limit
        .map(|limit| format!(" LIMIT {limit}"))
        .unwrap_or_default();
    let sql = match view {
        ListingView::Exceptions => format!(
            "SELECT run_id FROM runs WHERE status <> 'ended' \
             AND (anomalies <> '' OR status IN ('held','silent','stale')){keyset}{filter} \
             UNION ALL SELECT run_id FROM runs INDEXED BY idx_runs_ended_at \
             WHERE status = 'ended' AND ended_at >= ?1 AND anomalies <> ''{keyset}{filter} \
             ORDER BY run_id DESC{page}"
        ),
        ListingView::All => format!(
            "SELECT run_id FROM runs WHERE status <> 'ended'{keyset}{filter} \
             UNION ALL SELECT run_id FROM runs INDEXED BY idx_runs_ended_at \
             WHERE status = 'ended' AND ended_at >= ?1{keyset}{filter} \
             ORDER BY run_id DESC{page}"
        ),
    };
    ListingStatement { sql, params }
}

/// The page read's WHERE/ORDER tail (story 21-5): the arms capped at
/// `fetch` rows (the caller asks for one more than the page so it can tell
/// a full final page from a page with more behind it), then the outer row
/// read by primary key, newest first.
pub(super) fn listing_page_tail(
    view: ListingView,
    selector: Option<&Selector>,
    after: Option<&str>,
    cutoff: &str,
    fetch: usize,
) -> ListingStatement {
    let arms = listing_arms(view, selector, after, cutoff, Some(fetch));
    ListingStatement {
        sql: format!("WHERE r.run_id IN ({}) ORDER BY r.run_id DESC", arms.sql),
        params: arms.params,
    }
}

/// The listed count (story 21-5): every row the view's arms select under
/// the Selector — the same arms the page reads, unlimited.
pub(super) fn listing_listed_count(
    view: ListingView,
    selector: Option<&Selector>,
    cutoff: &str,
) -> ListingStatement {
    let arms = listing_arms(view, selector, None, cutoff, None);
    ListingStatement {
        sql: format!("SELECT COUNT(*) FROM runs WHERE run_id IN ({})", arms.sql),
        params: arms.params,
    }
}

/// The healthy-open count (story 21-5). It rides `idx_runs_open`: its
/// `status <> 'ended'` conjunct names the partial predicate verbatim
/// (`active` implies it; the explicit conjunct is what makes the index
/// usable).
pub(super) fn listing_healthy_count(selector: Option<&Selector>) -> ListingStatement {
    let mut sql = "SELECT COUNT(*) FROM runs WHERE status <> 'ended' AND status = 'active' \
                   AND anomalies = ''"
        .to_string();
    let mut params = Vec::new();
    if let Some(selector) = selector {
        let (arms, selector_params) = selector_arms_sql(selector, 1);
        sql.push_str(&format!(" AND run_id IN ({arms})"));
        params = selector_params;
    }
    ListingStatement { sql, params }
}

/// The Selector arms' `run_id` selection (the 21-9 `select_runs` shape,
/// minus its status filter and limit — the view predicate composes beside
/// it): the first term drives `idx_run_labels_kv`, every other probes the
/// `run_labels` primary key. Parameters are NUMBERED from `first` (so the
/// clause can repeat inside both view arms and bind once). Returns the SQL
/// and the parameters it binds.
pub(super) fn selector_arms_sql(selector: &Selector, first: usize) -> (String, Vec<String>) {
    let terms = selector.terms();
    let Some(((first_key, first_value), rest)) = terms.split_first() else {
        // Unreachable (a Selector always carries a term); an impossible arm
        // keeps the composition total anyway.
        return ("SELECT NULL AS run_id WHERE 0".to_string(), Vec::new());
    };
    let mut next = first;
    let mut placeholder = || {
        let text = format!("?{next}");
        next += 1;
        text
    };
    let mut sql = format!(
        "SELECT l0.run_id FROM run_labels l0 WHERE l0.key = {} AND l0.value = {}",
        placeholder(),
        placeholder()
    );
    let mut params = vec![first_key.clone(), first_value.clone()];
    for (key, value) in rest {
        sql.push_str(&format!(
            " AND EXISTS (SELECT 1 FROM run_labels lx WHERE lx.run_id = l0.run_id \
             AND lx.key = {} AND lx.value = {})",
            placeholder(),
            placeholder()
        ));
        params.push(key.clone());
        params.push(value.clone());
    }
    (sql, params)
}

/// Schema v13 DDL (story 21-2): the Run registry's tables.
///
/// * `runs` — one row per Run (AD-25). `instance_id` is the hosting
///   instance (nullable for instance-less Runs, which arrive with the shared
///   proxy); `process_owner = 1` marks the supervised Run that owns the
///   instance's process, at most one non-ended per instance (partial unique
///   index). `anomalies` is a sorted comma list (`''` = none), indexed for
///   exception-first reads. `lease_expires_at` stays NULL until Leases exist.
/// * `run_labels` — `key=value` with its source; indexed on (key, value)
///   for Selector matching.
/// * `run_limits` — ceilings set directly on a Run (dimension, scope).
/// * `run_identities` — external id → run id, unique per binding, so a
///   retried open or a concurrent first call never duplicates a Run.
const SCHEMA_V13: &str = "\
CREATE TABLE runs (
    run_id           TEXT PRIMARY KEY,
    control_depth    TEXT NOT NULL,
    wire_path        INTEGER NOT NULL DEFAULT 0,
    status           TEXT NOT NULL,
    anomalies        TEXT NOT NULL DEFAULT '',
    parent_run_id    TEXT REFERENCES runs(run_id) ON DELETE SET NULL,
    instance_id      INTEGER REFERENCES agent_instances(id) ON DELETE CASCADE,
    process_owner    INTEGER NOT NULL DEFAULT 0,
    end_cause        TEXT,
    lease_expires_at TEXT,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    ended_at         TEXT
);
CREATE INDEX idx_runs_parent ON runs(parent_run_id);
CREATE INDEX idx_runs_instance ON runs(instance_id, status);
CREATE UNIQUE INDEX idx_runs_open_owner ON runs(instance_id)
    WHERE process_owner = 1 AND status <> 'ended';
CREATE INDEX idx_runs_anomalous ON runs(status) WHERE anomalies <> '';
CREATE TABLE run_labels (
    run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    key    TEXT NOT NULL,
    value  TEXT NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY (run_id, key)
);
CREATE INDEX idx_run_labels_kv ON run_labels(key, value);
CREATE TABLE run_limits (
    run_id      TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    dimension   TEXT NOT NULL,
    scope       TEXT NOT NULL,
    limit_value INTEGER NOT NULL,
    action      TEXT NOT NULL,
    source      TEXT NOT NULL,
    set_at      TEXT NOT NULL,
    PRIMARY KEY (run_id, dimension, scope)
);
CREATE TABLE run_identities (
    binding     TEXT NOT NULL,
    external_id TEXT NOT NULL,
    run_id      TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL,
    UNIQUE (binding, external_id)
);
";

/// The backfill: the live run of every spawn record becomes a supervised,
/// process-owning Run with its legacy id and its projected status; every
/// other run id the ledger, the breach records or the latches name becomes an
/// `ended` Run; then `run_latches` gains its foreign key (AD-27: "story 21-2
/// adds the reference"). `?1` is the upgrade time.
const BACKFILL_V13: &str = "\
INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, instance_id,
    process_owner, end_cause, created_at, updated_at, ended_at)
SELECT r.run_id, 'supervised',
    COALESCE((SELECT u.metering_source = 'engine-observed' FROM usage_events u
              WHERE u.instance_id = r.instance_id ORDER BY u.id DESC LIMIT 1), 0),
    CASE i.state WHEN 'paused' THEN 'held'
                 WHEN 'stopped' THEN 'ended' WHEN 'registered' THEN 'ended'
                 WHEN 'failed' THEN 'ended'
                 ELSE 'active' END,
    CASE i.state WHEN 'failed' THEN 'failed' ELSE '' END,
    r.instance_id, 1,
    CASE WHEN i.state IN ('stopped', 'registered', 'failed') THEN 'before-runs' END,
    COALESCE((SELECT MIN(u.occurred_at) FROM usage_events u WHERE u.run_id = r.run_id), ?1),
    ?1,
    CASE WHEN i.state IN ('stopped', 'registered', 'failed') THEN ?1 END
FROM agent_runtime r JOIN agent_instances i ON i.id = r.instance_id
WHERE r.run_id IS NOT NULL;
INSERT OR IGNORE INTO runs (run_id, control_depth, wire_path, status, instance_id,
    process_owner, end_cause, created_at, updated_at, ended_at)
SELECT run_id, 'supervised', MAX(metering_source = 'engine-observed'), 'ended',
    MIN(instance_id), 1, 'before-runs', MIN(occurred_at), ?1, MAX(occurred_at)
FROM usage_events GROUP BY run_id;
INSERT OR IGNORE INTO runs (run_id, control_depth, wire_path, status, instance_id,
    process_owner, end_cause, created_at, updated_at, ended_at)
SELECT run_id, 'supervised', 0, 'ended', MIN(instance_id), 1, 'before-runs', ?1, ?1, ?1
FROM breach_events GROUP BY run_id;
INSERT OR IGNORE INTO runs (run_id, control_depth, wire_path, status, process_owner,
    end_cause, created_at, updated_at, ended_at)
SELECT DISTINCT run_id, 'supervised', 0, 'ended', 1, 'before-runs', ?1, ?1, ?1
FROM run_latches;
CREATE TABLE run_latches_v13 (
    run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    dimension  TEXT NOT NULL,
    scope      TEXT NOT NULL,
    action     TEXT NOT NULL,
    latched_at TEXT NOT NULL,
    PRIMARY KEY (run_id, dimension, scope, action)
);
INSERT INTO run_latches_v13 SELECT run_id, dimension, scope, action, latched_at FROM run_latches;
DROP TABLE run_latches;
ALTER TABLE run_latches_v13 RENAME TO run_latches;
";

/// Run the v13 step (DDL + backfill + stamp) in one `BEGIN IMMEDIATE`,
/// stepping aside when another process finished it while this one waited.
pub(super) fn migrate_v13(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 13 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V13).map_err(backend)?;
        let now = crate::time::now_rfc3339();
        for statement in BACKFILL_V13.split_inclusive(";\n") {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            if statement.contains("?1") {
                conn.execute(statement, [&now]).map_err(backend)?;
            } else {
                conn.execute_batch(statement).map_err(backend)?;
            }
        }
        conn.execute_batch("PRAGMA user_version = 13")
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

/// Schema v14 (story 21-9): each bound label's origin, backfilled `config`
/// on every bound row (the only bound source v13 wrote); a Caller row keeps
/// NULL.
const SCHEMA_V14: &str = "\
ALTER TABLE run_labels ADD COLUMN origin TEXT;
UPDATE run_labels SET origin = 'config' WHERE source = 'bound';
";

/// Run the v14 step (DDL + backfill + stamp) in one `BEGIN IMMEDIATE`,
/// stepping aside when another process finished it while this one waited.
pub(super) fn migrate_v14(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 14 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V14).map_err(backend)?;
        conn.execute_batch("PRAGMA user_version = 14")
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

/// Schema v15 (story 21-3): Policies and their resolution.
///
/// * `policies` — one row per Policy. `selector` is NULL only on the
///   seeded `default` Policy (the fallback layer, not a match-all
///   Selector). `scent_check` is NULL when unset, `'off'` when explicit
///   (the seeded default carries `'off'`, PRD §13 Q9's interim).
/// * `policy_rates` — the `default` Policy's model-keyed Rate table
///   (exact, case-sensitive model match; ships empty).
/// * `run_policy` — each Run's stored resolution plus the caller-resolved
///   inputs that produced it (so a re-resolution inside a writer job
///   reuses the last config read).
/// * `breach_default_upgrade` — the FR-21 upgrade's pending list AND
///   marker in one table: rows exist from the migration transaction on, so
///   resolution fail-closes to `pause` for every row across the whole
///   upgrade window (a crash can never leave an upgraded instance on the
///   new `warn` default); `done_at` marks the finish.
const SCHEMA_V15: &str = "\
CREATE TABLE policies (
    name                     TEXT PRIMARY KEY,
    selector                 TEXT,
    tokens_per_run           INTEGER,
    tokens_cumulative        INTEGER,
    dollars_per_run_micros   INTEGER,
    dollars_cumulative_micros INTEGER,
    breach_action            TEXT,
    scent_check              TEXT,
    rate_input_micros        INTEGER,
    rate_output_micros       INTEGER,
    rate_cached_micros       INTEGER,
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL
);
CREATE TABLE policy_rates (
    policy               TEXT NOT NULL REFERENCES policies(name) ON DELETE CASCADE,
    model                TEXT NOT NULL,
    input_micros_per_1m  INTEGER NOT NULL,
    output_micros_per_1m INTEGER NOT NULL,
    cached_micros_per_1m INTEGER,
    PRIMARY KEY (policy, model)
);
CREATE TABLE run_policy (
    run_id     TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,
    resolution TEXT NOT NULL,
    inputs     TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE breach_default_upgrade (
    instance_id    INTEGER PRIMARY KEY REFERENCES agent_instances(id) ON DELETE CASCADE,
    config_written INTEGER NOT NULL DEFAULT 0,
    listed_at      TEXT NOT NULL,
    done_at        TEXT
);
";

/// Run the v15 step (DDL + the seeded `default` Policy + the pending list +
/// the stamp) in one `BEGIN IMMEDIATE`, stepping aside when another process
/// finished it while this one waited.
///
/// The pending list is built through [`BreachDefaultSource`] — the REAL
/// persisted layers (the embedded engine-default const, the kind-default
/// layer, the instance `config.toml`), resolved exactly as runtime resolves
/// them, so the test is "no `budget.breach_action` in any persisted layer
/// the instance resolves through". The invocation-override layer is
/// ephemeral and ignored (an override still outranks at start, as today).
pub(super) fn migrate_v15(
    conn: &Connection,
    breach: &dyn BreachDefaultSource,
) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 15 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V15).map_err(backend)?;
        let now = crate::time::now_rfc3339();
        // The seeded `default` Policy (warn, `off`, no ceilings, an empty
        // Rate table). `#[default]`'s `Pause` is only the fail-closed seed.
        conn.execute(
            "INSERT INTO policies (name, selector, breach_action, scent_check, created_at, \
             updated_at) VALUES ('default', NULL, ?1, ?2, ?3, ?3)",
            rusqlite::params![
                BreachAction::Warn.as_str(),
                ScentCheckMode::Off.as_str(),
                now,
            ],
        )
        .map_err(backend)?;
        // The pending list: every registered instance whose persisted
        // layers carry no explicit `budget.breach_action` (a MALFORMED
        // value is present — not pending; the fail-closed read keeps it
        // `pause`).
        let mut stmt = conn
            .prepare("SELECT id, kind, agent_home FROM agent_instances")
            .map_err(backend)?;
        let instances = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        drop(stmt);
        for (id, kind, home) in instances {
            let inputs = breach.resolve(&kind, std::path::Path::new(&home));
            if inputs.explicit.breach_action.is_none() && inputs.malformed_action.is_none() {
                conn.execute(
                    "INSERT INTO breach_default_upgrade (instance_id, listed_at) \
                     VALUES (?1, ?2)",
                    rusqlite::params![id, now],
                )
                .map_err(backend)?;
            }
        }
        conn.execute_batch("PRAGMA user_version = 15")
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

/// Schema v16 (story 21-5): the Run listing's three partial indexes.
///
/// * `idx_runs_open` serves the `all` view's open arm and the open/healthy
///   counts.
/// * `idx_runs_exception` serves the exceptions view's arms.
/// * `idx_runs_ended_at` bounds the ended-inside-window arm by the window
///   (the exception index alone would walk every historical anomalous row).
///
/// Additive DDL only, one crash-atomic step like every step before it.
const SCHEMA_V16: &str = "\
CREATE INDEX idx_runs_open ON runs(run_id) WHERE status <> 'ended';
CREATE INDEX idx_runs_exception ON runs(run_id)
    WHERE anomalies <> '' OR status IN ('held','silent','stale');
CREATE INDEX idx_runs_ended_at ON runs(ended_at) WHERE status = 'ended';
";

/// Run the v16 step (DDL + stamp) in one `BEGIN IMMEDIATE`, stepping aside
/// when another process finished it while this one waited.
pub(super) fn migrate_v16(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 16 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V16).map_err(backend)?;
        conn.execute_batch("PRAGMA user_version = 16")
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

/// Schema v19 (story 22-2): the Run custody columns. Four additive
/// `ALTER TABLE`s — `custody_state` (`on`/`off`), `custody_reason`
/// (`held`/`no-held-key`/`opted-out`/`pre-custody`), `custody_origin` (the
/// origin frozen at start, for the Run's life) and `custody_key_ref` (the
/// `secret:NAME` reference — a reference only, never a value; the drift
/// note lives in the story's change log). NO backfill: a Run live at
/// upgrade honestly reads `pre-custody` (NULL state) and its next launch
/// picks custody up.
const SCHEMA_V19: &str = "\
ALTER TABLE runs ADD COLUMN custody_state TEXT;
ALTER TABLE runs ADD COLUMN custody_reason TEXT;
ALTER TABLE runs ADD COLUMN custody_origin TEXT;
ALTER TABLE runs ADD COLUMN custody_key_ref TEXT;
";

/// Run the v19 step (DDL + stamp) in one `BEGIN IMMEDIATE`, stepping aside
/// when another process finished it while this one waited.
pub(super) fn migrate_v19(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 19 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V19).map_err(backend)?;
        conn.execute_batch("PRAGMA user_version = 19")
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

/// Reads an instance's persisted config layers for the FR-21 upgrade
/// (story 21-3): the merged explicit limits and model key the real
/// resolution would produce from the engine-default const, the kind-default
/// layer and the instance `config.toml`. A layer that cannot be read counts
/// as carrying the key (fail closed — the runtime read degrades to `pause`
/// the same way).
pub(crate) trait BreachDefaultSource {
    fn resolve(&self, kind: &str, home: &std::path::Path) -> crate::domain::runs::ResolveInputs;
}

/// The empty source: no instances to read (an in-memory or fresh database).
pub(crate) struct NoBreachDefault;

impl BreachDefaultSource for NoBreachDefault {
    fn resolve(&self, _kind: &str, _home: &std::path::Path) -> crate::domain::runs::ResolveInputs {
        crate::domain::runs::ResolveInputs::default()
    }
}

/// The columns a [`RunRecord`] reads, joined to the instance name.
const RUN_COLUMNS: &str = "r.run_id, r.control_depth, r.wire_path, r.status, r.anomalies, \
     r.parent_run_id, i.name, r.process_owner, r.end_cause, r.created_at, r.updated_at, \
     r.ended_at";

/// `FROM runs r LEFT JOIN agent_instances i` — the join every Run read uses.
const RUN_FROM: &str = "FROM runs r LEFT JOIN agent_instances i ON i.id = r.instance_id";

/// Decode one `run_labels` row (shared by the one-Run read and the Fleet's
/// batched read, so both refuse a corrupt row with the same words).
fn decode_label(
    run_id: &str,
    key: String,
    value: String,
    source: &str,
    origin: Option<String>,
) -> Result<Label, StoreError> {
    let source = LabelSource::from_wire(source).ok_or_else(|| StoreError::CorruptRow {
        name: "run_labels".to_string(),
        detail: format!("Run `{run_id}` label `{key}` has an unknown source `{source}`"),
    })?;
    let origin =
        match (source, origin) {
            (LabelSource::Caller, _) => None,
            // A bound row with no origin predates origins: configuration
            // was the only bound source then.
            (LabelSource::Bound, None) => Some(LabelOrigin::Config),
            (LabelSource::Bound, Some(raw)) => Some(LabelOrigin::from_wire(&raw).ok_or_else(
                || StoreError::CorruptRow {
                    name: "run_labels".to_string(),
                    detail: format!("Run `{run_id}` label `{key}` has an unknown origin `{raw}`"),
                },
            )?),
        };
    Ok(Label {
        key,
        value,
        source,
        origin,
    })
}

/// The `run_limits` dimension of the Run-level Breach Action row (story
/// 21-6): `limit_value` 1 = the action column is the Run's action, 0 = the
/// Run sets no action of its own (the action column is a placeholder).
/// When the row exists it decides the Run's action; a Run without it (a
/// child opened by 21-2, which stores the action on every ceiling row)
/// keeps the ceiling rows' action.
const ACTION_DIMENSION: &str = "action";

/// The scope column of the action row.
const ACTION_SCOPE: &str = "run";

/// The dimension prefix of the Caller layer's rows (story 21-6): a Caller's
/// limits are their own rows (`caller.tokens` / `per-run`, …,
/// `caller.action` / `run`) beside the Operator/Host rows on the same Run,
/// so a Caller write never replaces — or re-labels — an Operator's limit,
/// and the resolver applies the Caller's only where tighter. No schema
/// step: the existing key `(run_id, dimension, scope)` holds both layers.
const CALLER_PREFIX: &str = "caller.";

/// The dimension a layer's row is stored under.
fn slot_dimension(slot: crate::ports::LimitSlot, dimension: &str) -> String {
    match slot {
        crate::ports::LimitSlot::Run => dimension.to_string(),
        crate::ports::LimitSlot::Caller => format!("{CALLER_PREFIX}{dimension}"),
    }
}

/// Folds one layer of a Run's `run_limits` rows into its [`RunLimits`].
#[derive(Default)]
struct LayerFold {
    limits: RunLimits,
    /// The action carried on the ceiling rows (the 21-2 child-open shape).
    ceiling_action: Option<BreachAction>,
    /// The action row's verdict, when the layer has one.
    action_row: Option<Option<BreachAction>>,
}

impl LayerFold {
    fn finish(self) -> RunLimits {
        let mut limits = self.limits;
        limits.breach_action = match self.action_row {
            Some(action) => action,
            None => self.ceiling_action,
        };
        limits
    }
}

/// Folds one Run's `run_limits` rows into its two layers (shared by the
/// one-Run reads and the Fleet's batched read).
#[derive(Default)]
struct LimitsFold {
    /// The Operator/Host layer.
    run: LayerFold,
    /// The Caller layer.
    caller: LayerFold,
}

impl LimitsFold {
    fn row(
        &mut self,
        run_id: &str,
        dimension: &str,
        scope: &str,
        limit: i64,
        action: &str,
    ) -> Result<(), StoreError> {
        let corrupt = |detail: String| StoreError::CorruptRow {
            name: "run_limits".to_string(),
            detail,
        };
        let parsed: BreachAction = action.parse().map_err(|_| {
            corrupt(format!(
                "Run `{run_id}` has an unknown Breach Action `{action}`"
            ))
        })?;
        let (layer, dimension) = match dimension.strip_prefix(CALLER_PREFIX) {
            Some(dimension) => (&mut self.caller, dimension),
            None => (&mut self.run, dimension),
        };
        let value = limit.max(0);
        match (dimension, scope) {
            (ACTION_DIMENSION, _) => {
                layer.action_row = Some((value != 0).then_some(parsed));
                return Ok(());
            }
            ("tokens", "per-run") => layer.limits.token_per_run = Some(value as u64),
            ("dollars", "per-run") => layer.limits.dollar_per_run_micros = Some(Micros(value)),
            ("tokens", "cumulative") => layer.limits.token_cumulative = Some(value as u64),
            ("dollars", "cumulative") => {
                layer.limits.dollar_cumulative_micros = Some(Micros(value))
            }
            (other, scope) => {
                return Err(corrupt(format!(
                    "Run `{run_id}` has an unknown limit `{other}` / `{scope}`"
                )))
            }
        }
        layer.ceiling_action = Some(parsed);
        Ok(())
    }

    /// The Operator/Host layer and the Caller layer.
    fn finish(self) -> (RunLimits, RunLimits) {
        (self.run.finish(), self.caller.finish())
    }
}

fn read_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let run_id: String = row.get(0)?;
    let depth_raw: String = row.get(1)?;
    let status_raw: String = row.get(3)?;
    let corrupt = |what: &str, raw: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("Run `{run_id}` has an unknown {what} `{raw}`").into(),
        )
    };
    Ok(RunRecord {
        control_depth: ControlDepth::from_wire(&depth_raw)
            .ok_or_else(|| corrupt("control depth", &depth_raw))?,
        status: RunStatus::from_wire(&status_raw).ok_or_else(|| corrupt("status", &status_raw))?,
        wire_path: row.get::<_, i64>(2)? != 0,
        anomalies: Anomaly::decode(&row.get::<_, String>(4)?),
        parent_run_id: row.get::<_, Option<String>>(5)?.map(RunId::from_wire),
        instance: row.get(6)?,
        process_owner: row.get::<_, i64>(7)? != 0,
        end_cause: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        ended_at: row.get(11)?,
        run_id: RunId::from_wire(run_id),
    })
}

fn corrupt_run(err: rusqlite::Error) -> StoreError {
    match err {
        rusqlite::Error::FromSqlConversionFailure(_, _, detail) => StoreError::CorruptRow {
            name: "runs".to_string(),
            detail: detail.to_string(),
        },
        other => backend(other),
    }
}

impl SqliteStore {
    /// Test-only: run raw SQL on the store's connection (fixtures outside
    /// the `store` module).
    #[cfg(test)]
    pub(crate) fn execute_batch_for_test(&self, sql: &str) {
        self.conn().execute_batch(sql).expect("test fixture SQL");
    }

    /// Test-only: one text column of one row.
    #[cfg(test)]
    pub(crate) fn query_string_for_test(&self, sql: &str) -> String {
        self.conn()
            .query_row(sql, [], |row| row.get::<_, String>(0))
            .expect("test fixture query")
    }

    /// Run `body` in a named SAVEPOINT of the open batch transaction: its
    /// writes roll back alone on an error (Run bookkeeping that must never
    /// fail the job carrying it).
    pub(crate) fn in_savepoint<T>(
        &self,
        name: &str,
        body: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        super::sqlite::in_savepoint(self.conn(), name, body)
    }

    /// Open process-owning Runs whose instance is `failed`, `stopped` or
    /// `registered` (no live process): the sweep at engine open ends them.
    pub(crate) fn open_owners_without_a_process(&self) -> Result<Vec<RunRecord>, StoreError> {
        self.records(
            "WHERE r.process_owner = 1 AND r.status <> 'ended' \
             AND i.state IN ('failed', 'stopped', 'registered') ORDER BY r.run_id",
            [],
        )
    }

    fn optional_instance_id(&self, instance: Option<&str>) -> Result<Option<i64>, StoreError> {
        let Some(name) = instance else {
            return Ok(None);
        };
        let iname = InstanceName::new(name).map_err(|e| StoreError::CorruptRow {
            name: name.to_string(),
            detail: format!("invalid instance name on a Run: {e}"),
        })?;
        self.instance_id(&iname)?
            .map(Some)
            .ok_or_else(|| StoreError::NotFound {
                name: name.to_string(),
            })
    }

    /// Insert one label row with its source and, when bound, its origin.
    fn insert_label(&self, run_id: &RunId, label: &Label) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO run_labels (run_id, key, value, source, origin) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    run_id.as_str(),
                    label.key,
                    label.value,
                    label.source.as_str(),
                    label.bound_origin().map(|o| o.as_str()),
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// The bench seeding's rollup write (story 21-5, `#[doc(hidden)]`
    /// engine surface): one `usage_rollup_run` row, exactly the shape the
    /// usage pipeline's fold would leave.
    pub(crate) fn seed_rollup_for_perf(
        &self,
        instance: &InstanceName,
        run_id: &RunId,
        tokens: u64,
    ) -> Result<(), StoreError> {
        let instance_id: i64 = self
            .instance_id(instance)?
            .ok_or_else(|| StoreError::NotFound {
                name: instance.as_str().to_string(),
            })?;
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO usage_rollup_run (instance_id, run_id, events, \
                 input_tokens, output_tokens, cached_tokens, cached_unknown, cost_micros) \
                 VALUES (?1, ?2, 1, ?3, 0, 0, 0, 0)",
                rusqlite::params![instance_id, run_id.as_str(), tokens as i64],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn records(
        &self,
        sql_tail: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(&format!("SELECT {RUN_COLUMNS} {RUN_FROM} {sql_tail}"))
            .map_err(backend)?;
        let rows = stmt.query_map(params, read_record).map_err(backend)?;
        rows.collect::<Result<_, _>>().map_err(corrupt_run)
    }
}

impl RunRecords for SqliteStore {
    fn insert_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let record = &run.record;
        let instance_id = self.optional_instance_id(record.instance.as_deref())?;
        self.conn()
            .execute(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 parent_run_id, instance_id, process_owner, end_cause, created_at, updated_at, \
                 ended_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    record.run_id.as_str(),
                    record.control_depth.as_str(),
                    record.wire_path as i64,
                    record.status.as_str(),
                    Anomaly::encode(&record.anomalies),
                    record.parent_run_id.as_ref().map(RunId::as_str),
                    instance_id,
                    record.process_owner as i64,
                    record.end_cause,
                    record.created_at,
                    record.updated_at,
                    record.ended_at,
                ],
            )
            .map_err(backend)?;
        for label in &run.labels {
            self.insert_label(&record.run_id, label)?;
        }
        let action = run.limits.action();
        let limits = [
            (
                "tokens",
                run.limits
                    .token_per_run
                    .map(|t| t.min(i64::MAX as u64) as i64),
            ),
            ("dollars", run.limits.dollar_per_run_micros.map(|m| m.0)),
        ];
        for (dimension, limit) in limits {
            if let Some(limit) = limit {
                self.conn()
                    .execute(
                        "INSERT INTO run_limits (run_id, dimension, scope, limit_value, action, \
                         source, set_at) VALUES (?1, ?2, 'per-run', ?3, ?4, 'host', ?5)",
                        rusqlite::params![
                            record.run_id.as_str(),
                            dimension,
                            limit,
                            action.as_str(),
                            record.created_at
                        ],
                    )
                    .map_err(backend)?;
            }
        }
        if let Some((binding, external_id)) = &run.identity {
            self.conn()
                .execute(
                    "INSERT INTO run_identities (binding, external_id, run_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        binding,
                        external_id,
                        record.run_id.as_str(),
                        record.created_at
                    ],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    fn run_record(&self, run_id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.conn()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} {RUN_FROM} WHERE r.run_id = ?1"),
                [run_id.as_str()],
                read_record,
            )
            .optional()
            .map_err(corrupt_run)
    }

    fn run_labels(&self, run_id: &RunId) -> Result<Vec<Label>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT key, value, source, origin FROM run_labels WHERE run_id = ?1 ORDER BY key",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([run_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(backend)?;
        let mut labels = Vec::new();
        for row in rows {
            let (key, value, source, origin) = row.map_err(backend)?;
            labels.push(decode_label(run_id.as_str(), key, value, &source, origin)?);
        }
        Ok(labels)
    }

    fn run_limits(&self, run_id: &RunId) -> Result<RunLimits, StoreError> {
        Ok(self.run_limit_layers(run_id)?.0)
    }

    fn run_caller_limits(&self, run_id: &RunId) -> Result<RunLimits, StoreError> {
        Ok(self.run_limit_layers(run_id)?.1)
    }

    fn replace_run_limits(
        &self,
        run_id: &RunId,
        slot: crate::ports::LimitSlot,
        limits: &RunLimits,
        source: &str,
        at: &str,
    ) -> Result<(), StoreError> {
        let action = limits.action();
        let clamp = |value: u64| value.min(i64::MAX as u64) as i64;
        let wanted = [
            ("tokens", "per-run", limits.token_per_run.map(clamp)),
            (
                "dollars",
                "per-run",
                limits.dollar_per_run_micros.map(|m| m.0.max(0)),
            ),
            ("tokens", "cumulative", limits.token_cumulative.map(clamp)),
            (
                "dollars",
                "cumulative",
                limits.dollar_cumulative_micros.map(|m| m.0.max(0)),
            ),
            (
                ACTION_DIMENSION,
                ACTION_SCOPE,
                limits
                    .is_set()
                    .then_some(i64::from(limits.breach_action.is_some())),
            ),
        ];
        // The oldest ceiling row of this layer as it stood BEFORE the write
        // (its action, source and time): the 21-2 child-open shape's
        // provenance for an action no action row recorded yet.
        let prior_ceiling: Option<(String, String, String)> = self
            .conn()
            .query_row(
                "SELECT action, source, set_at FROM run_limits \
                 WHERE run_id = ?1 AND scope <> ?2 AND dimension IN (?3, ?4) \
                 ORDER BY set_at, dimension, scope LIMIT 1",
                rusqlite::params![
                    run_id.as_str(),
                    ACTION_SCOPE,
                    slot_dimension(slot, "tokens"),
                    slot_dimension(slot, "dollars"),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(backend)?;
        for (dimension, scope, value) in wanted {
            let dimension = slot_dimension(slot, dimension);
            let existing: Option<(i64, String)> = self
                .conn()
                .query_row(
                    "SELECT limit_value, action FROM run_limits \
                     WHERE run_id = ?1 AND dimension = ?2 AND scope = ?3",
                    rusqlite::params![run_id.as_str(), dimension, scope],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(backend)?;
            match (value, existing) {
                (None, None) => {}
                (None, Some(_)) => {
                    self.conn()
                        .execute(
                            "DELETE FROM run_limits \
                             WHERE run_id = ?1 AND dimension = ?2 AND scope = ?3",
                            rusqlite::params![run_id.as_str(), dimension, scope],
                        )
                        .map_err(backend)?;
                }
                // Provenance: a row whose value holds keeps who set it and
                // when. The action column a ceiling row carries (the 21-2
                // shape's fallback) follows the layer's action in place;
                // the action row's own value is its action, so a change of
                // action is a change of that row.
                (Some(value), Some((stored, stored_action)))
                    if stored == value
                        && (scope != ACTION_SCOPE || stored_action == action.as_str()) =>
                {
                    if stored_action != action.as_str() {
                        self.conn()
                            .execute(
                                "UPDATE run_limits SET action = ?4 \
                                 WHERE run_id = ?1 AND dimension = ?2 AND scope = ?3",
                                rusqlite::params![
                                    run_id.as_str(),
                                    dimension,
                                    scope,
                                    action.as_str()
                                ],
                            )
                            .map_err(backend)?;
                    }
                }
                (Some(value), existing) => {
                    // Story 22-1 (the 21-6 review's N4): the FIRST action row
                    // of a layer written in the 21-2 child-open shape (the
                    // action carried on its ceiling rows, no action row)
                    // keeps the provenance of those rows when the action is
                    // unchanged — the next writer of some other limit did
                    // not set it.
                    let inherited = (scope == ACTION_SCOPE && existing.is_none())
                        .then_some(())
                        .and(prior_ceiling.as_ref())
                        .filter(|(stored_action, _, _)| stored_action == action.as_str())
                        .map(|(_, source, at)| (source.clone(), at.clone()));
                    let (row_source, row_at) = match &inherited {
                        Some((source, at)) => (source.as_str(), at.as_str()),
                        None => (source, at),
                    };
                    self.conn()
                        .execute(
                            "INSERT INTO run_limits (run_id, dimension, scope, limit_value, \
                             action, source, set_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                             ON CONFLICT (run_id, dimension, scope) DO UPDATE SET \
                             limit_value = excluded.limit_value, action = excluded.action, \
                             source = excluded.source, set_at = excluded.set_at",
                            rusqlite::params![
                                run_id.as_str(),
                                dimension,
                                scope,
                                value,
                                action.as_str(),
                                row_source,
                                row_at
                            ],
                        )
                        .map_err(backend)?;
                }
            }
        }
        Ok(())
    }

    fn run_chain(&self, run_id: &RunId) -> Result<Vec<RunRecord>, StoreError> {
        // Walk upward one row at a time: the chain is at most
        // MAX_RUN_NESTING long, and a cycle (impossible through the engine,
        // which never re-parents) is cut at twice that.
        let mut chain = Vec::new();
        let mut next = Some(run_id.clone());
        while let Some(id) = next.take() {
            if chain.len() >= 2 * crate::domain::MAX_RUN_NESTING {
                break;
            }
            let Some(record) = self.run_record(&id)? else {
                break;
            };
            next = record.parent_run_id.clone();
            chain.push(record);
        }
        Ok(chain)
    }

    fn open_process_run(&self, instance: &InstanceName) -> Result<Option<RunRecord>, StoreError> {
        Ok(self
            .records(
                "WHERE i.name = ?1 AND r.process_owner = 1 AND r.status <> 'ended' LIMIT 1",
                [instance.as_str()],
            )?
            .pop())
    }

    fn latest_process_run(&self, instance: &InstanceName) -> Result<Option<RunRecord>, StoreError> {
        Ok(self
            .records(
                "WHERE i.name = ?1 AND r.process_owner = 1 ORDER BY r.run_id DESC LIMIT 1",
                [instance.as_str()],
            )?
            .pop())
    }

    fn open_in_process_descendants(&self, run_id: &RunId) -> Result<Vec<RunRecord>, StoreError> {
        self.records(
            "WHERE r.run_id IN (\
                 WITH RECURSIVE tree(id, depth) AS (\
                     SELECT ?1, 0 \
                     UNION ALL \
                     SELECT c.run_id, t.depth + 1 FROM runs c JOIN tree t ON c.parent_run_id = t.id \
                     WHERE t.depth < 16) \
                 SELECT id FROM tree WHERE depth > 0) \
             AND r.status <> 'ended' \
             AND r.process_owner = 0 \
             AND r.instance_id = (SELECT instance_id FROM runs WHERE run_id = ?1) \
             ORDER BY r.created_at, r.run_id",
            [run_id.as_str()],
        )
    }

    fn save_run(&self, record: &RunRecord) -> Result<(), StoreError> {
        let changed = self
            .conn()
            .execute(
                "UPDATE runs SET control_depth = ?2, wire_path = ?3, status = ?4, anomalies = ?5, \
                 end_cause = ?6, updated_at = ?7, ended_at = ?8 WHERE run_id = ?1",
                rusqlite::params![
                    record.run_id.as_str(),
                    record.control_depth.as_str(),
                    record.wire_path as i64,
                    record.status.as_str(),
                    Anomaly::encode(&record.anomalies),
                    record.end_cause,
                    record.updated_at,
                    record.ended_at,
                ],
            )
            .map_err(backend)?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                name: record.run_id.as_str().to_string(),
            });
        }
        Ok(())
    }

    fn replace_labels(
        &self,
        run_id: &RunId,
        source: LabelSource,
        labels: &[Label],
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "DELETE FROM run_labels WHERE run_id = ?1 AND source = ?2",
                rusqlite::params![run_id.as_str(), source.as_str()],
            )
            .map_err(backend)?;
        for label in labels {
            // A key held at the other source: a bound write replaces a
            // Caller's row (bound wins, FR-49), and a Caller write never
            // replaces a bound row — the `WHERE` makes the upsert a no-op
            // (story 21-9's defense in depth behind `check_caller_labels`).
            let origin = match source {
                LabelSource::Bound => Some(label.origin.unwrap_or(LabelOrigin::Config).as_str()),
                LabelSource::Caller => None,
            };
            self.conn()
                .execute(
                    "INSERT INTO run_labels (run_id, key, value, source, origin) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(run_id, key) DO UPDATE SET value = excluded.value, \
                     source = excluded.source, origin = excluded.origin \
                     WHERE excluded.source = 'bound'",
                    rusqlite::params![
                        run_id.as_str(),
                        label.key,
                        label.value,
                        source.as_str(),
                        origin
                    ],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    fn set_run_labels(&self, run_id: &RunId, labels: &[Label]) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "DELETE FROM run_labels WHERE run_id = ?1",
                [run_id.as_str()],
            )
            .map_err(backend)?;
        for label in labels {
            self.insert_label(run_id, label)?;
        }
        Ok(())
    }

    fn select_runs(&self, selector: &Selector, limit: usize) -> Result<Vec<RunRecord>, StoreError> {
        // The first term drives through `idx_run_labels_kv`; every other term
        // probes the (run_id, key) primary key. Non-ended Runs only, ordered
        // by run id, at most `limit` rows.
        let terms = selector.terms();
        let Some(((first_key, first_value), rest)) = terms.split_first() else {
            return Ok(Vec::new());
        };
        let mut sql = String::from(
            "WHERE r.run_id IN (SELECT l0.run_id FROM run_labels l0 \
             WHERE l0.key = ? AND l0.value = ?",
        );
        let mut params: Vec<&str> = vec![first_key, first_value];
        for (key, value) in rest {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM run_labels lx WHERE lx.run_id = l0.run_id \
                 AND lx.key = ? AND lx.value = ?)",
            );
            params.push(key);
            params.push(value);
        }
        let limit = limit.min(i64::MAX as usize).to_string();
        sql.push_str(") AND r.status <> 'ended' ORDER BY r.run_id LIMIT ");
        sql.push_str(&limit);
        self.records(&sql, rusqlite::params_from_iter(params))
    }

    fn identity_run(&self, binding: &str, external_id: &str) -> Result<Option<RunId>, StoreError> {
        self.conn()
            .query_row(
                "SELECT run_id FROM run_identities WHERE binding = ?1 AND external_id = ?2",
                [binding, external_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(backend)
            .map(|id| id.map(RunId::from_wire))
    }

    fn open_children(&self, run_id: &RunId) -> Result<u64, StoreError> {
        self.conn()
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE parent_run_id = ?1 AND status <> 'ended'",
                [run_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
            .map_err(backend)
    }

    fn open_runs(&self) -> Result<Vec<RunRecord>, StoreError> {
        self.records("WHERE r.status <> 'ended' ORDER BY r.run_id", [])
    }

    fn list_runs(
        &self,
        view: ListingView,
        selector: Option<&Selector>,
        after: Option<&str>,
        limit: usize,
        cutoff: &str,
    ) -> Result<(Vec<RunRecord>, Option<String>), StoreError> {
        // The view arms read the v16 partial indexes (each arm's WHERE names
        // an index predicate verbatim) with the keyset and the Selector
        // inside each arm; the outer row read probes the runs primary key
        // per id. `run_id` DESC is newest-first (ULID ids encode creation
        // time; legacy `run-…` ids sort below every `run_…` id and land
        // last). One row past the page is fetched: a cursor rides only when
        // that row exists, so a final page that is exactly full ends the
        // read (`None`) instead of pointing at an empty page.
        let statement = listing_page_tail(view, selector, after, cutoff, limit.saturating_add(1));
        let mut rows =
            self.records(&statement.sql, rusqlite::params_from_iter(statement.params))?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|record| record.run_id.as_str().to_string())
        } else {
            None
        };
        Ok((rows, next))
    }

    fn listing_counts(
        &self,
        view: ListingView,
        selector: Option<&Selector>,
        cutoff: &str,
    ) -> Result<ListingCounts, StoreError> {
        let count = |statement: ListingStatement| -> Result<u64, StoreError> {
            let n: i64 = self
                .conn()
                .query_row(
                    &statement.sql,
                    rusqlite::params_from_iter(statement.params),
                    |row| row.get(0),
                )
                .map_err(backend)?;
            Ok(n.max(0) as u64)
        };
        let listed = count(listing_listed_count(view, selector, cutoff))?;
        let healthy = count(listing_healthy_count(selector))?;
        Ok(ListingCounts {
            listed,
            healthy_open: healthy,
        })
    }

    fn ensure_run_row(&self, run_id: &RunId, instance: &InstanceName) -> Result<(), StoreError> {
        let now = crate::time::now_rfc3339();
        self.conn()
            .execute(
                "INSERT OR IGNORE INTO runs (run_id, control_depth, wire_path, status, \
                 instance_id, process_owner, end_cause, created_at, updated_at, ended_at) \
                 VALUES (?1, 'supervised', 0, 'ended', \
                 (SELECT id FROM agent_instances WHERE name = ?2), 1, 'unregistered', ?3, ?3, ?3)",
                rusqlite::params![run_id.as_str(), instance.as_str(), now],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn insert_token(&self, token: &crate::ports::TokenRow) -> Result<(), StoreError> {
        super::tokens_sqlite::insert_token(self.conn(), token)
    }

    fn token(
        &self,
        hash: &crate::domain::credential::TokenHash,
    ) -> Result<Option<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::token(self.conn(), hash)
    }

    fn set_token_state(
        &self,
        hash: &crate::domain::credential::TokenHash,
        state: crate::domain::credential::TokenState,
        at: &str,
        cause: &str,
    ) -> Result<bool, StoreError> {
        super::tokens_sqlite::set_token_state(self.conn(), hash, state, at, cause)
    }

    fn revoke_retired(
        &self,
        hash: &crate::domain::credential::TokenHash,
        at: &str,
        cause: &str,
    ) -> Result<bool, StoreError> {
        super::tokens_sqlite::revoke_retired(self.conn(), hash, at, cause)
    }

    fn active_tokens(&self, run_id: &RunId) -> Result<Vec<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::active_tokens(self.conn(), run_id)
    }

    fn run_tokens(&self, run_id: &RunId) -> Result<Vec<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::run_tokens(self.conn(), run_id)
    }

    fn active_issued_under(
        &self,
        roots: &[crate::domain::credential::TokenHash],
    ) -> Result<Vec<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::issued_under(
            self.conn(),
            roots,
            crate::domain::credential::TokenState::Active,
        )
    }

    fn retired_issued_under(
        &self,
        roots: &[crate::domain::credential::TokenHash],
    ) -> Result<Vec<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::issued_under(
            self.conn(),
            roots,
            crate::domain::credential::TokenState::Retired,
        )
    }

    fn active_key_rows(&self) -> Result<Vec<crate::ports::TokenRow>, StoreError> {
        super::tokens_sqlite::active_key_rows(self.conn())
    }

    fn set_run_upstream(&self, run_id: &RunId, upstream: Option<&str>) -> Result<(), StoreError> {
        self.record_run_upstream(run_id, upstream)
    }

    fn set_run_custody(
        &self,
        run_id: &RunId,
        custody: Option<crate::ports::RunCustodyRow>,
    ) -> Result<(), StoreError> {
        let (state, reason, origin, key_ref) = match custody {
            Some(row) => (Some(row.state), Some(row.reason), row.origin, row.key_ref),
            None => (None, None, None, None),
        };
        self.conn()
            .execute(
                "UPDATE runs SET custody_state = ?2, custody_reason = ?3, custody_origin = ?4, \
                 custody_key_ref = ?5 WHERE run_id = ?1",
                rusqlite::params![run_id.as_str(), state, reason, origin, key_ref],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn run_custody(
        &self,
        run_id: &RunId,
    ) -> Result<Option<crate::ports::RunCustodyRow>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT custody_state, custody_reason, custody_origin, custody_key_ref \
                 FROM runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        Ok(row.and_then(|(state, reason, origin, key_ref)| {
            let state = state?;
            Some(crate::ports::RunCustodyRow {
                state,
                reason: reason.unwrap_or_else(|| "pre-custody".to_string()),
                origin,
                key_ref,
            })
        }))
    }

    fn recently_revoked_keys(
        &self,
        since: &str,
        limit: usize,
    ) -> Result<Vec<crate::domain::credential::TokenHash>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT token_hash FROM tokens WHERE class = 'run' AND state = 'revoked' \
                 AND revoked_at >= ?1 ORDER BY revoked_at DESC LIMIT ?2",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                rusqlite::params![since, limit.min(i64::MAX as usize) as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map_err(backend)?;
        let mut hashes = Vec::new();
        for row in rows {
            if let Ok(bytes) = <[u8; 32]>::try_from(row.map_err(backend)?) {
                hashes.push(crate::domain::credential::TokenHash::from_bytes(bytes));
            }
        }
        Ok(hashes)
    }

    fn entry_extras(
        &self,
        run_id: &RunId,
    ) -> Result<crate::domain::runs::snapshot::EntryExtras, StoreError> {
        use crate::ports::RunStore;
        let latches = self.latches(run_id)?;
        let upstream = self.run_upstream(run_id)?;
        let identity = self.run_identity(run_id)?;
        // Story 22-2: a Run whose custody is ON carries its held-key
        // reference and frozen origin into the snapshot — references only;
        // the value stays in the custody cache.
        let custody = self
            .run_custody(run_id)?
            .filter(|row| row.state == "on")
            .and_then(|row| {
                Some(crate::domain::custody::CustodyInfo {
                    key_ref: std::sync::Arc::from(row.key_ref?.as_str()),
                    origin: std::sync::Arc::from(row.origin?.as_str()),
                })
            });
        Ok(crate::domain::runs::snapshot::EntryExtras {
            disposition: crate::domain::runs::snapshot::Disposition::of_latches(&latches),
            upstream: upstream.map(std::sync::Arc::from),
            identity,
            custody,
        })
    }
}

/// The ancestors of `run_id` (nearest first) with their hosting instance
/// ids — the rollup fold adds each usage row to every one of them.
pub(super) fn ancestors_for_fold(
    conn: &Connection,
    run_id: &str,
) -> Result<Vec<(Option<i64>, String)>, StoreError> {
    let mut stmt = conn
        .prepare_cached(
            "WITH RECURSIVE up(id, depth) AS (\
                 SELECT parent_run_id, 1 FROM runs WHERE run_id = ?1 \
                 UNION ALL \
                 SELECT r.parent_run_id, u.depth + 1 FROM runs r JOIN up u ON r.run_id = u.id \
                 WHERE u.depth < 16) \
             SELECT r.instance_id, r.run_id FROM up JOIN runs r ON r.run_id = up.id \
             ORDER BY up.depth",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(backend)?;
    rows.collect::<Result<_, _>>().map_err(backend)
}

// ---------------------------------------------------------------------------
// Policies and their resolution (story 21-3, schema v15).
// ---------------------------------------------------------------------------

impl PolicyRecords for SqliteStore {
    fn policies(&self) -> Result<Vec<PolicyRecord>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT name FROM policies ORDER BY name")
            .map_err(backend)?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        drop(stmt);
        names.iter().map(|name| self.read_policy(name)).collect()
    }

    fn policy(&self, name: &str) -> Result<Option<PolicyRecord>, StoreError> {
        let exists: Option<i64> = self
            .conn()
            .query_row("SELECT 1 FROM policies WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .map_err(backend)?;
        Ok(match exists {
            Some(_) => Some(self.read_policy(name)?),
            None => None,
        })
    }

    /// The RAW stored Selector text of one Policy, undecoded:
    /// `Ok(None)` = no row with the name; `Ok(Some(None))` = a row with a
    /// NULL Selector (the `default` shape); `Ok(Some(Some(raw)))` = the
    /// text as stored, PARSED BY NO ONE. The corrupt-row remediation path
    /// (deleting a Policy whose Selector no longer parses) needs exactly
    /// this — every decoded read of such a row fails, so the delete must
    /// be drivable by name plus whatever raw text is there.
    fn policy_selector_raw(&self, name: &str) -> Result<Option<Option<String>>, StoreError> {
        self.conn()
            .query_row(
                "SELECT selector FROM policies WHERE name = ?1",
                [name],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(backend)
    }

    fn put_policy(&self, record: &PolicyRecord) -> Result<(), StoreError> {
        let now = crate::time::now_rfc3339();
        let selector = record.selector.as_ref().map(Selector::to_string);
        let rate = record.limits.rate;
        self.conn()
            .execute(
                "INSERT INTO policies (name, selector, tokens_per_run, tokens_cumulative, \
                 dollars_per_run_micros, dollars_cumulative_micros, breach_action, scent_check, \
                 rate_input_micros, rate_output_micros, rate_cached_micros, created_at, \
                 updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12) \
                 ON CONFLICT(name) DO UPDATE SET selector = excluded.selector, \
                 tokens_per_run = excluded.tokens_per_run, \
                 tokens_cumulative = excluded.tokens_cumulative, \
                 dollars_per_run_micros = excluded.dollars_per_run_micros, \
                 dollars_cumulative_micros = excluded.dollars_cumulative_micros, \
                 breach_action = excluded.breach_action, scent_check = excluded.scent_check, \
                 rate_input_micros = excluded.rate_input_micros, \
                 rate_output_micros = excluded.rate_output_micros, \
                 rate_cached_micros = excluded.rate_cached_micros, \
                 updated_at = excluded.updated_at",
                rusqlite::params![
                    record.name,
                    selector,
                    record
                        .limits
                        .tokens_per_run
                        .map(|v| v.min(i64::MAX as u64) as i64),
                    record
                        .limits
                        .tokens_cumulative
                        .map(|v| v.min(i64::MAX as u64) as i64),
                    record
                        .limits
                        .dollars_per_run_micros
                        .map(|v| v.min(i64::MAX as u64) as i64),
                    record
                        .limits
                        .dollars_cumulative_micros
                        .map(|v| v.min(i64::MAX as u64) as i64),
                    record.limits.breach_action.map(|a| a.as_str()),
                    record.limits.scent_check.map(|m| m.as_str()),
                    rate.map(|r| r.input_micros_per_1m.min(i64::MAX as u64) as i64),
                    rate.map(|r| r.output_micros_per_1m.min(i64::MAX as u64) as i64),
                    rate.and_then(|r| r
                        .cached_micros_per_1m
                        .map(|c| c.min(i64::MAX as u64) as i64)),
                    now,
                ],
            )
            .map_err(backend)?;
        self.conn()
            .execute("DELETE FROM policy_rates WHERE policy = ?1", [&record.name])
            .map_err(backend)?;
        for (model, rate) in &record.rates {
            self.conn()
                .execute(
                    "INSERT INTO policy_rates (policy, model, input_micros_per_1m, \
                     output_micros_per_1m, cached_micros_per_1m) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        record.name,
                        model,
                        rate.input_micros_per_1m.min(i64::MAX as u64) as i64,
                        rate.output_micros_per_1m.min(i64::MAX as u64) as i64,
                        rate.cached_micros_per_1m
                            .map(|c| c.min(i64::MAX as u64) as i64),
                    ],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    fn delete_policy(&self, name: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM policies WHERE name = ?1", [name])
            .map_err(backend)?;
        Ok(())
    }

    fn run_policy(&self, run_id: &RunId) -> Result<Option<RunPolicy>, StoreError> {
        let row: Option<(String, String, String)> = self
            .conn()
            .query_row(
                "SELECT resolution, inputs, updated_at FROM run_policy WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(backend)?;
        row.map(|(resolution, inputs, updated_at)| {
            decode_run_policy(&resolution, &inputs, &updated_at)
        })
        .transpose()
    }

    fn set_run_policy(
        &self,
        run_id: &RunId,
        resolution: &crate::domain::runs::PolicyResolution,
        inputs: &crate::domain::runs::ResolveInputs,
        at: &str,
    ) -> Result<(), StoreError> {
        let resolution =
            serde_json::to_string(resolution).map_err(|err| StoreError::CorruptRow {
                name: "run_policy".to_string(),
                detail: format!("the resolution could not be serialized: {err}"),
            })?;
        let inputs = serde_json::to_string(inputs).map_err(|err| StoreError::CorruptRow {
            name: "run_policy".to_string(),
            detail: format!("the resolution inputs could not be serialized: {err}"),
        })?;
        self.conn()
            .execute(
                "INSERT INTO run_policy (run_id, resolution, inputs, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(run_id) DO UPDATE SET resolution = excluded.resolution, \
                 inputs = excluded.inputs, updated_at = excluded.updated_at",
                rusqlite::params![run_id.as_str(), resolution, inputs, at],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn breach_default_kept(&self, instance: &str) -> Result<bool, StoreError> {
        let kept: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM breach_default_upgrade b \
                 JOIN agent_instances i ON i.id = b.instance_id WHERE i.name = ?1",
                [instance],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        Ok(kept.is_some())
    }

    fn breach_default_pending(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT i.name FROM breach_default_upgrade b \
                 JOIN agent_instances i ON i.id = b.instance_id \
                 WHERE b.done_at IS NULL ORDER BY i.name",
            )
            .map_err(backend)?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(names)
    }

    fn finish_breach_default(
        &self,
        written: &[(String, bool)],
        at: &str,
    ) -> Result<Vec<String>, StoreError> {
        // Stage 3 is idempotent ACROSS CONCURRENT OPENS: the UPDATE claims
        // only rows still pending (`done_at IS NULL`), and the names
        // returned are the ones THIS job finished — the event fires for
        // exactly those, so two engines that both read the same pending
        // list emit exactly one event between them.
        let mut finished = Vec::new();
        for (name, config_written) in written {
            let changed = self
                .conn()
                .execute(
                    "UPDATE breach_default_upgrade SET config_written = ?1, done_at = ?2 \
                     WHERE done_at IS NULL \
                       AND instance_id = (SELECT id FROM agent_instances WHERE name = ?3)",
                    rusqlite::params![*config_written as i64, at, name],
                )
                .map_err(backend)?;
            if changed > 0 {
                finished.push(name.clone());
            }
        }
        Ok(finished)
    }

    fn clear_breach_default(&self, instance: &str) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "DELETE FROM breach_default_upgrade \
                 WHERE instance_id = (SELECT id FROM agent_instances WHERE name = ?1)",
                [instance],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn run_resolution_facts(
        &self,
        run_id: &RunId,
    ) -> Result<Option<crate::ports::ResolutionFacts>, StoreError> {
        use crate::ports::RunRecords;
        let Some(record) = self.run_record(run_id)? else {
            return Ok(None);
        };
        let labels = self.run_labels(run_id)?;
        let (limits, caller_limits) = self.run_limit_layers(run_id)?;
        let kept_on_pause = match &record.instance {
            Some(instance) => self.breach_default_kept(instance)?,
            None => false,
        };
        Ok(Some(crate::ports::ResolutionFacts {
            record,
            labels,
            limits,
            caller_limits,
            kept_on_pause,
        }))
    }

    fn open_owner_resolution_facts(&self) -> Result<crate::ports::OpenOwnerFacts, StoreError> {
        use std::collections::{HashMap, HashSet};
        // Four reads for the whole Fleet, whatever its size: the open
        // process-owning Runs, their labels, their per-run limits and the
        // kept-on-pause markers. Each Run's entry decodes exactly as
        // `run_resolution_facts` would (the same helpers, the same
        // per-Run order: labels by key, limits by dimension — the
        // primary-key order the one-Run read walks).
        const OPEN_OWNER: &str = "r.process_owner = 1 AND r.status <> 'ended'";
        let records = self.records(&format!("WHERE {OPEN_OWNER}"), [])?;
        let mut labels: HashMap<String, Vec<Label>> = HashMap::new();
        {
            let mut stmt = self
                .conn()
                .prepare(&format!(
                    "SELECT l.run_id, l.key, l.value, l.source, l.origin FROM run_labels l \
                     JOIN runs r ON r.run_id = l.run_id WHERE {OPEN_OWNER} \
                     ORDER BY l.run_id, l.key"
                ))
                .map_err(backend)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(backend)?;
            for row in rows {
                let (run_id, key, value, source, origin) = row.map_err(backend)?;
                let label = decode_label(&run_id, key, value, &source, origin)?;
                labels.entry(run_id).or_default().push(label);
            }
        }
        let mut limits: HashMap<String, (RunLimits, RunLimits)> = HashMap::new();
        {
            let mut stmt = self
                .conn()
                .prepare(&format!(
                    "SELECT l.run_id, l.dimension, l.scope, l.limit_value, l.action \
                     FROM run_limits l JOIN runs r ON r.run_id = l.run_id \
                     WHERE {OPEN_OWNER} ORDER BY l.run_id, l.dimension, l.scope"
                ))
                .map_err(backend)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(backend)?;
            let mut folds: HashMap<String, LimitsFold> = HashMap::new();
            for row in rows {
                let (run_id, dimension, scope, limit, action) = row.map_err(backend)?;
                folds
                    .entry(run_id.clone())
                    .or_default()
                    .row(&run_id, &dimension, &scope, limit, &action)?;
            }
            for (run_id, fold) in folds {
                limits.insert(run_id, fold.finish());
            }
        }
        let kept: HashSet<String> = {
            let mut stmt = self
                .conn()
                .prepare(
                    "SELECT i.name FROM breach_default_upgrade b \
                     JOIN agent_instances i ON i.id = b.instance_id",
                )
                .map_err(backend)?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(backend)?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(backend)?;
            names
        };
        let runs = records
            .into_iter()
            .map(|record| {
                let run_id = record.run_id.as_str();
                let kept_on_pause = record
                    .instance
                    .as_deref()
                    .is_some_and(|instance| kept.contains(instance));
                let (run_limits, caller_limits) = limits.remove(run_id).unwrap_or_default();
                crate::ports::ResolutionFacts {
                    labels: labels.remove(run_id).unwrap_or_default(),
                    limits: run_limits,
                    caller_limits,
                    kept_on_pause,
                    record,
                }
            })
            .collect();
        Ok(crate::ports::OpenOwnerFacts { runs, kept })
    }
}

impl SqliteStore {
    /// Read one Policy row with its Rate table. A row whose Selector no
    /// longer parses is a typed corrupt-row error — the policies read fails
    /// closed upstream (money code never silently drops a standing limit).
    fn read_policy(&self, name: &str) -> Result<PolicyRecord, StoreError> {
        #[allow(clippy::type_complexity)]
        let row: (
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = self
            .conn()
            .query_row(
                "SELECT selector, tokens_per_run, tokens_cumulative, dollars_per_run_micros, \
                 dollars_cumulative_micros, breach_action, scent_check, rate_input_micros, \
                 rate_output_micros, rate_cached_micros FROM policies WHERE name = ?1",
                [name],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .map_err(backend)?;
        let (
            selector,
            tokens_per_run,
            tokens_cumulative,
            dollars_per_run,
            dollars_cumulative,
            breach_action,
            scent_check,
            rate_input,
            rate_output,
            rate_cached,
        ) = row;
        let corrupt = |detail: String| StoreError::CorruptRow {
            name: "policies".to_string(),
            detail,
        };
        let selector = match selector {
            None => {
                if name != DEFAULT_POLICY_NAME {
                    return Err(corrupt(format!(
                        "policy `{name}` has no Selector (only the default Policy may)"
                    )));
                }
                None
            }
            Some(raw) => Some(
                Selector::parse(&raw)
                    .map_err(|err| corrupt(format!("policy `{name}` Selector `{raw}`: {err}")))?,
            ),
        };
        let breach_action = breach_action
            .map(|raw| raw.parse::<BreachAction>())
            .transpose()
            .map_err(|err| corrupt(format!("policy `{name}`: {err}")))?;
        let scent_check = scent_check
            .map(|raw| {
                ScentCheckMode::from_wire(&raw)
                    .ok_or_else(|| corrupt(format!("policy `{name}` has an unknown mode `{raw}`")))
            })
            .transpose()?;
        let rate = match (rate_input, rate_output) {
            (Some(input), Some(output)) => {
                let mut rate = Rate::new(input.max(0) as u64, output.max(0) as u64);
                if let Some(cached) = rate_cached {
                    rate = rate.with_cached(cached.max(0) as u64);
                }
                Some(rate)
            }
            _ => None,
        };
        let mut rates = std::collections::BTreeMap::new();
        if name == DEFAULT_POLICY_NAME {
            let mut stmt = self
                .conn()
                .prepare(
                    "SELECT model, input_micros_per_1m, output_micros_per_1m, \
                     cached_micros_per_1m FROM policy_rates WHERE policy = ?1 ORDER BY model",
                )
                .map_err(backend)?;
            let entries = stmt
                .query_map([name], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .map_err(backend)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(backend)?;
            for (model, input, output, cached) in entries {
                let mut rate = Rate::new(input.max(0) as u64, output.max(0) as u64);
                if let Some(cached) = cached {
                    rate = rate.with_cached(cached.max(0) as u64);
                }
                rates.insert(model, rate);
            }
        }
        Ok(PolicyRecord {
            name: name.to_string(),
            selector,
            limits: crate::domain::runs::PolicyLimits {
                tokens_per_run: tokens_per_run.map(|v| v.max(0) as u64),
                tokens_cumulative: tokens_cumulative.map(|v| v.max(0) as u64),
                dollars_per_run_micros: dollars_per_run.map(|v| v.max(0) as u64),
                dollars_cumulative_micros: dollars_cumulative.map(|v| v.max(0) as u64),
                breach_action,
                scent_check,
                rate,
            },
            rates,
        })
    }
}

/// Decode one `run_policy` row (typed corruption — the stored resolution is
/// money config; a row that cannot be read must fail the read).
fn decode_run_policy(
    resolution: &str,
    inputs: &str,
    updated_at: &str,
) -> Result<RunPolicy, StoreError> {
    let resolution: crate::domain::runs::PolicyResolution = serde_json::from_str(resolution)
        .map_err(|err| StoreError::CorruptRow {
            name: "run_policy".to_string(),
            detail: format!("a stored resolution could not be decoded: {err}"),
        })?;
    let inputs: crate::domain::runs::ResolveInputs =
        serde_json::from_str(inputs).map_err(|err| StoreError::CorruptRow {
            name: "run_policy".to_string(),
            detail: format!("stored resolution inputs could not be decoded: {err}"),
        })?;
    Ok(RunPolicy {
        resolution,
        inputs,
        updated_at: updated_at.to_string(),
    })
}

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// One Run's two limit layers (the Operator/Host layer, the Caller
    /// layer) in one read.
    fn run_limit_layers(&self, run_id: &RunId) -> Result<(RunLimits, RunLimits), StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT dimension, scope, limit_value, action FROM run_limits \
                 WHERE run_id = ?1",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([run_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(backend)?;
        let mut fold = LimitsFold::default();
        for row in rows {
            let (dimension, scope, limit, action) = row.map_err(backend)?;
            fold.row(run_id.as_str(), &dimension, &scope, limit, &action)?;
        }
        Ok(fold.finish())
    }
}
