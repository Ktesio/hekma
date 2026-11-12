//! Run Key issuance, refresh and revocation as store-writer job bodies
//! (story 22-1, spine AD-22 / AD-25 / AD-27).
//!
//! Every change here writes the `tokens` row, notes one [`CredentialEvent`]
//! per token and the matching key-index change into the job's
//! [`RunChanges`]; [`super::ops::commit`] appends the events to the outbox
//! in the same transaction and publishes the key index with the Run entries
//! after commit — so a revocation is durable and visible to the data path
//! before the caller is acknowledged.
//!
//! The cleartext never enters a job: a token is minted BEFORE its job (an
//! RNG failure refuses before anything is written) and only its hash and
//! token id reach the store.

use super::ops::RunChanges;
use super::snapshot::{KeyChange, KeyEntry};
use super::{Anomaly, RunActor, RunError, RunRecord};
use crate::domain::credential::{
    CredentialChange, CredentialEvent, RevokeOutcome, RevokedCredential, TokenClass, TokenHash,
    TokenState, TokenStub, CREDENTIAL_EVENT_SCHEMA_VERSION,
};
use crate::domain::usage::RunId;
use crate::ports::{RunRecords, StoreError, TokenRow};

/// Why a credential left service (the `cause` on its event and row).
pub(crate) mod cause {
    /// Its supervised process was confirmed exited.
    pub(crate) const PROCESS_EXITED: &str = "process-exited";
    /// Its Run ended.
    pub(crate) const RUN_ENDED: &str = "run-ended";
    /// The Operator revoked it (`hekma run revoke-keys`).
    pub(crate) const OPERATOR: &str = "operator";
    /// The Operator's cascade reached it.
    pub(crate) const CASCADE: &str = "operator-cascade";
    /// A refresh replaced it.
    pub(crate) const REFRESHED: &str = "refreshed";
    /// A used refresh credential was presented again.
    pub(crate) const REFRESH_REUSE: &str = "refresh-reuse";
    /// It was seen in a URL.
    pub(crate) const EXPOSED_IN_URL: &str = "exposed-in-url";
}

/// What to record for one issued token.
pub(crate) struct Issue {
    pub token: TokenStub,
    pub launch_generation: Option<u64>,
    pub paired: Option<TokenHash>,
    pub issued_by_hash: Option<TokenHash>,
    /// `operator`, `host`, `engine`, or the issuing credential's token id.
    pub issuer: String,
    pub custody_eligible: bool,
    pub prior_token_id: Option<String>,
}

/// Insert `issue`'s token for `run` and note its `issued` event and (for a
/// Run Key or announce credential of a non-ended Run) its key-index entry.
pub(crate) fn issue<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run: &RunRecord,
    issue: Issue,
    at: &str,
) -> Result<(), StoreError> {
    let hash = issue.token.hash;
    let class = issue.token.class;
    let token_id = hash.token_id();
    store.insert_token(&TokenRow {
        hash,
        token_id: token_id.clone(),
        class,
        run_id: run.run_id.clone(),
        launch_generation: issue.launch_generation,
        paired_hash: issue.paired,
        issued_by_hash: issue.issued_by_hash,
        custody_eligible: issue.custody_eligible,
        state: TokenState::Active,
        issued_at: at.to_string(),
        revoked_at: None,
        revoke_cause: None,
    })?;
    changes.credentials.push(CredentialEvent {
        schema_version: CREDENTIAL_EVENT_SCHEMA_VERSION,
        change: CredentialChange::Issued,
        class,
        token_id: token_id.clone(),
        run_id: run.run_id.clone(),
        launch_generation: issue.launch_generation,
        issued_by: Some(issue.issuer),
        prior_token_id: issue.prior_token_id,
        cause: None,
        at: at.to_string(),
    });
    if class != TokenClass::Refresh && !run.is_ended() {
        changes.keys.push(KeyChange::Add(
            hash,
            KeyEntry {
                class,
                run_id: run.run_id.clone(),
                token_id,
                launch_generation: issue.launch_generation,
                custody_eligible: issue.custody_eligible,
                process_owner: run.process_owner,
            },
        ));
    }
    Ok(())
}

