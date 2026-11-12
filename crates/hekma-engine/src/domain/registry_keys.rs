//! The Registry's credential reads (story 22-1): the metadata listing and
//! the store-side state of a presentation the snapshot did not recognise.
//! Both read on the read-only connection; neither returns a value or a hash.

use crate::domain::credential::{
    hash_presented, CredentialMatch, CredentialSummary, PresentedState, TokenClass, TokenState,
};
use crate::domain::runs::RunError;
use crate::domain::usage::RunId;
use crate::domain::{Registry, SecretString};
use crate::ports::RunRecords;

impl Registry {
    /// Every credential of one Run, any state, oldest first: metadata only.
    pub(crate) fn run_keys(&self, run_id: &RunId) -> Result<Vec<CredentialSummary>, RunError> {
        let rows = self.store.snapshot(|store| {
            if store.run_record(run_id)?.is_none() {
                return Ok(None);
            }
            Ok(Some(store.run_tokens(run_id)?))
        })?;
        let rows = rows.ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
        Ok(rows
            .into_iter()
            .map(|row| CredentialSummary {
                token_id: row.token_id,
                class: row.class,
                state: row.state,
                launch_generation: row.launch_generation,
                issued_by: row.issued_by_hash.map(|hash| hash.token_id()),
                issued_at: row.issued_at,
                revoked_at: row.revoked_at,
                revoke_cause: row.revoke_cause,
            })
            .collect())
    }

    /// A second read-only connection to this registry's state.db, for the
    /// credential lookups an unauthenticated request can cause (story 22-1):
    /// the engine holds it beside — never behind — the registry mutex, so an
    /// unresolvable presentation never queues behind supervision and never
    /// reaches the store writer.
    pub(crate) fn open_credential_reader(
        &self,
        read_only: bool,
    ) -> Result<crate::store::SqliteStore, crate::ports::StoreError> {
        let db = self.paths.state_db();
        if !read_only {
            crate::store::SqliteStore::open_read_only(&db)
        } else if db.exists() {
            crate::store::SqliteStore::open_existing_read_only(&db)
        } else {
            crate::store::SqliteStore::open_empty_read_only()
        }
    }
}

/// What a presented credential is, read from `store` (a read-only
/// connection): unknown, revoked, a used refresh credential, or active.
pub(crate) fn presented_state(
    store: &crate::store::SqliteStore,
    presented: &SecretString,
) -> Result<PresentedState, RunError> {
    let hash = hash_presented(presented);
    Ok(store
        .snapshot(|store| store.token(&hash))?
        .map_or(PresentedState::Unknown, |row| {
            if !row.hash.ct_eq(&hash) {
                return PresentedState::Unknown;
            }
            let matched = CredentialMatch {
                class: row.class,
                run_id: row.run_id,
                token_id: row.token_id,
                supervised: row.launch_generation.is_some() && row.class != TokenClass::Announce,
                hash: row.hash,
            };
            match row.state {
                TokenState::Active => PresentedState::Active(matched),
                TokenState::Retired => PresentedState::Used(matched),
                TokenState::Revoked => PresentedState::Revoked,
            }
        }))
}
