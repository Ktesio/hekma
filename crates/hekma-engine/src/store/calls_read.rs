//! The calls store's read port: a bounded, read-only reader over the
//! `calls/` segment files.
//!
//! This reader takes NO registry lock, sends NOTHING to the writer and
//! holds no transaction: each segment is opened through its own
//! read-only connection (`SQLITE_OPEN_READ_ONLY`, a 500 ms busy timeout,
//! closed on return), so a reader never holds a WAL checkpoint open and
//! never blocks the proxy's writer batch. It works on a read-only engine
//! and on an engine whose proxy is disabled; a missing `calls/` directory
//! is an empty listing, never an error.
//!
//! Segments are read newest-name-first (the files are named by their UTC
//! creation date) and merged by call id, so overlapping segments stay
//! correct; a file that will not open, is not SQLite, or carries a newer
//! `user_version` is skipped, counted in `skipped_segments` and named in
//! `skipped_files` — a partial listing that says so, never a silent gap.
//! A schema 1 segment (before the read indexes) reads correctly too.
//!
//! Every page is bounded: the segments are walked together as one merge
//! by call id, and the whole page examines at most [`CALLS_SCAN_BUDGET`]
//! rows across segments — a filter that matches sparsely stops there,
//! returns what it found, and says so (`scan_limited`, the cursor it
//! continues from, and how far back it looked).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::domain::calls::{
    decode_call_cursor, encode_call_cursor, expose_model, is_call_id, is_run_id, CallDisposition,
    CallError, CallListing, CallOutcome, CallRecord, CallView, CallsQuery, StatusClass,
    CALLS_SCAN_BUDGET,
};
use crate::domain::RunId;
use crate::store::calls_sqlite::CALLS_SCHEMA_VERSION;

/// The busy timeout a read-only segment connection sets: bounded, like the
/// store's writer window, so a reader wedged behind a writer batch gives
/// up rather than hanging.
const SEGMENT_BUSY_TIMEOUT: Duration = Duration::from_millis(500);

/// The column list every read selects, BY NAME — never `SELECT *` into a
/// pass-through, so a column a later story adds is not exposed by accident
/// until the reader names it.
const CALL_COLUMNS: &str = "call_id, run_id, token_id, control_depth, binding, disposition, \
     hold_end, outcome, status, status_class, model, input_tokens, output_tokens, cached_tokens, \
     started_at, ended_at, latency_us, upstream_us";

/// The Control Depths a record can carry (the filter's closed list).
const DEPTHS: [&str; 4] = ["observed", "wire", "announced", "supervised"];

// ---------------------------------------------------------------------------
// Query resolution: validation and the window's id bounds.
// ---------------------------------------------------------------------------

/// A validated [`CallsQuery`], ready to run: the closed-list filters as
/// their wire names, the half-open window as call-id bounds (1 ms of slack
/// either side — the id's embedded time is the admission instant and the
/// stored `started_at` a second read of the clock, so a boundary-straddling
/// record stays inside the index range) plus the exact `started_at`
/// residuals that hold the half-open edges to the microsecond.
pub(crate) struct ResolvedCallsQuery {
    pub(crate) run_id: Option<String>,
    pub(crate) token_id: Option<String>,
    pub(crate) outcome: Option<&'static str>,
    pub(crate) disposition: Option<&'static str>,
    pub(crate) status_class: Option<&'static str>,
    pub(crate) model: Option<String>,
    pub(crate) depth: Option<&'static str>,
    pub(crate) binding: Option<String>,
    pub(crate) id_lower: Option<String>,
    pub(crate) id_upper: Option<String>,
    pub(crate) started_from: Option<String>,
    pub(crate) started_to: Option<String>,
    pub(crate) view: CallView,
    pub(crate) after: Option<String>,
    pub(crate) limit: usize,
}

/// Validate a [`CallsQuery`] into its resolved form. This is the ONE
/// validation site: the Control API, the CLI and embedders all pass
/// through it, so a refusal is the same refusal everywhere.
pub(crate) fn resolve_calls_query(query: &CallsQuery) -> Result<ResolvedCallsQuery, CallError> {
    let run_id = match &query.run {
        Some(raw) => {
            if !is_run_id(raw) {
                return Err(CallError::InvalidRun { value: raw.clone() });
            }
            Some(raw.clone())
        }
        None => None,
    };
    let outcome = match &query.outcome {
        Some(raw) => {
            Some(
                CallOutcome::from_filter(raw).ok_or_else(|| CallError::InvalidFilter {
                    param: "outcome",
                    detail: format!(
                        "one of {} — {raw} is not one this build knows",
                        [
                            "completed",
                            "refused",
                            "upstream-error",
                            "client-disconnect",
                            "shutdown",
                            "crash",
                            "recovered"
                        ]
                        .join(", ")
                    ),
                })?,
            )
        }
        None => None,
    };
    let disposition = match &query.disposition {
        Some(raw) => {
            Some(
                CallDisposition::from_filter(raw).ok_or_else(|| CallError::InvalidFilter {
                    param: "disposition",
                    detail: format!(
                        "one of {} — {raw} is not one this build knows",
                        ["pass", "block", "enforce", "hold", "reroute", "reject"].join(", ")
                    ),
                })?,
            )
        }
        None => None,
    };
    let status_class = match &query.status_class {
        Some(raw) => {
            Some(
                StatusClass::from_filter(raw).ok_or_else(|| CallError::InvalidFilter {
                    param: "status_class",
                    detail: format!(
                        "one of {} — {raw} is not one this build knows",
                        ["2xx", "3xx", "4xx", "5xx", "refused", "transport-error"].join(", ")
                    ),
                })?,
            )
        }
        None => None,
    };
    let depth = match &query.depth {
        Some(raw) => Some(
            DEPTHS
                .iter()
                .find(|known| **known == *raw)
                .copied()
                .ok_or_else(|| CallError::InvalidFilter {
                    param: "depth",
                    detail: format!(
                        "one of {} — {raw} is not one this build knows",
                        DEPTHS.join(", ")
                    ),
                })?,
        ),
        None => None,
    };
    let parse_instant =
        |raw: &Option<String>, which: &str| -> Result<Option<(u64, u32)>, CallError> {
            match raw.as_deref() {
                Some(text) => Ok(Some(crate::time::parse_rfc3339(text).ok_or_else(|| {
                    CallError::InvalidTime {
                        detail: format!(
                        "{which} must be an RFC 3339 UTC instant (e.g. 2026-11-12T09:30:00Z) — \
                         {text} could not be read"
                    ),
                    }
                })?)),
                None => Ok(None),
            }
        };
    let from = parse_instant(&query.from, "from")?;
    let to = parse_instant(&query.to, "to")?;
    if let (Some(from), Some(to)) = (from, to) {
        if to <= from {
            return Err(CallError::InvalidTime {
                detail: "to must be after from — the window [from, to) is half-open and empty the \
                     other way"
                    .to_string(),
            });
        }
    }
    // The window's id bounds (with 1 ms of slack) and the exact residuals.
    let millis_of =
        |instant: Option<(u64, u32)>| instant.map(|(secs, _)| secs.saturating_mul(1000));
    let id_lower = millis_of(from)
        .map(|ms| format!("call_{}", crate::domain::ulid_floor(ms.saturating_sub(1))));
    let id_upper =
        millis_of(to).map(|ms| format!("call_{}", crate::domain::ulid_floor(ms.saturating_add(1))));
    let started_from = from.map(|(secs, micros)| crate::time::format_rfc3339_micros(secs, micros));
    let started_to = to.map(|(secs, micros)| crate::time::format_rfc3339_micros(secs, micros));
    let after = match &query.after {
        Some(raw) => Some(decode_call_cursor(raw)?),
        None => None,
    };
    Ok(ResolvedCallsQuery {
        run_id,
        token_id: query.token_id.clone(),
        outcome: outcome.map(|value| match value {
            CallOutcome::Completed => "completed",
            CallOutcome::Refused => "refused",
            CallOutcome::UpstreamError => "upstream-error",
            CallOutcome::ClientDisconnect => "client-disconnect",
            CallOutcome::Shutdown => "shutdown",
            CallOutcome::Crash => "crash",
            CallOutcome::Recovered => "recovered",
            CallOutcome::Other(_) => unreachable!("from_filter never yields Other"),
        }),
        disposition: disposition.map(|value| match value {
            CallDisposition::Pass => "pass",
            CallDisposition::Block => "block",
            CallDisposition::Enforce => "enforce",
            CallDisposition::Hold => "hold",
            CallDisposition::Reroute => "reroute",
            CallDisposition::Reject => "reject",
            CallDisposition::Other(_) => unreachable!("from_filter never yields Other"),
        }),
        status_class: status_class.map(|value| match value {
            StatusClass::Success => "2xx",
            StatusClass::Redirect => "3xx",
            StatusClass::ClientError => "4xx",
            StatusClass::ServerError => "5xx",
            StatusClass::Refused => "refused",
            StatusClass::TransportError => "transport-error",
            StatusClass::Other(_) => unreachable!("from_filter never yields Other"),
        }),
        model: query.model.clone(),
        depth,
        binding: query.binding.clone(),
        id_lower,
        id_upper,
        started_from,
        started_to,
        view: query.view,
        after,
        limit: query.clamped_limit(),
    })
}