/// Issue a Run Key and its paired refresh credential for `run` (a Control
/// API child open, a refresh, a supervised launch whose Adapter maps a
/// refresh variable).
#[allow(clippy::too_many_arguments)]
pub(crate) fn issue_pair<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run: &RunRecord,
    key: TokenStub,
    refresh: TokenStub,
    launch_generation: Option<u64>,
    issued_by_hash: Option<TokenHash>,
    issuer: &str,
    custody_eligible: bool,
    prior: Option<(String, String)>,
    at: &str,
) -> Result<(), StoreError> {
    let (prior_key, prior_refresh) = match prior {
        Some((key, refresh)) => (Some(key), Some(refresh)),
        None => (None, None),
    };
    issue(
        store,
        changes,
        run,
        Issue {
            token: key,
            launch_generation,
            paired: Some(refresh.hash),
            issued_by_hash,
            issuer: issuer.to_string(),
            custody_eligible,
            prior_token_id: prior_key,
        },
        at,
    )?;
    issue(
        store,
        changes,
        run,
        Issue {
            token: refresh,
            launch_generation,
            paired: Some(key.hash),
            issued_by_hash,
            issuer: issuer.to_string(),
            custody_eligible,
            prior_token_id: prior_refresh,
        },
        at,
    )
}

/// Move `row` out of service (`revoked`, or `retired` for a used refresh
/// credential) and note `change`. Nothing when it is no longer active.
fn take_out<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    row: &TokenRow,
    state: TokenState,
    change: CredentialChange,
    cause: &str,
    at: &str,
) -> Result<bool, StoreError> {
    if !store.set_token_state(&row.hash, state, at, cause)? {
        return Ok(false);
    }
    changes.credentials.push(CredentialEvent {
        schema_version: CREDENTIAL_EVENT_SCHEMA_VERSION,
        change,
        class: row.class,
        token_id: row.token_id.clone(),
        run_id: row.run_id.clone(),
        launch_generation: row.launch_generation,
        issued_by: None,
        prior_token_id: None,
        cause: Some(cause.to_string()),
        at: at.to_string(),
    });
    if row.class != TokenClass::Refresh {
        changes.keys.push(KeyChange::Remove(row.hash));
    }
    Ok(true)
}

/// Revoke one active row (`revoked` event).
pub(crate) fn revoke<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    row: &TokenRow,
    cause: &str,
    at: &str,
) -> Result<bool, StoreError> {
    take_out(
        store,
        changes,
        row,
        TokenState::Revoked,
        CredentialChange::Revoked,
        cause,
        at,
    )
}

/// Revoke every used (`retired`) refresh credential of `run_id`: once the
/// Run's reuse has been detected or acknowledged, an old refresh credential
/// presented again is a plain `credential-revoked` with no side effect —
/// never a fresh reuse that revokes the Run's current pair. One `revoked`
/// event per row moved.
pub(crate) fn revoke_used<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    cause: &str,
    at: &str,
) -> Result<(), StoreError> {
    revoke_used_where(store, changes, run_id, |_| true, cause, at)
}

/// [`revoke_used`] for the used refresh credentials `which` selects (a
/// launch's exit takes only its launch-issued ones). Wherever a set of a
/// Run's credentials leaves service, its used refresh credentials leave
/// with it: a retired one left behind would stay a kill switch — presented
/// again, it is reuse, which revokes the Run's live pair.
pub(crate) fn revoke_used_where<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    which: impl Fn(&TokenRow) -> bool,
    cause: &str,
    at: &str,
) -> Result<(), StoreError> {
    for row in store.run_tokens(run_id)? {
        if row.state == TokenState::Retired && which(&row) {
            revoke_retired_row(store, changes, &row, cause, at)?;
        }
    }
    Ok(())
}

/// Move one used (`retired`) refresh credential to `revoked` and note its
/// `revoked` event. Nothing when it is no longer retired.
fn revoke_retired_row<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    row: &TokenRow,
    cause: &str,
    at: &str,
) -> Result<bool, StoreError> {
    if !store.revoke_retired(&row.hash, at, cause)? {
        return Ok(false);
    }
    changes.credentials.push(CredentialEvent {
        schema_version: CREDENTIAL_EVENT_SCHEMA_VERSION,
        change: CredentialChange::Revoked,
        class: row.class,
        token_id: row.token_id.clone(),
        run_id: row.run_id.clone(),
        launch_generation: row.launch_generation,
        issued_by: None,
        prior_token_id: None,
        cause: Some(cause.to_string()),
        at: at.to_string(),
    });
    Ok(true)
}

