//! Retention settings and ended-Run compaction (story 24-4, spine AD-27).
//!
//! Schema v24 adds three tables: `retention` (one row, the two windows),
//! `compaction_state` (one row, what the passes have done) and
//! `compacted_runs` (what a compacted Run keeps). A compaction job reduces
//! a bounded batch of ended Runs inside one writer transaction: the totals
//! move from the Run's own rollup row (which already includes its
//! descendants), then the Run's detail rows go — `runs` (cascading its
//! labels, limits, identities, latches, Policy resolution and token rows),
//! `run_dispositions`, `telemetry_counters` and `usage_rollup_run`. Its
//! `usage_events`, breach records, events and outbox rows stay.

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::backend;
use crate::domain::runs::retention::{
    CompactedRun, CompactionState, RetentionChange, RetentionSettings,
};
use crate::domain::RunId;
use crate::ports::{RunRecords, StoreError};

/// Schema v24: the retention row, the compaction state and the compacted
/// Runs. Additive; no backfill (no row means the defaults).
pub(super) const SCHEMA_V24: &str = "\
CREATE TABLE IF NOT EXISTS retention (
    id                   INTEGER PRIMARY KEY CHECK (id = 1),
    run_retention_days   INTEGER NOT NULL DEFAULT 7,
    calls_retention_days INTEGER NOT NULL DEFAULT 7,
    updated_at           TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS compaction_state (
    id               INTEGER PRIMARY KEY CHECK (id = 1),
    last_pass_at     TEXT,
    runs_compacted   INTEGER NOT NULL DEFAULT 0,
    segments_dropped INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS compacted_runs (
    run_id         TEXT PRIMARY KEY,
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cached_tokens  INTEGER NOT NULL,
    cost_micros    INTEGER NOT NULL,
    labels         TEXT NOT NULL,
    end_cause      TEXT NOT NULL,
    control_depth  TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    ended_at       TEXT NOT NULL,
    compacted_at   TEXT NOT NULL
);
";

/// Schema v25 (story 24-4 phase 3): the healthy-count covering index (its
/// predicate is [`super::runs_sqlite::HEALTHY_PREDICATE`], verbatim), and
/// `run_identities(run_id)`, so a Run delete's cascade reads an index
/// instead of the whole table.
pub(super) const SCHEMA_V25: &str = "\
CREATE INDEX IF NOT EXISTS idx_runs_healthy ON runs(run_id) \
WHERE status = 'active' AND anomalies = '';
CREATE INDEX IF NOT EXISTS idx_run_identities_run ON run_identities(run_id);
";

/// Undo v25 on a fully migrated store, for the older upgrade tests.
#[cfg(test)]
pub(crate) const UNDO_V25_FOR_TEST: &str =
    "DROP INDEX idx_runs_healthy; DROP INDEX idx_run_identities_run;";

/// Run the v25 step.
pub(super) fn migrate_v25(conn: &Connection) -> Result<(), StoreError> {
    super::runs_sqlite::migrate_runs_step(conn, 25, SCHEMA_V25)
}

/// Undo v24 on a fully migrated store, for the older upgrade tests.
#[cfg(test)]
pub(crate) const UNDO_V24_FOR_TEST: &str = "\
DROP TABLE retention;
DROP TABLE compaction_state;
DROP TABLE compacted_runs;
";

/// Run the v24 step.
pub(super) fn migrate_v24(conn: &Connection) -> Result<(), StoreError> {
    super::runs_sqlite::migrate_runs_step(conn, 24, SCHEMA_V24)
}

/// The candidates a compaction job reads: ended before the cutoff, in
/// ended order after the job's key, riding `idx_runs_ended_at` (its
/// predicate repeated literally).
const CANDIDATES_SQL: &str = "SELECT run_id, ended_at FROM runs \
     WHERE status = 'ended' AND ended_at < ?1 AND (ended_at > ?2 OR (ended_at = ?2 AND run_id > ?3)) \
     ORDER BY ended_at, run_id LIMIT ?4";

/// Whether any descendant of `?1` is still open. The walk rides
/// `idx_runs_parent` and carries each row's status, so it never joins back
/// to `runs` (a join let the planner scan every open Run per candidate:
/// about 2 ms each at 100,000 Runs, found by the Run-scale ladder).
const OPEN_DESCENDANT_SQL: &str = "WITH RECURSIVE d(id, status) AS ( \
         SELECT run_id, status FROM runs WHERE parent_run_id = ?1 \
         UNION ALL SELECT r.run_id, r.status FROM runs r JOIN d ON r.parent_run_id = d.id) \
     SELECT EXISTS (SELECT 1 FROM d WHERE status <> 'ended')";

/// Where a compaction pass has reached: the last candidate it read, so a
/// Run that stays (an open descendant) never starves the next job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompactionKey {
    pub ended_at: String,
    pub run_id: String,
}

