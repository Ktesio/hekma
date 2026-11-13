//! The per-call records store (story 23-1, spine AD-26 / AD-27).
//!
//! `<state-dir>/calls/` holds SQLite segment files named by their UTC
//! creation date. This story creates the first and writes only it (rotation,
//! retention and drop-whole come later, on the same naming). The segment is
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

/// The calls segment's schema version.
pub(crate) const CALLS_SCHEMA_VERSION: i64 = 1;

const CALLS_V1: &str = "\
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

/// The segment this open writes: the existing one (the lexicographically
/// first `*.db` — there is only one until rotation exists), else a new one
/// named by today's UTC date.
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
    Ok(match segments.into_iter().next() {
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
    if version < CALLS_SCHEMA_VERSION {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;\n{CALLS_V1}\nPRAGMA calls.user_version = {CALLS_SCHEMA_VERSION};\n\
             COMMIT;"
        ))
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
        attach(self.conn(), segment)
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
                 output_tokens, cached_tokens, started_at, ended_at, latency_us, upstream_us) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                 ?16, ?17)",
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
        );
        let rows: Vec<Row> = {
            let mut stmt = self
                .conn()
                .prepare(
                    "SELECT call_id, run_id, token_id, control_depth, binding, started_at, \
                     owner_pid, owner_start_time FROM calls.open_calls ORDER BY call_id",
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
                    ))
                })
                .map_err(backend)?;
            mapped.collect::<Result<_, _>>().map_err(backend)?
        };
        let now = crate::time::now_rfc3339();
        let (mut crashed, mut recovered) = (0, 0);
        for (call_id, run_id, token_id, depth, binding, started_at, pid, start_time) in rows {
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
                control_depth: depth_str(&depth),
                binding,
                disposition: crate::domain::calls::CallDisposition::Pass,
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
            control_depth: "wire",
            binding: None,
            disposition: CallDisposition::Pass,
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
        conn.execute_batch("PRAGMA calls.user_version = 2").unwrap();
        conn.execute_batch("DETACH DATABASE calls").unwrap();
        let again = Connection::open_in_memory().unwrap();
        assert!(matches!(
            attach(&again, Some(&first)),
            Err(StoreError::SchemaTooNew {
                found: 2,
                supported: 1
            })
        ));
        // An in-memory segment works for stores without a state directory.
        let memory = Connection::open_in_memory().unwrap();
        attach(&memory, None).unwrap();
    }

    #[test]
    fn depth_strings_round_trip() {
        for depth in ["supervised", "announced", "wire", "observed"] {
            assert_eq!(depth_str(depth), depth);
        }
        assert_eq!(depth_str("bogus"), "observed");
    }
}
