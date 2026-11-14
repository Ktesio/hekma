//! Story 24-3 (schema v29): the host posture row — which engine owns the data
//! path and whether it runs in Handoff Mode, written at open before it
//! adopts anything, so the next open can tell what a crashed owner asked
//! for (adopt its agents, or clean them up).

use rusqlite::OptionalExtension;

use super::sqlite::backend;
use super::SqliteStore;
use crate::ports::StoreError;

/// The v29 step.
pub(super) const V29_DDL: &str = "CREATE TABLE host_state (\n\
     id INTEGER PRIMARY KEY CHECK (id = 1),\n\
     pid INTEGER NOT NULL,\n\
     start_time INTEGER NOT NULL,\n\
     handoff TEXT NOT NULL CHECK (handoff IN ('on', 'off', 'unset')),\n\
     started_at TEXT NOT NULL\n\
     );";

/// Test-only: undo v29.
#[cfg(test)]
pub(crate) const UNDO_V29_FOR_TEST: &str = "DROP TABLE host_state;";

/// The recorded owner: its process fingerprint and its Handoff Mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostPostureRow {
    pub pid: u32,
    pub start_time: u64,
    /// `Some(on)` when the Operator chose a Handoff Mode; `None` when it
    /// ran the default without choosing (recorded `unset`).
    pub handoff: Option<bool>,
    pub started_at: String,
}

impl SqliteStore {
    /// The recorded owner, when one was recorded.
    pub(crate) fn host_posture(&self) -> Result<Option<HostPostureRow>, StoreError> {
        self.conn()
            .query_row(
                "SELECT pid, start_time, handoff, started_at FROM host_state WHERE id = 1",
                [],
                |row| {
                    Ok(HostPostureRow {
                        pid: row.get::<_, i64>(0)? as u32,
                        start_time: row.get::<_, i64>(1)? as u64,
                        handoff: match row.get::<_, String>(2)?.as_str() {
                            "on" => Some(true),
                            "off" => Some(false),
                            _ => None,
                        },
                        started_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(backend)
    }

    /// Record this owner (replacing the previous one).
    pub(crate) fn put_host_posture(&self, row: &HostPostureRow) -> Result<(), StoreError> {
        self.conn()
            .execute(
                "INSERT INTO host_state (id, pid, start_time, handoff, started_at) \
                 VALUES (1, ?1, ?2, ?3, ?4) \
                 ON CONFLICT (id) DO UPDATE SET pid = excluded.pid, \
                 start_time = excluded.start_time, handoff = excluded.handoff, \
                 started_at = excluded.started_at",
                rusqlite::params![
                    i64::from(row.pid),
                    row.start_time as i64,
                    match row.handoff {
                        Some(true) => "on",
                        Some(false) => "off",
                        None => "unset",
                    },
                    row.started_at
                ],
            )
            .map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_posture_row_is_absent_then_recorded_then_replaced() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.host_posture().unwrap(), None);
        let row = HostPostureRow {
            pid: 7,
            start_time: 99,
            handoff: Some(true),
            started_at: "t".into(),
        };
        store.put_host_posture(&row).unwrap();
        assert_eq!(store.host_posture().unwrap(), Some(row.clone()));
        for handoff in [Some(false), None] {
            let next = HostPostureRow {
                pid: 8,
                handoff,
                ..row.clone()
            };
            store.put_host_posture(&next).unwrap();
            assert_eq!(store.host_posture().unwrap(), Some(next));
        }
    }
}