/// Revoke every active token of `run_id` that `which` selects; returns the
/// rows revoked.
pub(crate) fn revoke_run_tokens<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    which: impl Fn(&TokenRow) -> bool,
    cause: &str,
    at: &str,
) -> Result<Vec<TokenRow>, StoreError> {
    let mut revoked = Vec::new();
    for row in store.active_tokens(run_id)? {
        if which(&row) && revoke(store, changes, &row, cause, at)? {
            revoked.push(row);
        }
    }
    Ok(revoked)
}

/// The in-transaction check every Caller-authorized job makes (story 22-1):
/// the presenting Run Key's row is still `active` (a request authenticated
/// before a revocation and executed after it is refused with no effect),
/// and `target` is the key's own Run or one of its descendants. Returns the
/// presenting row for a Caller, `None` for the Operator or a Host.
pub(crate) fn caller_check<S: RunRecords + ?Sized>(
    store: &S,
    actor: &RunActor,
    target: &RunId,
) -> Result<Option<TokenRow>, RunError> {
    let Some(row) = caller_key(store, actor)? else {
        return Ok(None);
    };
    if !within(store, &row.run_id, target)? {
        return Err(RunError::RunOutOfScope {
            run_id: target.as_str().to_string(),
        });
    }
    Ok(Some(row))
}

/// The key half of [`caller_check`]: a Caller's presenting Run Key is still
/// an `active` Run Key of its Run, read inside the job. Returns its row for
/// a Caller, `None` for the Operator or a Host.
pub(crate) fn caller_key<S: RunRecords + ?Sized>(
    store: &S,
    actor: &RunActor,
) -> Result<Option<TokenRow>, RunError> {
    let RunActor::Caller { run_id, token_hash } = actor else {
        return Ok(None);
    };
    let row = store
        .token(token_hash)?
        .ok_or(RunError::CredentialUnknown)?;
    if !row.hash.ct_eq(token_hash)
        || row.state != TokenState::Active
        || row.class != TokenClass::RunKey
        || &row.run_id != run_id
    {
        return Err(RunError::CredentialRevoked);
    }
    Ok(Some(row))
}

/// Whether `target` is `root` or nests beneath it (the parent chain, at most
/// [`super::MAX_RUN_NESTING`] levels). A missing target is out of scope
/// only after its own not-found refusal has had its chance (the caller
/// reads the target first).
pub(crate) fn within<S: RunRecords + ?Sized>(
    store: &S,
    root: &RunId,
    target: &RunId,
) -> Result<bool, StoreError> {
    if target == root {
        return Ok(true);
    }
    Ok(store
        .run_chain(target)?
        .iter()
        .any(|record| &record.run_id == root))
}

/// Revoke every remaining active token of a Run that just ended, and its
/// used refresh credentials (story 22-1: `run-ended`, no cascade —
/// children outlive their parent's end).
pub(crate) fn on_run_ended<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    at: &str,
) -> Result<(), StoreError> {
    revoke_run_tokens(store, changes, run_id, |_| true, cause::RUN_ENDED, at)?;
    revoke_used(store, changes, run_id, cause::RUN_ENDED, at)
}

/// Revoke a supervised launch's credentials once its process is confirmed
/// exited (story 22-1, P1: a key lives exactly as long as its process, and
/// the previous generation is revoked before any relaunch spawns) — its
/// used refresh credentials too, so none can trigger a reuse against the
/// next launch.
pub(crate) fn on_process_exited<S: RunRecords + ?Sized>(
    store: &S,
    changes: &mut RunChanges,
    run_id: &RunId,
    at: &str,
) -> Result<(), StoreError> {
    let launched = |row: &TokenRow| row.launch_generation.is_some();
    revoke_run_tokens(store, changes, run_id, launched, cause::PROCESS_EXITED, at)?;
    revoke_used_where(store, changes, run_id, launched, cause::PROCESS_EXITED, at)
}

