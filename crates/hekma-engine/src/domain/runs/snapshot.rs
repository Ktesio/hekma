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
    /// A revoked (or rotated) key leaves it; a Run Key joins the
    /// recently-revoked set (story 23-1).
    Remove(TokenHash),
    /// A Run Key revoked before this open (story 23-1): the
    /// recently-revoked set's load, newest first.
    Revoked(TokenHash),
}

/// How long a revoked Run Key is told apart from an unknown one (story
/// 23-1): 24 hours.
pub(crate) const RECENTLY_REVOKED_SECS: u64 = 24 * 60 * 60;

/// The most hashes the recently-revoked set keeps; the oldest are evicted.
pub(crate) const RECENTLY_REVOKED_MAX: usize = 100_000;

/// The bounded recently-revoked Run Key set (story 23-1, R1.8): a revoked
/// key answers `credential_revoked` rather than `unknown_credential` for
/// 24 hours, at most [`RECENTLY_REVOKED_MAX`] hashes (oldest first out).
#[derive(Clone, Debug, Default)]
pub(crate) struct RevokedSet {
    hashes: std::collections::HashSet<TokenHash>,
    order: std::collections::VecDeque<(TokenHash, std::time::Instant)>,
}

impl RevokedSet {
    /// Whether `hash` was revoked recently.
    pub(crate) fn contains(&self, hash: &TokenHash) -> bool {
        self.hashes.contains(hash)
    }

    /// The number of hashes held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Record a revocation at `at`, evicting past the age and size bounds.
    pub(crate) fn insert(&mut self, hash: TokenHash, at: std::time::Instant) {
        if self.hashes.insert(hash) {
            self.order.push_back((hash, at));
        }
        self.evict(at);
    }

    fn evict(&mut self, now: std::time::Instant) {
        let age = std::time::Duration::from_secs(RECENTLY_REVOKED_SECS);
        while let Some((hash, at)) = self.order.front().copied() {
            let expired = now.checked_duration_since(at).is_some_and(|d| d > age);
            if self.order.len() > RECENTLY_REVOKED_MAX || expired {
                self.order.pop_front();
                self.hashes.remove(&hash);
            } else {
                break;
            }
        }
    }
}

/// The `block` and breach reasons of a Run (story 23-1, AD-25): one bit
/// per `(dimension, scope)` in [`crate::domain::FENCE_REASON_ORDER`].
/// Shaped for the hold/reject/reroute reasons a later story adds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Disposition {
    /// Reasons latched under `block`: the proxy refuses the Run's (and its
    /// descendants') non-safe calls while any is set.
    pub breach_block: u8,
    /// Reasons latched under any action: `x-hekma-breach` names them.
    pub breach_latched: u8,
}

impl Disposition {
    /// The bit for one reason.
    pub(crate) fn bit(reason: crate::domain::LatchKey) -> u8 {
        crate::domain::FENCE_REASON_ORDER
            .iter()
            .position(|candidate| *candidate == reason)
            .map(|index| 1u8 << index)
            .unwrap_or(0)
    }

    /// The reasons set in `bits`, in the canonical order.
    pub(crate) fn reasons(bits: u8) -> Vec<crate::domain::LatchKey> {
        crate::domain::FENCE_REASON_ORDER
            .iter()
            .enumerate()
            .filter(|(index, _)| bits & (1 << index) != 0)
            .map(|(_, reason)| *reason)
            .collect()
    }

    /// The disposition of a Run's latch rows.
    pub(crate) fn of_latches(
        latches: &[(crate::domain::LatchKey, crate::domain::BreachAction)],
    ) -> Self {
        let mut disposition = Self::default();
        for (key, action) in latches {
            let bit = Self::bit(*key);
            disposition.breach_latched |= bit;
            if *action == crate::domain::BreachAction::Block {
                disposition.breach_block |= bit;
            }
        }
        disposition
    }
}

/// What a snapshot entry carries beyond the Run's own row (story 23-1),
/// read in the job that publishes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EntryExtras {
    pub disposition: Disposition,
    /// A process-owning Run's recorded upstream.
    pub upstream: Option<Arc<str>>,
    /// The Run's `(namespace, external id)` identity, when it has one.
    pub identity: Option<(String, String)>,
    /// The Run's held-key custody (story 22-2): the reference and the
    /// frozen origin, when custody is on.
    pub custody: Option<crate::domain::custody::CustodyInfo>,
}

/// One Operator endpoint binding as the data path sees it (story 23-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingEntry {
    pub name: String,
    /// `/` plus segments, no trailing slash (`/` itself is the catch-all).
    pub prefix: String,
    pub upstream: Arc<str>,
    /// The bound labels its Runs carry (sorted by key).
    pub labels: Vec<(String, String)>,
    /// The binding's inert custodied-key reference (story 22-2: a
    /// `secret:NAME` string, never a value; the swap never draws on it
    /// without a custody-eligible credential).
    pub custodied_key: Option<String>,
}

