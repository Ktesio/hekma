//! The credential table (story 22-1, spine AD-22 / AD-27): schema v17 and
//! the SQLite side of the token port on [`crate::ports::RunRecords`].
//!
//! `tokens` keeps the SHA-256 of each credential (the primary key, so a
//! lookup is by the FULL hash) and its metadata — never a value. The token
//! id is a display label: it is stored for records and reports, and no
//! query here looks up, joins or deduplicates on it.
//!
//! v17 is one crash-atomic step (the 11-3 rule): additive DDL with its
//! `user_version = 17` stamp in one `BEGIN IMMEDIATE`, one-way, no backfill
//! (a Run live at upgrade cannot be handed a key — its environment is
//! fixed — so it runs without one until its next launch).

use rusqlite::{Connection, OptionalExtension};

use super::sqlite::backend;
use crate::domain::credential::{TokenClass, TokenHash, TokenState};
use crate::domain::RunId;
use crate::ports::{StoreError, TokenRow};

/// The v17 DDL.
///
/// * `token_hash` — the SHA-256 of the whole token (32 bytes, checked).
/// * `token_id` — the first 12 hex of the hash: a label, deliberately not
///   unique and not indexed.
/// * `launch_generation` — set on a supervised launch's credentials.
/// * `paired_hash` — a Run Key's refresh credential and back.
/// * `issued_by_hash` — the credential it was issued under (a Caller's Run
///   Key, a used refresh credential); NULL when the Operator, a Host or the
///   engine issued it. The cascade walks it.
/// * `custody_eligible` — recorded at mint for the custody story; read by
///   nothing yet.
/// * `state` — `active`, `retired` (a used refresh credential, kept for
///   reuse detection) or `revoked`.
const SCHEMA_V17: &str = "\
CREATE TABLE IF NOT EXISTS tokens (
    token_hash        BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
    token_id          TEXT NOT NULL,
    class             TEXT NOT NULL CHECK(class IN ('run','refresh','announce')),
    run_id            TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    launch_generation INTEGER,
    paired_hash       BLOB,
    issued_by_hash    BLOB,
    custody_eligible  INTEGER NOT NULL DEFAULT 0,
    state             TEXT NOT NULL CHECK(state IN ('active','retired','revoked')),
    issued_at         TEXT NOT NULL,
    revoked_at        TEXT,
    revoke_cause      TEXT
);
CREATE INDEX IF NOT EXISTS idx_tokens_run ON tokens(run_id, class);
CREATE INDEX IF NOT EXISTS idx_tokens_active ON tokens(run_id) WHERE state = 'active';
CREATE INDEX IF NOT EXISTS idx_tokens_issued_by ON tokens(issued_by_hash)
    WHERE issued_by_hash IS NOT NULL;
";

/// Run the v17 step (DDL + stamp) in one `BEGIN IMMEDIATE`, stepping aside
/// when another process finished it while this one waited.
pub(super) fn migrate_v17(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(backend)?;
    let body = || -> Result<bool, StoreError> {
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend)?;
        if version >= 17 {
            return Ok(false);
        }
        conn.execute_batch(SCHEMA_V17).map_err(backend)?;
        conn.execute_batch("PRAGMA user_version = 17")
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

/// The columns every token read selects, in [`read_token`]'s order.
const TOKEN_COLUMNS: &str = "token_hash, token_id, class, run_id, launch_generation, \
     paired_hash, issued_by_hash, custody_eligible, state, issued_at, revoked_at, revoke_cause";

/// A corrupt row (a stored class or state this engine does not know, or a
/// hash of the wrong length) — named, never guessed.
fn corrupt(detail: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(detail.into())),
    )
}

fn hash_of(bytes: Vec<u8>) -> rusqlite::Result<TokenHash> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| corrupt("a token hash is not 32 bytes"))?;
    Ok(TokenHash::from_bytes(array))
}

fn optional_hash(bytes: Option<Vec<u8>>) -> rusqlite::Result<Option<TokenHash>> {
    bytes.map(hash_of).transpose()
}