/// The refresh job (story 22-1, P3): a Run's active refresh credential
/// buys a new Run Key and refresh pair once.
///
/// * Active: the used credential is retired, its paired Run Key revoked at
///   once, and `pair` issued on the same Run, launch generation and custody
///   flag, `issued_by` = the used credential.
/// * Retired (presented again): the Run's current credentials of the
///   credential's own launch generation are revoked (`refresh-reuse`; a
///   later launch's credentials were never reachable from it), the
///   `credential-reuse` Anomaly is raised, one
///   `reuse-detected` event names the presented credential — and the answer
///   is `credential-revoked`. These changes COMMIT with the refusal (the
///   value's `Err`), so a silent takeover becomes a visible one.
/// * Revoked: `credential-revoked`, nothing written. Unknown (or not a
///   refresh credential): `unauthorized`, nothing written.
pub(crate) fn refresh<S: RunRecords + ?Sized>(
    store: &S,
    presented: &TokenHash,
    pair: (TokenStub, TokenStub),
    at: &str,
) -> Result<(Result<RunId, RunError>, RunChanges), RunError> {
    let mut changes = RunChanges::default();
    let row = match store.token(presented)? {
        Some(row) if row.class == TokenClass::Refresh && row.hash.ct_eq(presented) => row,
        _ => return Err(RunError::CredentialUnknown),
    };
    match row.state {
        TokenState::Revoked => return Err(RunError::CredentialRevoked),
        TokenState::Retired => {
            // Reuse: revoke what the Run holds now in the credential's own
            // generation (its refresh chain), raise the Anomaly. A later
            // launch's key was issued by the engine, never through this
            // chain, so a stale credential of an exited launch is no kill
            // switch for the live one.
            let record = store
                .run_record(&row.run_id)?
                .ok_or(RunError::CredentialRevoked)?;
            changes.credentials.push(CredentialEvent {
                schema_version: CREDENTIAL_EVENT_SCHEMA_VERSION,
                change: CredentialChange::ReuseDetected,
                class: row.class,
                token_id: row.token_id.clone(),
                run_id: row.run_id.clone(),
                launch_generation: row.launch_generation,
                issued_by: None,
                prior_token_id: None,
                cause: Some(cause::REFRESH_REUSE.to_string()),
                at: at.to_string(),
            });
            revoke_run_tokens(
                store,
                &mut changes,
                &row.run_id,
                |active| {
                    active.class != TokenClass::Announce
                        && active.launch_generation == row.launch_generation
                },
                cause::REFRESH_REUSE,
                at,
            )?;
            // The used credentials leave for good: presenting this one (or an
            // older one) again is a plain refusal, not a second reuse.
            revoke_used(store, &mut changes, &row.run_id, cause::REFRESH_REUSE, at)?;
            if !record.is_ended() && !record.anomalies.contains(&Anomaly::CredentialReuse) {
                let mut anomalies = record.anomalies.clone();
                anomalies.push(Anomaly::CredentialReuse);
                let status = record.status;
                super::ops::move_to(store, &mut changes, record, status, anomalies, None, at)?;
            }
            return Ok((Err(RunError::CredentialRevoked), changes));
        }
        TokenState::Active => {}
    }
    let record = store
        .run_record(&row.run_id)?
        .ok_or(RunError::CredentialRevoked)?;
    if record.is_ended() {
        return Err(RunError::CredentialRevoked);
    }
    take_out(
        store,
        &mut changes,
        &row,
        TokenState::Retired,
        CredentialChange::Rotated,
        cause::REFRESHED,
        at,
    )?;
    let mut prior_key = String::new();
    if let Some(paired) = &row.paired_hash {
        if let Some(key) = store.token(paired)? {
            prior_key = key.token_id.clone();
            take_out(
                store,
                &mut changes,
                &key,
                TokenState::Revoked,
                CredentialChange::Rotated,
                cause::REFRESHED,
                at,
            )?;
        }
    }
    let (key, refresh) = pair;
    issue_pair(
        store,
        &mut changes,
        &record,
        key,
        refresh,
        row.launch_generation,
        Some(row.hash),
        &row.token_id,
        row.custody_eligible,
        Some((prior_key, row.token_id.clone())),
        at,
    )?;
    Ok((Ok(record.run_id), changes))
}

