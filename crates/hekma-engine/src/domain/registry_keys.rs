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

    /// What a presented credential is, read from the store.
    pub(crate) fn credential_state(
        &self,
        presented: &SecretString,
    ) -> Result<PresentedState, RunError> {
        let hash = hash_presented(presented);
        Ok(self.store.snapshot(|store| store.token(&hash))?.map_or(
            PresentedState::Unknown,
            |row| match row.state {
                TokenState::Active if row.hash.ct_eq(&hash) => {
                    PresentedState::Active(CredentialMatch {
                        class: row.class,
                        run_id: row.run_id,
                        token_id: row.token_id,
                        supervised: row.launch_generation.is_some()
                            && row.class != TokenClass::Announce,
                        hash: row.hash,
                    })
                }
                TokenState::Active => PresentedState::Unknown,
                TokenState::Retired | TokenState::Revoked => PresentedState::Revoked,
            },
        ))
    }
}
