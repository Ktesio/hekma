//! The per-call records store (story 23-1, spine AD-26 / AD-27).
//!
//! `<state-dir>/calls/` holds SQLite segment files named by their UTC
//! creation date. The writer writes one, the head: it rolls to a new
//! `<UTC date>.db` between batches when the date changes (story 24-4),
//! carrying this process's still-open calls with it, so a call's start and
//! its close land in one file. Files past the calls retention window are
//! deleted whole, never one holding an open call. The head is
//! ATTACHed to the store writer's connection as `calls`, so a writer batch
//! commits state.db and the calls store together — atomic per file under
//! WAL, not across files, which the open-time closure below accounts for.
//!
//! Tables (the segment's own `user_version` 1):
//!
//! * `calls` — one closing record per call, keyed by its call id.
//! * `open_calls` — a call-start record, deleted by its close; a row left by
//!   a dead owner is closed at the next open (`recovered` or `crash`).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::backend;
use crate::domain::calls::{CallOutcome, CallRecord, CallStart, StatusClass};
use crate::domain::RunId;
use crate::ports::{ProcessFingerprint, StoreError};

/// The calls segment's schema version. Version 2 (the read surface) adds
/// the two indexes the bounded reads need — additive, applied by the
/// proxy-owning engine when it opens the segment it writes; a read-only
/// reader never migrates and still reads version 1 (only more slowly for
/// Run-scoped pages).
pub(crate) const CALLS_SCHEMA_VERSION: i64 = 3;

pub(crate) const CALLS_V1: &str = "\
CREATE TABLE IF NOT EXISTS calls.calls (
    call_id       TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL,
    token_id      TEXT,
    control_depth TEXT NOT NULL,
    binding       TEXT,
    disposition   TEXT NOT NULL,
    hold_end      TEXT,
    outcome       TEXT NOT NULL,
    status        INTEGER,
    status_class  TEXT NOT NULL,
    model         TEXT,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    cached_tokens INTEGER,
    started_at    TEXT NOT NULL,
    ended_at      TEXT NOT NULL,
    latency_us    INTEGER NOT NULL,
    upstream_us   INTEGER
);
CREATE INDEX IF NOT EXISTS calls.idx_calls_run ON calls(run_id, started_at);
CREATE TABLE IF NOT EXISTS calls.open_calls (
    call_id          TEXT PRIMARY KEY,
    run_id           TEXT NOT NULL,
    token_id         TEXT,
    control_depth    TEXT NOT NULL,
    binding          TEXT,
    started_at       TEXT NOT NULL,
    owner_pid        INTEGER,
    owner_start_time INTEGER
);
";

/// The read surface's additive index step (schema 2): the Run-scoped
/// keyset's index and the `problems` view's partial index. The partial
/// index's `WHERE` text is [`CALLS_PROBLEM_WHERE`] verbatim — SQLite uses a
/// partial index only when the statement carries its predicate as written,
/// so the reader's `problems` statement repeats the same constant and the
/// plan pin catches a drift.
const CALLS_V2: &str = "\
CREATE INDEX IF NOT EXISTS calls.idx_calls_run_call ON calls(run_id, call_id);
CREATE INDEX IF NOT EXISTS calls.idx_calls_problem ON calls(call_id) \
WHERE disposition <> 'pass' OR outcome <> 'completed' OR status_class NOT IN ('2xx','3xx');
";

/// The schema 2 DDL as the read tests build it (schema-1 and schema-2
/// seeded segments).
#[cfg(test)]
pub(crate) fn calls_v2_for_test() -> &'static str {
    CALLS_V2
}