/// One compaction job's result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompactionStep {
    /// Runs compacted by this job.
    pub compacted: u64,
    /// Where the next job starts; `None` when no candidate is left.
    pub next: Option<CompactionKey>,
}

fn days(raw: i64) -> u32 {
    u32::try_from(raw.max(0)).unwrap_or(u32::MAX)
}

impl super::SqliteStore {
    /// The two windows (the defaults when never set).
    pub(crate) fn retention_settings(&self) -> Result<RetentionSettings, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT run_retention_days, calls_retention_days, updated_at \
                 FROM retention WHERE id = 1",
                [],
                |row| {
                    Ok(RetentionSettings {
                        run_retention_days: days(row.get(0)?),
                        calls_retention_days: days(row.get(1)?),
                        updated_at: Some(row.get(2)?),
                    })
                },
            )
            .optional()
            .map_err(backend)?;
        Ok(row.unwrap_or_default())
    }

    /// Apply a validated change and return the new settings.
    pub(crate) fn set_retention(
        &self,
        change: &RetentionChange,
        at: &str,
    ) -> Result<RetentionSettings, StoreError> {
        let current = self.retention_settings()?;
        let next = RetentionSettings {
            run_retention_days: change
                .run_retention_days
                .unwrap_or(current.run_retention_days),
            calls_retention_days: change
                .calls_retention_days
                .unwrap_or(current.calls_retention_days),
            updated_at: Some(at.to_string()),
        };
        self.conn()
            .execute(
                "INSERT INTO retention (id, run_retention_days, calls_retention_days, updated_at) \
                 VALUES (1, ?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET \
                 run_retention_days = excluded.run_retention_days, \
                 calls_retention_days = excluded.calls_retention_days, \
                 updated_at = excluded.updated_at",
                rusqlite::params![
                    i64::from(next.run_retention_days),
                    i64::from(next.calls_retention_days),
                    at
                ],
            )
            .map_err(backend)?;
        Ok(next)
    }

    /// What the compaction passes have done.
    pub(crate) fn compaction_state(&self) -> Result<CompactionState, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT last_pass_at, runs_compacted, segments_dropped \
                 FROM compaction_state WHERE id = 1",
                [],
                |row| {
                    Ok(CompactionState {
                        last_pass_at: row.get(0)?,
                        runs_compacted: row.get::<_, i64>(1)?.max(0) as u64,
                        segments_dropped: row.get::<_, i64>(2)?.max(0) as u64,
                    })
                },
            )
            .optional()
            .map_err(backend)?;
        Ok(row.unwrap_or_default())
    }

    /// Add a pass's figures to the compaction state.
    pub(crate) fn note_compaction(
        &self,
        runs: u64,
        segments: u64,
        at: &str,
    ) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO compaction_state (id, last_pass_at, runs_compacted, segments_dropped) \
                 VALUES (1, ?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET \
                 last_pass_at = excluded.last_pass_at, \
                 runs_compacted = compaction_state.runs_compacted + excluded.runs_compacted, \
                 segments_dropped = compaction_state.segments_dropped + excluded.segments_dropped",
                rusqlite::params![
                    at,
                    runs.min(i64::MAX as u64) as i64,
                    segments.min(i64::MAX as u64) as i64
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// A compacted Run's summary, when `run_id` was compacted.
    pub(crate) fn compacted_run(&self, run_id: &str) -> Result<Option<CompactedRun>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT run_id, input_tokens, output_tokens, cached_tokens, cost_micros, labels, \
                 end_cause, control_depth, created_at, ended_at, compacted_at \
                 FROM compacted_runs WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(backend)?;
        let Some((id, input, output, cached, cost, labels, cause, depth, created, ended, at)) = row
        else {
            return Ok(None);
        };
        let labels = serde_json::from_str(&labels).map_err(|err| {
            StoreError::Backend(format!(
                "the compacted Run {id} has unreadable labels ({err})"
            ))
        })?;
        Ok(Some(CompactedRun {
            run_id: id,
            input_tokens: input.max(0) as u64,
            output_tokens: output.max(0) as u64,
            cached_tokens: cached.max(0) as u64,
            cost_micros: cost,
            labels,
            end_cause: cause,
            control_depth: depth,
            created_at: created,
            ended_at: ended,
            compacted_at: at,
        }))
    }

    /// One compaction job: read candidates after `from` (ended before
    /// `cutoff`), skip any with an open descendant or that is the
    /// unattributed Run, and compact up to `limit` of them. Scans at most
    /// `4 × limit` candidates, so a long run of blocked Runs cannot make a
    /// job unbounded; the returned key resumes the pass.
    pub(crate) fn compact_ended_runs(
        &self,
        cutoff: &str,
        from: &CompactionKey,
        limit: usize,
        now: &str,
    ) -> Result<CompactionStep, StoreError> {
        let scan = limit.saturating_mul(4).max(1);
        let candidates: Vec<(String, String)> = {
            let mut stmt = self.conn().prepare(CANDIDATES_SQL).map_err(backend)?;
            let rows = stmt
                .query_map(
                    rusqlite::params![cutoff, from.ended_at, from.run_id, scan as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(backend)?;
            rows.collect::<Result<_, _>>().map_err(backend)?
        };
        let legacy = self.legacy_run()?;
        let mut step = CompactionStep::default();
        let full = candidates.len() == scan;
        for (run_id, ended_at) in candidates {
            step.next = Some(CompactionKey {
                ended_at: ended_at.clone(),
                run_id: run_id.clone(),
            });
            if legacy.as_ref().is_some_and(|l| l.as_str() == run_id) {
                continue;
            }
            let open_descendant: bool = self
                .conn()
                .query_row(OPEN_DESCENDANT_SQL, [&run_id], |row| row.get(0))
                .map_err(backend)?;
            if open_descendant {
                continue;
            }
            self.compact_one(&run_id, now)?;
            step.compacted += 1;
            if step.compacted as usize >= limit {
                return Ok(step);
            }
        }
        if !full {
            step.next = None;
        }
        Ok(step)
    }

    /// Reduce one ended Run to its compacted row.
    fn compact_one(&self, run_id: &str, now: &str) -> Result<(), StoreError> {
        let id = RunId::from_wire(run_id.to_string());
        let labels = serde_json::to_string(&self.run_labels(&id)?).map_err(|err| {
            StoreError::Backend(format!("could not encode the labels of {run_id} ({err})"))
        })?;
        let (depth, cause, created, ended): (String, Option<String>, String, Option<String>) = self
            .conn()
            .query_row(
                "SELECT control_depth, end_cause, created_at, ended_at FROM runs \
                     WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(backend)?;
        let totals: (i64, i64, i64, i64) = self
            .conn()
            .query_row(
                "SELECT input_tokens, output_tokens, cached_tokens, cost_micros \
                 FROM usage_rollup_run WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(backend)?
            .unwrap_or_default();
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO compacted_runs (run_id, input_tokens, output_tokens, \
                 cached_tokens, cost_micros, labels, end_cause, control_depth, created_at, \
                 ended_at, compacted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    run_id,
                    totals.0,
                    totals.1,
                    totals.2,
                    totals.3,
                    labels,
                    cause.unwrap_or_default(),
                    depth,
                    created,
                    ended.unwrap_or_default(),
                    now
                ],
            )
            .map_err(backend)?;
        for table in ["run_dispositions", "telemetry_counters", "usage_rollup_run"] {
            self.conn()
                .execute(&format!("DELETE FROM {table} WHERE run_id = ?1"), [run_id])
                .map_err(backend)?;
        }
        self.conn()
            .execute("DELETE FROM runs WHERE run_id = ?1", [run_id])
            .map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    const NOW: &str = "2026-11-14T00:00:00Z";
    const CUTOFF: &str = "2026-11-07T00:00:00Z";

    fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    /// A Run row: `ended_at` `None` keeps it open.
    fn run(store: &SqliteStore, id: &str, parent: Option<&str>, ended_at: Option<&str>) {
        let status = if ended_at.is_some() {
            "ended"
        } else {
            "active"
        };
        store
            .conn()
            .execute(
                "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
                 parent_run_id, process_owner, end_cause, created_at, updated_at, ended_at) \
                 VALUES (?1, 'announced', 0, ?2, '', ?3, 0, ?4, '2026-10-01T00:00:00Z', \
                 '2026-10-01T00:00:00Z', ?5)",
                rusqlite::params![id, status, parent, ended_at.map(|_| "closed"), ended_at],
            )
            .unwrap();
    }

    /// The detail rows compaction deletes, and a rollup.
    fn detail(store: &SqliteStore, id: &str, input: i64) {
        store
            .conn()
            .execute_batch(&format!(
                "INSERT INTO run_labels (run_id, key, value, source) VALUES ('{id}', 'team', 'red', 'caller');
                 INSERT INTO run_limits (run_id, dimension, scope, limit_value, action, source, set_at)
                     VALUES ('{id}', 'tokens', 'per-run', 100, 'warn', 'explicit', 't');
                 INSERT INTO run_latches (run_id, dimension, scope, action, latched_at)
                     VALUES ('{id}', 'tokens', 'per-run', 'warn', 't');
                 INSERT INTO run_dispositions (run_id, reason, set_by, set_at)
                     VALUES ('{id}', 'hold', 'operator', 't');
                 INSERT INTO telemetry_counters (run_id, token_type, last_value, updated_at)
                     VALUES ('{id}', 'input', 5, 't');
                 INSERT INTO usage_rollup_run (run_id, events, input_tokens, output_tokens,
                     cached_tokens, cached_unknown, cost_micros)
                     VALUES ('{id}', 2, {input}, 20, 3, 0, 1500);"
            ))
            .unwrap();
    }

    fn count(store: &SqliteStore, sql: &str) -> i64 {
        store.conn().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn compact_all(store: &SqliteStore, limit: usize) -> u64 {
        let mut key = CompactionKey::default();
        let mut total = 0;
        loop {
            let step = store.compact_ended_runs(CUTOFF, &key, limit, NOW).unwrap();
            total += step.compacted;
            match step.next {
                Some(next) => key = next,
                None => return total,
            }
        }
    }

    #[test]
    fn the_settings_default_to_seven_days_and_change_one_window_at_a_time() {
        let (_dir, store) = store();
        assert_eq!(
            store.retention_settings().unwrap(),
            RetentionSettings::default()
        );
        let set = store
            .set_retention(
                &RetentionChange {
                    run_retention_days: Some(30),
                    calls_retention_days: None,
                },
                NOW,
            )
            .unwrap();
        assert_eq!((set.run_retention_days, set.calls_retention_days), (30, 7));
        let set = store
            .set_retention(
                &RetentionChange {
                    run_retention_days: None,
                    calls_retention_days: Some(2),
                },
                NOW,
            )
            .unwrap();
        assert_eq!((set.run_retention_days, set.calls_retention_days), (30, 2));
        assert_eq!(store.retention_settings().unwrap(), set);
        assert_eq!(
            store.compaction_state().unwrap(),
            CompactionState::default()
        );
        store.note_compaction(3, 1, NOW).unwrap();
        store.note_compaction(2, 0, "2026-11-15T00:00:00Z").unwrap();
        let state = store.compaction_state().unwrap();
        assert_eq!((state.runs_compacted, state.segments_dropped), (5, 1));
        assert_eq!(state.last_pass_at.as_deref(), Some("2026-11-15T00:00:00Z"));
    }

    #[test]
    fn an_ended_run_past_the_window_keeps_its_totals_labels_and_cause_and_loses_its_detail() {
        let (_dir, store) = store();
        run(&store, "run_PARENT", None, None);
        run(
            &store,
            "run_OLD",
            Some("run_PARENT"),
            Some("2026-11-01T00:00:00Z"),
        );
        detail(&store, "run_OLD", 10);
        detail(&store, "run_PARENT", 99);
        store
            .conn()
            .execute_batch(
                "INSERT INTO usage_events (instance_id, run_id, input_tokens, output_tokens, \
                 metering_source, occurred_at) SELECT NULL, 'run_OLD', 10, 20, 'proxy', 't' \
                 WHERE 0",
            )
            .ok();
        assert_eq!(compact_all(&store, 500), 1);
        let kept = store.compacted_run("run_OLD").unwrap().expect("compacted");
        assert_eq!(
            (
                kept.input_tokens,
                kept.output_tokens,
                kept.cached_tokens,
                kept.cost_micros
            ),
            (10, 20, 3, 1500)
        );
        assert_eq!(kept.end_cause, "closed");
        assert_eq!(kept.control_depth, "announced");
        assert_eq!(kept.ended_at, "2026-11-01T00:00:00Z");
        assert_eq!(kept.compacted_at, NOW);
        assert_eq!(kept.labels.len(), 1);
        assert_eq!(kept.labels[0].key, "team");
        for table in [
            "runs",
            "run_labels",
            "run_limits",
            "run_latches",
            "run_dispositions",
            "telemetry_counters",
            "usage_rollup_run",
        ] {
            let left = count(
                &store,
                &format!("SELECT COUNT(*) FROM {table} WHERE run_id = 'run_OLD'"),
            );
            assert_eq!(left, 0, "{table} still holds the compacted Run");
        }
        // The parent's own rollup (which already counted the child) stays.
        let parent: i64 = count(
            &store,
            "SELECT input_tokens FROM usage_rollup_run WHERE run_id = 'run_PARENT'",
        );
        assert_eq!(parent, 99);
        assert!(store.compacted_run("run_PARENT").unwrap().is_none());
        assert!(store.compacted_run("run_NEVER").unwrap().is_none());
    }

    #[test]
    fn an_open_descendant_a_recent_end_and_the_unattributed_run_keep_a_run_whole() {
        let (_dir, store) = store();
        run(&store, "run_A", None, Some("2026-11-01T00:00:00Z"));
        run(&store, "run_B", Some("run_A"), Some("2026-11-02T00:00:00Z"));
        run(&store, "run_C", Some("run_B"), None);
        run(&store, "run_RECENT", None, Some("2026-11-12T00:00:00Z"));
        run(&store, "run_LEGACY", None, Some("2026-11-01T00:00:00Z"));
        store
            .conn()
            .execute(
                "INSERT INTO proxy_state (id, legacy_run) VALUES (1, 'run_LEGACY') \
                 ON CONFLICT(id) DO UPDATE SET legacy_run = excluded.legacy_run",
                [],
            )
            .unwrap();
        assert_eq!(compact_all(&store, 500), 0, "a grandchild is still open");
        // The grandchild ends (recently): both ancestors become eligible.
        store
            .conn()
            .execute(
                "UPDATE runs SET status = 'ended', ended_at = '2026-11-13T00:00:00Z' \
                 WHERE run_id = 'run_C'",
                [],
            )
            .unwrap();
        assert_eq!(compact_all(&store, 500), 2);
        assert!(store.compacted_run("run_A").unwrap().is_some());
        assert!(store.compacted_run("run_B").unwrap().is_some());
        for kept in ["run_C", "run_RECENT", "run_LEGACY"] {
            let left = count(
                &store,
                &format!("SELECT COUNT(*) FROM runs WHERE run_id = '{kept}'"),
            );
            assert_eq!(left, 1, "{kept} was compacted");
        }
        // The child's parent link to the compacted Run is cleared, not dangling.
        let parent: Option<String> = store
            .conn()
            .query_row(
                "SELECT parent_run_id FROM runs WHERE run_id = 'run_C'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, None);
    }

    #[test]
    fn a_job_compacts_at_most_its_batch_and_scans_a_bounded_run_of_blocked_rows() {
        let (_dir, store) = store();
        for i in 0..5 {
            run(
                &store,
                &format!("run_E{i}"),
                None,
                Some("2026-11-01T00:00:00Z"),
            );
        }
        let step = store
            .compact_ended_runs(CUTOFF, &CompactionKey::default(), 2, NOW)
            .unwrap();
        assert_eq!(step.compacted, 2);
        let next = step.next.expect("more remain");
        let step = store.compact_ended_runs(CUTOFF, &next, 2, NOW).unwrap();
        assert_eq!(step.compacted, 2);
        let step = store
            .compact_ended_runs(CUTOFF, &step.next.unwrap(), 2, NOW)
            .unwrap();
        assert_eq!((step.compacted, step.next), (1, None));
        // Ten blocked Runs: a job with batch 1 scans 4 and hands on its key.
        for i in 0..10 {
            run(
                &store,
                &format!("run_P{i}"),
                None,
                Some("2026-11-02T00:00:00Z"),
            );
            run(
                &store,
                &format!("run_Q{i}"),
                Some(&format!("run_P{i}")),
                None,
            );
        }
        let step = store
            .compact_ended_runs(CUTOFF, &CompactionKey::default(), 1, NOW)
            .unwrap();
        assert_eq!(step.compacted, 0);
        assert!(step.next.is_some(), "a full scan hands on its key");
        assert_eq!(compact_all(&store, 1), 0);
    }

    #[test]
    fn unreadable_compacted_labels_are_a_store_error_not_a_panic() {
        let (_dir, store) = store();
        store
            .conn()
            .execute(
                "INSERT INTO compacted_runs VALUES ('run_X', 1, 2, 3, 4, 'not json', 'c', \
                 'announced', 't', 't', 't')",
                [],
            )
            .unwrap();
        let err = store.compacted_run("run_X").unwrap_err().to_string();
        assert!(err.contains("unreadable labels"), "{err}");
    }
}

#[cfg(test)]
mod plan_pins {
    use super::*;
    use crate::store::SqliteStore;

    fn plan(store: &SqliteStore, sql: &str) -> Vec<String> {
        store
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["run_X"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_compaction_candidate_costs_index_reads_not_a_walk_of_every_open_run() {
        // Story 24-4: the Run-scale ladder found the descendant check
        // scanning every open Run per candidate, and a Run delete's cascade
        // scanning `run_identities`. Both now read indexes only.
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("state.db")).unwrap();
        let walk = plan(&store, OPEN_DESCENDANT_SQL);
        assert!(
            walk.iter()
                .all(|line| !line.contains("idx_runs_open") && line != "SCAN runs"),
            "{walk:?}"
        );
        assert!(
            walk.iter().any(|line| line.contains("idx_runs_parent")),
            "{walk:?}"
        );
        let delete = plan(&store, "DELETE FROM runs WHERE run_id = ?1");
        assert!(
            delete.iter().all(|line| !line.starts_with("SCAN ")),
            "a cascade scans a table: {delete:?}"
        );
        assert!(
            delete
                .iter()
                .any(|line| line.contains("idx_run_identities_run")),
            "{delete:?}"
        );
    }
}
