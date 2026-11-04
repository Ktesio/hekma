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

use super::{Label, RunLimits, RunRecord};
use crate::domain::usage::RunId;

/// One non-ended Run as the data path sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunEntry {
    pub record: RunRecord,
    pub labels: Vec<Label>,
    pub limits: RunLimits,
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

    /// The next snapshot with `changes` applied: an ended entry leaves the
    /// index, any other replaces (or adds) its Run.
    fn with(&self, changes: Vec<RunEntry>) -> Self {
        let mut next = self.clone();
        next.generation = self.generation.wrapping_add(1);
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
    pub(crate) fn publish(&self, changes: Vec<RunEntry>) {
        if changes.is_empty() {
            return;
        }
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = guard.with(changes);
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