/// The segment this open writes: the newest existing `*.db` (segments are
/// named by UTC date, so the lexicographically last; there is only one until
/// rotation exists), else a new one named by today's UTC date.
pub(crate) fn segment_path(calls_dir: &Path) -> Result<PathBuf, StoreError> {
    std::fs::create_dir_all(calls_dir).map_err(|err| {
        StoreError::Backend(format!(
            "could not create the calls directory {}: {err}",
            calls_dir.display()
        ))
    })?;
    let mut segments: Vec<PathBuf> = std::fs::read_dir(calls_dir)
        .map_err(|err| {
            StoreError::Backend(format!(
                "could not read the calls directory {}: {err}",
                calls_dir.display()
            ))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
        .collect();
    segments.sort();
    Ok(match segments.pop() {
        Some(existing) => existing,
        None => {
            let today = crate::time::now_rfc3339();
            let date = today.get(..10).unwrap_or("calls");
            calls_dir.join(format!("{date}.db"))
        }
    })
}

/// ATTACH `segment` (`None` = an in-memory segment, for tests and stores
/// opened without a state directory) as `calls` and create its tables.
/// Must run outside a transaction (before the writer starts).
pub(crate) fn attach(conn: &Connection, segment: Option<&Path>) -> Result<(), StoreError> {
    let target = segment
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| ":memory:".to_string());
    conn.execute("ATTACH DATABASE ?1 AS calls", [target])
        .map_err(backend)?;
    if segment.is_some() {
        // WAL like state.db; a failure (an exotic filesystem) keeps the
        // default journal, which is still correct.
        let _ = conn.query_row("PRAGMA calls.journal_mode = WAL", [], |_| Ok(()));
    }
    let version: i64 = conn
        .query_row("PRAGMA calls.user_version", [], |row| row.get(0))
        .map_err(backend)?;
    if version > CALLS_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: CALLS_SCHEMA_VERSION,
        });
    }
    if version < 1 {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;\n{CALLS_V1}\nPRAGMA calls.user_version = 1;\nCOMMIT;"
        ))
        .map_err(|err| {
            let _ = conn.execute_batch("ROLLBACK");
            backend(err)
        })?;
    }
    if version < 2 {
        // The read surface's step: two additive indexes, stamped in ONE
        // crash-atomic transaction (a crash between the DDL and the stamp
        // re-runs cleanly — every statement is IF NOT EXISTS).
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;\n{CALLS_V2}\nPRAGMA calls.user_version = 2;\nCOMMIT;"
        ))
        .map_err(|err| {
            let _ = conn.execute_batch("ROLLBACK");
            backend(err)
        })?;
    }
    if version < 3 {
        // Story 24-3: when a call parked at a hold, so a crash closes it as
        // a held call (`hold_end: crash`). Additive; the ALTER and the stamp
        // commit together.
        conn.execute_batch(
            "BEGIN IMMEDIATE;\nALTER TABLE calls.open_calls ADD COLUMN held_at TEXT;\n\
             PRAGMA calls.user_version = 3;\nCOMMIT;",
        )
        .map_err(|err| {
            let _ = conn.execute_batch("ROLLBACK");
            backend(err)
        })?;
    }
    Ok(())
}

fn opt_i64(value: Option<u64>) -> Option<i64> {
    value.map(|v| v.min(i64::MAX as u64) as i64)
}

impl super::SqliteStore {
    /// Attach the calls store to this (writer) connection.
    pub(crate) fn attach_calls(&self, segment: Option<&Path>) -> Result<(), StoreError> {
        attach(self.conn(), segment)?;
        let mut head = self.calls_head.lock().unwrap_or_else(|p| p.into_inner());
        *head = CallsHead {
            path: segment.map(Path::to_path_buf),
            retry_at: None,
        };
        Ok(())
    }

    /// Roll the head segment when the UTC date has moved past its name
    /// (story 24-4). Runs between writer batches. This process's open calls
    /// move to the new head (copied, then deleted from the old file, so a
    /// crash in between leaves a copy the next open absorbs); another
    /// owner's stay where its own close will find them. A failure keeps
    /// writing the old head and retries a minute later.
    pub(crate) fn rotate_calls_if_due(&self) {
        let today = crate::time::now_rfc3339();
        let Some(today) = today.get(..10) else {
            return;
        };
        self.rotate_calls_to(today);
    }

    /// [`Self::rotate_calls_if_due`] for a given date (the test seam).
    pub(crate) fn rotate_calls_to(&self, today: &str) -> Option<PathBuf> {
        let mut head = self.calls_head.lock().unwrap_or_else(|p| p.into_inner());
        let current = head.path.clone()?;
        let date = crate::domain::runs::retention::segment_date(&current)?;
        if date.as_str() >= today {
            return None;
        }
        if head
            .retry_at
            .is_some_and(|at| std::time::Instant::now() < at)
        {
            return None;
        }
        let next = current.with_file_name(format!("{today}.db"));
        match self.roll_head(&current, &next) {
            Ok(()) => {
                *head = CallsHead {
                    path: Some(next.clone()),
                    retry_at: None,
                };
                Some(next)
            }
            Err(_) => {
                head.retry_at =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
                None
            }
        }
    }

    fn roll_head(&self, current: &Path, next: &Path) -> Result<(), StoreError> {
        let mine = i64::from(std::process::id());
        let carried = read_open_rows(self.conn(), "calls.open_calls", Some(mine))?;
        self.conn()
            .execute("DETACH DATABASE calls", [])
            .map_err(backend)?;
        if let Err(err) = attach(self.conn(), Some(next)) {
            // Keep writing the old head rather than none.
            let _ = attach(self.conn(), Some(current));
            return Err(err);
        }
        // The new head is attached now, so the roll has happened whatever
        // follows. A carry that fails leaves the rows in the old file, where
        // the next open's absorption finds them once this process is gone.
        if !carried.is_empty()
            && insert_open_rows(self.conn(), "calls.open_calls", &carried).is_ok()
        {
            if let Ok(old) = Connection::open(current) {
                let _ = delete_open_rows(&old, "open_calls", &carried);
            }
        }
        Ok(())
    }