// ---------------------------------------------------------------------------
// The statements (built here so the reads and the EXPLAIN QUERY PLAN pins
// run the SAME SQL).
// ---------------------------------------------------------------------------

/// One statement and the parameters it binds, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallsStatement {
    pub(crate) sql: String,
    pub(crate) params: Vec<String>,
}

/// Build the keyset page statement for one segment: the fixed column list
/// and ONLY the predicates an INDEX serves — the keyset bound, the window's
/// id bounds, the Run filter, and the `problems` view's partial-index
/// predicate (repeated verbatim, because SQLite uses a partial index only
/// when the statement carries its predicate as written). The RESIDUAL
/// filters (token id, outcome, disposition, status class, model, Control
/// Depth, binding, the exact `started_at` window edges) apply in the
/// reader, row by row — that is what makes the scan budget count every row
/// the index walk examines, not just the ones that match.
pub(crate) fn page_statement(query: &ResolvedCallsQuery, fetch: usize) -> CallsStatement {
    let mut params: Vec<String> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if let Some(after) = &query.after {
        params.push(after.clone());
        clauses.push(format!("call_id < ?{}", params.len()));
    }
    if let Some(lower) = &query.id_lower {
        params.push(lower.clone());
        clauses.push(format!("call_id >= ?{}", params.len()));
    }
    if let Some(upper) = &query.id_upper {
        params.push(upper.clone());
        clauses.push(format!("call_id < ?{}", params.len()));
    }
    if let Some(run_id) = &query.run_id {
        params.push(run_id.clone());
        clauses.push(format!("run_id = ?{}", params.len()));
    }
    // The problems view's predicate, LITERALLY as the partial index writes
    // it (see CALLS_PROBLEM_WHERE).
    if query.view == CallView::Problems {
        clauses.push(format!("({})", crate::domain::calls::CALLS_PROBLEM_WHERE));
    }
    let sql = format!(
        "SELECT {CALL_COLUMNS} FROM calls{} ORDER BY call_id DESC LIMIT {fetch}",
        if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        }
    );
    CallsStatement { sql, params }
}

/// The by-id probe statement.
pub(crate) fn probe_statement(call_id: &str) -> CallsStatement {
    CallsStatement {
        sql: format!("SELECT {CALL_COLUMNS} FROM calls WHERE call_id = ?1"),
        params: vec![call_id.to_string()],
    }
}

// ---------------------------------------------------------------------------
// Segments.
// ---------------------------------------------------------------------------

/// The `calls/*.db` segment files, newest name first. `-wal`/`-shm` sidecar
/// files and anything else in the directory are ignored (their extensions
/// differ); a directory that does not exist is an empty list.
fn segments_newest_first(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "db"))
        .collect();
    // Newest name first: the files are named by their UTC creation date, so
    // name order is recency order until rotation ships its own.
    paths.sort();
    paths.reverse();
    paths
}

/// Open one segment read-only. `Err(reason)` names why the file cannot be
/// served (it is then skipped and counted, never a failed listing).
fn open_segment(path: &Path) -> Result<Connection, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|err| format!("{name}: could not be read as a calls file ({err})"))?;
    conn.busy_timeout(SEGMENT_BUSY_TIMEOUT)
        .map_err(|err| format!("{name}: could not be read as a calls file ({err})"))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|err| format!("{name}: could not be read as a calls file ({err})"))?;
    if version > CALLS_SCHEMA_VERSION {
        return Err(format!("{name}: written by a newer Hekma"));
    }
    if version < 1 {
        return Err(format!("{name}: not a calls file this engine records"));
    }
    Ok(conn)
}

/// One row as a record. The model text is masked at this boundary — the
/// stored value is never returned when it is shaped like a Hekma
/// credential, and bytes outside visible ASCII become `?`.
fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    let status: Option<i64> = row.get(8)?;
    Ok(CallRecord {
        call_id: row.get(0)?,
        run_id: RunId::from_wire(row.get::<_, String>(1)?),
        token_id: row.get(2)?,
        control_depth: row.get(3)?,
        binding: row.get(4)?,
        disposition: CallDisposition::from_raw(&row.get::<_, String>(5)?),
        hold_end: row.get(6)?,
        outcome: CallOutcome::from_raw(&row.get::<_, String>(7)?),
        status: status
            .filter(|code| (100..=599).contains(code))
            .map(|code| code as u16),
        status_class: StatusClass::from_raw(&row.get::<_, String>(9)?),
        model: row
            .get::<_, Option<String>>(10)?
            .map(|raw| expose_model(&raw)),
        input_tokens: row.get::<_, Option<i64>>(11)?.map(|n| n.max(0) as u64),
        output_tokens: row.get::<_, Option<i64>>(12)?.map(|n| n.max(0) as u64),
        cached_tokens: row.get::<_, Option<i64>>(13)?.map(|n| n.max(0) as u64),
        started_at: row.get(14)?,
        ended_at: row.get(15)?,
        latency_us: row.get::<_, i64>(16)?.max(0) as u64,
        upstream_us: row.get::<_, Option<i64>>(17)?.map(|n| n.max(0) as u64),
    })
}

/// Read one page over the calls directory. Takes no lock, holds no
/// transaction, sends nothing to the writer.
pub(crate) fn read_page(dir: &Path, query: &CallsQuery) -> Result<CallListing, CallError> {
    read_page_with_budget(dir, query, CALLS_SCAN_BUDGET)
}

