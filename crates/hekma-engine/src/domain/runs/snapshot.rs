//! The published Run snapshot (story 21-2, spine AD-25).
//!
//! The data path (the Metering Proxy from story 23.1 on) never takes an
//! engine mutex and never opens SQLite: it reads an immutable snapshot of the
//! non-ended Runs. The store writer is its only publisher — a Run-changing
//! job registers a post-commit hook carrying the entries it changed, and the
//! hook swaps in the next snapshot before the job's caller is acked.
//!
//! A publish is copy-on-write: the next map shares every unchanged entry by
//! `Arc` and replaces only the changed ones, so it costs O(open Runs) pointer
//! copies; making it O(changed entries) is the scale work of story 24.4.
//! Readers clone the current `Arc` under a read lock held for that clone
//! only. Until one data-path owner exists (story 21.4) each engine process
//! publishes its own snapshot from its own writer.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::{Label, PolicyResolution, RunLimits, RunRecord};
use crate::domain::credential::{TokenClass, TokenHash};
use crate::domain::usage::RunId;

/// One active Run Key or announce credential as the data path sees it
/// (story 22-1): never a value — the map key is the token's hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyEntry {
    pub class: TokenClass,
    pub run_id: RunId,
    pub token_id: String,
    pub launch_generation: Option<u64>,
    /// Recorded at mint for the custody story; read by nothing yet.
    #[allow(dead_code)]
    pub custody_eligible: bool,
    /// Whether the key's Run owns a supervised process.
    pub process_owner: bool,
}

/// One change to the key index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeyChange {
    /// An issued key joins the index.
    Add(TokenHash, KeyEntry),
    /// A revoked (or rotated) key leaves it.
    Remove(TokenHash),
}

/// One non-ended Run as the data path sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunEntry {
    pub record: RunRecord,
    pub labels: Vec<Label>,
    /// The Operator/Host layer of the Run's own limits ONLY (story 22-1,
    /// the 21-6 review's N3): a Caller's layer is not here. The enforceable
    /// view is `policy` — the stored resolution, every layer merged.
    pub limits: RunLimits,
    /// The Run's stored Policy resolution (story 21-3, AD-25: resolved
    /// Policies join the published snapshot, so the data path never
    /// re-implements resolution). `None` before the first resolution.
    pub policy: Option<PolicyResolution>,
}

/// Build one snapshot entry: the record, its labels and limits, and its
/// stored resolution when one exists.
pub(crate) fn entry_of(
    record: &RunRecord,
    labels: &[Label],
    limits: RunLimits,
    resolution: Option<&PolicyResolution>,
) -> RunEntry {
    RunEntry {
        record: record.clone(),
        labels: labels.to_vec(),
        limits,
        policy: resolution.cloned(),
    }
}

/// An immutable view of every non-ended Run.
#[derive(Clone, Debug, Default)]
pub(crate) struct RunSnapshot {
    /// Bumped on every publish.
    pub generation: u64,
    /// Every non-ended Run by id.
    pub runs: HashMap<RunId, Arc<RunEntry>>,
    /// Each instance's open process-owning Run.
    pub owners: HashMap<String, RunId>,
    /// Story 22-1: the active Run Keys and announce credentials of
    /// non-ended Runs, by hash. Refresh credentials are never here (a
    /// refresh is a write, and reuse detection reads retired rows).
    pub keys: HashMap<TokenHash, Arc<KeyEntry>>,
}

// The data path's reads: the Metering Proxy consumes them once it reads the
// snapshot (Run Keys, the shared proxy); the engine's tests read them today.
#[cfg_attr(not(test), allow(dead_code))]
impl RunSnapshot {
    /// The open Run with this id.
    pub(crate) fn run(&self, run_id: &RunId) -> Option<&Arc<RunEntry>> {
        self.runs.get(run_id)
    }

    /// The instance's open process-owning Run.
    pub(crate) fn owner_of(&self, instance: &str) -> Option<&Arc<RunEntry>> {
        self.owners.get(instance).and_then(|id| self.runs.get(id))
    }

    /// The active key with this hash (story 22-1).
    #[cfg(test)]
    pub(crate) fn key(&self, hash: &TokenHash) -> Option<&Arc<KeyEntry>> {
        self.keys.get(hash)
    }

    /// The next snapshot with `changes` applied: an ended entry leaves the
    /// index, any other replaces (or adds) its Run; then the key changes.
    fn with(&self, changes: Vec<RunEntry>, keys: Vec<KeyChange>) -> Self {
        let mut next = self.clone();
        next.generation = self.generation.wrapping_add(1);
        for change in keys {
            match change {
                KeyChange::Add(hash, entry) => {
                    next.keys.insert(hash, Arc::new(entry));
                }
                KeyChange::Remove(hash) => {
                    next.keys.remove(&hash);
                }
            }
        }
        for entry in changes {
            let id = entry.record.run_id.clone();
            let owner = entry
                .record
                .process_owner
                .then(|| entry.record.instance.clone())
                .flatten();
            if entry.record.is_ended() {
                next.runs.remove(&id);
                if let Some(instance) = owner {
                    if next.owners.get(&instance) == Some(&id) {
                        next.owners.remove(&instance);
                    }
                }
            } else {
                if let Some(instance) = owner {
                    next.owners.insert(instance, id.clone());
                }
                next.runs.insert(id, Arc::new(entry));
            }
        }
        next
    }
}