fn read_token(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenRow> {
    let class: String = row.get(2)?;
    let state: String = row.get(8)?;
    Ok(TokenRow {
        hash: hash_of(row.get(0)?)?,
        token_id: row.get(1)?,
        class: TokenClass::from_store(&class)
            .ok_or_else(|| corrupt(format!("unknown token class `{class}`")))?,
        run_id: RunId::from_wire(row.get::<_, String>(3)?),
        launch_generation: row.get::<_, Option<i64>>(4)?.map(|g| g as u64),
        paired_hash: optional_hash(row.get(5)?)?,
        issued_by_hash: optional_hash(row.get(6)?)?,
        custody_eligible: row.get::<_, i64>(7)? != 0,
        state: TokenState::from_store(&state)
            .ok_or_else(|| corrupt(format!("unknown token state `{state}`")))?,
        issued_at: row.get(9)?,
        revoked_at: row.get(10)?,
        revoke_cause: row.get(11)?,
    })
}

fn corrupt_token(err: rusqlite::Error) -> StoreError {
    match err {
        rusqlite::Error::FromSqlConversionFailure(_, _, inner) => StoreError::CorruptRow {
            name: "tokens".to_string(),
            detail: inner.to_string(),
        },
        other => backend(other),
    }
}

fn collect(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<TokenRow>, StoreError> {
    let mut stmt = conn.prepare_cached(sql).map_err(backend)?;
    let rows = stmt.query_map(params, read_token).map_err(backend)?;
    rows.collect::<Result<_, _>>().map_err(corrupt_token)
}

pub(super) fn insert_token(conn: &Connection, token: &TokenRow) -> Result<(), StoreError> {
    conn.prepare_cached(
        "INSERT INTO tokens (token_hash, token_id, class, run_id, launch_generation, \
         paired_hash, issued_by_hash, custody_eligible, state, issued_at, revoked_at, \
         revoke_cause) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .and_then(|mut stmt| {
        stmt.execute(rusqlite::params![
            &token.hash.as_bytes()[..],
            token.token_id,
            token.class.store_str(),
            token.run_id.as_str(),
            token.launch_generation.map(|g| g as i64),
            token.paired_hash.as_ref().map(|h| h.as_bytes().to_vec()),
            token.issued_by_hash.as_ref().map(|h| h.as_bytes().to_vec()),
            token.custody_eligible as i64,
            token.state.as_str(),
            token.issued_at,
            token.revoked_at,
            token.revoke_cause,
        ])
    })
    .map_err(backend)?;
    Ok(())
}

pub(super) fn token(conn: &Connection, hash: &TokenHash) -> Result<Option<TokenRow>, StoreError> {
    let mut stmt = conn
        .prepare_cached(&format!(
            "SELECT {TOKEN_COLUMNS} FROM tokens WHERE token_hash = ?1"
        ))
        .map_err(backend)?;
    stmt.query_row([&hash.as_bytes()[..]], read_token)
        .optional()
        .map_err(corrupt_token)
}

pub(super) fn set_token_state(
    conn: &Connection,
    hash: &TokenHash,
    state: TokenState,
    at: &str,
    cause: &str,
) -> Result<bool, StoreError> {
    let changed = conn
        .prepare_cached(
            "UPDATE tokens SET state = ?2, revoked_at = ?3, revoke_cause = ?4 \
             WHERE token_hash = ?1 AND state = 'active'",
        )
        .and_then(|mut stmt| {
            stmt.execute(rusqlite::params![
                &hash.as_bytes()[..],
                state.as_str(),
                at,
                cause
            ])
        })
        .map_err(backend)?;
    Ok(changed > 0)
}

pub(super) fn revoke_retired(
    conn: &Connection,
    hash: &TokenHash,
    at: &str,
    cause: &str,
) -> Result<bool, StoreError> {
    let changed = conn
        .prepare_cached(
            "UPDATE tokens SET state = 'revoked', revoked_at = ?2, revoke_cause = ?3 \
             WHERE token_hash = ?1 AND state = 'retired'",
        )
        .and_then(|mut stmt| stmt.execute(rusqlite::params![&hash.as_bytes()[..], at, cause]))
        .map_err(backend)?;
    Ok(changed > 0)
}

pub(super) fn active_tokens(
    conn: &Connection,
    run_id: &RunId,
) -> Result<Vec<TokenRow>, StoreError> {
    collect(
        conn,
        &format!(
            "SELECT {TOKEN_COLUMNS} FROM tokens WHERE run_id = ?1 AND state = 'active' \
             ORDER BY issued_at, rowid"
        ),
        [run_id.as_str()],
    )
}

pub(super) fn run_tokens(conn: &Connection, run_id: &RunId) -> Result<Vec<TokenRow>, StoreError> {
    collect(
        conn,
        &format!("SELECT {TOKEN_COLUMNS} FROM tokens WHERE run_id = ?1 ORDER BY issued_at, rowid"),
        [run_id.as_str()],
    )
}

pub(super) fn active_issued_under(
    conn: &Connection,
    roots: &[TokenHash],
) -> Result<Vec<TokenRow>, StoreError> {
    let mut found: Vec<TokenRow> = Vec::new();
    for root in roots {
        // The walk visits every descendant whatever its state (a revoked
        // child key may still have live grandchildren), and returns the
        // active ones. `UNION` (not `UNION ALL`) ends any cycle.
        let rows = collect(
            conn,
            &format!(
                "WITH RECURSIVE under(hash) AS (\
                     SELECT token_hash FROM tokens WHERE issued_by_hash = ?1 \
                     UNION \
                     SELECT t.token_hash FROM tokens t JOIN under u ON t.issued_by_hash = u.hash\
                 ) \
                 SELECT {TOKEN_COLUMNS} FROM tokens \
                 WHERE token_hash IN (SELECT hash FROM under) AND state = 'active' \
                 ORDER BY issued_at, rowid"
            ),
            [&root.as_bytes()[..]],
        )?;
        for row in rows {
            if !found.iter().any(|seen| seen.hash.ct_eq(&row.hash)) {
                found.push(row);
            }
        }
    }
    Ok(found)
}

pub(super) fn active_key_rows(conn: &Connection) -> Result<Vec<TokenRow>, StoreError> {
    collect(
        conn,
        &format!(
            "SELECT {} FROM tokens t JOIN runs r ON r.run_id = t.run_id \
             WHERE t.state = 'active' AND t.class IN ('run','announce') AND r.status <> 'ended' \
             ORDER BY t.issued_at, t.rowid",
            TOKEN_COLUMNS
                .split(", ")
                .map(|column| format!("t.{column}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        rusqlite::params![],
    )
}