/// The Operator's `revoke-keys` job (story 22-1, R1.6): the Run's active
/// credentials and — with `cascade` — every active credential issued under
/// any credential the Run ever held (a child opened with one of its keys,
/// that child's refresh chain). Revocation never ends a Run. Clears the
/// `credential-reuse` Anomaly (the Operator's acknowledgement).
pub(crate) fn revoke_keys<S: RunRecords + ?Sized>(
    store: &S,
    run_id: &RunId,
    cascade: bool,
    at: &str,
) -> Result<(RevokeOutcome, RunChanges), RunError> {
    let mut changes = RunChanges::default();
    let record = store
        .run_record(run_id)?
        .ok_or_else(|| RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        })?;
    let mut revoked = Vec::new();
    for row in revoke_run_tokens(store, &mut changes, run_id, |_| true, cause::OPERATOR, at)? {
        revoked.push(RevokedCredential {
            run_id: row.run_id,
            class: row.class,
            token_id: row.token_id,
            cascaded: false,
        });
    }
    // The Operator's acknowledgement: no used refresh credential of the Run
    // can trigger a reuse afterwards (they were out of service already, so
    // the report lists only what was active).
    revoke_used(store, &mut changes, run_id, cause::OPERATOR, at)?;
    if cascade {
        // The roots are EVERY credential the Run held, whatever its state
        // now: a child opened with a key the Run has since rotated is still
        // the Run's work.
        let roots: Vec<TokenHash> = store.run_tokens(run_id)?.iter().map(|r| r.hash).collect();
        for row in store.active_issued_under(&roots)? {
            if revoke(store, &mut changes, &row, cause::CASCADE, at)? {
                revoked.push(RevokedCredential {
                    run_id: row.run_id,
                    class: row.class,
                    token_id: row.token_id,
                    cascaded: true,
                });
            }
        }
        // The descendants' used refresh credentials leave with them (out of
        // service already, so the report lists only what was active).
        for row in store.retired_issued_under(&roots)? {
            revoke_retired_row(store, &mut changes, &row, cause::CASCADE, at)?;
        }
    }
    let reuse_cleared = record.anomalies.contains(&Anomaly::CredentialReuse);
    if reuse_cleared {
        let anomalies: Vec<Anomaly> = record
            .anomalies
            .iter()
            .copied()
            .filter(|anomaly| *anomaly != Anomaly::CredentialReuse)
            .collect();
        let status = record.status;
        super::ops::move_to(store, &mut changes, record, status, anomalies, None, at)?;
    }
    Ok((
        RevokeOutcome {
            run_id: run_id.clone(),
            cascade,
            revoked,
            reuse_cleared,
        },
        changes,
    ))
}

/// Revoke credentials seen in a URL (story 22-1, Ask First 6): they have
/// reached someone's access log. Only active tokens and used refresh
/// credentials change (a used one so seen would otherwise stay a reuse
/// trigger in that log's readers' hands); an unknown value changes nothing.
pub(crate) fn revoke_exposed<S: RunRecords + ?Sized>(
    store: &S,
    hashes: &[TokenHash],
    at: &str,
) -> Result<RunChanges, StoreError> {
    let mut changes = RunChanges::default();
    for hash in hashes {
        if let Some(row) = store.token(hash)? {
            if !row.hash.ct_eq(hash) {
                continue;
            }
            match row.state {
                TokenState::Active => {
                    revoke(store, &mut changes, &row, cause::EXPOSED_IN_URL, at)?;
                }
                TokenState::Retired => {
                    revoke_retired_row(store, &mut changes, &row, cause::EXPOSED_IN_URL, at)?;
                }
                TokenState::Revoked => {}
            }
        }
    }
    Ok(changes)
}

/// The snapshot's key index at open: every active Run Key and announce
/// credential of a non-ended Run.
pub(crate) fn open_keys<S: RunRecords + ?Sized>(store: &S) -> Result<Vec<KeyChange>, StoreError> {
    let mut keys = Vec::new();
    for row in store.active_key_rows()? {
        let process_owner = store
            .run_record(&row.run_id)?
            .is_some_and(|record| record.process_owner);
        keys.push(KeyChange::Add(
            row.hash,
            KeyEntry {
                class: row.class,
                run_id: row.run_id,
                token_id: row.token_id,
                launch_generation: row.launch_generation,
                custody_eligible: row.custody_eligible,
                process_owner,
            },
        ));
    }
    Ok(keys)
}