/// [`read_page`] with the scan budget given (the tests lower it; production
/// always passes [`CALLS_SCAN_BUDGET`]).
///
/// The segments are walked TOGETHER, as one merge by call id descending:
/// every step examines the highest id any segment still holds. So the rows
/// a page has examined are always a prefix of the merged order, whatever
/// ids each file holds (a call that started before a rotation and closed
/// after, a restored backup, a clock step): the next page's keyset
/// continues below the last of them with no gap and no duplicate, the
/// budget counts the merged walk, and `searched_back_to` names the start of
/// the last row examined in that order.
fn read_page_with_budget(
    dir: &Path,
    query: &CallsQuery,
    budget: usize,
) -> Result<CallListing, CallError> {
    // The query is validated first: a typed refusal stands whether or not
    // the store exists yet.
    let resolved = resolve_calls_query(query)?;
    // A directory that exists but cannot be read is a surfaced failure; a
    // missing one is an empty listing.
    if let Err(err) = std::fs::read_dir(dir) {
        return match err.kind() {
            std::io::ErrorKind::NotFound => Ok(CallListing {
                items: Vec::new(),
                next_cursor: None,
                scan_limited: false,
                searched_back_to: None,
                skipped_segments: 0,
                skipped_files: Vec::new(),
            }),
            _ => Err(CallError::Unavailable {
                detail: format!(
                    "the calls directory {} could not be read ({err})",
                    dir.display()
                ),
            }),
        };
    }
    let fetch = resolved.limit.saturating_add(1);
    // Each segment's statement stops one row past the budget: the merged
    // walk never examines more than `budget` rows in all, and the one extra
    // row tells a budget stop from a segment read to its end.
    let statement = page_statement(&resolved, budget.saturating_add(1));
    let mut skipped_files: Vec<String> = Vec::new();
    // A file that will not open, has no calls table, or holds a row that
    // will not decode is skipped and named: the other segments still serve.
    let mut conns: Vec<(PathBuf, Connection)> = Vec::new();
    for segment in segments_newest_first(dir) {
        match open_segment(&segment) {
            Ok(conn) => conns.push((segment, conn)),
            Err(reason) => skipped_files.push(reason),
        }
    }
    let mut statements: Vec<(&Path, rusqlite::Statement<'_>)> = Vec::new();
    for (path, conn) in &conns {
        match conn.prepare(&statement.sql) {
            Ok(stmt) => statements.push((path.as_path(), stmt)),
            Err(err) => skipped_files.push(unreadable_segment(path, &err)),
        }
    }
    let mut walks: Vec<SegmentWalk<'_>> = Vec::new();
    for (path, stmt) in statements.iter_mut() {
        match stmt.query(rusqlite::params_from_iter(statement.params.iter())) {
            Ok(rows) => {
                let mut walk = SegmentWalk {
                    path,
                    rows: Some(rows),
                    head: None,
                };
                walk.advance(&mut skipped_files);
                walks.push(walk);
            }
            Err(err) => skipped_files.push(unreadable_segment(path, &err)),
        }
    }
    let mut found: Vec<CallRecord> = Vec::new();
    let mut examined: usize = 0;
    let mut last_examined: Option<(String, String)> = None;
    let mut budget_stop = false;
    while found.len() < fetch {
        // The segment whose next row has the highest id.
        let Some(next) = walks
            .iter()
            .enumerate()
            .filter_map(|(index, walk)| walk.head.as_ref().map(|head| (index, head)))
            .max_by(|a, b| a.1.call_id.cmp(&b.1.call_id))
            .map(|(index, _)| index)
        else {
            break;
        };
        if examined >= budget {
            budget_stop = true;
            break;
        }
        let Some(record) = walks[next].head.take() else {
            break;
        };
        walks[next].advance(&mut skipped_files);
        // The same id in another segment (a replay in a re-created file) is
        // one call: its copies are passed over, not listed twice.
        for walk in walks.iter_mut() {
            while walk
                .head
                .as_ref()
                .is_some_and(|head| head.call_id == record.call_id)
            {
                walk.advance(&mut skipped_files);
            }
        }
        examined += 1;
        last_examined = Some((record.call_id.clone(), record.started_at.clone()));
        if residual_matches(&resolved, &record) {
            found.push(record);
        }
    }
    let skipped_segments = skipped_files.len() as u64;
    if found.len() > resolved.limit {
        found.truncate(resolved.limit);
        let next_cursor = found
            .last()
            .map(|record| encode_call_cursor(&record.call_id));
        return Ok(CallListing {
            items: found,
            next_cursor,
            scan_limited: false,
            searched_back_to: None,
            skipped_segments,
            skipped_files,
        });
    }
    // The page did not fill. If the budget stopped the walk, the page is
    // partial and the cursor continues from the last row examined; else
    // every segment was read to its end and the read is exhausted.
    let (next_cursor, scan_limited, searched_back_to) = if budget_stop {
        (
            last_examined.as_ref().map(|(id, _)| encode_call_cursor(id)),
            true,
            last_examined.as_ref().map(|(_, started)| started.clone()),
        )
    } else {
        (None, false, None)
    };
    Ok(CallListing {
        items: found,
        next_cursor,
        scan_limited,
        searched_back_to,
        skipped_segments,
        skipped_files,
    })
}

/// One segment's place in the merged walk: its rows in id-descending order
/// and the next one not yet examined.
struct SegmentWalk<'s> {
    path: &'s Path,
    /// `None` once the segment is read to its end or failed mid-walk.
    rows: Option<rusqlite::Rows<'s>>,
    head: Option<CallRecord>,
}

impl SegmentWalk<'_> {
    /// Step to the next row. A row that fails to read ends this segment's
    /// walk and names it in `skipped`; what it already gave stands.
    fn advance(&mut self, skipped: &mut Vec<String>) {
        self.head = None;
        let Some(rows) = self.rows.as_mut() else {
            return;
        };
        let next = match rows.next() {
            Ok(Some(row)) => row_to_record(row).map(Some),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        };
        match next {
            Ok(Some(record)) => self.head = Some(record),
            Ok(None) => self.rows = None,
            Err(err) => {
                skipped.push(unreadable_segment(self.path, &err));
                self.rows = None;
            }
        }
    }
}

/// The skip reason for a segment that opened but could not be read.
fn unreadable_segment(path: &Path, err: &rusqlite::Error) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("{name}: could not be read as a calls file ({err})")
}

/// The residual filter match, applied in the reader (not the SQL) so the
/// scan budget counts every row the index walk examines: the equality
/// filters on the stored values, plus the window's EXACT `started_at`
/// edges (the id bounds in the SQL carry 1 ms of slack; these hold the
/// half-open window to the microsecond).
fn residual_matches(query: &ResolvedCallsQuery, record: &CallRecord) -> bool {
    if let Some(token_id) = &query.token_id {
        if record.token_id.as_deref() != Some(token_id.as_str()) {
            return false;
        }
    }
    if let Some(outcome) = query.outcome {
        if record.outcome.as_str() != outcome {
            return false;
        }
    }
    if let Some(disposition) = query.disposition {
        if record.disposition.as_str() != disposition {
            return false;
        }
    }
    if let Some(status_class) = query.status_class {
        if record.status_class.as_str() != status_class {
            return false;
        }
    }
    if let Some(model) = &query.model {
        if record.model.as_deref() != Some(model.as_str()) {
            return false;
        }
    }
    if let Some(depth) = query.depth {
        if record.control_depth != depth {
            return false;
        }
    }
    if let Some(binding) = &query.binding {
        if record.binding.as_deref() != Some(binding.as_str()) {
            return false;
        }
    }
    if let Some(from) = &query.started_from {
        if record.started_at < *from {
            return false;
        }
    }
    if let Some(to) = &query.started_to {
        if record.started_at >= *to {
            return false;
        }
    }
    true
}