/// What the published snapshot alone answers about an event's subject Run
/// (story 21-5, the SSE Selector filter's lock-free fast path).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubjectLabels {
    /// The subject Run is in the snapshot: its CURRENT labels.
    Found(Vec<Label>),
    /// The event names no Run (`policy`, `breach_default_upgraded`).
    NoSubject,
    /// The event names a Run (or an instance) the snapshot does not hold —
    /// an ended Run, or a transition with no open owner: the store answers.
    NotInSnapshot,
}

/// The subject Run's labels as far as `snapshot` knows them: Run, usage
/// and breach events by their run id, a transition by its instance's open
/// process-owning Run.
pub(crate) fn subject_labels(
    snapshot: &RunSnapshot,
    event: &crate::domain::EngineEvent,
) -> SubjectLabels {
    use crate::domain::EngineEvent;
    let entry = match event {
        EngineEvent::Run(event) => snapshot.run(&event.run_id),
        EngineEvent::UsageUpdate(update) => snapshot.run(&update.event.run_id),
        EngineEvent::BudgetBreach(breach) => snapshot.run(&RunId::from_wire(breach.run_id.clone())),
        EngineEvent::Transition(transition) => snapshot.owner_of(&transition.instance),
        // Story 22-1: a credential event's subject is its Run.
        EngineEvent::Credential(event) => snapshot.run(&event.run_id),
        EngineEvent::Policy(_) | EngineEvent::BreachDefaultUpgraded(_) => {
            return SubjectLabels::NoSubject
        }
    };
    match entry {
        Some(entry) => SubjectLabels::Found(entry.labels.clone()),
        None => SubjectLabels::NotInSnapshot,
    }
}

/// Holds the current snapshot; only store-writer post-commit hooks publish.
#[derive(Debug, Default)]
pub(crate) struct SnapshotPublisher {
    current: RwLock<Arc<RunSnapshot>>,
}

impl SnapshotPublisher {
    /// The current snapshot (a cheap `Arc` clone).
    pub(crate) fn current(&self) -> Arc<RunSnapshot> {
        match self.current.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Publish the next snapshot with `changes` applied. Called only from a
    /// store-writer post-commit hook, so publishes follow commit order.
    #[cfg(test)]
    pub(crate) fn publish(&self, changes: Vec<RunEntry>) {
        self.publish_changes(changes, Vec::new());
    }

    /// Publish Run entries and key-index changes together (story 22-1: a
    /// revocation and the Run end it rides with become visible at once).
    pub(crate) fn publish_changes(&self, changes: Vec<RunEntry>, keys: Vec<KeyChange>) {
        if changes.is_empty() && keys.is_empty() {
            return;
        }
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = guard.with(changes, keys);
        *guard = Arc::new(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runs::{ControlDepth, RunStatus};

    fn entry(id: &str, instance: Option<&str>, owner: bool, status: RunStatus) -> RunEntry {
        RunEntry {
            record: RunRecord {
                run_id: RunId::from_wire(id),
                control_depth: ControlDepth::Supervised,
                wire_path: true,
                status,
                anomalies: Vec::new(),
                parent_run_id: None,
                instance: instance.map(str::to_string),
                process_owner: owner,
                end_cause: None,
                created_at: "t".into(),
                updated_at: "t".into(),
                ended_at: None,
            },
            labels: vec![Label::bound("team", "a")],
            limits: RunLimits::default(),
            policy: None,
        }
    }

    #[test]
    fn publishing_adds_replaces_and_drops_ended_runs() {
        let publisher = SnapshotPublisher::default();
        let before = publisher.current();
        publisher.publish(vec![
            entry("r1", Some("a"), true, RunStatus::Active),
            entry("c1", Some("a"), false, RunStatus::Active),
        ]);
        let first = publisher.current();
        assert_eq!(first.generation, 1);
        assert_eq!(first.runs.len(), 2);
        assert_eq!(first.owner_of("a").unwrap().record.run_id.as_str(), "r1");
        // An older reader keeps its snapshot (immutable).
        assert!(before.runs.is_empty());

        publisher.publish(vec![entry("c1", Some("a"), false, RunStatus::Held)]);
        let second = publisher.current();
        assert_eq!(
            second.run(&RunId::from_wire("c1")).unwrap().record.status,
            RunStatus::Held
        );
        // The unchanged entry is shared, not copied.
        assert!(Arc::ptr_eq(
            first.run(&RunId::from_wire("r1")).unwrap(),
            second.run(&RunId::from_wire("r1")).unwrap()
        ));

        publisher.publish(vec![entry("r1", Some("a"), true, RunStatus::Ended)]);
        let third = publisher.current();
        assert!(third.run(&RunId::from_wire("r1")).is_none());
        assert!(third.owner_of("a").is_none());
        assert_eq!(third.runs.len(), 1);
        // An empty publish is a no-op.
        publisher.publish(Vec::new());
        assert_eq!(publisher.current().generation, third.generation);
    }

    #[test]
    fn an_ended_stale_owner_does_not_drop_the_new_owner() {
        let publisher = SnapshotPublisher::default();
        publisher.publish(vec![entry("new", Some("a"), true, RunStatus::Active)]);
        publisher.publish(vec![entry("old", Some("a"), true, RunStatus::Ended)]);
        assert_eq!(
            publisher
                .current()
                .owner_of("a")
                .unwrap()
                .record
                .run_id
                .as_str(),
            "new"
        );
    }
}