/// The child open as `actor` (story 22-1): a Caller's open is checked inside
/// the job — its key still active (first, on every path), the parent within
/// its subtree, and fewer
/// than [`super::MAX_CALLER_OPEN_CHILDREN`] open children under the parent —
/// then the child opens as [`super::ops::open_child`] opens it (its labels
/// are Caller labels through the Caller guard), and `credentials`, when
/// given, are issued to it: `issued_by` = the presenting key for a Caller,
/// the custody flag inherited from it (true for the Operator or a Host).
/// Returns the Run and whether credentials were issued (a retried external
/// id returns the existing Run and issues none: display once).
pub(crate) fn open_child_as<S: RunRecords + crate::ports::RunStore + ?Sized>(
    store: &S,
    actor: &RunActor,
    request: &super::ChildRunRequest,
    run_id: RunId,
    inputs: Option<super::policy::ResolveInputs>,
    credentials: Option<(TokenStub, TokenStub)>,
    at: &str,
) -> Result<((RunId, bool), RunChanges), RunError> {
    // A Caller's key is re-read FIRST, on every path: a key revoked between
    // authentication and this job is refused here whatever the request
    // names — an unresolvable parent or a retried external id included.
    let caller = caller_key(store, actor)?;
    if let Some(row) = &caller {
        let parent_id = match &request.parent {
            super::RunParent::Run(id) => Some(id.clone()),
            super::RunParent::Instance(name) => crate::domain::InstanceName::new(name)
                .ok()
                .map(|name| store.open_process_run(&name))
                .transpose()?
                .flatten()
                .map(|record| record.run_id),
        };
        // No parent resolves: the open's own refusal names it.
        if let Some(parent_id) = parent_id {
            if store.run_record(&parent_id)?.is_none() {
                return Err(RunError::NotFound {
                    run_id: parent_id.as_str().to_string(),
                });
            }
            if !within(store, &row.run_id, &parent_id)? {
                return Err(RunError::RunOutOfScope {
                    run_id: parent_id.as_str().to_string(),
                });
            }
            if store.open_children(&parent_id)? >= super::MAX_CALLER_OPEN_CHILDREN {
                return Err(RunError::TooManyChildren {
                    parent: parent_id.as_str().to_string(),
                    max: super::MAX_CALLER_OPEN_CHILDREN,
                });
            }
        }
    }
    let minted = run_id.clone();
    let caller_inputs = inputs.clone();
    // A Caller's external ids are its own Run's namespace: a retry by the
    // same Run's keys returns the same child, and no Run Key can capture or
    // probe an id the Operator, a Host or another Run's keys will use.
    let binding = match actor {
        RunActor::Caller { run_id: root, .. } => caller_identity_binding(root),
        RunActor::Operator | RunActor::Host => super::HOST_IDENTITY_BINDING.to_string(),
    };
    let (opened, mut changes) =
        super::ops::open_child_bound(store, request, run_id, inputs, &binding, at)?;
    // A Caller's open-time ceilings are the Caller's OWN layer (FR-60: a
    // Caller only tightens), never the Run's Operator/Host layer, which
    // outranks every Policy: `open_child` validated and wrote them as the
    // Run's rows, so they move to the Caller layer and the Run re-resolves.
    if caller.is_some() && opened == minted && request.limits.is_set() {
        store.replace_run_limits(
            &opened,
            crate::ports::LimitSlot::Run,
            &super::RunLimits::default(),
            "caller",
            at,
        )?;
        store.replace_run_limits(
            &opened,
            crate::ports::LimitSlot::Caller,
            &request.limits,
            "caller",
            at,
        )?;
        changes.extend(super::policy::resolve_run(
            store,
            &opened,
            caller_inputs,
            at,
        )?);
    }
    if opened != minted {
        // A retried external id: the existing Run, no new credentials. Under
        // a Caller's own binding it is a child a key of the same Run opened
        // beneath that Run (Runs never re-parent), so it is in scope.
        return Ok(((opened, false), changes));
    }
    let Some((key, refresh)) = credentials else {
        return Ok(((opened, false), changes));
    };
    let record = store
        .run_record(&opened)?
        .ok_or_else(|| RunError::NotFound {
            run_id: opened.as_str().to_string(),
        })?;
    issue_pair(
        store,
        &mut changes,
        &record,
        key,
        refresh,
        None,
        caller.as_ref().map(|row| row.hash),
        &actor.issuer(),
        caller
            .as_ref()
            .map(|row| row.custody_eligible)
            .unwrap_or(true),
        None,
        at,
    )?;
    Ok(((opened, true), changes))
}

/// The identity binding a Caller's external ids resolve under: one
/// namespace per Run whose key opens (`caller:<run id>`), apart from the
/// Operator's and Hosts' `host` binding.
pub(crate) fn caller_identity_binding(root: &RunId) -> String {
    format!("caller:{}", root.as_str())
}

/// Close a child Run as `actor` (story 22-1): a Caller may close only within
/// its own Run's subtree, with its key re-validated in the job.
pub(crate) fn close_as<S: RunRecords + ?Sized>(
    store: &S,
    actor: &RunActor,
    run_id: &RunId,
    at: &str,
) -> Result<RunChanges, RunError> {
    if matches!(actor, RunActor::Caller { .. }) {
        if store.run_record(run_id)?.is_none() {
            return Err(RunError::NotFound {
                run_id: run_id.as_str().to_string(),
            });
        }
        caller_check(store, actor, run_id)?;
    }
    super::ops::close(store, run_id, at)
}