impl BindingEntry {
    /// The rest of `path` after this binding's prefix, when the prefix
    /// matches on a segment boundary (`/openai` matches `/openai` and
    /// `/openai/v1`, never `/openai2`).
    pub(crate) fn strip<'a>(&self, path: &'a str) -> Option<&'a str> {
        if self.prefix == "/" {
            return Some(path);
        }
        let rest = path.strip_prefix(self.prefix.as_str())?;
        (rest.is_empty() || rest.starts_with('/')).then_some(rest)
    }
}

/// Every binding, longest prefix first (so the first match is the longest).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BindingTable {
    entries: Vec<Arc<BindingEntry>>,
}

impl BindingTable {
    /// A table from `entries` in any order.
    pub(crate) fn new(mut entries: Vec<BindingEntry>) -> Self {
        entries.sort_by(|a, b| {
            b.prefix
                .len()
                .cmp(&a.prefix.len())
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        Self {
            entries: entries.into_iter().map(Arc::new).collect(),
        }
    }

    /// The longest binding matching `path` and the rest of the path.
    pub(crate) fn matching<'a>(&self, path: &'a str) -> Option<(&Arc<BindingEntry>, &'a str)> {
        self.entries
            .iter()
            .find_map(|entry| entry.strip(path).map(|rest| (entry, rest)))
    }

    /// The binding named `name` (the custody lookup and the tests').
    pub(crate) fn by_name(&self, name: &str) -> Option<&Arc<BindingEntry>> {
        self.entries.iter().find(|entry| entry.name == name)
    }
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
    /// Its `block` and breach reasons (story 23-1), rebuilt from its
    /// durable latch rows by every job that publishes the entry.
    pub disposition: Disposition,
    /// A process-owning Run's recorded upstream (story 23-1).
    pub upstream: Option<Arc<str>>,
    /// Its `(namespace, external id)` identity (story 23-1).
    pub identity: Option<(String, String)>,
    /// Its held-key custody (story 22-2): the `secret:NAME` reference and
    /// the frozen origin, when custody is on. A child Run reads up the
    /// chain ([`RunSnapshot::custody_of`]) — a child of a custodied
    /// supervised Run keeps custody.
    pub custody: Option<crate::domain::custody::CustodyInfo>,
}

/// Build one snapshot entry: the record, its labels and limits, its stored
/// resolution when one exists, and the extras the data path reads.
pub(crate) fn entry_of(
    record: &RunRecord,
    labels: &[Label],
    limits: RunLimits,
    resolution: Option<&PolicyResolution>,
    extras: EntryExtras,
) -> RunEntry {
    RunEntry {
        record: record.clone(),
        labels: labels.to_vec(),
        limits,
        policy: resolution.cloned(),
        disposition: extras.disposition,
        upstream: extras.upstream,
        identity: extras.identity,
        custody: extras.custody,
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
    /// Story 23-1: the Run Keys revoked in the last 24 hours (bounded).
    pub revoked: Arc<RevokedSet>,
    /// Story 23-1: the Operator endpoint bindings.
    pub bindings: Arc<BindingTable>,
    /// Story 23-1: open Runs by their `(namespace, external id)` identity.
    pub identities: HashMap<(String, String), RunId>,
    /// Story 23-1: the one adopted pre-proxy observed Run that receives
    /// calls matching no binding and no Run route (the legacy root route).
    pub legacy_root: Option<RunId>,
    /// Story 23-1: Run routes published before their Run's launch job
    /// commits (the agent may call as soon as it is spawned): the upstream
    /// and the Run-start `block` reasons, until the Run's own entry carries
    /// them.
    pub pending_routes: HashMap<RunId, (Arc<str>, u8)>,
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

    /// The ancestor chain of `run_id` from the Run itself up, at most
    /// [`super::MAX_RUN_NESTING`] levels (story 23-1: the fence and the
    /// breach header walk it). Stops at a parent that is not open.
    pub(crate) fn chain(&self, run_id: &RunId) -> Vec<&Arc<RunEntry>> {
        let mut chain = Vec::new();
        let mut next = self.runs.get(run_id);
        while let Some(entry) = next {
            chain.push(entry);
            if chain.len() >= super::MAX_RUN_NESTING {
                break;
            }
            next = entry
                .record
                .parent_run_id
                .as_ref()
                .and_then(|parent| self.runs.get(parent));
        }
        chain
    }

    /// Whether `run_id` is `ancestor` or beneath it (story 23-1: a Run Key
    /// on a Run route must name the route's Run or a descendant).
    pub(crate) fn is_within(&self, run_id: &RunId, ancestor: &RunId) -> bool {
        self.chain(run_id)
            .iter()
            .any(|entry| entry.record.run_id == *ancestor)
    }

    /// The `block` reasons over a Run's whole ancestor chain, and the
    /// latched reasons of any action (story 23-1).
    pub(crate) fn disposition_over(&self, run_id: &RunId) -> Disposition {
        let mut over = Disposition::default();
        let chain = self.chain(run_id);
        for entry in &chain {
            over.breach_block |= entry.disposition.breach_block;
            over.breach_latched |= entry.disposition.breach_latched;
            if let Some((_, bits)) = self.pending_routes.get(&entry.record.run_id) {
                over.breach_block |= bits;
            }
        }
        if chain.is_empty() {
            if let Some((_, bits)) = self.pending_routes.get(run_id) {
                over.breach_block |= bits;
            }
        }
        over
    }

    /// The upstream a Run route forwards to: the first recorded upstream up
    /// the Run's chain, else its pending route's (story 23-1).
    pub(crate) fn route_upstream(&self, run_id: &RunId) -> Option<Arc<str>> {
        self.chain(run_id)
            .iter()
            .find_map(|entry| entry.upstream.clone())
            .or_else(|| {
                self.pending_routes
                    .get(run_id)
                    .map(|(up, _)| Arc::clone(up))
            })
    }

    /// The custody a call attributed to `run_id` may draw on (story 22-2):
    /// the FIRST held-key custody up the Run's chain — a supervised Run's
    /// own, or the process-owning ancestor's for its in-process children
    /// (a child of a custodied supervised Run keeps custody) — else, for a
    /// binding's Run, a split beneath it or a Run beneath either, the
    /// binding's custodied key, pinned to the binding's upstream. The
    /// caller draws on it ONLY for a call authenticated by a
    /// custody-eligible Run Key (P4): a binding's Run reached with no
    /// credential never does.
    pub(crate) fn custody_of(&self, run_id: &RunId) -> Option<crate::domain::custody::CustodyInfo> {
        let chain = self.chain(run_id);
        if let Some(found) = chain.iter().find_map(|entry| entry.custody.clone()) {
            return Some(found);
        }
        chain.iter().find_map(|entry| {
            let (namespace, external) = entry.identity.as_ref()?;
            let binding = if namespace == super::bindings::BINDING_NAMESPACE {
                external.as_str()
            } else {
                namespace.strip_prefix("binding:")?
            };
            let binding = self.bindings.by_name(binding)?;
            let key_ref = binding.custodied_key.as_deref()?;
            Some(crate::domain::custody::CustodyInfo {
                key_ref: Arc::from(key_ref),
                origin: Arc::clone(&binding.upstream),
            })
        })
    }

    /// The next snapshot with `changes` applied: an ended entry leaves the
    /// index, any other replaces (or adds) its Run; then the key changes.
    fn with(&self, changes: Vec<RunEntry>, keys: Vec<KeyChange>) -> Self {
        let mut next = self.clone();
        next.generation = self.generation.wrapping_add(1);
        let now = std::time::Instant::now();
        let mut revoked: Option<RevokedSet> = None;
        for change in keys {
            match change {
                KeyChange::Add(hash, entry) => {
                    next.keys.insert(hash, Arc::new(entry));
                }
                KeyChange::Remove(hash) => {
                    if let Some(entry) = next.keys.remove(&hash) {
                        if entry.class == TokenClass::RunKey {
                            revoked
                                .get_or_insert_with(|| (*self.revoked).clone())
                                .insert(hash, now);
                        }
                    }
                }
                KeyChange::Revoked(hash) => {
                    revoked
                        .get_or_insert_with(|| (*self.revoked).clone())
                        .insert(hash, now);
                }
            }
        }
        if let Some(revoked) = revoked {
            next.revoked = Arc::new(revoked);
        }
        for entry in changes {
            let id = entry.record.run_id.clone();
            let owner = entry
                .record
                .process_owner
                .then(|| entry.record.instance.clone())
                .flatten();
            // The identity index follows the entry (a replaced entry's old
            // identity leaves it first).
            if let Some(previous) = next.runs.get(&id) {
                if let Some(identity) = &previous.identity {
                    if next.identities.get(identity) == Some(&id) {
                        next.identities.remove(identity);
                    }
                }
            }
            if entry.record.is_ended() {
                next.runs.remove(&id);
                if let Some(instance) = owner {
                    if next.owners.get(&instance) == Some(&id) {
                        next.owners.remove(&instance);
                    }
                }
                if next.legacy_root.as_ref() == Some(&id) {
                    next.legacy_root = None;
                }
            } else {
                if let Some(instance) = owner {
                    next.owners.insert(instance, id.clone());
                }
                if let Some(identity) = &entry.identity {
                    next.identities.insert(identity.clone(), id.clone());
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
        EngineEvent::Policy(_)
        | EngineEvent::BreachDefaultUpgraded(_)
        | EngineEvent::Binding(_)
        | EngineEvent::SecretRevealed(_) => return SubjectLabels::NoSubject,
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

    /// Replace the binding table (story 23-1: a binding job's post-commit
    /// hook; at most 256 entries, so a whole-table swap).
    pub(crate) fn publish_bindings(&self, table: BindingTable) {
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut next = (**guard).clone();
        next.generation = next.generation.wrapping_add(1);
        next.bindings = Arc::new(table);
        *guard = Arc::new(next);
    }

    /// Publish (or retire) a pending Run route (story 23-1).
    pub(crate) fn publish_pending_route(&self, run: RunId, route: Option<(Arc<str>, u8)>) {
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut next = (**guard).clone();
        next.generation = next.generation.wrapping_add(1);
        match route {
            Some(route) => {
                next.pending_routes.insert(run, route);
            }
            None => {
                next.pending_routes.remove(&run);
            }
        }
        *guard = Arc::new(next);
    }

    /// Set (or clear) the legacy root route (story 23-1: the supervisor
    /// decides it once at open).
    pub(crate) fn publish_legacy_root(&self, run: Option<RunId>) {
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.legacy_root == run {
            return;
        }
        let mut next = (**guard).clone();
        next.generation = next.generation.wrapping_add(1);
        next.legacy_root = run.filter(|id| next.runs.contains_key(id));
        *guard = Arc::new(next);
    }

    /// Replace the whole Run and key state from a fresh read (story 23-1:
    /// the foreign-commit reload). Unlike [`Self::publish_changes`], Runs the
    /// fresh read does not hold LEAVE the snapshot (a foreign engine ended
    /// them), the key index is rebuilt from the active keys, and the
    /// recently-revoked set merges the fresh read's revocations with the ones
    /// already held (both are bounded and age out together). Pending routes
    /// and the legacy root survive (this engine's own in-memory decisions).
    pub(crate) fn publish_reload(&self, entries: Vec<RunEntry>, keys: Vec<KeyChange>) {
        let mut guard = match self.current.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut next = (**guard).clone();
        next.generation = next.generation.wrapping_add(1);
        next.runs.clear();
        next.owners.clear();
        next.identities.clear();
        next.keys.clear();
        for entry in &entries {
            let id = entry.record.run_id.clone();
            if let Some(instance) = entry
                .record
                .process_owner
                .then(|| entry.record.instance.clone())
                .flatten()
            {
                next.owners.insert(instance, id.clone());
            }
            if let Some(identity) = &entry.identity {
                next.identities.insert(identity.clone(), id.clone());
            }
            next.runs.insert(id, Arc::new(entry.clone()));
        }
        if let Some(root) = next.legacy_root.clone() {
            if !next.runs.contains_key(&root) {
                next.legacy_root = None;
            }
        }
        let now = std::time::Instant::now();
        let mut revoked = (*next.revoked).clone();
        for change in keys {
            match change {
                KeyChange::Add(hash, entry) => {
                    next.keys.insert(hash, Arc::new(entry));
                }
                KeyChange::Remove(hash) | KeyChange::Revoked(hash) => {
                    revoked.insert(hash, now);
                }
            }
        }
        next.revoked = Arc::new(revoked);
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
            disposition: Disposition::default(),
            upstream: None,
            identity: None,
            custody: None,
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

    fn hash(n: u32) -> TokenHash {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        TokenHash::from_bytes(bytes)
    }

    #[test]
    fn a_revoked_key_is_told_apart_for_24_hours_then_forgotten() {
        let start = std::time::Instant::now();
        let mut set = RevokedSet::default();
        set.insert(hash(1), start);
        set.insert(hash(1), start); // a repeat is one entry, not two
        assert_eq!(set.len(), 1);
        let day = std::time::Duration::from_secs(RECENTLY_REVOKED_SECS);
        set.insert(hash(2), start + day);
        assert!(set.contains(&hash(1)), "exactly 24 hours is still recent");
        set.insert(hash(3), start + day + std::time::Duration::from_secs(1));
        assert!(!set.contains(&hash(1)), "past 24 hours it is unknown again");
        assert!(set.contains(&hash(2)) && set.contains(&hash(3)));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn the_revoked_set_keeps_at_most_its_bound_oldest_out() {
        let now = std::time::Instant::now();
        let mut set = RevokedSet::default();
        let max = u32::try_from(RECENTLY_REVOKED_MAX).unwrap();
        for n in 0..=max {
            set.insert(hash(n), now);
        }
        assert_eq!(set.len(), RECENTLY_REVOKED_MAX);
        assert!(!set.contains(&hash(0)), "the oldest is evicted first");
        assert!(set.contains(&hash(1)) && set.contains(&hash(max)));
    }
}
