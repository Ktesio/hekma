//! The reconciliation store (story 19-5): append-only submissions of a
//! Run's confirmed usage. Schema v26.

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::backend;
use crate::domain::runs::reconcile::{ReconcileRequest, ReconcileSource, Reconciliation};
use crate::domain::RunId;
use crate::domain::{EstimateLabel, Micros};
use crate::ports::StoreError;

/// Schema v26: one row per accepted submission.
pub(super) const SCHEMA_V26: &str = "\
CREATE TABLE IF NOT EXISTS run_reconciliations (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id        TEXT NOT NULL,
    reference     TEXT NOT NULL,
    source        TEXT NOT NULL CHECK (source IN ('provider','host')),
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cached_tokens INTEGER,
    cost_micros   INTEGER,
    note          TEXT,
    submitted_by  TEXT NOT NULL,
    submitted_at  TEXT NOT NULL,
    UNIQUE (run_id, reference)
);
CREATE INDEX IF NOT EXISTS idx_run_reconciliations_run ON run_reconciliations(run_id, id);
";

/// Undo v26 on a fully migrated store, for the older upgrade tests.
#[cfg(test)]
pub(crate) const UNDO_V26_FOR_TEST: &str = "DROP TABLE run_reconciliations;";

/// Run the v26 step.
pub(super) fn migrate_v26(conn: &Connection) -> Result<(), StoreError> {
    super::runs_sqlite::migrate_runs_step(conn, 26, SCHEMA_V26)
}

const COLUMNS: &str = "run_id, reference, source, input_tokens, output_tokens, cached_tokens, \
     cost_micros, note, submitted_by, submitted_at";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Reconciliation> {
    let source: String = row.get(2)?;
    let count = |value: i64| value.max(0) as u64;
    Ok(Reconciliation {
        run_id: RunId::from_wire(row.get::<_, String>(0)?),
        figures: ReconcileRequest {
            reference: row.get(1)?,
            source: ReconcileSource::from_wire(&source).unwrap_or(ReconcileSource::Host),
            input_tokens: count(row.get(3)?),
            output_tokens: count(row.get(4)?),
            cached_tokens: row.get::<_, Option<i64>>(5)?.map(count),
            cost_micros: row.get::<_, Option<i64>>(6)?.map(Micros),
            note: row.get(7)?,
        },
        submitted_by: row.get(8)?,
        submitted_at: row.get(9)?,
        estimate: EstimateLabel::Reconciled,
    })
}

impl super::SqliteStore {
    /// The submission stored under `(run, reference)`, when one is.
    pub(crate) fn reconciliation(
        &self,
        run_id: &RunId,
        reference: &str,
    ) -> Result<Option<Reconciliation>, StoreError> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM run_reconciliations WHERE run_id = ?1 AND reference = ?2"
                ),
                rusqlite::params![run_id.as_str(), reference],
                read_row,
            )
            .optional()
            .map_err(backend)
    }

    /// Store an accepted submission.
    pub(crate) fn insert_reconciliation(&self, row: &Reconciliation) -> Result<(), StoreError> {
        let figures = &row.figures;
        let signed = |value: u64| value.min(i64::MAX as u64) as i64;
        self.conn()
            .execute(
                &format!(
                    "INSERT INTO run_reconciliations ({COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                ),
                rusqlite::params![
                    row.run_id.as_str(),
                    figures.reference,
                    figures.source.as_str(),
                    signed(figures.input_tokens),
                    signed(figures.output_tokens),
                    figures.cached_tokens.map(signed),
                    figures.cost_micros.map(Micros::get),
                    figures.note,
                    row.submitted_by,
                    row.submitted_at,
                ],
            )
            .map_err(backend)?;
        Ok(())
    }

    /// Every submission for `run_id`, newest first.
    pub(crate) fn reconciliations(
        &self,
        run_id: &RunId,
    ) -> Result<Vec<Reconciliation>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(&format!(
                "SELECT {COLUMNS} FROM run_reconciliations WHERE run_id = ?1 ORDER BY id DESC"
            ))
            .map_err(backend)?;
        let rows = stmt
            .query_map([run_id.as_str()], read_row)
            .map_err(backend)?;
        rows.collect::<Result<_, _>>().map_err(backend)
    }

    /// The newest submission for `run_id` and the submission count.
    pub(crate) fn current_reconciliation(
        &self,
        run_id: &RunId,
    ) -> Result<Option<(Reconciliation, u64)>, StoreError> {
        let count: i64 = self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM run_reconciliations WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(backend)?;
        if count == 0 {
            return Ok(None);
        }
        let current = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM run_reconciliations WHERE run_id = ?1 \
                     ORDER BY id DESC LIMIT 1"
                ),
                [run_id.as_str()],
                read_row,
            )
            .map_err(backend)?;
        Ok(Some((current, count as u64)))
    }
}
