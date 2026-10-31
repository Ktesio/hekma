//! The SQLite implementation of the Run-scale port [`RunStore`] and the
//! schema v12 step that creates its tables (story 21-1, spine AD-27).
//!
//! v12 is one crash-atomic step (the 11-3 rule): its DDL, the one-time import
//! of the legacy event files, the latch seed and the rollup backfill commit
//! together with the `user_version = 12` stamp, or none of it does.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::{backend, clamp_tokens, classify_usage_insert, SqliteStore};
use crate::domain::{
    cost_micros_with_cached, BreachAction, BreachDimension, BreachScope, BudgetBreachEvent,
    EngineEvent, InstanceName, LatchKey, Micros, OutboxEntry, Rate, RecordOutcome, RunId,
    TransitionEvent, UsageEvent, UsageTotals, OUTBOX_SCHEMA_VERSION,
};
use crate::ports::{OutboxAppend, OutboxBounds, Page, Rollup, RunStore, StoreError};

/// Schema v12 DDL (story 21-1, AD-27): the store writer's tables.
///
/// * `agent_instances.launch_generation` — bumped at every spawn, Restart
///   Policy relaunch and adoption; usage rows carry the generation they were
///   metered under, so a sequence that restarts never collides.
/// * `usage_events` is rebuilt (SQLite cannot relax `NOT NULL` in place):
///   `instance_id` becomes nullable (instance-less Runs, AD-27), and it gains
///   `launch_generation` and a nullable `call_id` (the shared proxy's per-call
///   id, story 23.1). Row ids are copied, so commit order is unchanged. Two
///   partial dedup indexes replace the v3 one: self-reported and observed rows
///   key on (instance, run, generation, sequence); proxied rows on (run, call).
/// * `usage_rollup_instance` / `usage_rollup_run` — the totals the evaluator
///   and the Fleet read, folded at commit. `usage_rollup_meta.folded_through`
///   is the highest `usage_events` id folded: every fold takes ALL rows above
///   it, so a row some other writer inserted (an older engine still running
///   during the upgrade) is folded by the next commit or open, never missed.
///   `usage_events.id` becomes AUTOINCREMENT so an id is never reused below
///   the watermark after a delete. Token sums are `u64` stored
///   bit-cast in the signed column (lossless round trip); `cached_unknown`
///   counts rows whose cached subset is unknown (a pre-v7 row).
/// * `transition_events` / `breach_events` — the durable event records (D1);
///   the event JSON verbatim plus the indexed columns reads filter on.
/// * `run_latches` — the durable per-Run breach latch, keyed by the run id
///   string with no foreign key until story 21.2 adds `runs`.
/// * `outbox` / `outbox_meta` — the global event outbox (replacing
///   `outbox.jsonl` and `outbox-ack.json`).
pub(super) const SCHEMA_V12: &str = "\
ALTER TABLE agent_instances ADD COLUMN launch_generation INTEGER NOT NULL DEFAULT 0;
CREATE TABLE usage_events_v12 (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id          INTEGER REFERENCES agent_instances(id) ON DELETE CASCADE,
    run_id               TEXT NOT NULL,
    input_tokens         INTEGER NOT NULL,
    output_tokens        INTEGER NOT NULL,
    metering_source      TEXT NOT NULL,
    occurred_at          TEXT NOT NULL,
    sequence             INTEGER NOT NULL DEFAULT 0,
    input_micros_per_1m  INTEGER,
    output_micros_per_1m INTEGER,
    cached_tokens        INTEGER,
    cached_micros_per_1m INTEGER,
    launch_generation    INTEGER NOT NULL DEFAULT 0,
    call_id              TEXT
);
INSERT INTO usage_events_v12 (id, instance_id, run_id, input_tokens, output_tokens,
    metering_source, occurred_at, sequence, input_micros_per_1m, output_micros_per_1m,
    cached_tokens, cached_micros_per_1m)
    SELECT id, instance_id, run_id, input_tokens, output_tokens, metering_source,
        occurred_at, sequence, input_micros_per_1m, output_micros_per_1m, cached_tokens,
        cached_micros_per_1m
    FROM usage_events ORDER BY id;
DROP TABLE usage_events;
ALTER TABLE usage_events_v12 RENAME TO usage_events;
CREATE INDEX idx_usage_events_instance ON usage_events(instance_id);
CREATE UNIQUE INDEX idx_usage_events_dedup
    ON usage_events(instance_id, run_id, launch_generation, sequence) WHERE call_id IS NULL;
CREATE UNIQUE INDEX idx_usage_events_call
    ON usage_events(run_id, call_id) WHERE call_id IS NOT NULL;
