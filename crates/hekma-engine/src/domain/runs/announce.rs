//! Announcing (story 23-2, FR-52, spine AD-25 / AD-26).
//!
//! A framework or plain client that Hekma did not start tells Hekma who it
//! is by **announcing** an identity: an external id beneath the binding it
//! calls through (`binding:<name>` + id — the namespace a hint split uses)
//! or beneath a Run (`run:<parent>` + id — a hint child's). The registry
//! key is the unification: a Run discovered on the wire and later
//! announced under the same identity stays ONE Run.
//!
//! Two channels share [`announce`]: the `x-hekma-announce` request header
//! on a proxied call and `POST /v0/runs/announce` under a binding's
//! `hk_ann_` credential. A claim that CREATES a Run mints its Run Key and
//! refresh credential exactly once (custody-eligible only on the endpoint,
//! which presented a credential); a re-announce renews the Lease and
//! upserts Caller labels and never mints, so a revoked key cannot be
//! re-obtained by announcing.
//!
//! Announcing is attribution, not authentication: on this machine any
//! process can announce any identity.

use super::lease::{note_activity, plus, LeaseOptions};
use super::ops::{self, RunChanges};
use super::{
    keys, labels, ControlDepth, Label, LabelOrigin, LabelSource, RunActor, RunError, RunId,
    RunRecord, RunStatus, MAX_CALLER_OPEN_CHILDREN, MAX_ROOT_OPEN_DESCENDANTS, MAX_RUN_NESTING,
};
use crate::domain::credential::{TokenHash, TokenStub};
use crate::ports::{NewRun, RunRecords, StoreError};
use crate::store::SqliteStore;

/// Where an announced identity lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AnnounceScope {
    /// Beneath a binding (`binding:<name>`), carrying its labels as bound.
    Binding(String),
    /// Beneath a Run (`run:<parent>`): a child.
    Parent(RunId),
}

/// One claim, as either channel sends it to the writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnnounceClaim {
    pub scope: AnnounceScope,
    pub external: String,
    /// Caller labels (already validated), applied through the Caller guard.
    pub caller_labels: Vec<(String, String)>,
    /// The endpoint channel: an `hk_ann_` was presented, so the minted key
    /// is custody-eligible and the Run records its creator.
    pub credentialed: bool,
    /// The presenting `hk_ann_`'s hash (the endpoint channel).
    pub created_by: Option<TokenHash>,
    /// Who issued the minted pair (`announce`, or the `hk_ann_`'s token id).
    pub issuer: String,
    /// The header channel: the announcing call is the Run's wire path.
    pub wire: bool,
}

impl AnnounceClaim {
    /// The `(namespace, external id)` key in `run_identities`.
    pub(crate) fn key(&self) -> (String, String) {
        let namespace = match &self.scope {
            AnnounceScope::Binding(binding) => super::bindings::split_namespace(binding),
            AnnounceScope::Parent(parent) => format!("run:{}", parent.as_str()),
        };
        (namespace, self.external.clone())
    }
}

/// What [`announce`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Claimed {
    /// A new Run took the identity; `minted` when its pair was issued.
    Created { run: RunId, minted: bool },
    /// The identity's open Run was renewed (no mint).
    Renewed { run: RunId },
    /// A breadth cap is reached: nothing was created.
    Capped,
    /// The parent (or binding) is gone: nothing was created.
    ParentGone,
}