/// The by-id probe: the newest segment that holds the id serves it.
pub(crate) fn read_call(dir: &Path, call_id: &str) -> Result<CallRecord, CallError> {
    if !is_call_id(call_id) {
        return Err(CallError::InvalidCallId {
            call_id: call_id.to_string(),
        });
    }
    let statement = probe_statement(call_id);
    let mut unreadable: Option<String> = None;
    for segment in segments_newest_first(dir) {
        // A file the listing skips (newer schema, not SQLite) is skipped
        // here too.
        let Ok(conn) = open_segment(&segment) else {
            continue;
        };
        let found = conn
            .query_row(
                &statement.sql,
                rusqlite::params_from_iter(statement.params.iter()),
                row_to_record,
            )
            .optional();
        match found {
            Ok(Some(record)) => return Ok(record),
            Ok(None) => continue,
            // A row that will not decode, or a file with no calls table:
            // another segment may still serve the id, so keep looking.
            Err(err) => {
                unreadable.get_or_insert_with(|| unreadable_segment(&segment, &err));
            }
        }
    }
    // A file that could not be read might hold the id: that is a surfaced
    // failure, never "not found".
    match unreadable {
        Some(detail) => Err(CallError::Unavailable { detail }),
        None => Err(CallError::NotFound {
            call_id: call_id.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::calls::{CallsQuery, CALLS_MAX_LIMIT};

    /// Two ULID-shaped Run ids the seeded rows attribute to.
    const PARENT: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";
    const CHILD: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2V";
    const OLD: &str = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2W";

    /// A call id whose embedded time is `unix_millis` and whose random half
    /// differs by `nonce` (the ULID's last character carries the low bits).
    fn call_id_at(unix_millis: u64, nonce: u8) -> String {
        let floor = crate::domain::ulid_floor(unix_millis);
        let mut chars: Vec<char> = floor.chars().collect();
        let last = chars.len() - 1;
        let alphabet = crate::domain::CROCKFORD;
        chars[last] = alphabet[((nonce as usize) % 32) & 0x1F] as char;
        format!("call_{}", chars.into_iter().collect::<String>())
    }

    /// A filter setter (the refusal table's rows).
    type Setter = Box<dyn Fn(&mut CallsQuery)>;

    /// A row's full column values (each test fills what it asserts).
    #[derive(Clone, Default)]
    struct Row {
        call_id: String,
        run_id: &'static str,
        token_id: Option<&'static str>,
        depth: &'static str,
        binding: Option<&'static str>,
        disposition: &'static str,
        outcome: &'static str,
        status: Option<i64>,
        status_class: &'static str,
        model: Option<&'static str>,
        input: Option<i64>,
        output: Option<i64>,
        cached: Option<i64>,
        started_at: String,
        ended_at: String,
        latency_us: i64,
        upstream_us: Option<i64>,
    }

    impl Row {
        fn new(call_id: &str, run_id: &'static str) -> Self {
            Row {
                call_id: call_id.to_string(),
                run_id,
                depth: "wire",
                disposition: "pass",
                outcome: "completed",
                status: Some(200),
                status_class: "2xx",
                model: Some("gpt-demo"),
                input: Some(10),
                output: Some(5),
                cached: Some(2),
                started_at: "2026-11-12T00:00:00.000000Z".to_string(),
                ended_at: "2026-11-12T00:00:01.000000Z".to_string(),
                latency_us: 1_000_000,
                upstream_us: Some(900_000),
                ..Row::default()
            }
        }

        fn insert(&self, conn: &Connection) {
            conn.execute(
                "INSERT OR IGNORE INTO calls (call_id, run_id, token_id, control_depth, binding, \
                 disposition, hold_end, outcome, status, status_class, model, input_tokens, \
                 output_tokens, cached_tokens, started_at, ended_at, latency_us, upstream_us) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                 ?15, ?16, ?17)",
                rusqlite::params![
                    self.call_id,
                    self.run_id,
                    self.token_id,
                    self.depth,
                    self.binding,
                    self.disposition,
                    self.outcome,
                    self.status,
                    self.status_class,
                    self.model,
                    self.input,
                    self.output,
                    self.cached,
                    self.started_at,
                    self.ended_at,
                    self.latency_us,
                    self.upstream_us,
                ],
            )
            .unwrap();
        }
    }

    /// Create `calls/<name>.db` at `version` and insert `rows` (direct SQL,
    /// no processes): the file is ATTACHed as `calls` on a scratch
    /// connection, exactly the way the writer sees it. Version 1 segments
    /// carry the pre-read indexes only.
    fn seed_segment(dir: &Path, name: &str, version: i64, rows: &[Row]) {
        use crate::store::calls_sqlite::CALLS_V1;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS calls;",
            path.to_string_lossy()
        ))
        .unwrap();
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;\n{CALLS_V1}\n{}",
            match version {
                1 => "PRAGMA calls.user_version = 1;\nCOMMIT;".to_string(),
                2 => "PRAGMA calls.user_version = 2;\nCOMMIT;".to_string(),
                other => format!("PRAGMA calls.user_version = {other};\nCOMMIT;"),
            }
        ))
        .unwrap();
        if version >= 2 {
            conn.execute_batch(&format!(
                "BEGIN IMMEDIATE;\n{}COMMIT;",
                crate::store::calls_sqlite::calls_v2_for_test()
            ))
            .unwrap();
        }
        for row in rows {
            row.insert(&conn);
        }
    }

    /// 2026-11-12T00:00:00Z in Unix milliseconds, the seeded records' base.
    const BASE_MS: u64 = 1_794_441_600_000;

    fn listing(dir: &Path, query: &CallsQuery) -> CallListing {
        read_page(dir, query).unwrap()
    }

    /// The acceptance fixture: five records in one schema-2 segment —
    /// `call_E`..`call_A` newest first — plus a second, older segment.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        seed_segment(
            &dir,
            "2026-11-12.db",
            2,
            &[
                Row::new(&call_id_at(BASE_MS + 4_000, 4), PARENT),
                Row::new(&call_id_at(BASE_MS + 3_000, 3), CHILD),
                Row {
                    disposition: "block",
                    outcome: "refused",
                    status: Some(429),
                    status_class: "refused",
                    run_id: PARENT,
                    ..Row::new(&call_id_at(BASE_MS + 2_000, 2), PARENT)
                },
                Row {
                    outcome: "upstream-error",
                    status: None,
                    status_class: "transport-error",
                    input: None,
                    output: None,
                    cached: None,
                    ..Row::new(&call_id_at(BASE_MS + 1_000, 1), PARENT)
                },
                Row::new(&call_id_at(BASE_MS, 0), CHILD),
            ],
        );
        (tmp, dir)
    }

    #[test]
    fn the_first_page_lists_newest_first_with_exactly_the_documented_fields() {
        let (_tmp, dir) = fixture();
        let page = listing(&dir, &CallsQuery::all());
        assert_eq!(
            page.items
                .iter()
                .map(|r| r.call_id.clone())
                .collect::<Vec<_>>(),
            vec![
                call_id_at(BASE_MS + 4_000, 4),
                call_id_at(BASE_MS + 3_000, 3),
                call_id_at(BASE_MS + 2_000, 2),
                call_id_at(BASE_MS + 1_000, 1),
                call_id_at(BASE_MS, 0),
            ]
        );
        let value = serde_json::to_value(&page.items[0]).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "binding",
                "cached_tokens",
                "call_id",
                "control_depth",
                "disposition",
                "ended_at",
                "hold_end",
                "input_tokens",
                "latency_us",
                "model",
                "outcome",
                "output_tokens",
                "run_id",
                "started_at",
                "status",
                "status_class",
                "token_id",
                "upstream_us",
            ],
            "the item carries exactly the documented fields, nothing more"
        );
        // Unknown token counts are null, never zero; hold_end is null.
        let upstream_failed = &page.items[3];
        assert_eq!(value["hold_end"], serde_json::Value::Null);
        assert_eq!(upstream_failed.input_tokens, None);
        let failed = serde_json::to_value(upstream_failed).unwrap();
        assert_eq!(failed["input_tokens"], serde_json::Value::Null);
        assert_eq!(failed["output_tokens"], serde_json::Value::Null);
        // The clean rows' counts are numbers, and a known cached subset
        // survives.
        assert_eq!(page.items[0].input_tokens, Some(10));
        assert_eq!(page.items[0].cached_tokens, Some(2));
        // token_id is null for a call that presented no Run Key.
        assert_eq!(page.items[0].token_id, None);
    }

    #[test]
    fn a_token_id_label_reads_back_and_filters_by_equality() {
        let (_tmp, dir) = fixture();
        // Give one row a token id label directly.
        {
            let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
            conn.execute(
                "UPDATE calls SET token_id = 'abcdef012345' WHERE call_id = ?1",
                [call_id_at(BASE_MS + 4_000, 4)],
            )
            .unwrap();
        }
        let mut query = CallsQuery::all();
        query.token_id = Some("abcdef012345".to_string());
        let page = listing(&dir, &query);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].token_id.as_deref(), Some("abcdef012345"));
        // A label no call presented matches nothing — a filter, never an
        // error.
        query.token_id = Some("000000000000".to_string());
        assert!(listing(&dir, &query).items.is_empty());
    }

    #[test]
    fn the_run_filter_never_expands_a_subtree() {
        let (_tmp, dir) = fixture();
        let mut query = CallsQuery::all();
        query.run = Some(CHILD.to_string());
        let child = listing(&dir, &query);
        assert!(child
            .items
            .iter()
            .all(|record| record.run_id.as_str() == CHILD));
        assert_eq!(child.items.len(), 2);
        query.run = Some(PARENT.to_string());
        let parent = listing(&dir, &query);
        assert!(parent
            .items
            .iter()
            .all(|record| record.run_id.as_str() == PARENT));
        assert_eq!(parent.items.len(), 3);
        // A malformed run filter is the typed refusal.
        query.run = Some("not-a-run".to_string());
        assert_eq!(read_page(&dir, &query).unwrap_err().code(), "invalid-run");
    }

    #[test]
    fn every_residual_filter_applies_alone_and_combined() {
        let (_tmp, dir) = fixture();
        let by = |f: fn(&mut CallsQuery)| {
            let mut query = CallsQuery::all();
            f(&mut query);
            listing(&dir, &query)
        };
        // outcome alone.
        let refused = by(|q| q.outcome = Some("refused".to_string()));
        assert_eq!(refused.items.len(), 1);
        assert_eq!(refused.items[0].outcome.as_str(), "refused");
        // disposition alone.
        let blocked = by(|q| q.disposition = Some("block".to_string()));
        assert_eq!(blocked.items.len(), 1);
        assert_eq!(blocked.items[0].disposition.as_str(), "block");
        // status_class alone (the 429 refusal is class `refused`).
        let transport = by(|q| q.status_class = Some("transport-error".to_string()));
        assert_eq!(transport.items.len(), 1);
        // model alone (every seeded row names the same model).
        let model = by(|q| q.model = Some("gpt-demo".to_string()));
        assert_eq!(model.items.len(), 5);
        // depth alone (the fixture's rows are `wire`).
        let wire = by(|q| q.depth = Some("wire".to_string()));
        assert_eq!(wire.items.len(), 5);
        // binding alone: the fixture has none set.
        let bound = by(|q| q.binding = Some("openai".to_string()));
        assert!(bound.items.is_empty());
        // Combined: run + outcome narrows to the parent's refusal.
        let combined = by(|q| {
            q.run = Some(PARENT.to_string());
            q.outcome = Some("refused".to_string());
        });
        assert_eq!(combined.items.len(), 1);
        assert_eq!(combined.items[0].run_id.as_str(), PARENT);
        // A closed-list value this build does not know refuses, naming the
        // parameter.
        let setters: Vec<(Setter, &str)> = vec![
            (
                Box::new(|q: &mut CallsQuery| q.outcome = Some("superceded".into())),
                "outcome",
            ),
            (
                Box::new(|q: &mut CallsQuery| q.disposition = Some("quarantine".into())),
                "disposition",
            ),
            (
                Box::new(|q: &mut CallsQuery| q.status_class = Some("1xx".into())),
                "status_class",
            ),
            (
                Box::new(|q: &mut CallsQuery| q.depth = Some("dreaming".into())),
                "depth",
            ),
        ];
        for (setter, param) in setters {
            let mut query = CallsQuery::all();
            setter(&mut query);
            match read_page(&dir, &query) {
                Err(CallError::InvalidFilter { param: named, .. }) => assert_eq!(named, param),
                other => panic!("expected invalid-filter({param}), got {other:?}"),
            }
        }
    }

    #[test]
    fn the_problems_view_lists_everything_but_a_clean_pass() {
        let (_tmp, dir) = fixture();
        let mut query = CallsQuery::all();
        query.view = CallView::Problems;
        let page = listing(&dir, &query);
        // The refused, the upstream error; the two clean 200s are absent.
        assert_eq!(page.items.len(), 2);
        assert!(page
            .items
            .iter()
            .all(|record| record.outcome.as_str() != "completed"
                || record.status_class.as_str() != "2xx"
                || record.disposition.as_str() != "pass"));
        // A 429 COMPLETED call is a problem (status class, not outcome).
        seed_more_problems(&dir);
        let page = listing(&dir, &query);
        assert!(page
            .items
            .iter()
            .any(|record| record.status == Some(429) && record.outcome.as_str() == "completed"));
    }

    /// A completed `429` and a `crash`/`recovered` closure inside the same
    /// segment (direct SQL: the shapes the proxy writes).
    fn seed_more_problems(dir: &Path) {
        let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
        for (nonce, outcome, status, class) in [
            (11u8, "completed", 429i64, "4xx"),
            (12u8, "crash", 500, "5xx"),
            (13u8, "recovered", 503, "5xx"),
        ] {
            Row {
                outcome,
                status: Some(status),
                status_class: class,
                ..Row::new(&call_id_at(BASE_MS + 5_000, nonce), PARENT)
            }
            .insert(&conn);
        }
    }

    #[test]
    fn pages_are_disjoint_and_the_cursor_bridges_records_that_close_late() {
        let (_tmp, dir) = fixture();
        let mut query = CallsQuery::all();
        query.limit = 2;
        let first = listing(&dir, &query);
        assert_eq!(first.items.len(), 2);
        let cursor = first.next_cursor.clone().expect("five records page");
        // A record closes between the pages with an id BELOW the cursor
        // (+2.5s < the cursor's +3s): it appears on the later page.
        {
            let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
            Row::new(&call_id_at(BASE_MS + 2_500, 9), CHILD).insert(&conn);
        }
        query.after = Some(cursor);
        let second = listing(&dir, &query);
        assert_eq!(second.items.len(), 2);
        // Disjoint, descending, and the late-closing record is there.
        let ids: Vec<&str> = second.items.iter().map(|r| r.call_id.as_str()).collect();
        assert!(ids.contains(&call_id_at(BASE_MS + 2_500, 9).as_str()));
        assert!(first.items[0].call_id > second.items[0].call_id);
        // Follow to exhaustion: next_cursor is null exactly then.
        query.after = second.next_cursor.clone();
        let third = listing(&dir, &query);
        assert_eq!(third.items.len(), 2);
        assert!(third.next_cursor.is_none(), "the read is exhausted");
        // A record with an id ABOVE an already-served cursor is not chased:
        // re-reading from the newest page finds it, a later page does not.
        {
            let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
            Row::new(&call_id_at(BASE_MS + 6_000, 8), PARENT).insert(&conn);
        }
        let mut again = CallsQuery::all();
        again.limit = 2;
        let reread = listing(&dir, &again);
        assert!(reread
            .items
            .iter()
            .any(|record| record.call_id == call_id_at(BASE_MS + 6_000, 8)));
        let mut later = CallsQuery::all();
        later.limit = 2;
        later.after = reread.next_cursor.clone();
        let later_page = listing(&dir, &later);
        assert!(later_page
            .items
            .iter()
            .all(|record| record.call_id != call_id_at(BASE_MS + 6_000, 8)));
    }

    #[test]
    fn a_foreign_cursor_refuses_in_both_kinds() {
        let (_tmp, dir) = fixture();
        let mut query = CallsQuery::all();
        // The Run listing's codec over a RUN id: not a call id.
        query.after = Some(crate::encode_cursor("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"));
        assert_eq!(
            read_page(&dir, &query).unwrap_err().code(),
            "invalid-cursor"
        );
        // Garbage, padding, a v2: prefix.
        let padded = format!("{}==", crate::encode_cursor("x"));
        for bad in ["nonsense".to_string(), "dnVuXzE=".to_string(), padded] {
            let mut query = CallsQuery::all();
            query.after = Some(bad.to_string());
            assert_eq!(
                read_page(&dir, &query).unwrap_err().code(),
                "invalid-cursor",
                "{bad}"
            );
        }
    }

    #[test]
    fn the_time_window_is_half_open_on_the_call_start_to_the_microsecond() {
        let (_tmp, dir) = fixture();
        // Seed records at the exact edges: T0 exactly (inclusive), T0.5s,
        // T0.999999s, and T1 exactly (exclusive).
        {
            let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
            for (nonce, micros) in [(21u8, 0u32), (22, 500_000), (23, 999_999)] {
                Row {
                    started_at: crate::time::format_rfc3339_micros(BASE_MS / 1000, micros),
                    ..Row::new(&call_id_at(BASE_MS, nonce), CHILD)
                }
                .insert(&conn);
            }
            Row {
                started_at: crate::time::format_rfc3339_micros(BASE_MS / 1000 + 1, 0),
                ..Row::new(&call_id_at(BASE_MS + 1_000, 24), CHILD)
            }
            .insert(&conn);
        }
        let mut query = CallsQuery::all();
        query.from = Some("2026-11-12T00:00:00Z".to_string());
        query.to = Some("2026-11-12T00:00:01Z".to_string());
        let page = listing(&dir, &query);
        let ids: Vec<&str> = page.items.iter().map(|r| r.call_id.as_str()).collect();
        // A record starting exactly at `from` is listed; one starting
        // exactly at `to` is not (the fixture's BASE_MS row and the three
        // edge rows are inside; the +1s row is outside).
        assert!(ids.contains(&call_id_at(BASE_MS, 21).as_str()), "{ids:?}");
        assert!(ids.contains(&call_id_at(BASE_MS, 23).as_str()));
        assert!(!ids.contains(&call_id_at(BASE_MS + 1_000, 24).as_str()));
        assert!(!ids.contains(&call_id_at(BASE_MS + 4_000, 4).as_str()));
        // Sub-second bounds hold to the microsecond.
        query.from = Some("2026-11-12T00:00:00.5Z".to_string());
        query.to = Some("2026-11-12T00:00:01Z".to_string());
        let page = listing(&dir, &query);
        let ids: Vec<&str> = page.items.iter().map(|r| r.call_id.as_str()).collect();
        assert!(ids.contains(&call_id_at(BASE_MS, 22).as_str()));
        assert!(ids.contains(&call_id_at(BASE_MS, 23).as_str()));
        assert!(!ids.contains(&call_id_at(BASE_MS, 21).as_str()), "{ids:?}");
        // An unparseable bound, and `to` at or before `from`, refuse.
        for (from, to) in [
            ("not-a-time", "2026-11-12T00:00:01Z"),
            ("2026-11-12T00:00:01Z", "2026-11-12T00:00:01Z"),
            ("2026-11-12T00:00:02Z", "2026-11-12T00:00:01Z"),
            ("2026-11-12T00:00:00Z", "2026-11-12T00:00:00+01:00"),
        ] {
            let mut query = CallsQuery::all();
            query.from = Some(from.to_string());
            query.to = Some(to.to_string());
            assert_eq!(
                read_page(&dir, &query).unwrap_err().code(),
                "invalid-time",
                "{from}..{to}"
            );
        }
    }

    #[test]
    fn limit_zero_serves_the_maximum_and_above_it_clamps() {
        let (_tmp, dir) = fixture();
        let mut query = CallsQuery::all();
        query.limit = 0;
        assert_eq!(listing(&dir, &query).items.len(), 5);
        query.limit = CALLS_MAX_LIMIT + 5;
        // Clamps to the maximum: the five seeded records, one cursor-less
        // page.
        let page = listing(&dir, &query);
        assert_eq!(page.items.len(), 5);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn two_segments_merge_newest_first_and_the_cursor_crosses_the_boundary() {
        let (_tmp, dir) = fixture();
        // An OLDER segment holding a record that sorts between the newest
        // segment's rows.
        seed_segment(
            &dir,
            "2026-11-11.db",
            2,
            &[Row::new(&call_id_at(BASE_MS + 3_500, 31), OLD)],
        );
        let mut query = CallsQuery::all();
        query.limit = 2;
        let first = listing(&dir, &query);
        // Newest first across BOTH segments.
        assert_eq!(first.items[0].call_id, call_id_at(BASE_MS + 4_000, 4));
        assert_eq!(first.items[1].call_id, call_id_at(BASE_MS + 3_500, 31));
        let mut seen: Vec<String> = first.items.iter().map(|r| r.call_id.clone()).collect();
        let mut cursor = first.next_cursor.clone();
        while let Some(cursor_value) = cursor {
            let mut next = CallsQuery::all();
            next.limit = 2;
            next.after = Some(cursor_value);
            let page = listing(&dir, &next);
            seen.extend(page.items.iter().map(|r| r.call_id.clone()));
            cursor = page.next_cursor.clone();
        }
        // Every record once, no gap, no duplicate.
        assert_eq!(seen.len(), 6);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 6, "no duplicates");
        let mut descending = seen.clone();
        descending.sort_by(|a, b| b.cmp(a));
        assert_eq!(seen, descending, "descending across the boundary");
    }

    /// Every page of `query` (its own limit) from the first to the end,
    /// with `budget`: the ids in listing order, and each page's
    /// `searched_back_to`.
    fn walk_all(dir: &Path, query: &CallsQuery, budget: usize) -> (Vec<String>, usize) {
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut next = query.clone();
            next.after = cursor.clone();
            let page = read_page_with_budget(dir, &next, budget).unwrap();
            pages += 1;
            assert!(pages < 200, "the cursor must advance");
            if let (Some(back_to), Some(last)) = (&page.searched_back_to, page.items.last()) {
                assert!(
                    *back_to <= last.started_at,
                    "searched_back_to {back_to} is newer than an item on its own page"
                );
            }
            seen.extend(page.items.iter().map(|r| r.call_id.clone()));
            match page.next_cursor {
                Some(next_cursor) => cursor = Some(next_cursor),
                None => break,
            }
        }
        (seen, pages)
    }

    /// A row whose stored start agrees with its id (`BASE_MS + offset`).
    fn row_at(offset: u64, nonce: u8, model: &'static str) -> Row {
        let mut row = Row::new(&call_id_at(BASE_MS + offset, nonce), PARENT);
        let micros = (offset % 60_000) * 1_000;
        row.started_at = format!(
            "2026-11-12T00:{:02}:{:02}.{:06}Z",
            offset / 60_000,
            micros / 1_000_000,
            micros % 1_000_000
        );
        row.model = Some(model);
        row
    }

    #[test]
    fn interleaved_segments_page_with_no_gap_and_no_duplicate_whatever_their_names_say() {
        // Review 1, F1: the newest-named segment, an older-named one whose
        // ids fall BETWEEN the newest's, and an even older-named one whose
        // ids are ABOVE every other (a clock step across a rotation, a
        // restored backup). Name order is not id order.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        let newest = [100, 96, 92, 91, 90];
        let between = [95, 94, 93];
        let above = [120, 110];
        let seed = |name: &str, offsets: &[u64], nonce: u8, model: &'static str| {
            let rows: Vec<Row> = offsets
                .iter()
                .map(|offset| row_at(offset * 1_000, nonce, model))
                .collect();
            seed_segment(&dir, name, 2, &rows);
        };
        seed("2026-11-12.db", &newest, 1, "hay");
        seed("2026-11-11.db", &between, 2, "needle");
        seed("2026-11-10.db", &above, 3, "hay");
        let mut expected: Vec<String> = Vec::new();
        for (offsets, nonce) in [(&newest[..], 1), (&between[..], 2), (&above[..], 3)] {
            for offset in offsets {
                expected.push(call_id_at(BASE_MS + offset * 1_000, nonce));
            }
        }
        expected.sort_by(|a, b| b.cmp(a));
        for limit in [1, 2, 3, 4, 50] {
            let mut query = CallsQuery::all();
            query.limit = limit;
            let (seen, _) = walk_all(&dir, &query, CALLS_SCAN_BUDGET);
            assert_eq!(seen, expected, "limit {limit}");
            // A budget smaller than a page: still every record, once.
            for budget in [1, 2, 3, 5] {
                let (seen, _) = walk_all(&dir, &query, budget);
                assert_eq!(seen, expected, "limit {limit}, budget {budget}");
            }
        }
        // A sparse filter under a tight budget finds every needle, once,
        // in order, and never names a searched-back time newer than a page
        // item (asserted in the walk).
        let mut query = CallsQuery::all();
        query.model = Some("needle".to_string());
        query.limit = 1;
        for budget in [1, 2, 4] {
            let (seen, _) = walk_all(&dir, &query, budget);
            let needles: Vec<String> = between
                .iter()
                .map(|offset| call_id_at(BASE_MS + offset * 1_000, 2))
                .collect();
            assert_eq!(seen, needles, "budget {budget}");
        }
    }

    #[test]
    fn a_replayed_id_in_two_segments_lists_once_and_never_ends_the_read_early() {
        // Review 1, F4: a re-created segment holding a copy of a newer
        // file's record must neither list it twice nor make a full page
        // look like the last one.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        seed_segment(
            &dir,
            "2026-11-12.db",
            2,
            &[row_at(50_000, 1, "gpt-demo"), row_at(40_000, 1, "gpt-demo")],
        );
        seed_segment(
            &dir,
            "2026-11-11.db",
            2,
            &[row_at(50_000, 1, "gpt-demo"), row_at(30_000, 1, "gpt-demo")],
        );
        let mut query = CallsQuery::all();
        query.limit = 2;
        let first = listing(&dir, &query);
        assert_eq!(
            first.items.iter().map(|r| &r.call_id).collect::<Vec<_>>(),
            [
                &call_id_at(BASE_MS + 50_000, 1),
                &call_id_at(BASE_MS + 40_000, 1)
            ]
        );
        assert!(first.next_cursor.is_some(), "a record remains below");
        let (seen, _) = walk_all(&dir, &query, CALLS_SCAN_BUDGET);
        assert_eq!(seen.len(), 3, "{seen:?}");
    }

    #[test]
    fn a_file_with_no_calls_table_or_an_unreadable_row_is_skipped_not_fatal() {
        // Review 1, F5 and F6: another application's SQLite file (a
        // plausible version stamp, no calls table) and a segment holding a
        // row that will not decode are skipped and named; the other
        // segments still serve, and the by-id read never calls a record it
        // could not read "not found".
        let (_tmp, dir) = fixture();
        {
            let conn = Connection::open(dir.join("2026-11-13.db")).unwrap();
            conn.execute_batch("CREATE TABLE other (x INTEGER); PRAGMA user_version = 1;")
                .unwrap();
        }
        let bad = call_id_at(BASE_MS + 9_000, 9);
        seed_segment(&dir, "2026-11-09.db", 2, &[Row::new(&bad, OLD)]);
        {
            let conn = Connection::open(dir.join("2026-11-09.db")).unwrap();
            conn.execute("UPDATE calls SET latency_us = 'not a number'", [])
                .unwrap();
        }
        let page = listing(&dir, &CallsQuery::all());
        assert_eq!(page.items.len(), 5, "the readable segment still serves");
        assert_eq!(page.skipped_segments, 2, "{:?}", page.skipped_files);
        for name in ["2026-11-13.db", "2026-11-09.db"] {
            assert!(
                page.skipped_files.iter().any(|note| note.contains(name)),
                "{name}: {:?}",
                page.skipped_files
            );
        }
        // By id: a record in a readable segment is served past the foreign
        // file; the unreadable row is a surfaced failure, not a 404; an id
        // no file holds is still "not found" only when every file read.
        let good = call_id_at(BASE_MS + 4_000, 4);
        assert_eq!(read_call(&dir, &good).unwrap().call_id, good);
        assert_eq!(
            read_call(&dir, &bad).unwrap_err().code(),
            "calls-unavailable"
        );
        std::fs::remove_file(dir.join("2026-11-09.db")).unwrap();
        std::fs::remove_file(dir.join("2026-11-13.db")).unwrap();
        assert_eq!(read_call(&dir, &bad).unwrap_err().code(), "call-not-found");
    }

    #[test]
    fn skipped_segments_are_counted_named_and_never_fail_the_listing() {
        let (_tmp, dir) = fixture();
        // A segment written by a newer build, and a non-SQLite file with
        // the .db extension.
        {
            let conn = Connection::open(dir.join("2026-11-13.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE calls (call_id TEXT PRIMARY KEY); PRAGMA user_version = 4;",
            )
            .unwrap();
        }
        std::fs::write(dir.join("2026-11-10.db"), b"not sqlite at all").unwrap();
        let page = listing(&dir, &CallsQuery::all());
        assert_eq!(page.skipped_segments, 2, "{:?}", page.skipped_files);
        assert_eq!(page.items.len(), 5, "the readable segments still serve");
        assert!(
            page.skipped_files
                .iter()
                .any(|note| note.contains("2026-11-13.db") && note.contains("newer Hekma")),
            "{:?}",
            page.skipped_files
        );
        assert!(
            page.skipped_files
                .iter()
                .any(|note| note.contains("2026-11-10.db")),
            "{:?}",
            page.skipped_files
        );
        // The document counts them.
        let document = page.into_document(CallsQuery::all().echoed_filters());
        assert_eq!(document.skipped_segments, 2);
    }

    #[test]
    fn a_missing_directory_is_an_empty_listing_and_a_schema_1_segment_reads() {
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp.path().join("calls");
        let page = listing(&absent, &CallsQuery::all());
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert_eq!(page.skipped_segments, 0);
        // Review 1, F2: with no store yet, a malformed query is still
        // refused, never an empty page.
        let refusals: [(Setter, &str); 4] = [
            (
                Box::new(|q| q.after = Some("not-a-cursor".into())),
                "invalid-cursor",
            ),
            (
                Box::new(|q| q.outcome = Some("bogus".into())),
                "invalid-filter",
            ),
            (
                Box::new(|q| q.from = Some("yesterday".into())),
                "invalid-time",
            ),
            (Box::new(|q| q.run = Some("nope".into())), "invalid-run"),
        ];
        for (set, code) in refusals {
            let mut query = CallsQuery::all();
            set(&mut query);
            assert_eq!(read_page(&absent, &query).unwrap_err().code(), code);
        }
        // A schema 1 segment (no read indexes) reads correctly.
        let dir = tmp.path().join("calls2");
        seed_segment(
            &dir,
            "2026-11-12.db",
            1,
            &[
                Row::new(&call_id_at(BASE_MS, 0), PARENT),
                Row::new(&call_id_at(BASE_MS + 1_000, 1), CHILD),
            ],
        );
        let page = listing(&dir, &CallsQuery::all());
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.skipped_segments, 0);
        let mut query = CallsQuery::all();
        query.run = Some(CHILD.to_string());
        assert_eq!(listing(&dir, &query).items.len(), 1);
    }

    #[test]
    fn the_scan_budget_bounds_a_sparse_filter_and_says_so() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        // More rows than a lowered budget, a filter that matches almost
        // nothing.
        let rows: Vec<Row> = (0..50)
            .map(|index| {
                let mut row = Row::new(&call_id_at(BASE_MS + index, (index % 32) as u8), PARENT);
                row.model = Some(if index == 25 { "needle" } else { "hay" });
                row
            })
            .collect();
        seed_segment(&dir, "2026-11-12.db", 2, &rows);
        // The scan budget lowered to 10 rows for this test's reads.
        let listing =
            |dir: &Path, query: &CallsQuery| read_page_with_budget(dir, query, 10).unwrap();
        let mut query = CallsQuery::all();
        query.model = Some("needle".to_string());
        let page = listing(&dir, &query);
        // The budget stopped the page before it filled: partial, honest,
        // continuing.
        assert!(page.scan_limited, "{:?}", page.items.len());
        assert!(page.next_cursor.is_some());
        assert!(page.searched_back_to.is_some());
        // Following the cursors reaches an exhausted read without losing
        // the needle.
        let mut found = page.items.clone();
        let mut cursor = page.next_cursor.clone();
        let mut hops = 0;
        while let Some(cursor_value) = cursor {
            let mut next = query.clone();
            next.after = Some(cursor_value);
            let page = listing(&dir, &next);
            found.extend(page.items.iter().cloned());
            cursor = page.next_cursor.clone();
            hops += 1;
            assert!(hops < 20, "the cursor must advance");
        }
        assert_eq!(found.len(), 1, "the needle is found exactly once");
        assert_eq!(found[0].model.as_deref(), Some("needle"));
        // The same data through the problems view is served by its index
        // with no scan limit (the partial index walks only problems).
        let mut problems = query.clone();
        problems.view = CallView::Problems;
        let page = listing(&dir, &problems);
        assert!(!page.scan_limited);
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn a_page_that_fills_before_the_budget_is_a_normal_page() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        let rows: Vec<Row> = (0..30)
            .map(|index| Row::new(&call_id_at(BASE_MS + index, (index % 32) as u8), PARENT))
            .collect();
        seed_segment(&dir, "2026-11-12.db", 2, &rows);
        // The scan budget lowered to 10 rows for this test's reads.
        let listing =
            |dir: &Path, query: &CallsQuery| read_page_with_budget(dir, query, 10).unwrap();
        let mut query = CallsQuery::all();
        query.limit = 5;
        let page = listing(&dir, &query);
        assert_eq!(page.items.len(), 5);
        assert!(page.next_cursor.is_some());
        assert!(!page.scan_limited, "the page filled first");
        assert!(page.searched_back_to.is_none());
    }

    #[test]
    fn the_by_id_probe_serves_an_older_segment_and_refuses_shapes() {
        let (_tmp, dir) = fixture();
        seed_segment(
            &dir,
            "2026-11-11.db",
            2,
            &[Row::new(&call_id_at(BASE_MS - 1_000, 41), OLD)],
        );
        let old = call_id_at(BASE_MS - 1_000, 41);
        let record = read_call(&dir, &old).unwrap();
        assert_eq!(record.call_id, old);
        assert_eq!(record.run_id.as_str(), OLD);
        // A known id from the newest segment.
        let newest = call_id_at(BASE_MS + 4_000, 4);
        assert_eq!(read_call(&dir, &newest).unwrap().call_id, newest);
        // An unknown well-formed id is NotFound, naming the id.
        let missing = call_id_at(BASE_MS + 100_000, 0);
        match read_call(&dir, &missing) {
            Err(CallError::NotFound { call_id }) => assert_eq!(call_id, missing),
            other => panic!("expected NotFound, got {other:?}"),
        }
        // A malformed id refuses.
        assert_eq!(
            read_call(&dir, "not-a-call-id").unwrap_err().code(),
            "invalid-call-id"
        );
        // A missing directory is NotFound, never an error about the dir.
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            read_call(&tmp.path().join("calls"), &missing),
            Err(CallError::NotFound { .. })
        ));
    }

    #[test]
    fn a_credential_shaped_model_is_never_returned_and_unknown_values_render() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("calls");
        // A full credential shape (prefix + 52 body characters).
        const CRED: &str = "hk_run_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(CRED.len(), "hk_run_".len() + 52);
        seed_segment(
            &dir,
            "2026-11-12.db",
            2,
            &[
                Row {
                    model: Some(CRED),
                    ..Row::new(&call_id_at(BASE_MS, 0), PARENT)
                },
                Row {
                    model: Some("sk-live-abc123"),
                    ..Row::new(&call_id_at(BASE_MS + 1_000, 1), PARENT)
                },
                Row {
                    outcome: "superceded",
                    disposition: "quarantine",
                    status_class: "1xx",
                    status: Some(999),
                    ..Row::new(&call_id_at(BASE_MS + 2_000, 2), PARENT)
                },
            ],
        );
        let page = listing(&dir, &CallsQuery::all());
        assert_eq!(page.items[2].model.as_deref(), Some("[redacted]"));
        // The stored value never appears in the whole document.
        let document = serde_json::to_string(&page).unwrap();
        assert!(!document.contains(CRED), "the credential never leaks");
        assert!(document.contains("sk-live-abc123"), "another shape shows");
        // Unknown enum values render as raw strings; a tampered status
        // (999 is no HTTP status this surface knows) reads as absent.
        assert_eq!(page.items[0].outcome.as_str(), "superceded");
        assert_eq!(page.items[0].disposition.as_str(), "quarantine");
        assert_eq!(page.items[0].status_class.as_str(), "1xx");
        assert_eq!(page.items[0].status, None);
        // The by-id probe masks too.
        let record = read_call(&dir, &call_id_at(BASE_MS, 0)).unwrap();
        assert_eq!(record.model.as_deref(), Some("[redacted]"));
    }

    // -----------------------------------------------------------------------
    // The plan pins: every production statement at schema 2 reads an index.
    // -----------------------------------------------------------------------

    /// EXPLAIN QUERY PLAN's detail rows for a statement.
    fn plan(dir: &Path, statement: &CallsStatement) -> Vec<String> {
        let conn = Connection::open(dir.join("2026-11-12.db")).unwrap();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", statement.sql))
            .unwrap();
        stmt.query_map(rusqlite::params_from_iter(statement.params.iter()), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn resolved(query: &CallsQuery) -> ResolvedCallsQuery {
        resolve_calls_query(query).unwrap()
    }

    #[test]
    fn every_production_statement_reads_an_index_with_no_sort_and_no_bare_scan() {
        let (_tmp, dir) = fixture();
        let statements: Vec<(&str, CallsStatement)> = vec![
            (
                "the unscoped page",
                page_statement(&resolved(&CallsQuery::all()), 51),
            ),
            (
                "the Run-scoped page",
                page_statement(
                    &resolved(&{
                        let mut query = CallsQuery::all();
                        query.run = Some(PARENT.to_string());
                        query
                    }),
                    51,
                ),
            ),
            (
                "the problems page",
                page_statement(
                    &resolved(&{
                        let mut query = CallsQuery::all();
                        query.view = CallView::Problems;
                        query
                    }),
                    51,
                ),
            ),
            ("the by-id probe", probe_statement(&call_id_at(BASE_MS, 0))),
            (
                "the windowed page",
                page_statement(
                    &resolved(&{
                        let mut query = CallsQuery::all();
                        query.from = Some("2026-11-12T00:00:00Z".to_string());
                        query.to = Some("2026-11-12T01:00:00Z".to_string());
                        query
                    }),
                    51,
                ),
            ),
        ];
        for (name, statement) in &statements {
            let rows = plan(&dir, statement);
            let joined = rows.join(" | ");
            assert!(
                joined.contains("USING INDEX"),
                "{name}: the plan must read an index — {joined}"
            );
            assert!(
                !joined.contains("TEMP B-TREE"),
                "{name}: no sort may stand behind the page — {joined}"
            );
            assert!(
                !rows.iter().any(|row| row.trim() == "SCAN calls"),
                "{name}: a bare table scan is forbidden — {joined}"
            );
        }
        // The specific index each shape chose.
        let run_scoped = plan(
            &dir,
            &page_statement(
                &resolved(&{
                    let mut query = CallsQuery::all();
                    query.run = Some(PARENT.to_string());
                    query
                }),
                51,
            ),
        )
        .join(" | ");
        assert!(
            run_scoped.contains("idx_calls_run_call"),
            "the Run-scoped keyset reads (run_id, call_id) — {run_scoped}"
        );
        let problems = plan(
            &dir,
            &page_statement(
                &resolved(&{
                    let mut query = CallsQuery::all();
                    query.view = CallView::Problems;
                    query
                }),
                51,
            ),
        )
        .join(" | ");
        assert!(
            problems.contains("idx_calls_problem"),
            "the problems view reads its partial index — {problems}"
        );
        // The problems statement carries the index predicate literally.
        let statement = page_statement(
            &resolved(&{
                let mut query = CallsQuery::all();
                query.view = CallView::Problems;
                query
            }),
            51,
        );
        assert!(
            statement
                .sql
                .contains(crate::domain::calls::CALLS_PROBLEM_WHERE),
            "the statement repeats the index's WHERE text"
        );
    }
}