CREATE TABLE usage_rollup_instance (
    instance_id    INTEGER PRIMARY KEY REFERENCES agent_instances(id) ON DELETE CASCADE,
    events         INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_unknown INTEGER NOT NULL DEFAULT 0,
    cost_micros    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE usage_rollup_run (
    instance_id    INTEGER NOT NULL REFERENCES agent_instances(id) ON DELETE CASCADE,
    run_id         TEXT NOT NULL,
    events         INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_tokens  INTEGER NOT NULL DEFAULT 0,
    cached_unknown INTEGER NOT NULL DEFAULT 0,
    cost_micros    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (instance_id, run_id)
);
CREATE TABLE usage_rollup_meta (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    folded_through INTEGER NOT NULL DEFAULT 0
);
INSERT INTO usage_rollup_meta (id, folded_through) VALUES (1, 0);
CREATE TABLE transition_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id INTEGER NOT NULL REFERENCES agent_instances(id) ON DELETE CASCADE,
    event       TEXT NOT NULL
);
CREATE INDEX idx_transition_events_instance ON transition_events(instance_id, id);
CREATE TABLE breach_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id INTEGER NOT NULL REFERENCES agent_instances(id) ON DELETE CASCADE,
    run_id      TEXT NOT NULL,
    dimension   TEXT NOT NULL,
    scope       TEXT NOT NULL,
    action      TEXT NOT NULL,
    event       TEXT NOT NULL
);
CREATE INDEX idx_breach_events_instance ON breach_events(instance_id, id);
CREATE INDEX idx_breach_events_run ON breach_events(run_id);
CREATE TABLE run_latches (
    run_id     TEXT NOT NULL,
    dimension  TEXT NOT NULL,
    scope      TEXT NOT NULL,
    action     TEXT NOT NULL,
    latched_at TEXT NOT NULL,
    PRIMARY KEY (run_id, dimension, scope, action)
);
CREATE TABLE outbox (
    seq   INTEGER PRIMARY KEY AUTOINCREMENT,
    event TEXT NOT NULL,
    bytes INTEGER NOT NULL
);
CREATE TABLE outbox_meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    ack_seq     INTEGER,
    total_bytes INTEGER NOT NULL DEFAULT 0
);
INSERT INTO outbox_meta (id, ack_seq, total_bytes) VALUES (1, NULL, 0);
";

/// What the legacy event files held for one instance, read tolerantly.
#[derive(Debug)]
pub(crate) struct LegacyLogs<T> {
    pub records: Vec<T>,
    /// Lines that could not be imported, already phrased for the operator.
    pub notes: Vec<String>,
}

impl<T> Default for LegacyLogs<T> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// The legacy outbox journal's retained entries and ack cursor.
#[derive(Debug, Default)]
pub(crate) struct LegacyOutbox {
    pub entries: Vec<OutboxEntry>,
    pub ack: Option<u64>,
}

/// Where the v12 step reads the pre-21-1 event files from. The domain layer
/// implements it (it owns the file formats); a store opened without one
/// imports nothing.
pub(crate) trait LegacySource {
    fn transitions(&self, instance: &str) -> LegacyLogs<TransitionEvent>;
    fn breaches(&self, instance: &str) -> LegacyLogs<BudgetBreachEvent>;
    /// `Err` refuses the open: seqs must never be minted from an unverified
    /// journal (the 17-4 rule).
    fn outbox(&self) -> Result<LegacyOutbox, String>;
}

/// The import-nothing source (in-memory and direct-open test stores).
#[cfg(test)]
pub(crate) struct NoLegacy;

#[cfg(test)]
impl LegacySource for NoLegacy {
    fn transitions(&self, _instance: &str) -> LegacyLogs<TransitionEvent> {
        LegacyLogs::default()
    }
    fn breaches(&self, _instance: &str) -> LegacyLogs<BudgetBreachEvent> {
        LegacyLogs::default()
    }
    fn outbox(&self) -> Result<LegacyOutbox, String> {
        Ok(LegacyOutbox::default())
    }
}