    /// Move the open calls a dead owner left in segments other than the
    /// head into the head (story 24-4), so the open-time closure closes
    /// them. A live owner's rows, and rows whose owner cannot be told, stay
    /// put. Returns how many moved.
    pub(crate) fn absorb_open_calls(
        &self,
        calls_dir: &Path,
        alive: &dyn Fn(&ProcessFingerprint) -> Option<bool>,
    ) -> Result<u64, StoreError> {
        let head = self
            .calls_head
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .path
            .clone();
        let Some(head) = head else {
            return Ok(0);
        };
        let mut moved = 0;
        for segment in segment_files(calls_dir)? {
            if segment == head {
                continue;
            }
            let conn = Connection::open(&segment).map_err(backend)?;
            let has_table: bool = conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'open_calls')",
                    [],
                    |row| row.get(0),
                )
                .map_err(backend)?;
            if !has_table {
                continue;
            }
            let rows: Vec<OpenRow> = read_open_rows(&conn, "open_calls", None)?
                .into_iter()
                .filter(|row| row.owner_dead(alive))
                .collect();
            if rows.is_empty() {
                continue;
            }
            insert_open_rows(self.conn(), "calls.open_calls", &rows)?;
            delete_open_rows(&conn, "open_calls", &rows)?;
            moved += rows.len() as u64;
        }
        Ok(moved)
    }

    /// The head segment's path (`None` for an in-memory segment).
    pub(crate) fn calls_head_path(&self) -> Option<PathBuf> {
        self.calls_head
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .path
            .clone()
    }

    /// Story 24-3: mark an open call as parked at a hold (`at`), so a crash
    /// closes it as a held call.
    pub(crate) fn mark_open_call_held(&self, call_id: &str, at: &str) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "UPDATE calls.open_calls SET held_at = ?2 WHERE call_id = ?1 AND held_at IS NULL",
                [call_id, at],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Insert a call-start record (`open_calls`). A replay (same call id)
    /// is a no-op.
    pub(crate) fn insert_open_call(&self, start: &CallStart) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT OR IGNORE INTO calls.open_calls (call_id, run_id, token_id, \
                 control_depth, binding, started_at, owner_pid, owner_start_time) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    start.call_id,
                    start.run_id.as_str(),
                    start.token_id,
                    start.control_depth,
                    start.binding,
                    start.started_at,
                    start.owner.map(|f| i64::from(f.pid)),
                    start
                        .owner
                        .map(|f| f.start_time.min(i64::MAX as u64) as i64),
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Write a call's closing record and delete its `open_calls` row.
    /// Returns `false` when the record already exists (a replayed close —
    /// idempotent, nothing changes).
    pub(crate) fn insert_call_record(&self, record: &CallRecord) -> Result<bool, StoreError> {
        let inserted = self
            .conn()
            .execute(
                "INSERT OR IGNORE INTO calls.calls (call_id, run_id, token_id, control_depth, \
                 binding, disposition, outcome, status, status_class, model, input_tokens, \
                 output_tokens, cached_tokens, started_at, ended_at, latency_us, upstream_us, \
                 hold_end) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                 ?16, ?17, ?18)",
                rusqlite::params![
                    record.call_id,
                    record.run_id.as_str(),
                    record.token_id,
                    record.control_depth,
                    record.binding,
                    record.disposition.as_str(),
                    record.outcome.as_str(),
                    record.status.map(i64::from),
                    record.status_class.as_str(),
                    record.model,
                    opt_i64(record.input_tokens),
                    opt_i64(record.output_tokens),
                    opt_i64(record.cached_tokens),
                    record.started_at,
                    record.ended_at,
                    record.latency_us.min(i64::MAX as u64) as i64,
                    opt_i64(record.upstream_us),
                    record.hold_end,
                ],
            )
            .map_err(backend)?;
        self.conn()
            .execute(
                "DELETE FROM calls.open_calls WHERE call_id = ?1",
                [&record.call_id],
            )
            .map_err(backend)?;
        Ok(inserted == 1)
    }

    /// Close every `open_calls` row whose owner `alive` reports dead
    /// (`Some(false)`): `recovered` when state.db holds a usage row for its
    /// call id, else `crash` with unknown tokens. A live owner's rows and an
    /// indeterminate probe (`None`) are left alone. Returns (crash,
    /// recovered) counts.
    pub(crate) fn close_orphaned_calls(
        &self,
        alive: &dyn Fn(&ProcessFingerprint) -> Option<bool>,
    ) -> Result<(u64, u64), StoreError> {
        type Row = (
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            Option<i64>,
            Option<i64>,
            Option<String>,
        );
        let rows: Vec<Row> = {
            let mut stmt = self
                .conn()
                .prepare(
                    "SELECT call_id, run_id, token_id, control_depth, binding, started_at, \
                     owner_pid, owner_start_time, held_at FROM calls.open_calls \
                     ORDER BY call_id",
                )
                .map_err(backend)?;
            let mapped = stmt
                .query_map([], |row| {
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
                    ))
                })
                .map_err(backend)?;
            mapped.collect::<Result<_, _>>().map_err(backend)?
        };
        let now = crate::time::now_rfc3339();
        let (mut crashed, mut recovered) = (0, 0);
        for (call_id, run_id, token_id, depth, binding, started_at, pid, start_time, held_at) in
            rows
        {
            let dead = match (pid, start_time) {
                (Some(pid), Some(start_time)) => {
                    let fingerprint = ProcessFingerprint::new(
                        u32::try_from(pid).unwrap_or(u32::MAX),
                        start_time.max(0) as u64,
                    );
                    alive(&fingerprint) == Some(false)
                }
                // No owner recorded (an engine that could not fingerprint
                // itself): only a dead owner may be closed, so leave it.
                _ => false,
            };
            if !dead {
                continue;
            }
            let usage: Option<(i64, i64, Option<i64>)> = self
                .conn()
                .query_row(
                    "SELECT input_tokens, output_tokens, cached_tokens FROM usage_events \
                     WHERE run_id = ?1 AND call_id = ?2",
                    [&run_id, &call_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(backend)?;
            let outcome = if usage.is_some() {
                recovered += 1;
                CallOutcome::Recovered
            } else {
                crashed += 1;
                CallOutcome::Crash
            };
            let record = CallRecord {
                call_id,
                run_id: RunId::from_wire(run_id),
                token_id,
                control_depth: depth_str(&depth).to_string(),
                binding,
                // Story 24-3: a call parked at a hold when its owner died
                // closes as a held call the crash ended.
                disposition: if held_at.is_some() {
                    crate::domain::calls::CallDisposition::Hold
                } else {
                    crate::domain::calls::CallDisposition::Pass
                },
                hold_end: held_at.as_ref().map(|_| "crash".to_string()),
                outcome,
                status: None,
                status_class: StatusClass::TransportError,
                model: None,
                input_tokens: usage.map(|u| u.0.max(0) as u64),
                output_tokens: usage.map(|u| u.1.max(0) as u64),
                cached_tokens: usage.and_then(|u| u.2).map(|c| c.max(0) as u64),
                started_at,
                ended_at: now.clone(),
                latency_us: 0,
                upstream_us: None,
            };
            self.insert_call_record(&record)?;
        }
        Ok((crashed, recovered))
    }

    /// Test reads: (record count, open count) and one record's fields.
    #[cfg(test)]
    pub(crate) fn calls_counts_for_test(&self) -> (i64, i64) {
        let count = |sql: &str| -> i64 {
            self.conn()
                .query_row(sql, [], |row| row.get(0))
                .unwrap_or(-1)
        };
        (
            count("SELECT COUNT(*) FROM calls.calls"),
            count("SELECT COUNT(*) FROM calls.open_calls"),
        )
    }

    /// One call record as (outcome, disposition, status class, input tokens).
    #[cfg(test)]
    pub(crate) fn call_record_for_test(
        &self,
        call_id: &str,
    ) -> Option<(String, String, String, Option<i64>)> {
        self.conn()
            .query_row(
                "SELECT outcome, disposition, status_class, input_tokens FROM calls.calls \
                 WHERE call_id = ?1",
                [call_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .ok()
            .flatten()
    }
}

/// The writer's head segment and when a failed roll may be retried.
#[derive(Debug, Default)]
pub(crate) struct CallsHead {
    pub(crate) path: Option<PathBuf>,
    pub(crate) retry_at: Option<std::time::Instant>,
}

/// One `open_calls` row, as rotation and absorption move it.
#[derive(Clone, Debug)]
struct OpenRow {
    call_id: String,
    run_id: String,
    token_id: Option<String>,
    control_depth: String,
    binding: Option<String>,
    started_at: String,
    owner_pid: Option<i64>,
    owner_start_time: Option<i64>,
}

impl OpenRow {
    fn owner_dead(&self, alive: &dyn Fn(&ProcessFingerprint) -> Option<bool>) -> bool {
        match (self.owner_pid, self.owner_start_time) {
            (Some(pid), Some(start_time)) => {
                let fingerprint = ProcessFingerprint::new(
                    u32::try_from(pid).unwrap_or(u32::MAX),
                    start_time.max(0) as u64,
                );
                alive(&fingerprint) == Some(false)
            }
            _ => false,
        }
    }
}

fn read_open_rows(
    conn: &Connection,
    table: &str,
    owner_pid: Option<i64>,
) -> Result<Vec<OpenRow>, StoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT call_id, run_id, token_id, control_depth, binding, started_at, owner_pid, \
             owner_start_time FROM {table} WHERE ?1 IS NULL OR owner_pid = ?1 ORDER BY call_id"
        ))
        .map_err(backend)?;
    let rows = stmt
        .query_map([owner_pid], |row| {
            Ok(OpenRow {
                call_id: row.get(0)?,
                run_id: row.get(1)?,
                token_id: row.get(2)?,
                control_depth: row.get(3)?,
                binding: row.get(4)?,
                started_at: row.get(5)?,
                owner_pid: row.get(6)?,
                owner_start_time: row.get(7)?,
            })
        })
        .map_err(backend)?;
    rows.collect::<Result<_, _>>().map_err(backend)
}