/// The writer job both channels share: claim `claim`'s identity — renew
/// its open Run, or create one (releasing an ended Run's identity first)
/// and issue `pair` to it.
pub(crate) fn announce(
    store: &SqliteStore,
    claim: &AnnounceClaim,
    run_id: RunId,
    pair: Option<(TokenStub, TokenStub)>,
    options: &LeaseOptions,
    now_unix: u64,
) -> Result<(Claimed, RunChanges), RunError> {
    let at = crate::time::format_rfc3339(now_unix);
    let mut changes = RunChanges::default();
    let (namespace, external) = claim.key();
    let caller: Vec<Label> = claim
        .caller_labels
        .iter()
        .map(|(k, v)| Label::caller(k.clone(), v.clone()))
        .collect();
    if let Some(existing) = store.identity_run(&namespace, &external)? {
        match store.run_record(&existing)? {
            Some(record) if !record.is_ended() => {
                renew(
                    store,
                    &mut changes,
                    record,
                    claim,
                    &caller,
                    options,
                    now_unix,
                )?;
                return Ok((Claimed::Renewed { run: existing }, changes));
            }
            _ => store.release_identity(&namespace, &external)?,
        }
    }
    let (parent, instance, held, bound) = match &claim.scope {
        AnnounceScope::Binding(binding) => {
            let Some(row) = store.binding(binding)? else {
                return Ok((Claimed::ParentGone, changes));
            };
            if store.open_identities_in(&namespace)? >= MAX_CALLER_OPEN_CHILDREN {
                return Ok((Claimed::Capped, changes));
            }
            let bound: Vec<Label> = row
                .labels
                .iter()
                .map(|(k, v)| Label::bound_by(k.clone(), v.clone(), LabelOrigin::Binding))
                .collect();
            (None, None, false, bound)
        }
        AnnounceScope::Parent(parent) => {
            let Some(parent) = store.run_record(parent)? else {
                return Ok((Claimed::ParentGone, changes));
            };
            if parent.is_ended() {
                return Ok((Claimed::ParentGone, changes));
            }
            if store.open_children(&parent.run_id)? >= MAX_CALLER_OPEN_CHILDREN
                || store.run_chain(&parent.run_id)?.len() >= MAX_RUN_NESTING
                || root_is_full(store, &parent.run_id)?
            {
                return Ok((Claimed::Capped, changes));
            }
            let held = parent.status == RunStatus::Held;
            (Some(parent.run_id), parent.instance, held, Vec::new())
        }
    };
    labels::check_caller_labels(&run_id, &bound, &caller)?;
    let mut all = bound;
    all.extend(caller);
    all.sort();
    let record = RunRecord {
        run_id: run_id.clone(),
        control_depth: if claim.wire {
            ControlDepth::Announced
        } else {
            ControlDepth::Observed
        },
        wire_path: claim.wire,
        status: if held {
            RunStatus::Held
        } else {
            RunStatus::Active
        },
        anomalies: Vec::new(),
        parent_run_id: parent,
        instance,
        process_owner: false,
        end_cause: None,
        created_at: at.clone(),
        updated_at: at.clone(),
        ended_at: None,
    };
    store.insert_run(&NewRun {
        record: record.clone(),
        labels: all,
        limits: super::RunLimits::default(),
        identity: Some((namespace, external)),
    })?;
    store.set_run_lease(
        &run_id,
        Some(&plus(now_unix, options.lease_secs)),
        Some(&at),
    )?;
    if let Some(hash) = &claim.created_by {
        store.set_run_created_by(&run_id, hash.as_bytes())?;
    }
    ops::note_created(
        store,
        &mut changes,
        &record,
        held.then(|| "parent-paused".to_string()),
    )?;
    changes.extend(super::policy::resolve_run(store, &run_id, None, &at)?);
    if let Some(entry) = ops::refreshed_entry(store, &run_id)? {
        changes.entries.push(entry);
    }
    let minted = pair.is_some();
    if let Some((key, refresh)) = pair {
        keys::issue_pair(
            store,
            &mut changes,
            &record,
            key,
            refresh,
            None,
            claim.created_by,
            &claim.issuer,
            claim.credentialed,
            None,
            &at,
        )?;
    }
    Ok((
        Claimed::Created {
            run: run_id,
            minted,
        },
        changes,
    ))
}

/// A re-announce of an open identity: the depth moves to `announced` when
/// the Run now has a wire path (one `depth-changed`, id kept), Caller labels
/// upsert through the guard (one `labels-changed` when they differ), and
/// the Lease renews (a `silent` Run returns to `active`). Never a mint.
fn renew(
    store: &SqliteStore,
    changes: &mut RunChanges,
    record: RunRecord,
    claim: &AnnounceClaim,
    caller: &[Label],
    options: &LeaseOptions,
    now_unix: u64,
) -> Result<(), RunError> {
    let at = crate::time::format_rfc3339(now_unix);
    let run_id = record.run_id.clone();
    let current = store.run_labels(&run_id)?;
    labels::check_caller_labels(&run_id, &current, caller)?;
    let wire_path = record.wire_path || claim.wire;
    let depth = match record.control_depth {
        ControlDepth::Observed | ControlDepth::Wire if wire_path => ControlDepth::Announced,
        other => other,
    };
    changes.extend(ops::sync_depth(store, record, depth, wire_path, &at)?);
    let mut prior: Vec<Label> = current
        .iter()
        .filter(|label| label.source == LabelSource::Caller)
        .cloned()
        .collect();
    prior.sort();
    let mut next = caller.to_vec();
    next.sort();
    if prior != next {
        store.replace_labels(&run_id, LabelSource::Caller, &next)?;
        if let Some(mut record) = store.run_record(&run_id)? {
            record.updated_at = at.clone();
            store.save_run(&record)?;
            ops::note_caller_labels(store, changes, &record, "announce")?;
        }
    }
    note_activity(store, changes, &run_id, options, now_unix, true)?;
    Ok(())
}

/// Whether the root of `parent`'s chain already has
/// [`MAX_ROOT_OPEN_DESCENDANTS`] open descendants (the per-root cap).
pub(crate) fn root_is_full<S: RunRecords + ?Sized>(
    store: &S,
    parent: &RunId,
) -> Result<bool, StoreError> {
    let chain = store.run_chain(parent)?;
    let root = chain
        .iter()
        .find(|record| record.parent_run_id.is_none())
        .map(|record| record.run_id.clone())
        .unwrap_or_else(|| parent.clone());
    Ok(store.open_descendants(&root)? >= MAX_ROOT_OPEN_DESCENDANTS)
}