/// Run the v12 step: DDL, import, latch seed, rollup backfill and the stamp in
/// ONE `BEGIN IMMEDIATE`. Returns the import notes for the engine's
/// diagnostic surface.
pub(super) fn migrate_v12(
    conn: &Connection,
    legacy: &dyn LegacySource,
) -> Result<Vec<String>, StoreError> {
    // Read the outbox first, outside the transaction: an unreadable journal
    // refuses before anything is written.
    let outbox = legacy
        .outbox()
        .map_err(|detail| StoreError::Backend(format!("outbox import refused: {detail}")))?;
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    // Another process may have run this step while we waited for the write
    // lock: re-check the version under the lock, and step aside if so.
    let version: i64 = match conn.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(backend(err));
        }
    };
    if version >= 12 {
        let _ = conn.execute_batch("ROLLBACK");
        return Ok(Vec::new());
    }
    match migrate_v12_body(conn, legacy, &outbox) {
        Ok(notes) => match conn.execute_batch("PRAGMA user_version = 12; COMMIT;") {
            Ok(()) => Ok(notes),
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(backend(err))
            }
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn migrate_v12_body(
    conn: &Connection,
    legacy: &dyn LegacySource,
    outbox: &LegacyOutbox,
) -> Result<Vec<String>, StoreError> {
    conn.execute_batch(SCHEMA_V12).map_err(backend)?;
    let mut notes = Vec::new();
    let instances: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, name FROM agent_instances ORDER BY id")
            .map_err(backend)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(backend)?;
        rows.collect::<Result<_, _>>().map_err(backend)?
    };
    for (id, name) in &instances {
        let transitions = legacy.transitions(name);
        notes.extend(transitions.notes);
        for event in &transitions.records {
            insert_transition_row(conn, *id, event)?;
        }
        let breaches = legacy.breaches(name);
        notes.extend(breaches.notes);
        for event in &breaches.records {
            insert_breach_row(conn, *id, event)?;
        }
    }
    // Latches: the breaches already recorded for each live instance's current
    // Run, so a running agent never re-fires a breach after the upgrade.
    conn.execute(
        "INSERT OR IGNORE INTO run_latches (run_id, dimension, scope, action, latched_at) \
         SELECT b.run_id, b.dimension, b.scope, b.action, ?1 FROM breach_events b \
         JOIN agent_runtime r ON r.instance_id = b.instance_id AND r.run_id = b.run_id",
        [crate::time::now_rfc3339()],
    )
    .map_err(backend)?;
    // The outbox: entries keep their seqs; the sequence continues above the
    // journal's high water and the ack (the ack is the seq floor, 17-4).
    let mut total_bytes: i64 = 0;
    for entry in &outbox.entries {
        let event = serde_json::to_string(&entry.event).map_err(|e| {
            StoreError::Backend(format!("could not re-encode an outbox entry: {e}"))
        })?;
        let bytes = event.len() as i64;
        conn.execute(
            "INSERT INTO outbox (seq, event, bytes) VALUES (?1, ?2, ?3)",
            rusqlite::params![clamp_tokens(entry.seq), event, bytes],
        )
        .map_err(backend)?;
        total_bytes = total_bytes.saturating_add(bytes);
    }
    let floor = outbox
        .entries
        .last()
        .map_or(0, |entry| entry.seq)
        .max(outbox.ack.unwrap_or(0));
    if floor > 0 {
        let updated = conn
            .execute(
                "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'outbox'",
                [clamp_tokens(floor)],
            )
            .map_err(backend)?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('outbox', ?1)",
                [clamp_tokens(floor)],
            )
            .map_err(backend)?;
        }
    }
    conn.execute(
        "UPDATE outbox_meta SET ack_seq = ?1, total_bytes = ?2 WHERE id = 1",
        rusqlite::params![outbox.ack.map(clamp_tokens), total_bytes],
    )
    .map_err(backend)?;
    backfill_rollups(conn)?;
    Ok(notes)
}

/// Fold every existing usage row into the rollups, with the same per-row
/// pricing and saturation the old scan-based totals used.
fn backfill_rollups(conn: &Connection) -> Result<(), StoreError> {
    fold_unfolded_rows(conn)
}

/// Fold every usage row above the `folded_through` watermark into both
/// rollups and advance the watermark (story 21-1). The common case — called
/// right after one insert — folds exactly that row.
pub(super) fn fold_unfolded_rows(conn: &Connection) -> Result<(), StoreError> {
    let watermark: i64 = conn
        .query_row(
            "SELECT folded_through FROM usage_rollup_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(backend)?;
    let mut by_instance: HashMap<i64, RollupRow> = HashMap::new();
    let mut by_run: HashMap<(i64, String), RollupRow> = HashMap::new();
    let mut highest = watermark;
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, instance_id, run_id, input_tokens, output_tokens, \
                 input_micros_per_1m, output_micros_per_1m, cached_tokens, cached_micros_per_1m \
                 FROM usage_events WHERE id > ?1 ORDER BY id",
            )
            .map_err(backend)?;
        let mut rows = stmt.query([watermark]).map_err(backend)?;
        while let Some(row) = rows.next().map_err(backend)? {
            highest = highest.max(row.get::<_, i64>(0).map_err(backend)?);
            // An instance-less row (none exist before story 21.2) has no
            // per-instance rollup to fold into.
            let Some(instance_id) = row.get::<_, Option<i64>>(1).map_err(backend)? else {
                continue;
            };
            let run_id: String = row.get(2).map_err(backend)?;
            let stored = StoredUsage {
                input: row.get::<_, i64>(3).map_err(backend)?.max(0) as u64,
                output: row.get::<_, i64>(4).map_err(backend)?.max(0) as u64,
                rate_input: row.get(5).map_err(backend)?,
                rate_output: row.get(6).map_err(backend)?,
                cached: row
                    .get::<_, Option<i64>>(7)
                    .map_err(backend)?
                    .map(|c| c.max(0) as u64),
                rate_cached: row
                    .get::<_, Option<i64>>(8)
                    .map_err(backend)?
                    .map(|r| r.max(0) as u64),
            };
            by_instance.entry(instance_id).or_default().add(&stored);
            by_run
                .entry((instance_id, run_id))
                .or_default()
                .add(&stored);
        }
    }
    for (instance_id, delta) in &by_instance {
        let mut rollup = read_instance_rollup(conn, *instance_id)?;
        rollup.merge(delta);
        write_instance_rollup(conn, *instance_id, &rollup)?;
    }
    for ((instance_id, run_id), delta) in &by_run {
        let mut rollup = read_run_rollup(conn, *instance_id, run_id)?;
        rollup.merge(delta);
        write_run_rollup(conn, *instance_id, run_id, &rollup)?;
    }
    if highest > watermark {
        conn.execute(
            "UPDATE usage_rollup_meta SET folded_through = ?1 WHERE id = 1",
            [highest],
        )
        .map_err(backend)?;
    }
    Ok(())
}