fn insert_open_rows(conn: &Connection, table: &str, rows: &[OpenRow]) -> Result<(), StoreError> {
    for row in rows {
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO {table} (call_id, run_id, token_id, control_depth, \
                 binding, started_at, owner_pid, owner_start_time) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            rusqlite::params![
                row.call_id,
                row.run_id,
                row.token_id,
                row.control_depth,
                row.binding,
                row.started_at,
                row.owner_pid,
                row.owner_start_time
            ],
        )
        .map_err(backend)?;
    }
    Ok(())
}

fn delete_open_rows(conn: &Connection, table: &str, rows: &[OpenRow]) -> Result<(), StoreError> {
    for row in rows {
        conn.execute(
            &format!("DELETE FROM {table} WHERE call_id = ?1"),
            [&row.call_id],
        )
        .map_err(backend)?;
    }
    Ok(())
}

/// The `calls/*.db` files, oldest name first.
fn segment_files(calls_dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let entries = match std::fs::read_dir(calls_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(StoreError::Backend(format!(
                "could not read the calls directory {}: {err}",
                calls_dir.display()
            )))
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Delete whole every calls file dated before `cutoff_date` (`YYYY-MM-DD`)
/// that is not `head` and holds no open call (story 24-4): checkpoint it,
/// then remove the `.db` with its `-wal` and `-shm`. A file that cannot be
/// opened or removed now is left for the next pass. Returns how many went.
pub(crate) fn drop_expired_segments(
    calls_dir: &Path,
    head: Option<&Path>,
    cutoff_date: &str,
) -> u64 {
    let Ok(segments) = segment_files(calls_dir) else {
        return 0;
    };
    let mut dropped = 0;
    for segment in segments {
        if head.is_some_and(|head| head == segment) {
            continue;
        }
        let Some(date) = crate::domain::runs::retention::segment_date(&segment) else {
            continue;
        };
        if date.as_str() >= cutoff_date {
            continue;
        }
        let Ok(conn) = Connection::open(&segment) else {
            continue;
        };
        // A file with no `open_calls` table (never written) holds none; the
        // count is read only when the table exists (SQLite resolves a table
        // name when the statement is prepared, whichever branch runs).
        let has_table = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'open_calls')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(true);
        let open: i64 = if has_table {
            conn.query_row("SELECT COUNT(*) FROM open_calls", [], |row| row.get(0))
                .unwrap_or(-1)
        } else {
            0
        };
        if open != 0 {
            continue;
        }
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        drop(conn);
        let removed = std::fs::remove_file(&segment);
        if removed.is_err() && segment.exists() {
            continue;
        }
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = segment.clone().into_os_string();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
        dropped += 1;
    }
    dropped
}

/// The stored depth string back as the static name a record carries.
fn depth_str(raw: &str) -> &'static str {
    match raw {
        "supervised" => "supervised",
        "announced" => "announced",
        "wire" => "wire",
        _ => "observed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::calls::CallDisposition;
    use crate::store::SqliteStore;

    fn store() -> (tempfile::TempDir, SqliteStore) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
        let segment = segment_path(&tmp.path().join("calls")).unwrap();
        store.attach_calls(Some(&segment)).unwrap();
        (tmp, store)
    }

    fn start(call: &str, owner: Option<ProcessFingerprint>) -> CallStart {
        CallStart {
            call_id: call.to_string(),
            run_id: RunId::from_wire("run_A"),
            token_id: None,
            control_depth: "wire",
            binding: Some("openai".to_string()),
            started_at: "2026-11-12T00:00:00.000001Z".to_string(),
            owner,
        }
    }

    fn record(call: &str) -> CallRecord {
        CallRecord {
            call_id: call.to_string(),
            run_id: RunId::from_wire("run_A"),
            token_id: Some("abcdef012345".to_string()),
            control_depth: "wire".to_string(),
            binding: None,
            disposition: CallDisposition::Pass,
            hold_end: None,
            outcome: CallOutcome::Completed,
            status: Some(200),
            status_class: StatusClass::Success,
            model: Some("gpt-4o".to_string()),
            input_tokens: Some(3),
            output_tokens: Some(4),
            cached_tokens: None,
            started_at: "a".to_string(),
            ended_at: "b".to_string(),
            latency_us: 10,
            upstream_us: Some(5),
        }
    }

    #[test]
    fn a_close_replaces_its_start_once_and_replays_are_no_ops() {
        let (_tmp, store) = store();
        store.insert_open_call(&start("call_1", None)).unwrap();
        store.insert_open_call(&start("call_1", None)).unwrap();
        assert_eq!(store.calls_counts_for_test(), (0, 1));
        assert!(store.insert_call_record(&record("call_1")).unwrap());
        assert!(!store.insert_call_record(&record("call_1")).unwrap());
        assert_eq!(store.calls_counts_for_test(), (1, 0));
        assert_eq!(
            store.call_record_for_test("call_1"),
            Some((
                "completed".to_string(),
                "pass".to_string(),
                "2xx".to_string(),
                Some(3)
            ))
        );
    }

    #[test]
    fn orphans_of_a_dead_owner_close_and_a_live_or_unknown_owner_is_left_alone() {
        let (_tmp, store) = store();
        let dead = ProcessFingerprint::new(1, 1);
        let live = ProcessFingerprint::new(2, 2);
        let unknown = ProcessFingerprint::new(3, 3);
        store
            .insert_open_call(&start("call_crash", Some(dead)))
            .unwrap();
        store
            .insert_open_call(&start("call_rec", Some(dead)))
            .unwrap();
        store
            .insert_open_call(&start("call_live", Some(live)))
            .unwrap();
        store
            .insert_open_call(&start("call_unknown", Some(unknown)))
            .unwrap();
        store
            .insert_open_call(&start("call_noowner", None))
            .unwrap();
        store
            .conn()
            .execute_batch(
                "INSERT INTO usage_events (instance_id, run_id, input_tokens, output_tokens, \
                 metering_source, occurred_at, call_id) VALUES (NULL, 'run_A', 7, 8, \
                 'engine-observed', 't', 'call_rec')",
            )
            .unwrap();
        let probe = |f: &ProcessFingerprint| match f.pid {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        assert_eq!(store.close_orphaned_calls(&probe).unwrap(), (1, 1));
        assert_eq!(store.calls_counts_for_test(), (2, 3));
        assert_eq!(store.call_record_for_test("call_crash").unwrap().0, "crash");
        assert_eq!(
            store.call_record_for_test("call_rec"),
            Some((
                "recovered".to_string(),
                "pass".to_string(),
                "transport-error".to_string(),
                Some(7)
            ))
        );
        // A second pass finds nothing more to close.
        assert_eq!(store.close_orphaned_calls(&probe).unwrap(), (0, 0));
    }

    #[test]
    fn a_failed_held_at_step_rolls_back_and_leaves_the_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let segment = tmp.path().join("2026-11-03.db");
        {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "ATTACH DATABASE '{}' AS calls;\nBEGIN IMMEDIATE;\n{CALLS_V1}\n{CALLS_V2}\n\
                 ALTER TABLE calls.open_calls ADD COLUMN held_at TEXT;\n\
                 PRAGMA calls.user_version = 2;\nCOMMIT;",
                segment.to_string_lossy()
            ))
            .unwrap();
        }
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            attach(&conn, Some(&segment)).is_err(),
            "the duplicate column fails the step"
        );
        let version: i64 = Connection::open(&segment)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2, "rolled back: the stamp did not land");
    }

    #[test]
    fn a_call_parked_at_a_hold_when_its_owner_died_closes_as_a_held_crash() {
        let (_tmp, store) = store();
        let dead = ProcessFingerprint::new(1, 1);
        store
            .insert_open_call(&start("call_parked", Some(dead)))
            .unwrap();
        store.mark_open_call_held("call_parked", "t1").unwrap();
        // A second mark keeps the first time.
        store.mark_open_call_held("call_parked", "t2").unwrap();
        let held_at: Option<String> = store
            .conn()
            .query_row(
                "SELECT held_at FROM calls.open_calls WHERE call_id = 'call_parked'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(held_at.as_deref(), Some("t1"));
        let probe = |_: &ProcessFingerprint| Some(false);
        assert_eq!(store.close_orphaned_calls(&probe).unwrap(), (1, 0));
        let (outcome, disposition, _, _) = store.call_record_for_test("call_parked").unwrap();
        assert_eq!((outcome.as_str(), disposition.as_str()), ("crash", "hold"));
        let hold_end: Option<String> = store
            .conn()
            .query_row(
                "SELECT hold_end FROM calls.calls WHERE call_id = 'call_parked'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hold_end.as_deref(), Some("crash"));
    }

    #[test]
    fn the_segment_is_reused_and_a_newer_segment_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        let first = segment_path(&dir).unwrap();
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".db"));
        std::fs::write(&first, b"").unwrap();
        assert_eq!(segment_path(&dir).unwrap(), first);
        let conn = Connection::open_in_memory().unwrap();
        attach(&conn, Some(&first)).unwrap();
        conn.execute_batch("PRAGMA calls.user_version = 4").unwrap();
        conn.execute_batch("DETACH DATABASE calls").unwrap();
        let again = Connection::open_in_memory().unwrap();
        assert!(matches!(
            attach(&again, Some(&first)),
            Err(StoreError::SchemaTooNew { found: 4, .. })
        ));
        // An in-memory segment works for stores without a state directory.
        let memory = Connection::open_in_memory().unwrap();
        attach(&memory, None).unwrap();
    }

    #[test]
    fn a_schema_1_segment_migrates_to_2_in_one_step_and_a_crash_re_runs_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let segment = tmp.path().join("calls").join("2026-11-01.db");
        std::fs::create_dir_all(segment.parent().unwrap()).unwrap();
        // Build a schema 1 segment: v1 tables, stamped 1 (the pre-read
        // surface writer's shape).
        {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "ATTACH DATABASE '{}' AS calls;",
                segment.to_string_lossy()
            ))
            .unwrap();
            conn.execute_batch(&format!(
                "BEGIN IMMEDIATE;\n{CALLS_V1}\nPRAGMA calls.user_version = 1;\nCOMMIT;"
            ))
            .unwrap();
        }
        // The proxy-owning engine's attach migrates it: both indexes exist,
        // open calls can be marked held (story 24-3), and the stamp reads
        // the current version.
        {
            let conn = Connection::open_in_memory().unwrap();
            attach(&conn, Some(&segment)).unwrap();
        }
        {
            let conn = Connection::open(&segment).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, CALLS_SCHEMA_VERSION);
            let held: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('open_calls') WHERE name = 'held_at'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(held, 1, "the v3 step adds held_at");
            let indexes: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                indexes.contains(&"idx_calls_run_call".to_string()),
                "{indexes:?}"
            );
            assert!(
                indexes.contains(&"idx_calls_problem".to_string()),
                "{indexes:?}"
            );
            // The v1 index is untouched.
            assert!(
                indexes.contains(&"idx_calls_run".to_string()),
                "{indexes:?}"
            );
        }
        // A crash between the DDL and the stamp re-runs cleanly: apply the
        // DDL without the stamp (the torn state), then a full attach again.
        let torn = tmp.path().join("calls").join("2026-11-02.db");
        {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "ATTACH DATABASE '{}' AS calls;\nBEGIN IMMEDIATE;\n{CALLS_V1}\nCOMMIT;\n\
                 BEGIN IMMEDIATE;\n{CALLS_V2}\nCOMMIT;",
                torn.to_string_lossy()
            ))
            .unwrap();
        }
        {
            let conn = Connection::open_in_memory().unwrap();
            attach(&conn, Some(&torn)).unwrap();
        }
        let conn = Connection::open(&torn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, CALLS_SCHEMA_VERSION,
            "the stamp lands after the clean re-run"
        );
    }

    #[test]
    fn depth_strings_round_trip() {
        for depth in ["supervised", "announced", "wire", "observed"] {
            assert_eq!(depth_str(depth), depth);
        }
        assert_eq!(depth_str("bogus"), "observed");
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;
    use crate::domain::calls::CallStart;
    use crate::store::SqliteStore;

    fn start(call_id: &str, pid: u32) -> CallStart {
        CallStart {
            call_id: call_id.to_string(),
            run_id: RunId::from_wire("run_R".to_string()),
            token_id: None,
            control_depth: "announced",
            binding: None,
            started_at: "2026-11-13T23:59:59Z".to_string(),
            owner: Some(ProcessFingerprint::new(pid, 42)),
        }
    }

    fn open_ids(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT call_id FROM open_calls ORDER BY call_id")
            .unwrap();
        let ids = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        ids
    }

    #[test]
    fn the_head_rolls_on_the_date_and_carries_only_this_processes_open_calls() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        std::fs::create_dir_all(&calls).unwrap();
        let store = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        let old = calls.join("2026-11-13.db");
        store.attach_calls(Some(&old)).unwrap();
        store
            .insert_open_call(&start("call_MINE", std::process::id()))
            .unwrap();
        store.insert_open_call(&start("call_THEIRS", 1)).unwrap();
        assert_eq!(store.rotate_calls_to("2026-11-13"), None, "not due");
        let new = store.rotate_calls_to("2026-11-14").expect("rolled");
        assert_eq!(new, calls.join("2026-11-14.db"));
        assert_eq!(store.calls_head_path(), Some(new.clone()));
        assert_eq!(open_ids(&new), vec!["call_MINE".to_string()]);
        assert_eq!(open_ids(&old), vec!["call_THEIRS".to_string()]);
        assert_eq!(store.rotate_calls_to("2026-11-14"), None);
        // An in-memory head never rolls.
        let memory = SqliteStore::open(&dir.path().join("other.db")).unwrap();
        memory.attach_calls(None).unwrap();
        assert_eq!(memory.rotate_calls_to("2099-01-01"), None);
        assert_eq!(memory.calls_head_path(), None);
    }

    #[test]
    fn a_failed_roll_keeps_the_old_head_and_waits_before_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        std::fs::create_dir_all(&calls).unwrap();
        let store = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        let old = calls.join("2026-11-13.db");
        store.attach_calls(Some(&old)).unwrap();
        // A directory where the new file would go: the attach fails.
        std::fs::create_dir_all(calls.join("2026-11-14.db")).unwrap();
        assert_eq!(store.rotate_calls_to("2026-11-14"), None);
        assert_eq!(store.calls_head_path(), Some(old.clone()));
        store.insert_open_call(&start("call_AFTER", 7)).unwrap();
        assert_eq!(open_ids(&old), vec!["call_AFTER".to_string()]);
        // Inside the retry wait nothing is tried.
        std::fs::remove_dir(calls.join("2026-11-14.db")).unwrap();
        assert_eq!(store.rotate_calls_to("2026-11-14"), None);
    }

    #[test]
    fn a_dead_owners_open_calls_in_older_files_move_to_the_head_and_a_live_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        std::fs::create_dir_all(&calls).unwrap();
        let older = calls.join("2026-11-12.db");
        {
            let seed = SqliteStore::open(&dir.path().join("seed.db")).unwrap();
            seed.attach_calls(Some(&older)).unwrap();
            seed.insert_open_call(&start("call_DEAD", 111)).unwrap();
            seed.insert_open_call(&start("call_LIVE", 222)).unwrap();
        }
        std::fs::write(calls.join("notes.txt"), "not a segment").unwrap();
        let store = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        let head = calls.join("2026-11-14.db");
        store.attach_calls(Some(&head)).unwrap();
        let alive = |f: &ProcessFingerprint| Some(f.pid == 222);
        assert_eq!(store.absorb_open_calls(&calls, &alive).unwrap(), 1);
        assert_eq!(open_ids(&head), vec!["call_DEAD".to_string()]);
        assert_eq!(open_ids(&older), vec!["call_LIVE".to_string()]);
        assert_eq!(store.absorb_open_calls(&calls, &alive).unwrap(), 0);
        // With no head there is nothing to absorb into.
        let memory = SqliteStore::open(&dir.path().join("memory.db")).unwrap();
        memory.attach_calls(None).unwrap();
        assert_eq!(memory.absorb_open_calls(&calls, &alive).unwrap(), 0);
        assert_eq!(
            store
                .absorb_open_calls(&dir.path().join("missing"), &alive)
                .unwrap(),
            0
        );
    }

    #[test]
    fn files_past_the_window_go_whole_unless_they_hold_an_open_call_or_are_the_head() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        std::fs::create_dir_all(&calls).unwrap();
        let make = |name: &str, open: bool| {
            let path = calls.join(name);
            let seed = SqliteStore::open(&dir.path().join(format!("{name}.state"))).unwrap();
            seed.attach_calls(Some(&path)).unwrap();
            if open {
                seed.insert_open_call(&start("call_OPEN", 1)).unwrap();
            }
            path
        };
        let expired = make("2026-11-01.db", false);
        // A file never written (no tables at all) goes too.
        drop(Connection::open(calls.join("2026-11-03.db")).unwrap());
        let busy = make("2026-11-02.db", true);
        let recent = make("2026-11-10.db", false);
        let head = make("2026-10-01.db", false);
        std::fs::write(calls.join("2026-11-01.db-wal"), b"").unwrap();
        std::fs::write(calls.join("notes.db"), b"").unwrap();
        let dropped = drop_expired_segments(&calls, Some(&head), "2026-11-07");
        assert_eq!(dropped, 2);
        assert!(!calls.join("2026-11-03.db").exists());
        assert!(!expired.exists());
        assert!(!calls.join("2026-11-01.db-wal").exists());
        assert!(busy.exists() && recent.exists() && head.exists());
        assert!(calls.join("notes.db").exists(), "not a dated file");
        assert_eq!(drop_expired_segments(&calls, Some(&head), "2026-11-07"), 0);
        assert_eq!(
            drop_expired_segments(&dir.path().join("missing"), None, "2026-11-07"),
            0
        );
    }
}