/// What an endpoint announce answered (story 23-2).
#[derive(Debug)]
pub struct Announced {
    /// The announced Run.
    pub view: super::RunView,
    /// Whether this announce created it (`201`) or renewed it (`200`).
    pub created: bool,
    /// Its Run Key and refresh credential — only when this announce created
    /// the Run. Shown once; nothing can show them again.
    pub credentials: Option<crate::domain::credential::IssuedCredentials>,
}

/// Check an announce's external id and Caller labels (the hint rules: 1 to
/// 128 visible ASCII characters; at most 16 labels, each a valid label).
pub(crate) fn validate(external: &str, labels: &[(String, String)]) -> Result<(), RunError> {
    if !super::wire::valid_external_id(external) {
        return Err(RunError::Invalid {
            field: "external_id".to_string(),
            reason: format!(
                "an external id is 1 to {} visible ASCII characters",
                super::wire::MAX_EXTERNAL_ID_CHARS
            ),
        });
    }
    if labels.len() > super::wire::MAX_HINT_LABELS {
        return Err(RunError::Invalid {
            field: "labels".to_string(),
            reason: format!("at most {} labels", super::wire::MAX_HINT_LABELS),
        });
    }
    for (key, value) in labels {
        labels::validate_label_key(key)
            .and_then(|()| labels::validate_label_value(value))
            .map_err(|reason| RunError::Invalid {
                field: "labels".to_string(),
                reason,
            })?;
    }
    Ok(())
}

/// Who heartbeats or closes an announced Run.
#[derive(Clone, Debug)]
pub enum LeaseActor {
    /// The Operator, a Host, or a Run Key's Caller (its own subtree).
    Actor(RunActor),
    /// A binding's announce credential: only Runs it created.
    Announcer(crate::domain::credential::AnnounceCredential),
}

/// Check `actor` may act on `run` (inside the job): a Caller within its
/// subtree with its key still active; an announcer whose credential is
/// still active and created `run`.
fn authorize(store: &SqliteStore, actor: &LeaseActor, run: &RunRecord) -> Result<(), RunError> {
    match actor {
        LeaseActor::Actor(actor) => {
            keys::caller_check(store, actor, &run.run_id)?;
        }
        LeaseActor::Announcer(credential) => {
            match store.announce_credential(&credential.hash)? {
                Some((_, _, true)) => {}
                Some(_) => return Err(RunError::CredentialRevoked),
                None => return Err(RunError::CredentialUnknown),
            }
            let created = store.run_created_by(&run.run_id)?;
            let ours = created
                .as_deref()
                .is_some_and(|bytes| bytes == credential.hash.as_bytes().as_slice());
            if !ours {
                return Err(RunError::RunOutOfScope {
                    run_id: run.run_id.as_str().to_string(),
                });
            }
        }
    }
    Ok(())
}

/// The heartbeat job (story 23-2): extend the Lease of `run_id` (granting
/// one to a Run that held none) and lift it from `silent`. An ended Run
/// answers [`RunError::RunEnded`].
pub(crate) fn heartbeat(
    store: &SqliteStore,
    actor: &LeaseActor,
    run_id: &RunId,
    options: &LeaseOptions,
    now_unix: u64,
) -> Result<RunChanges, RunError> {
    let mut changes = RunChanges::default();
    // A Caller's key is re-read first, whatever the target.
    if let LeaseActor::Actor(actor) = actor {
        keys::caller_key(store, actor)?;
    }
    let Some(record) = store.run_record(run_id)? else {
        return Err(RunError::NotFound {
            run_id: run_id.as_str().to_string(),
        });
    };
    authorize(store, actor, &record)?;
    if record.is_ended() {
        return Err(RunError::RunEnded {
            run_id: run_id.as_str().to_string(),
        });
    }
    note_activity(store, &mut changes, run_id, options, now_unix, true)?;
    Ok(changes)
}

/// Close `run_id` as an announcer (story 23-2: "a Run that announces its
/// end" is the close verb): only a Run its credential created.
pub(crate) fn close_as(
    store: &SqliteStore,
    actor: &LeaseActor,
    run_id: &RunId,
    at: &str,
) -> Result<RunChanges, RunError> {
    match actor {
        LeaseActor::Actor(actor) => keys::close_as(store, actor, run_id, at),
        LeaseActor::Announcer(_) => {
            let Some(record) = store.run_record(run_id)? else {
                return Err(RunError::NotFound {
                    run_id: run_id.as_str().to_string(),
                });
            };
            authorize(store, actor, &record)?;
            ops::close(store, run_id, at)
        }
    }
}

#[cfg(test)]
#[path = "announce_tests.rs"]
mod tests;