/// One usage row as stored (clamped columns), the unit a rollup folds.
struct StoredUsage {
    input: u64,
    output: u64,
    rate_input: Option<i64>,
    rate_output: Option<i64>,
    cached: Option<u64>,
    rate_cached: Option<u64>,
}

impl StoredUsage {
    /// The row's cost at its own stored Rate (zero without both directions) —
    /// the same derivation the old per-row cost scan used.
    fn cost(&self) -> Micros {
        match (self.rate_input, self.rate_output) {
            (Some(ri), Some(ro)) => {
                let mut rate = Rate::new(ri.max(0) as u64, ro.max(0) as u64);
                if let Some(rc) = self.rate_cached {
                    rate = rate.with_cached(rc);
                }
                cost_micros_with_cached(self.input, self.output, self.cached, &rate)
            }
            _ => Micros::ZERO,
        }
    }
}

/// A rollup row in memory: saturating `u64` token sums and the cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RollupRow {
    events: u64,
    input: u64,
    output: u64,
    cached: u64,
    cached_unknown: u64,
    cost: Micros,
}

impl RollupRow {
    fn add(&mut self, row: &StoredUsage) {
        self.events = self.events.saturating_add(1);
        self.input = self.input.saturating_add(row.input);
        self.output = self.output.saturating_add(row.output);
        match row.cached {
            Some(cached) => self.cached = self.cached.saturating_add(cached),
            None => self.cached_unknown = self.cached_unknown.saturating_add(1),
        }
        self.cost = self.cost.saturating_add(row.cost());
    }

    /// Fold another rollup's sums in (saturating, like a row-by-row fold).
    fn merge(&mut self, other: &RollupRow) {
        self.events = self.events.saturating_add(other.events);
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached = self.cached.saturating_add(other.cached);
        self.cached_unknown = self.cached_unknown.saturating_add(other.cached_unknown);
        self.cost = self.cost.saturating_add(other.cost);
    }

    fn as_rollup(&self) -> Rollup {
        Rollup {
            totals: UsageTotals {
                input_tokens: self.input,
                output_tokens: self.output,
                // One unknown-cached row makes the whole scope unknown (14-6).
                cached_tokens: (self.cached_unknown == 0).then_some(self.cached),
            },
            cost: self.cost,
        }
    }

    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            events: row.get::<_, i64>(0)? as u64,
            input: row.get::<_, i64>(1)? as u64,
            output: row.get::<_, i64>(2)? as u64,
            cached: row.get::<_, i64>(3)? as u64,
            cached_unknown: row.get::<_, i64>(4)? as u64,
            cost: Micros(row.get::<_, i64>(5)?),
        })
    }
}

// The `u64` sums are stored bit-cast (`as i64`) and read back `as u64`, a
// lossless round trip through SQLite's signed integer column.
fn write_instance_rollup(
    conn: &Connection,
    instance_id: i64,
    rollup: &RollupRow,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO usage_rollup_instance \
         (instance_id, events, input_tokens, output_tokens, cached_tokens, cached_unknown, \
          cost_micros) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(instance_id) DO UPDATE SET events = excluded.events, \
         input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens, \
         cached_tokens = excluded.cached_tokens, cached_unknown = excluded.cached_unknown, \
         cost_micros = excluded.cost_micros",
        rusqlite::params![
            instance_id,
            rollup.events as i64,
            rollup.input as i64,
            rollup.output as i64,
            rollup.cached as i64,
            rollup.cached_unknown as i64,
            rollup.cost.0,
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn write_run_rollup(
    conn: &Connection,
    instance_id: i64,
    run_id: &str,
    rollup: &RollupRow,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO usage_rollup_run \
         (instance_id, run_id, events, input_tokens, output_tokens, cached_tokens, \
          cached_unknown, cost_micros) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT(instance_id, run_id) DO UPDATE SET events = excluded.events, \
         input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens, \
         cached_tokens = excluded.cached_tokens, cached_unknown = excluded.cached_unknown, \
         cost_micros = excluded.cost_micros",
        rusqlite::params![
            instance_id,
            run_id,
            rollup.events as i64,
            rollup.input as i64,
            rollup.output as i64,
            rollup.cached as i64,
            rollup.cached_unknown as i64,
            rollup.cost.0,
        ],
    )
    .map_err(backend)?;
    Ok(())
}

const ROLLUP_COLUMNS: &str =
    "events, input_tokens, output_tokens, cached_tokens, cached_unknown, cost_micros";

fn read_instance_rollup(conn: &Connection, instance_id: i64) -> Result<RollupRow, StoreError> {
    conn.query_row(
        &format!("SELECT {ROLLUP_COLUMNS} FROM usage_rollup_instance WHERE instance_id = ?1"),
        [instance_id],
        RollupRow::read,
    )
    .optional()
    .map_err(backend)
    .map(Option::unwrap_or_default)
}

fn read_run_rollup(
    conn: &Connection,
    instance_id: i64,
    run_id: &str,
) -> Result<RollupRow, StoreError> {
    conn.query_row(
        &format!(
            "SELECT {ROLLUP_COLUMNS} FROM usage_rollup_run WHERE instance_id = ?1 AND run_id = ?2"
        ),
        rusqlite::params![instance_id, run_id],
        RollupRow::read,
    )
    .optional()
    .map_err(backend)
    .map(Option::unwrap_or_default)
}

fn insert_transition_row(
    conn: &Connection,
    instance_id: i64,
    event: &TransitionEvent,
) -> Result<(), StoreError> {
    let json = serde_json::to_string(event)
        .map_err(|e| StoreError::Backend(format!("could not encode a transition event: {e}")))?;
    conn.execute(
        "INSERT INTO transition_events (instance_id, event) VALUES (?1, ?2)",
        rusqlite::params![instance_id, json],
    )
    .map_err(backend)?;
    Ok(())
}

fn insert_breach_row(
    conn: &Connection,
    instance_id: i64,
    event: &BudgetBreachEvent,
) -> Result<(), StoreError> {
    let json = serde_json::to_string(event)
        .map_err(|e| StoreError::Backend(format!("could not encode a breach event: {e}")))?;
    conn.execute(
        "INSERT INTO breach_events (instance_id, run_id, dimension, scope, action, event) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            instance_id,
            event.run_id,
            event.dimension.as_str(),
            event.scope.as_str(),
            event.action.as_str(),
            json,
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(json: &str, what: &str) -> rusqlite::Result<T> {
    serde_json::from_str(json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("corrupt stored {what}: {e}").into(),
        )
    })
}

fn parse_dimension(raw: &str) -> Option<BreachDimension> {
    [BreachDimension::Tokens, BreachDimension::Dollars]
        .into_iter()
        .find(|dimension| dimension.as_str() == raw)
}

fn parse_scope(raw: &str) -> Option<BreachScope> {
    [BreachScope::PerRun, BreachScope::Cumulative]
        .into_iter()
        .find(|scope| scope.as_str() == raw)
}

impl SqliteStore {
    /// The instance's row id, or `NotFound` (writes need the foreign key).
    fn require_instance_id(&self, name: &str) -> Result<i64, StoreError> {
        let iname = InstanceName::new(name).map_err(|e| StoreError::CorruptRow {
            name: name.to_string(),
            detail: format!("invalid instance name on an event: {e}"),
        })?;
        self.instance_id(&iname)?
            .ok_or_else(|| StoreError::NotFound {
                name: name.to_string(),
            })
    }

    /// Run several reads against ONE snapshot (a deferred read transaction),
    /// so they see one consistent state of the database. Inside an already
    /// open transaction the reads simply run in it.
    pub(crate) fn snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if !self.conn().is_autocommit() {
            return read(self);
        }
        self.conn()
            .execute_batch("BEGIN DEFERRED")
            .map_err(backend)?;
        let result = read(self);
        // A read transaction has nothing to keep; ending it can only fail if
        // the connection is gone, in which case the read's own error stands.
        let _ = self.conn().execute_batch("COMMIT");
        result
    }

    /// The instance's current launch generation (`NotFound` when absent).
    pub(crate) fn launch_generation(&self, name: &InstanceName) -> Result<u64, StoreError> {
        self.conn()
            .query_row(
                "SELECT launch_generation FROM agent_instances WHERE name = ?1",
                [name.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(backend)?
            .map(|g| g.max(0) as u64)
            .ok_or_else(|| StoreError::NotFound {
                name: name.as_str().to_string(),
            })
    }

    /// Test-only fault injection: drop the outbox table so every outbox
    /// append fails (the transaction-atomicity pins).
    #[cfg(test)]
    pub(crate) fn break_outbox_for_test(&self) {
        self.conn()
            .execute_batch("DROP TABLE outbox")
            .expect("drop outbox for test");
    }

    /// Delete the oldest outbox rows up to and including `through_seq`, and
    /// take their bytes off the running total.
    fn evict_outbox_through(&self, through_seq: i64) -> Result<(), StoreError> {
        let freed: i64 = self
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM outbox WHERE seq <= ?1",
                [through_seq],
                |row| row.get(0),
            )
            .map_err(backend)?;
        self.conn()
            .execute("DELETE FROM outbox WHERE seq <= ?1", [through_seq])
            .map_err(backend)?;
        self.conn()
            .execute(
                "UPDATE outbox_meta SET total_bytes = MAX(total_bytes - ?1, 0) WHERE id = 1",
                [freed],
            )
            .map_err(backend)?;
        Ok(())
    }
}

impl RunStore for SqliteStore {
    fn insert_usage(
        &self,
        event: &UsageEvent,
        rate: Option<Rate>,
        generation: u64,
    ) -> Result<RecordOutcome, StoreError> {
        let id = self.require_instance_id(&event.instance)?;
        // The per-event effective Rate (3-3) and cached Rate (14-6), clamped
        // into their signed columns exactly as before.
        let rate_input = rate.map(|r| clamp_tokens(r.input_micros_per_1m));
        let rate_output = rate.map(|r| clamp_tokens(r.output_micros_per_1m));
        let rate_cached = rate.and_then(|r| r.cached_micros_per_1m).map(clamp_tokens);
        let cached = event.cached_tokens.map(clamp_tokens);
        let result = self.conn().execute(
            "INSERT INTO usage_events \
             (instance_id, run_id, input_tokens, output_tokens, metering_source, occurred_at, \
              sequence, input_micros_per_1m, output_micros_per_1m, cached_tokens, \
              cached_micros_per_1m, launch_generation) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                event.run_id.as_str(),
                clamp_tokens(event.input_tokens),
                clamp_tokens(event.output_tokens),
                event.metering_source,
                event.occurred_at,
                clamp_tokens(event.sequence),
                rate_input,
                rate_output,
                cached,
                rate_cached,
                clamp_tokens(generation),
            ],
        );
        match result {
            Ok(_) => {}
            Err(err) => {
                return match classify_usage_insert(&err) {
                    Some(outcome) => Ok(outcome),
                    None => Err(StoreError::Backend(format!(
                        "usage-event insert failed: {err}"
                    ))),
                }
            }
        }
        // Fold the row (and any row another writer left unfolded) into both
        // rollups from the STORED (clamped) values, so a rollup equals the
        // sum the old per-row scan produced.
        fold_unfolded_rows(self.conn())?;
        Ok(RecordOutcome::Inserted)
    }

    fn instance_rollup(&self, name: &InstanceName) -> Result<Rollup, StoreError> {
        let Some(id) = self.instance_id(name)? else {
            return Ok(Rollup::zero());
        };
        Ok(read_instance_rollup(self.conn(), id)?.as_rollup())
    }

    fn run_rollup(&self, name: &InstanceName, run_id: &RunId) -> Result<Rollup, StoreError> {
        let Some(id) = self.instance_id(name)? else {
            return Ok(Rollup::zero());
        };
        Ok(read_run_rollup(self.conn(), id, run_id.as_str())?.as_rollup())
    }

    fn bump_launch_generation(&self, name: &InstanceName) -> Result<u64, StoreError> {
        let generation: Option<i64> = self
            .conn()
            .query_row(
                "UPDATE agent_instances SET launch_generation = launch_generation + 1 \
                 WHERE name = ?1 RETURNING launch_generation",
                [name.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        generation
            .map(|g| g.max(0) as u64)
            .ok_or_else(|| StoreError::NotFound {
                name: name.as_str().to_string(),
            })
    }

    fn append_transition(&self, event: &TransitionEvent) -> Result<(), StoreError> {
        let id = self.require_instance_id(&event.instance)?;
        insert_transition_row(self.conn(), id, event)
    }

    fn append_breach(&self, event: &BudgetBreachEvent) -> Result<(), StoreError> {
        let id = self.require_instance_id(&event.instance)?;
        insert_breach_row(self.conn(), id, event)
    }

    fn transitions_page(
        &self,
        name: &InstanceName,
        after: i64,
        limit: usize,
    ) -> Result<Page<TransitionEvent>, StoreError> {
        let Some(id) = self.instance_id(name)? else {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        };
        let limit = crate::ports::clamp_page(limit);
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, event FROM transition_events WHERE instance_id = ?1 AND id > ?2 \
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![id, after, limit as i64], |row| {
                let json: String = row.get(1)?;
                Ok((row.get::<_, i64>(0)?, decode(&json, "transition event")?))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(Page::from_rows(rows, limit))
    }

    fn breaches_page(
        &self,
        name: &InstanceName,
        after: i64,
        limit: usize,
    ) -> Result<Page<BudgetBreachEvent>, StoreError> {
        let Some(id) = self.instance_id(name)? else {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        };
        let limit = crate::ports::clamp_page(limit);
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT id, event FROM breach_events WHERE instance_id = ?1 AND id > ?2 \
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![id, after, limit as i64], |row| {
                let json: String = row.get(1)?;
                Ok((row.get::<_, i64>(0)?, decode(&json, "breach event")?))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(Page::from_rows(rows, limit))
    }

    fn run_breaches(
        &self,
        name: &InstanceName,
        run_id: &RunId,
    ) -> Result<Vec<BudgetBreachEvent>, StoreError> {
        let Some(id) = self.instance_id(name)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT event FROM breach_events WHERE instance_id = ?1 AND run_id = ?2 \
                 ORDER BY id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![id, run_id.as_str()], |row| {
                let json: String = row.get(0)?;
                decode(&json, "breach event")
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    fn latch(
        &self,
        run_id: &RunId,
        key: LatchKey,
        action: BreachAction,
        at: &str,
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT OR IGNORE INTO run_latches (run_id, dimension, scope, action, latched_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    run_id.as_str(),
                    key.dimension.as_str(),
                    key.scope.as_str(),
                    action.as_str(),
                    at,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    fn latches(&self, run_id: &RunId) -> Result<Vec<(LatchKey, BreachAction)>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT dimension, scope, action FROM run_latches WHERE run_id = ?1")
            .map_err(backend)?;
        let rows = stmt
            .query_map([run_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.into_iter()
            .map(|(dimension, scope, action)| {
                let corrupt = || StoreError::CorruptRow {
                    name: run_id.as_str().to_string(),
                    detail: format!("unreadable latch row ({dimension}, {scope}, {action})"),
                };
                Ok((
                    LatchKey::new(
                        parse_dimension(&dimension).ok_or_else(corrupt)?,
                        parse_scope(&scope).ok_or_else(corrupt)?,
                    ),
                    action.parse::<BreachAction>().map_err(|_| corrupt())?,
                ))
            })
            .collect()
    }

    fn append_outbox(&self, event: &EngineEvent, cap: u64) -> Result<OutboxAppend, StoreError> {
        let json = serde_json::to_string(event)
            .map_err(|e| StoreError::Backend(format!("could not encode an outbox event: {e}")))?;
        let incoming = json.len() as u64;
        let mut append = OutboxAppend::default();
        let (total, ack): (i64, Option<i64>) = self
            .conn()
            .query_row(
                "SELECT total_bytes, ack_seq FROM outbox_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(backend)?;
        let total = total.max(0) as u64;
        if total + incoming > cap {
            // Check-before-write retention (the 17-4 compaction rules): the
            // acked prefix first, then the oldest un-acked, never the last row.
            let mut need = total + incoming - cap;
            let last: i64 = self
                .conn()
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM outbox", [], |row| {
                    row.get(0)
                })
                .map_err(backend)?;
            // The whole acked prefix goes once compaction runs (acked means
            // consumed); un-acked rows go only while still over the cap. The
            // scan stops at the first row it keeps, so a flood past the cap
            // reads only the rows it evicts.
            let mut through: Option<i64> = None;
            {
                let mut stmt = self
                    .conn()
                    .prepare("SELECT seq, bytes FROM outbox WHERE seq < ?1 ORDER BY seq")
                    .map_err(backend)?;
                let mut cursor = stmt.query([last]).map_err(backend)?;
                while let Some(row) = cursor.next().map_err(backend)? {
                    let seq: i64 = row.get(0).map_err(backend)?;
                    let bytes: i64 = row.get(1).map_err(backend)?;
                    if ack.is_some_and(|ack| seq <= ack) {
                        append.dropped_acked += 1;
                    } else if need > 0 {
                        append.dropped_unacked += 1;
                    } else {
                        break;
                    }
                    need = need.saturating_sub(bytes.max(0) as u64);
                    through = Some(seq);
                }
            }
            if let Some(through) = through {
                self.evict_outbox_through(through)?;
            }
        }
        self.conn()
            .execute(
                "INSERT INTO outbox (event, bytes) VALUES (?1, ?2)",
                rusqlite::params![json, incoming as i64],
            )
            .map_err(backend)?;
        append.seq = self.conn().last_insert_rowid().max(0) as u64;
        self.conn()
            .execute(
                "UPDATE outbox_meta SET total_bytes = total_bytes + ?1 WHERE id = 1",
                [incoming as i64],
            )
            .map_err(backend)?;
        Ok(append)
    }

    fn outbox_page(&self, after: u64, limit: usize) -> Result<Page<OutboxEntry>, StoreError> {
        let limit = limit.max(1);
        let mut stmt = self
            .conn()
            .prepare("SELECT seq, event FROM outbox WHERE seq > ?1 ORDER BY seq LIMIT ?2")
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                rusqlite::params![clamp_tokens(after), limit as i64],
                |row| {
                    let seq: i64 = row.get(0)?;
                    let json: String = row.get(1)?;
                    Ok((
                        seq,
                        OutboxEntry {
                            schema_version: OUTBOX_SCHEMA_VERSION,
                            seq: seq.max(0) as u64,
                            event: decode(&json, "outbox event")?,
                        },
                    ))
                },
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(Page::from_rows(rows, limit))
    }

    fn outbox_bounds(&self) -> Result<OutboxBounds, StoreError> {
        let (first, last): (Option<i64>, Option<i64>) = self
            .conn()
            .query_row("SELECT MIN(seq), MAX(seq) FROM outbox", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(backend)?;
        let ack: Option<i64> = self
            .conn()
            .query_row("SELECT ack_seq FROM outbox_meta WHERE id = 1", [], |row| {
                row.get(0)
            })
            .map_err(backend)?;
        Ok(OutboxBounds {
            retained_from: first.map_or(1, |seq| seq.max(0) as u64),
            tail: last.map_or(0, |seq| seq.max(0) as u64),
            ack: ack.map(|seq| seq.max(0) as u64),
        })
    }

    fn set_outbox_ack(&self, seq: u64) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "UPDATE outbox_meta SET ack_seq = ?1 WHERE id = 1",
                [clamp_tokens(seq)],
            )
            .map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AgentInstance, BreachAction, BreachDimension, BreachScope, LifecycleState, TransitionCause,
    };
    use crate::ports::StateStore;

    fn name(raw: &str) -> InstanceName {
        InstanceName::new(raw).unwrap()
    }

    fn store_with(raw: &str) -> SqliteStore {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .create_instance(&AgentInstance {
                name: name(raw),
                kind: "mock".to_string(),
                state: LifecycleState::Registered,
                agent_home: format!("/x/agents/{raw}"),
                created_at: "2026-10-31T00:00:00Z".to_string(),
                updated_at: "2026-10-31T00:00:00Z".to_string(),
            })
            .unwrap();
        store
    }

    #[test]
    fn reads_for_an_unknown_instance_are_empty_and_writes_name_it() {
        let store = store_with("known");
        let ghost = name("ghost");
        let run = RunId::from_wire("run-1");
        assert!(store
            .transitions_page(&ghost, 0, 10)
            .unwrap()
            .items
            .is_empty());
        assert!(store.breaches_page(&ghost, 0, 10).unwrap().items.is_empty());
        assert!(store.run_breaches(&ghost, &run).unwrap().is_empty());
        assert_eq!(store.instance_rollup(&ghost).unwrap(), Rollup::zero());
        assert_eq!(store.run_rollup(&ghost, &run).unwrap(), Rollup::zero());
        assert!(matches!(
            store.launch_generation(&ghost),
            Err(StoreError::NotFound { .. })
        ));
        let stray = TransitionEvent::new(
            "ghost",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::AdapterReady,
            "2026-10-31T00:00:00Z",
        );
        assert!(matches!(
            store.append_transition(&stray),
            Err(StoreError::NotFound { .. })
        ));
        let mut bad = stray.clone();
        bad.instance = "Not A Name".to_string();
        assert!(matches!(
            store.append_transition(&bad),
            Err(StoreError::CorruptRow { .. })
        ));
    }

    #[test]
    fn keyset_pages_carry_the_next_key_until_the_read_is_caught_up() {
        let store = store_with("paged");
        for second in 0..5 {
            store
                .append_transition(&TransitionEvent::new(
                    "paged",
                    LifecycleState::Registered,
                    LifecycleState::Starting,
                    TransitionCause::AdapterReady,
                    format!("2026-10-31T00:00:0{second}Z"),
                ))
                .unwrap();
        }
        let first = store.transitions_page(&name("paged"), 0, 2).unwrap();
        assert_eq!(first.items.len(), 2);
        let next = first.next.expect("a full page carries its last key");
        let second = store.transitions_page(&name("paged"), next, 2).unwrap();
        let third = store
            .transitions_page(&name("paged"), second.next.unwrap(), 2)
            .unwrap();
        assert_eq!(third.items.len(), 1);
        assert_eq!(third.next, None, "a short page ends the read");
        assert_eq!(crate::ports::clamp_page(0), crate::ports::PAGE_MAX);
        assert_eq!(crate::ports::clamp_page(5_000), crate::ports::PAGE_MAX);
    }

    #[test]
    fn a_corrupt_stored_row_is_a_typed_error_never_a_silent_skip() {
        let store = store_with("corrupt");
        let run = RunId::from_wire("run-1");
        store
            .conn()
            .execute(
                "INSERT INTO run_latches VALUES ('run-1', 'tokens', 'sideways', 'pause', 'now')",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.latches(&run),
            Err(StoreError::CorruptRow { .. })
        ));
        store
            .conn()
            .execute(
                "INSERT INTO transition_events (instance_id, event) VALUES (1, 'not json')",
                [],
            )
            .unwrap();
        assert!(store.transitions_page(&name("corrupt"), 0, 10).is_err());
        store
            .latch(
                &RunId::from_wire("run-2"),
                LatchKey::new(BreachDimension::Dollars, BreachScope::Cumulative),
                BreachAction::Stop,
                "now",
            )
            .unwrap();
        assert_eq!(
            store.latches(&RunId::from_wire("run-2")).unwrap(),
            vec![(
                LatchKey::new(BreachDimension::Dollars, BreachScope::Cumulative),
                BreachAction::Stop
            )]
        );
    }

    #[test]
    fn a_snapshot_inside_an_open_transaction_reads_in_it() {
        let store = store_with("snap");
        store.conn().execute_batch("BEGIN").unwrap();
        let bounds = store.snapshot(|s| s.outbox_bounds()).unwrap();
        assert_eq!(
            (bounds.retained_from, bounds.tail, bounds.ack),
            (1, 0, None)
        );
        store.conn().execute_batch("COMMIT").unwrap();
    }

    struct AckOnly;

    impl LegacySource for AckOnly {
        fn transitions(&self, _instance: &str) -> LegacyLogs<TransitionEvent> {
            LegacyLogs::default()
        }
        fn breaches(&self, _instance: &str) -> LegacyLogs<BudgetBreachEvent> {
            LegacyLogs::default()
        }
        fn outbox(&self) -> Result<LegacyOutbox, String> {
            Ok(LegacyOutbox {
                entries: Vec::new(),
                ack: Some(41),
            })
        }
    }

    #[test]
    fn an_ack_with_an_empty_journal_seeds_the_sequence_above_it() {
        // An ack file survived an empty journal: the ack is the seq floor, so
        // the first new entry is 42, never a reused range below it.
        let dir = tempfile::TempDir::new().unwrap();
        let (store, notes) =
            SqliteStore::open_with_legacy(&dir.path().join("state.db"), &AckOnly).unwrap();
        assert!(notes.is_empty());
        let event = EngineEvent::Transition(TransitionEvent::new(
            "x",
            LifecycleState::Registered,
            LifecycleState::Starting,
            TransitionCause::AdapterReady,
            "2026-10-31T00:00:00Z",
        ));
        let append = store.append_outbox(&event, u64::MAX).unwrap();
        assert_eq!(append.seq, 42);
        assert_eq!(store.outbox_bounds().unwrap().ack, Some(41));
    }
}
